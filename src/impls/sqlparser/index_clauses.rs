//! Readers shared by the two shapes an index takes in a parsed script, a
//! `CREATE INDEX` and the key constraint backing one.

use alloc::vec::Vec;

use sqlparser::ast::{ColumnDef, CreateTable, Ident, IndexColumn, IndexOption, IndexType};

use crate::{
    errors::LookupError,
    structs::{IndexKey, ParserDB, TableAttribute},
    traits::{ColumnLike, TableLike},
};

/// The access method a statement wrote, the last `USING` it spelled.
///
/// MySQL takes the clause before the column list and again among the options
/// after it, and a later one overrides an earlier one.
pub(super) fn written_access_method<'a>(
    before_columns: Option<&'a IndexType>,
    options: &'a [IndexOption],
) -> Option<&'a IndexType> {
    options
        .iter()
        .rev()
        .find_map(|option| {
            match option {
                IndexOption::Using(method) => Some(method),
                IndexOption::Comment(_) => None,
            }
        })
        .or(before_columns)
}

/// The keys a column list writes, in order, each with the sort direction, the
/// null ordering and the operator class written for it.
pub(super) fn written_keys(columns: &[IndexColumn]) -> impl Iterator<Item = IndexKey<'_>> {
    columns.iter().map(|column| {
        let mut key = IndexKey::new(&column.column.expr);
        if let Some(order) = &column.column.options.sort {
            key = key.with_order(order);
        }
        if let Some(nulls_first) = column.column.options.nulls_first {
            key = key.with_nulls_first(nulls_first);
        }
        if let Some(operator_class) = &column.operator_class {
            key = key.with_operator_class(operator_class);
        }
        key
    })
}

/// The names an `INCLUDE` list writes, in order, each with whether it was
/// quoted.
pub(super) fn written_include_names(include: &[Ident]) -> impl Iterator<Item = (&str, bool)> {
    include.iter().map(|column| (column.value.as_str(), column.quote_style.is_some()))
}

/// The columns of `table` an `INCLUDE` list names, in the order written.
///
/// A name is compared as the creation compared it, which refused any name the
/// table does not declare.
pub(super) fn included_columns<'db>(
    include: &'db [Ident],
    table: &'db CreateTable,
    database: &'db ParserDB,
) -> Result<impl Iterator<Item = &'db TableAttribute<CreateTable, ColumnDef>>, LookupError> {
    let case = database.identifier_case();
    let columns: Vec<&TableAttribute<CreateTable, ColumnDef>> = table.columns(database)?.collect();
    Ok(include.iter().filter_map(move |name| {
        columns.iter().copied().find(|column| {
            case.identifiers_match(
                column.column_name(),
                column.column_name_is_quoted(),
                &name.value,
                name.quote_style.is_some(),
            )
        })
    }))
}
