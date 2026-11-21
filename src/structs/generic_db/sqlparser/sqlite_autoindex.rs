//! The indexes SQLite builds behind the primary key and unique constraints of
//! a table, and the `sqlite_autoindex_<table>_<n>` names it gives them.
//!
//! SQLite visits the constraints written on the columns in declaration order,
//! then those of the constraint list, and gives each one whose key no earlier
//! constraint of the statement indexes the next ordinal. Two keys are one
//! index when they list the same columns in the same order, each under the
//! same collation compared without regard to case, whatever their directions.
//! The rowid alias needs no index, and a `WITHOUT ROWID` table builds the
//! index of the key that would have been its alias after every other.
//! <https://www.sqlite.org/fileformat2.html#intschema>

use alloc::{format, string::String, vec::Vec};

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, Expr, IndexColumn, ObjectName, TableConstraint,
};

use super::{
    ParserDBBuilder, case_idents_match,
    constraint_names::{Slot, slots},
    key_nullability::is_integer_primary_key,
    object_name_last_identifier,
};
use crate::{
    structs::{IdentifierCase, TableMetadata},
    traits::TableLike,
};

/// One entry of a key as SQLite compares it: the column, and the collation
/// it is compared under.
struct KeyEntry<'node> {
    /// The position of the column among those of the table.
    column: Option<usize>,
    /// The written expression, for an entry naming no column of the table.
    expression: Option<&'node Expr>,
    /// The collation written on the entry, else the one declared on the
    /// column, else `BINARY`.
    collation: &'node str,
}

impl PartialEq for KeyEntry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.column == other.column
            && self.expression == other.expression
            && self.collation.eq_ignore_ascii_case(other.collation)
    }
}

/// A primary key or unique constraint of a table, as SQLite indexes it.
struct Key<'node> {
    /// Whether the constraint is the primary key.
    primary: bool,
    /// The entries of its key, a constraint on a column keying that column
    /// alone.
    entries: Vec<KeyEntry<'node>>,
}

/// Records on `table_metadata` the name of the index SQLite builds behind
/// each primary key and unique constraint of `node`, a table `builder`
/// ingests. Under every other dialect it records none.
pub(super) fn record_autoindex_names(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    table_metadata: &mut TableMetadata<CreateTable>,
) {
    if builder.dialect().is_sqlite() {
        table_metadata.set_sqlite_autoindex_names(autoindex_names(node, builder.identifier_case()));
    }
}

/// The name of the index SQLite builds behind each primary key and unique
/// constraint of `node`, in the order of [`slots`], with keys naming columns
/// under `case`. Constraints sharing an index answer the same name.
///
/// [`None`] for the rowid alias, which SQLite keys without an index.
fn autoindex_names(node: &CreateTable, case: IdentifierCase) -> Vec<Option<String>> {
    let without_rowid = node.is_without_rowid();
    let mut built: Vec<Vec<KeyEntry<'_>>> = Vec::new();
    let mut ordinals: Vec<Option<usize>> = Vec::new();
    let mut delayed = None;
    for key in slots(node).into_iter().filter_map(|slot| key_of(node, slot, case)) {
        if key.primary && keys_integer_column(node, &key.entries, case) {
            if without_rowid {
                delayed = Some((ordinals.len(), key.entries));
            }
            ordinals.push(None);
            continue;
        }
        ordinals.push(Some(ordinal_of(&mut built, key.entries)));
    }
    if let Some((position, entries)) = delayed {
        ordinals[position] = Some(ordinal_of(&mut built, entries));
    }
    let table = object_name_last_identifier(&node.name).map_or("", |name| name.value.as_str());
    ordinals
        .into_iter()
        .map(|ordinal| ordinal.map(|ordinal| format!("sqlite_autoindex_{table}_{ordinal}")))
        .collect()
}

/// The ordinal, counted from one, of the index among `built` whose key
/// equals `key`, building a new one when none does.
fn ordinal_of<'node>(built: &mut Vec<Vec<KeyEntry<'node>>>, key: Vec<KeyEntry<'node>>) -> usize {
    if let Some(position) = built.iter().position(|held| *held == key) {
        return position + 1;
    }
    built.push(key);
    built.len()
}

/// Whether `entries` key the one integer column of `node` that would make a
/// primary key the rowid alias, with keys naming columns under `case`.
fn keys_integer_column(node: &CreateTable, entries: &[KeyEntry<'_>], case: IdentifierCase) -> bool {
    matches!(
        entries,
        [KeyEntry { column: Some(position), .. }]
            if is_integer_primary_key(node, &node.columns[*position], case)
    )
}

/// The primary key or unique constraint `slot` of `node` holds, with keys
/// naming columns under `case`, or [`None`] for any other constraint.
fn key_of<'node>(
    node: &'node CreateTable,
    slot: Slot<'node>,
    case: IdentifierCase,
) -> Option<Key<'node>> {
    let (primary, entries) = match slot {
        Slot::Column { column, def, held, .. } => {
            let primary = match held.option {
                ColumnOption::PrimaryKey(_) => true,
                ColumnOption::Unique(_) => false,
                _ => return None,
            };
            let entry = KeyEntry {
                column: Some(column),
                expression: None,
                collation: declared_collation(def),
            };
            (primary, Vec::from([entry]))
        }
        Slot::Table(_, TableConstraint::PrimaryKey(key)) => {
            (true, table_entries(node, &key.columns, case))
        }
        Slot::Table(_, TableConstraint::Unique(key)) => {
            (false, table_entries(node, &key.columns, case))
        }
        Slot::Table(..) => return None,
    };
    Some(Key { primary, entries })
}

/// The entries `columns`, written in a constraint of `node`, key, naming
/// columns under `case`.
fn table_entries<'node>(
    node: &'node CreateTable,
    columns: &'node [IndexColumn],
    case: IdentifierCase,
) -> Vec<KeyEntry<'node>> {
    columns.iter().map(|entry| table_entry(node, &entry.column.expr, case)).collect()
}

/// The key entry `expression` writes in a constraint of `node`, naming its
/// column under `case`.
fn table_entry<'node>(
    node: &'node CreateTable,
    expression: &'node Expr,
    case: IdentifierCase,
) -> KeyEntry<'node> {
    let (expression, written) = match expression {
        Expr::Collate { expr, collation } => (&**expr, Some(collation_name(collation))),
        _ => (expression, None),
    };
    let column = node.columns.iter().position(|column| {
        matches!(expression, Expr::Identifier(name) if case_idents_match(&column.name, name, case))
    });
    let declared = column.map_or("BINARY", |position| declared_collation(&node.columns[position]));
    KeyEntry {
        column,
        expression: column.is_none().then_some(expression),
        collation: written.unwrap_or(declared),
    }
}

/// The collation `column` declares, the last one written winning, else
/// `BINARY`.
fn declared_collation(column: &ColumnDef) -> &str {
    column
        .options
        .iter()
        .rev()
        .find_map(|option| {
            match &option.option {
                ColumnOption::Collation(name) => Some(collation_name(name)),
                _ => None,
            }
        })
        .unwrap_or("BINARY")
}

/// The collation `name` spells, its last part.
fn collation_name(name: &ObjectName) -> &str {
    object_name_last_identifier(name).map_or("", |part| part.value.as_str())
}
