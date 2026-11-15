//! The columns a table takes from the query of `CREATE TABLE … AS`.
//!
//! PostgreSQL gives the table one column per output of the query, named as
//! the query names it unless the statement's column list renames it, and
//! typed as the query types it. Measured against PostgreSQL 18.6: no column
//! receives `NOT NULL`, a default, an identity or a constraint, even when the
//! column it reads carries one, a column it reads keeps its collation, a
//! serial column arrives as its integer type, and `WITH NO DATA` changes
//! nothing about the columns.
//!
//! An output reading a stored column takes that column's declared type. The
//! crate derives no type for an expression, so any other output leaves the
//! column untyped and keeps the output it was created from, with the catalog
//! the query read frozen, since PostgreSQL keeps no link from the table to it.
//!
//! PostgreSQL runs a top-level `SELECT … INTO t` as `CREATE TABLE t AS` over
//! the same query, so that statement takes this path as well.

use alloc::{borrow::ToOwned, boxed::Box, string::ToString, sync::Arc, vec, vec::Vec};

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, DataType, Expr, Ident, ObjectName, Query, Select,
    SelectInto, SetExpr, Statement, helpers::stmt_create_table::CreateTableBuilder,
};

use super::{ParserDBBuilder, view_rewrites, views};
use crate::{
    errors::Error,
    impls::serial_sequence_type,
    structs::{
        ColumnDefinition, ColumnScope, IdentifierCase, QueryOutput,
        metadata::{ColumnCreation, CreatingQuery},
    },
};

/// Whether the statement has the shape PostgreSQL's `CREATE TABLE … AS`
/// grammar allows, a list of bare column names at most.
///
/// Other dialects let typed columns or constraints stand beside the query,
/// each with rules of its own for merging them, so a statement of that shape
/// keeps the columns it declares.
fn takes_query_columns(create_table: &CreateTable) -> bool {
    create_table.constraints.is_empty()
        && create_table.like.is_none()
        && create_table.inherits.is_none()
        && create_table.partition_of.is_none()
        && create_table
            .columns
            .iter()
            .all(|column| column.data_type == DataType::Unspecified && column.options.is_empty())
}

/// The column a table receives for an output reading the stored `column`.
fn copied_column(name: Ident, column: &ColumnDef) -> ColumnDef {
    let options = column
        .options
        .iter()
        .filter(|option| {
            matches!(option.option, ColumnOption::Collation(_) | ColumnOption::CharacterSet(_))
        })
        .cloned()
        .collect();
    let data_type =
        serial_sequence_type(&column.data_type).unwrap_or_else(|| column.data_type.clone());
    ColumnDef { name, data_type, options }
}

/// The columns the query of a `CREATE TABLE … AS` statement produces, before
/// the statement's column list renames them.
pub(super) struct QueryColumns {
    columns: Vec<ColumnDef>,
    /// The ordinals of the outputs reading no stored column.
    computed: Vec<usize>,
    creating: CreatingQuery,
}

/// Reads the columns the query of a `CREATE TABLE … AS` statement produces.
///
/// Answers [`None`] for a statement that takes no columns from a query, and
/// for a query whose outputs cannot all be counted and named, such as a
/// wildcard over a set-returning function, which leaves the node with the
/// names its column list declares.
///
/// # Errors
///
/// Returns the lookup errors reading the query raises.
pub(super) fn query_columns(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
) -> Result<Option<QueryColumns>, Error> {
    let Some(query) = create_table.query.as_deref() else {
        return Ok(None);
    };
    if !takes_query_columns(create_table) {
        return Ok(None);
    }

    let mut bound = query.clone();
    views::bind_definition(builder, &mut bound);
    let read = view_rewrites::catalog(builder, core::iter::once(&bound));
    let mut columns = Vec::new();
    let mut output_names = Vec::new();
    let mut computed = Vec::new();
    {
        let scope = ColumnScope::from_query(&bound, &read, IdentifierCase::AsWritten)?;
        let Some(outputs) = scope.scope_for_query(&bound) else {
            return Ok(None);
        };
        for (ordinal, output) in outputs.outputs().enumerate() {
            let QueryOutput::Column { name: Some((name, quoted)), definition } = output else {
                return Ok(None);
            };
            output_names.push((name.to_owned(), quoted));
            let name =
                if quoted { Ident::with_quote('"', name) } else { Ident::new(name.to_owned()) };
            if let ColumnDefinition::Base { column, .. } = definition {
                columns.push(copied_column(name, column.attribute()));
            } else {
                computed.push(ordinal);
                columns.push(ColumnDef {
                    name,
                    data_type: DataType::Unspecified,
                    options: Vec::new(),
                });
            }
        }
    }
    Ok(Some(QueryColumns {
        columns,
        computed,
        creating: CreatingQuery::new(bound, output_names, read),
    }))
}

/// Gives the node of a `CREATE TABLE … AS` statement the columns
/// [`query_columns`] read, renamed by its column list, and answers the output
/// each column without a declared type was created from, by the column's name.
///
/// # Errors
///
/// Returns [`Error::TableColumnListTooLong`] when the column list names more
/// columns than the query produces.
pub(super) fn apply_query(
    create_table: &mut CreateTable,
    query: Option<QueryColumns>,
) -> Result<Vec<(Ident, ColumnCreation)>, Error> {
    let Some(QueryColumns { mut columns, computed, creating }) = query else {
        return Ok(Vec::new());
    };
    if create_table.columns.len() > columns.len() {
        return Err(Error::TableColumnListTooLong { table_name: create_table.name.to_string() });
    }
    for (column, declared) in columns.iter_mut().zip(core::mem::take(&mut create_table.columns)) {
        column.name = declared.name;
    }
    let creating = Arc::new(creating);
    let creations = computed
        .into_iter()
        .map(|ordinal| {
            (columns[ordinal].name.clone(), ColumnCreation::new(Arc::clone(&creating), ordinal))
        })
        .collect();
    create_table.columns = columns;
    Ok(creations)
}

/// The `CREATE TABLE … AS` statement PostgreSQL runs a `SELECT … INTO` as,
/// with the `INTO` clause of the leftmost `SELECT` turned into the table it
/// names, or the query unchanged when that `SELECT` writes no table.
///
/// MySQL's `SELECT … INTO` assigns variables and creates nothing, so the
/// caller applies this to PostgreSQL alone.
pub(super) fn select_into_table(mut query: Box<Query>) -> Statement {
    let Some(select) = leftmost_select(&mut query.body) else {
        return Statement::Query(query);
    };
    let Some(SelectInto { temporary, unlogged, targets, .. }) = select.into.take() else {
        return Statement::Query(query);
    };
    // PostgreSQL's grammar takes exactly one relation name after `INTO`.
    let name = match <[Expr; 1]>::try_from(targets) {
        Ok([Expr::Identifier(ident)]) => ObjectName::from(vec![ident]),
        Ok([Expr::CompoundIdentifier(parts)]) => ObjectName::from(parts),
        _ => return Statement::Query(query),
    };
    Statement::CreateTable(
        CreateTableBuilder::new(name)
            .temporary(temporary)
            .unlogged(unlogged)
            .query(Some(query))
            .build(),
    )
}

/// The `SELECT` an `INTO` clause may sit on, the first one a set operation
/// or a parenthesized query reaches.
fn leftmost_select(body: &mut SetExpr) -> Option<&mut Select> {
    match body {
        SetExpr::Select(select) => Some(select),
        SetExpr::Query(query) => leftmost_select(&mut query.body),
        SetExpr::SetOperation { left, .. } => leftmost_select(left),
        _ => None,
    }
}
