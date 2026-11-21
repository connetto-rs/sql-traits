//! Default btree operator classes for resolved PostgreSQL key types.

use alloc::{format, string::ToString};

use sqlparser::ast::{
    ColumnOption, CreateIndex, CreateTable, DataType, Expr, Ident, IndexColumn, IndexType,
    TableConstraint,
};

use super::{ParserDBBuilder, SqlparserDialect, domains};
use crate::{
    errors::Error,
    traits::DomainLike,
    utils::{
        identifier_resolution::stored_identifier,
        object_name::{Qualifier, qualifier_of},
        quote_identifier::quote_identifier,
    },
};

/// Checks the keys declared on a table.
pub(super) fn table(table: &CreateTable, builder: &ParserDBBuilder) -> Result<(), Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql {
        return Ok(());
    }
    for column in &table.columns {
        if column.options.iter().any(|option| {
            matches!(option.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_))
        }) {
            require_type(&column.data_type, builder)?;
        }
    }
    for constraint in &table.constraints {
        match constraint {
            TableConstraint::PrimaryKey(key) => columns(&key.columns, Some(table), builder)?,
            TableConstraint::Unique(key) => columns(&key.columns, Some(table), builder)?,
            TableConstraint::Exclude(key)
                if key.index_method.as_ref().is_none_or(|method| {
                    builder.identifier_case().identifiers_match(
                        &method.value,
                        method.quote_style.is_some(),
                        "btree",
                        false,
                    )
                }) =>
            {
                for element in &key.elements {
                    column(&element.column, Some(table), builder)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Checks the resolved key types of a btree index.
pub(super) fn index(
    index: &CreateIndex,
    table: Option<&CreateTable>,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    if *builder.dialect() == SqlparserDialect::PostgreSql
        && index.using.as_ref().is_none_or(|method| *method == IndexType::BTree)
    {
        columns(&index.columns, table, builder)?;
    }
    Ok(())
}

/// Checks every index key without inspecting included columns.
fn columns(
    columns: &[IndexColumn],
    table: Option<&CreateTable>,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    columns.iter().try_for_each(|key| column(key, table, builder))
}

/// Checks a key using its default operator class.
fn column(
    column: &IndexColumn,
    table: Option<&CreateTable>,
    builder: &ParserDBBuilder,
) -> Result<(), Error> {
    if column.operator_class.is_none()
        && let Some(data_type) = expression_type(&column.column.expr, table, builder)
    {
        require_type(data_type, builder)?;
    }
    Ok(())
}

/// Resolves column references and explicitly typed expressions.
fn expression_type<'expression>(
    expression: &'expression Expr,
    table: Option<&'expression CreateTable>,
    builder: &ParserDBBuilder,
) -> Option<&'expression DataType> {
    match expression {
        Expr::Nested(expression) => expression_type(expression, table, builder),
        Expr::Cast { data_type, .. } => Some(data_type),
        Expr::Identifier(name) => {
            table?
                .columns
                .iter()
                .find(|column| {
                    super::case_idents_match(&column.name, name, builder.identifier_case())
                })
                .map(|column| &column.data_type)
        }
        Expr::CompoundIdentifier(names) => {
            let name = names.last()?;
            table?
                .columns
                .iter()
                .find(|column| {
                    super::case_idents_match(&column.name, name, builder.identifier_case())
                })
                .map(|column| &column.data_type)
        }
        _ => None,
    }
}

/// Refuses a resolved type with no default btree operator class.
fn require_type(data_type: &DataType, builder: &ParserDBBuilder) -> Result<(), Error> {
    if let Some(missing) = missing_class(data_type, builder) {
        let type_name = match missing {
            MissingClass::Builtin(name) => name.to_string(),
            MissingClass::Domain(name, position) => {
                let spelling =
                    quote_identifier(&stored_identifier(&name.value, name.quote_style.is_some()));
                if matches!(domains::reach_type(builder, name), Some(domains::Reached::Domain(reached)) if reached == position)
                {
                    spelling
                } else {
                    let domain = &builder.domains()[position].0;
                    let schema = stored_identifier(
                        domain.schema().unwrap_or("public"),
                        domain.schema_is_quoted(),
                    );
                    format!("{}.{spelling}", quote_identifier(&schema))
                }
            }
        };
        return Err(Error::MissingDefaultBtreeOperatorClass { type_name });
    }
    Ok(())
}

/// The type PostgreSQL names when a default btree class is missing.
enum MissingClass<'name> {
    Builtin(&'static str),
    Domain(&'name Ident, usize),
}

/// Resolves scalars and domain bases lacking a default btree operator class.
fn missing_class<'name>(
    data_type: &'name DataType,
    builder: &ParserDBBuilder,
) -> Option<MissingClass<'name>> {
    let builtin = match data_type {
        DataType::JSON => "json",
        DataType::Custom(name, _) => {
            let ident = name.0.last()?.as_ident()?;
            if let Some(position) = domains::named_domain(builder, data_type) {
                return missing_class(&builder.domains()[position].0.data_type, builder)
                    .map(|_| MissingClass::Domain(ident, position));
            }
            let reached = match qualifier_of(name) {
                Qualifier::Absent => domains::reach_type(builder, ident),
                Qualifier::Named(schema, quoted) => {
                    domains::reach_in_schema(builder, ident, (schema, quoted))
                }
                Qualifier::RunTime => return None,
            };
            if !matches!(reached, Some(domains::Reached::Catalog)) {
                return None;
            }
            [
                "aclitem",
                "box",
                "cid",
                "circle",
                "gtsvector",
                "json",
                "jsonpath",
                "line",
                "lseg",
                "path",
                "pg_brin_bloom_summary",
                "pg_brin_minmax_multi_summary",
                "pg_snapshot",
                "point",
                "polygon",
                "refcursor",
                "txid_snapshot",
                "xid",
                "xml",
            ]
            .into_iter()
            .find(|name| {
                builder.identifier_case().identifiers_match(
                    &ident.value,
                    ident.quote_style.is_some(),
                    name,
                    false,
                )
            })?
        }
        _ => return None,
    };
    Some(MissingClass::Builtin(builtin))
}
