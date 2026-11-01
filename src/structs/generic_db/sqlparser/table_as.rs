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

use alloc::{borrow::ToOwned, string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{ColumnDef, ColumnOption, CreateTable, DataType, Ident};

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

/// Gives the node of a `CREATE TABLE … AS` statement the columns its query
/// produces, answering the output each column without a declared type was
/// created from, by the column's name.
///
/// A query whose outputs cannot all be counted and named, such as a wildcard
/// over a set-returning function, leaves the node with the names its column
/// list declares.
///
/// # Errors
///
/// Returns [`Error::TableColumnListTooLong`] when the column list names more
/// columns than the query produces, and the lookup errors reading the query
/// raises.
pub(super) fn apply_query(
    builder: &ParserDBBuilder,
    create_table: &mut CreateTable,
) -> Result<Vec<(Ident, ColumnCreation)>, Error> {
    let Some(query) = create_table.query.as_deref() else {
        return Ok(Vec::new());
    };
    if !takes_query_columns(create_table) {
        return Ok(Vec::new());
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
            return Ok(Vec::new());
        };
        for (ordinal, output) in outputs.outputs().enumerate() {
            let QueryOutput::Column { name: Some((name, quoted)), definition } = output else {
                return Ok(Vec::new());
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

    if create_table.columns.len() > columns.len() {
        return Err(Error::TableColumnListTooLong { table_name: create_table.name.to_string() });
    }
    for (column, declared) in columns.iter_mut().zip(core::mem::take(&mut create_table.columns)) {
        column.name = declared.name;
    }
    let creating = Arc::new(CreatingQuery::new(bound, output_names, read));
    let creations = computed
        .into_iter()
        .map(|ordinal| {
            (columns[ordinal].name.clone(), ColumnCreation::new(Arc::clone(&creating), ordinal))
        })
        .collect();
    create_table.columns = columns;
    Ok(creations)
}
