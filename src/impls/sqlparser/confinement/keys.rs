//! Unique keys and foreign keys that pin rows down.

use alloc::vec::Vec;

use crate::{
    errors::LookupError,
    structs::ConstraintDeferral,
    traits::{ColumnLike, DatabaseLike, ForeignKeyLike, IndexLike, TableIndexLike, TableLike},
};

/// The columns of one unique key.
pub(super) type Key<'db, DB> = Vec<&'db <DB as DatabaseLike>::Column>;

/// An unconditional unique key of a table over plain columns.
struct PlainKey<'db, DB: DatabaseLike> {
    /// The columns the index keys.
    columns: Key<'db, DB>,
    /// Whether its check may wait until later in the transaction, so the
    /// key may hold duplicate rows until then.
    deferrable: bool,
}

/// The unconditional unique keys of a table over plain columns.
fn plain_unique_keys<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
) -> Result<Vec<PlainKey<'db, DB>>, LookupError> {
    /// The plain columns and enforcement properties of an unconditional index.
    fn usable<'db, DB: DatabaseLike, I: TableIndexLike<DB = DB>>(
        database: &'db DB,
        index: &'db I,
    ) -> Result<Option<PlainKey<'db, DB>>, LookupError> {
        if index.predicate().is_some() || !index.is_simple(database)? {
            return Ok(None);
        }
        let columns: Vec<&DB::Column> = index.columns(database)?.collect();
        Ok(Some(PlainKey { columns, deferrable: index.deferral(database).is_deferrable() }))
    }
    let mut keys = Vec::new();
    for index in table.unique_indices(database)? {
        keys.extend(usable(database, index)?);
    }
    for index in table.indices(database)?.filter(|index| index.is_unique()) {
        keys.extend(usable(database, index)?);
    }
    Ok(keys)
}

/// The unique keys of a table through which an equality on every key column
/// matches at most one row: unconditional, over plain columns, nullable
/// ones included, since an equality never matches a `NULL`, and not
/// deferrable, since a deferrable key may hold duplicates until its check
/// runs.
pub(super) fn equality_keys<'db, DB: DatabaseLike>(
    database: &'db DB,
    table: &'db DB::Table,
) -> Result<Vec<Key<'db, DB>>, LookupError> {
    Ok(plain_unique_keys(database, table)?
        .into_iter()
        .filter_map(|key| (!key.deferrable).then_some(key.columns))
        .collect())
}

/// The `(child column, parent column)` pairs of one foreign key.
pub(super) type KeyPairs<'db, DB> =
    Vec<(&'db <DB as DatabaseLike>::Column, &'db <DB as DatabaseLike>::Column)>;

/// The `(child column, parent column)` pairs of `foreign_key`.
pub(super) fn key_pairs<'db, DB: DatabaseLike>(
    database: &'db DB,
    foreign_key: &'db DB::ForeignKey,
) -> Result<KeyPairs<'db, DB>, LookupError> {
    Ok(foreign_key.host_columns(database)?.zip(foreign_key.referenced_columns(database)?).collect())
}

/// Whether `foreign_key` checks every row in which `column` is not `NULL`,
/// which an enforced key does under `MATCH FULL` or when every other host
/// column is `NOT NULL`, since `MATCH SIMPLE` skips a row with a `NULL` in
/// its key. A `NOT ENFORCED` key checks no row.
fn checked_whenever_set<DB: DatabaseLike>(
    database: &DB,
    foreign_key: &DB::ForeignKey,
    column: &DB::Column,
) -> Result<bool, LookupError> {
    if !foreign_key.is_enforced(database) {
        return Ok(false);
    }
    if foreign_key.match_full(database) {
        return Ok(true);
    }
    for host in foreign_key.host_columns(database)? {
        if host != column && host.is_nullable(database)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether every row of the table holding `foreign_key` with a value in
/// `column` references a row at the end of every statement.
pub(super) fn binds<DB: DatabaseLike>(
    database: &DB,
    foreign_key: &DB::ForeignKey,
    column: &DB::Column,
) -> Result<bool, LookupError> {
    Ok(foreign_key.deferral(database) == ConstraintDeferral::NotDeferrable
        && foreign_key.is_validated(database)?
        && checked_whenever_set(database, foreign_key, column)?
        && no_inheritors(database, foreign_key.host_table(database))?)
}

/// Whether no table inherits from `table`, whose keys and constraints would
/// leave the rows of such a table unchecked although a read of `table`
/// yields them.
pub(super) fn no_inheritors<DB: DatabaseLike>(
    database: &DB,
    table: &DB::Table,
) -> Result<bool, LookupError> {
    Ok(table.inheritors(database)?.next().is_none())
}
