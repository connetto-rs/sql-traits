//! Refuses a column reference that reaches nothing, as PostgreSQL refuses one
//! with `42703` when the statement storing it runs, in the query of a view or
//! of `CREATE TABLE … AS`, an index expression or predicate, a trigger's
//! `WHEN` condition and a policy's clauses. A check constraint and a generated
//! column read their table's columns through
//! [`columns_in_expression`](crate::utils::columns_in_expression::columns_in_expression)
//! instead.

use alloc::vec::Vec;

use sqlparser::ast::{
    CreateIndex, Expr, Ident, ObjectName, Query, SelectItem, TableAlias, TableFactor,
    TableWithJoins,
};

use super::{
    ParserDBBuilder,
    function_bodies::{wrapping_query, wrapping_select},
    object_name_last_identifier, view_rewrites, views,
};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, dql::definition_graph::reference_sites::first_unknown_column},
    structs::IdentifierCase,
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        postgres_types::names_catalog_type,
    },
};

/// Whether `builder` models PostgreSQL, the engine whose refusal this is.
fn postgres(builder: &ParserDBBuilder) -> bool {
    matches!(builder.dialect(), SqlparserDialect::PostgreSql)
}

/// Whether `name`, applied to a row as `row.name`, may call a function or
/// cast the row to a type of that name, which is how PostgreSQL reads a
/// qualified name the row holds no column of.
fn names_routine(builder: &ParserDBBuilder, name: &Ident) -> bool {
    let quoted = name.quote_style.is_some();
    let declared = builder.functions().iter().any(|(function, _)| {
        object_name_last_identifier(&function.name).is_some_and(|stored| {
            identifiers_match(&stored.value, stored.quote_style.is_some(), &name.value, quoted)
        })
    });
    let folded = stored_identifier(&name.value, quoted);
    declared || builder.builtins().named(&folded).next().is_some() || names_catalog_type(&folded)
}

/// Refuses the first column reference of `query`, a definition whose names
/// are bound, that reaches nothing in the relations it reads.
///
/// # Errors
///
/// Returns [`Error::UnknownColumn`] naming the reference as written.
pub(super) fn refuse_in_query(builder: &ParserDBBuilder, query: &Query) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    let read = view_rewrites::catalog(builder, core::iter::once(query));
    let names_routine = |name: &Ident| names_routine(builder, name);
    match first_unknown_column(query, &read, IdentifierCase::AsWritten, &names_routine) {
        Some(reference) => Err(Error::UnknownColumn { reference }),
        None => Ok(()),
    }
}

/// Refuses the first column reference of `expressions` that reaches nothing,
/// each read as an output of a query over the relation `relation` binds to,
/// once under each of `aliases`, its own name for `None`.
///
/// # Errors
///
/// Returns [`Error::UnknownColumn`] naming the reference as written.
pub(super) fn refuse_in_expressions<'e>(
    builder: &ParserDBBuilder,
    relation: &ObjectName,
    aliases: &[Option<&str>],
    expressions: impl IntoIterator<Item = &'e Expr>,
) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    let projection: Vec<SelectItem> = expressions
        .into_iter()
        .map(|expression| SelectItem::UnnamedExpr(expression.clone()))
        .collect();
    if projection.is_empty() {
        return Ok(());
    }
    let from: Vec<TableWithJoins> = aliases
        .iter()
        .map(|alias| {
            TableWithJoins {
                relation: TableFactor::Table {
                    name: relation.clone(),
                    alias: alias.map(|alias| {
                        TableAlias {
                            explicit: true,
                            name: Ident::new(alias),
                            columns: Vec::new(),
                            at: None,
                        }
                    }),
                    args: None,
                    with_hints: Vec::new(),
                    version: None,
                    with_ordinality: false,
                    partitions: Vec::new(),
                    json_path: None,
                    sample: None,
                    index_directives: Vec::new(),
                    inheritance: None,
                },
                joins: Vec::new(),
            }
        })
        .collect();
    let mut query = wrapping_query(wrapping_select(projection, from, None));
    views::bind_definition(builder, &mut query);
    refuse_in_query(builder, &query)
}

/// Refuses the first column reference of the expressions or the predicate of
/// `create_index` that reaches nothing in the relation it indexes, whose name
/// is bound. A bare column entry is checked where the index is validated.
///
/// # Errors
///
/// Returns [`Error::UnknownColumn`] naming the reference as written.
pub(super) fn refuse_in_index(
    builder: &ParserDBBuilder,
    create_index: &CreateIndex,
) -> Result<(), Error> {
    let expressions = create_index
        .columns
        .iter()
        .map(|entry| &entry.column.expr)
        .filter(|expression| !matches!(expression, Expr::Identifier(_)))
        .chain(&create_index.predicate);
    refuse_in_expressions(builder, &create_index.table_name, &[None], expressions)
}
