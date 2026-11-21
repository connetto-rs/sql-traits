//! PostgreSQL virtual generated column type restrictions.

use sqlparser::ast::{ColumnDef, DataType, GeneratedExpressionMode};

use super::{ParserDBBuilder, domains};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, generation_storage},
    utils::object_name::object_name_identifiers,
};

/// Refuses a virtual generated column whose declared type is user-defined.
pub(super) fn column(
    builder: &ParserDBBuilder,
    column: &ColumnDef,
    data_type: &DataType,
) -> Result<(), Error> {
    if !matches!(builder.dialect(), SqlparserDialect::PostgreSql)
        || generation_storage(column, *builder.dialect()) != Some(GeneratedExpressionMode::Virtual)
    {
        return Ok(());
    }
    let Some(name) = domains::custom_name(data_type) else {
        return Ok(());
    };
    let Ok((schema, identifier)) = object_name_identifiers(name) else {
        return Ok(());
    };
    let reached = match schema {
        Some(schema) => {
            domains::reach_in_schema(
                builder,
                identifier,
                (&schema.value, schema.quote_style.is_some()),
            )
        }
        None => domains::reach_type(builder, identifier),
    };
    let column_name = builder.dialect().stored_name(&column.name);
    match reached {
        Some(domains::Reached::Domain(_)) => Err(Error::VirtualGeneratedDomainType { column_name }),
        Some(domains::Reached::UserType(..) | domains::Reached::RowType(_)) => {
            Err(Error::VirtualGeneratedUserDefinedType { column_name })
        }
        Some(domains::Reached::Catalog) | None => Ok(()),
    }
}
