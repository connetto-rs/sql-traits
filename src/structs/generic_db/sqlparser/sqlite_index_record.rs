//! The record SQLite stores for each row of an index, the entries
//! `pragma_index_xinfo` lists.
//!
//! The written keys come first, then the entries SQLite appends to find the
//! table row, the rowid of a rowid table or the primary key of a
//! `WITHOUT ROWID` table.
//! <https://www.sqlite.org/fileformat2.html#representation_of_sql_indices>

use alloc::vec::Vec;

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, GeneratedExpressionMode, IndexColumn, OrderBySort,
    UniqueConstraint,
};

use super::sqlite_autoindex::{KeyEntry, key_entry};
use crate::{
    structs::{IdentifierCase, SqliteIndexColumn, SqliteIndexed, TableAttribute, TableMetadata},
    traits::TableLike,
};

/// A column of a table this crate stores.
type Column = TableAttribute<CreateTable, ColumnDef>;

/// One entry of an index record.
type Entry<'db> = SqliteIndexColumn<'db, Column>;

/// The collation SQLite compares under when none is written or declared.
const BINARY: &str = "BINARY";

/// The record of the index `CREATE INDEX` builds over `keys` on `table`,
/// whose stored metadata is `metadata`, with keys naming columns under
/// `case`.
pub(crate) fn created_index_record<'db>(
    keys: &'db [IndexColumn],
    table: &'db CreateTable,
    metadata: &'db TableMetadata<CreateTable>,
    case: IdentifierCase,
) -> Vec<Entry<'db>> {
    let columns: Vec<&Column> = metadata.columns().collect();
    let mut record = key_record(keys, table, &columns, case);
    if table.is_without_rowid() {
        let primary_key = primary_key_record(table, metadata, &columns, case);
        append_primary_key(&mut record, &primary_key, PrimaryKeyOrder::Kept);
    } else {
        record.push(rowid());
    }
    record
}

/// The record of the index SQLite builds behind `constraint`, a primary key
/// or unique constraint of the table whose stored metadata is `metadata`,
/// with keys naming columns under `case`, or `None` for the rowid alias,
/// which has none.
///
/// Constraints sharing an index answer the record of that index, whose keys
/// keep the directions of the constraint SQLite built it for.
pub(crate) fn constraint_index_record<'db>(
    constraint: &'db TableAttribute<CreateTable, UniqueConstraint>,
    metadata: &'db TableMetadata<CreateTable>,
    case: IdentifierCase,
) -> Option<Vec<Entry<'db>>> {
    let table = constraint.table();
    let autoindex = metadata.sqlite_autoindex(constraint)?;
    let first = metadata.sqlite_autoindex_first(autoindex)?;
    let columns: Vec<&Column> = metadata.columns().collect();
    let mut record = key_record(&first.attribute().columns, table, &columns, case);
    if !table.is_without_rowid() {
        record.push(rowid());
    } else if autoindex.is_primary_key() {
        drop_repeated_entries(&mut record);
        let payload = columns.iter().filter(|column| {
            is_stored(column) && !record.iter().any(|entry| holds_column(entry, column))
        });
        let payload: Vec<Entry<'db>> = payload
            .map(|column| Entry::key(SqliteIndexed::Column(*column), false, BINARY).auxiliary())
            .collect();
        record.extend(payload);
    } else {
        let primary_key = primary_key_record(table, metadata, &columns, case);
        append_primary_key(&mut record, &primary_key, PrimaryKeyOrder::Ascending);
    }
    Some(record)
}

/// The direction an index of a `WITHOUT ROWID` table appends the primary key
/// entries in.
#[derive(Clone, Copy)]
enum PrimaryKeyOrder {
    /// The direction of the primary key, which `CREATE INDEX` keeps.
    Kept,
    /// Ascending whatever the primary key's direction, which SQLite writes
    /// for the index of a key constraint the table declares.
    Ascending,
}

/// The key entries of a key list on `table`, whose columns are `columns`,
/// naming columns under `case`.
fn key_record<'db>(
    keys: &'db [IndexColumn],
    table: &'db CreateTable,
    columns: &[&'db Column],
    case: IdentifierCase,
) -> Vec<Entry<'db>> {
    keys.iter()
        .map(|key| {
            let KeyEntry { column, collation, .. } = key_entry(table, &key.column.expr, case);
            let indexed = match column.and_then(|position| columns.get(position)) {
                Some(column) => SqliteIndexed::Column(*column),
                None => SqliteIndexed::Expression(&key.column.expr),
            };
            let descending = matches!(key.column.options.sort, Some(OrderBySort::Desc));
            Entry::key(indexed, descending, collation)
        })
        .collect()
}

/// The key entries of the primary key index of `table`, a `WITHOUT ROWID`
/// table, a column it lists twice under one collation kept once.
fn primary_key_record<'db>(
    table: &'db CreateTable,
    metadata: &'db TableMetadata<CreateTable>,
    columns: &[&'db Column],
    case: IdentifierCase,
) -> Vec<Entry<'db>> {
    let first = metadata
        .sqlite_primary_key_autoindex()
        .and_then(|autoindex| metadata.sqlite_autoindex_first(autoindex));
    let mut record = first.map_or_else(Vec::new, |first| {
        key_record(&first.attribute().columns, table, columns, case)
    });
    drop_repeated_entries(&mut record);
    record
}

/// Appends the entries of `primary_key` that no key of `record` already
/// holds under the same collation, as auxiliary entries in `order`.
fn append_primary_key<'db>(
    record: &mut Vec<Entry<'db>>,
    primary_key: &[Entry<'db>],
    order: PrimaryKeyOrder,
) {
    let keys = record.len();
    for entry in primary_key {
        if record[..keys].iter().any(|key| same_entry(key, entry)) {
            continue;
        }
        let descending = match order {
            PrimaryKeyOrder::Kept => entry.is_descending(),
            PrimaryKeyOrder::Ascending => false,
        };
        record.push(Entry::key(entry.indexed(), descending, entry.collation()).auxiliary());
    }
}

/// Drops every entry holding the column of an earlier one under the same
/// collation.
fn drop_repeated_entries(record: &mut Vec<Entry<'_>>) {
    let mut kept = Vec::with_capacity(record.len());
    for entry in record.drain(..) {
        if !kept.iter().any(|earlier| same_entry(earlier, &entry)) {
            kept.push(entry);
        }
    }
    *record = kept;
}

/// Whether two entries hold one column under the same collation, compared
/// without regard to case. An expression is never the same entry.
fn same_entry(left: &Entry<'_>, right: &Entry<'_>) -> bool {
    match (left.indexed(), right.indexed()) {
        (SqliteIndexed::Column(left_column), SqliteIndexed::Column(right_column)) => {
            core::ptr::eq(left_column, right_column)
                && left.collation().eq_ignore_ascii_case(right.collation())
        }
        _ => false,
    }
}

/// Whether `entry` holds `column`, under any collation.
fn holds_column(entry: &Entry<'_>, column: &Column) -> bool {
    matches!(entry.indexed(), SqliteIndexed::Column(held) if core::ptr::eq(held, column))
}

/// Whether the table stores `column`, which it does unless the column is
/// generated `VIRTUAL`, as SQLite generates it when no mode is written.
fn is_stored(column: &Column) -> bool {
    column.attribute().options.iter().all(|option| {
        match &option.option {
            ColumnOption::Generated { generation_expr: Some(_), generation_expr_mode, .. } => {
                matches!(generation_expr_mode, Some(GeneratedExpressionMode::Stored))
            }
            _ => true,
        }
    })
}

/// The auxiliary rowid entry of a rowid table.
const fn rowid<'db>() -> Entry<'db> {
    Entry::key(SqliteIndexed::Rowid, false, BINARY).auxiliary()
}
