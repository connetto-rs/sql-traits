//! PostgreSQL's `ALTER COLUMN ... SET STORAGE`, which sets the column's TOAST
//! storage mode, `pg_attribute.attstorage`. The model records no storage
//! mode, so the statement changes nothing it holds and only refuses what
//! PostgreSQL refuses.

use sqlparser::ast::{
    ColumnDef, DataType, UserDefinedTypeRepresentation, UserDefinedTypeSqlDefinitionOption,
    UserDefinedTypeStorage,
};

use super::{ParserDBBuilder, domains};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, bound_user_type},
    utils::postgres_types::PgType,
};

/// Refuses a storage mode other than `PLAIN` for a column whose type
/// PostgreSQL keeps plain, judged against the type the column has when the
/// command runs.
///
/// # Errors
///
/// Returns [`Error::ColumnStorageOnlyPlain`] for such a column.
pub(super) fn refuse_storage(
    builder: &ParserDBBuilder,
    declared: &ColumnDef,
    storage: UserDefinedTypeStorage,
) -> Result<(), Error> {
    if !matches!(builder.dialect(), SqlparserDialect::PostgreSql)
        || matches!(storage, UserDefinedTypeStorage::Plain)
        || !stores_plain(builder, &declared.data_type)
    {
        return Ok(());
    }
    Err(Error::ColumnStorageOnlyPlain {
        type_description: domains::format_type_for_builder(
            builder,
            &PgType::of(&declared.data_type),
        ),
    })
}

/// Whether `data_type` is known to keep every value plain, as a domain does
/// when its base type does. A type the model cannot place answers `false`, so
/// it is never refused.
fn stores_plain(builder: &ParserDBBuilder, data_type: &DataType) -> bool {
    let DataType::Custom(name, _) = data_type else {
        return PgType::of(data_type).stores_plain() == Some(true);
    };
    if let Some(position) = domains::named_domain(builder, data_type) {
        return stores_plain(builder, &builder.domains()[position].0.data_type);
    }
    let user_types = builder.user_defined_types().iter().map(|(type_, _)| type_.as_ref());
    match bound_user_type(user_types, name, builder.identifier_case()) {
        Some((_, true)) => false,
        Some((type_, false)) => {
            match type_.representation() {
                Some(UserDefinedTypeRepresentation::Enum { .. }) => true,
                // A base type declaring no storage keeps its values plain.
                Some(UserDefinedTypeRepresentation::SqlDefinition { options }) => {
                    !options.iter().any(|option| {
                        matches!(
                            option,
                            UserDefinedTypeSqlDefinitionOption::Storage(storage)
                                if !matches!(storage, UserDefinedTypeStorage::Plain)
                        )
                    })
                }
                _ => false,
            }
        }
        None => PgType::of(data_type).stores_plain() == Some(true),
    }
}
