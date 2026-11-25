//! Resolving stored relations and their column aliases.

use alloc::string::ToString;

use sqlparser::ast::{Ident, TableAlias};

use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{ColumnLike, DatabaseLike, TableLike},
    utils::identifier_resolution::identifiers_match,
};

/// Whether two written identifiers name the same object.
pub(super) fn same_name(left: &Ident, right: &Ident) -> bool {
    identifiers_match(
        &left.value,
        left.quote_style.is_some(),
        &right.value,
        right.quote_style.is_some(),
    )
}

/// The stored column of `table` that `column` names through the walked
/// relation's `alias`, whose column list renames the stored columns by
/// position.
///
/// # Errors
///
/// Returns an error when the table's columns cannot be read.
pub(super) fn stored_column<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
    alias: Option<&TableAlias>,
    column: &Ident,
) -> Result<Option<&'db DB::Column>, LookupError> {
    let aliases = alias.map_or(&[][..], |alias| alias.columns.as_slice());
    let mut columns = table.columns(database)?;
    if let Some(position) = aliases.iter().position(|alias| same_name(&alias.name, column)) {
        return Ok(columns.nth(position));
    }
    let rendered = column.to_string();
    Ok(columns.skip(aliases.len()).find(|stored| {
        IdentifierCase::AsWritten.names_stored(
            stored.column_name(),
            stored.column_name_is_quoted(),
            &rendered,
        )
    }))
}
