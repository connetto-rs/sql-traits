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

/// One entry of a key as SQLite compares it, the column and the collation it
/// is compared under.
pub(super) struct KeyEntry<'node> {
    /// The position of the column among those of the table.
    pub(super) column: Option<usize>,
    /// The written expression, for an entry naming no column of the table.
    pub(super) expression: Option<&'node Expr>,
    /// The collation written on the entry, else the one declared on the
    /// column, else `BINARY`.
    pub(super) collation: &'node str,
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

/// The index SQLite builds behind one or more primary key and unique
/// constraints of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqliteAutoindex {
    /// The `sqlite_autoindex_<table>_<n>` name SQLite gives it.
    name: String,
    /// Whether it backs the primary key, `origin` `pk` in
    /// `pragma_index_list`.
    primary_key: bool,
    /// The position among the constraints of the one SQLite built it for,
    /// whose directions it keeps.
    first: usize,
}

impl SqliteAutoindex {
    /// Returns the name SQLite gives the index.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Returns whether the index backs the primary key.
    pub(crate) const fn is_primary_key(&self) -> bool {
        self.primary_key
    }

    /// Returns the position among the primary key and unique constraints of
    /// the table of the one SQLite built the index for, whose directions it
    /// keeps.
    pub(crate) const fn first(&self) -> usize {
        self.first
    }
}

/// Records on `table_metadata` the index SQLite builds behind each primary
/// key and unique constraint of `node`, a table `builder` ingests. Under
/// every other dialect it records none.
pub(super) fn record_autoindexes(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    table_metadata: &mut TableMetadata<CreateTable>,
) {
    if builder.dialect().is_sqlite() {
        table_metadata.set_sqlite_autoindexes(autoindexes(node, builder.identifier_case()));
    }
}

/// The index SQLite builds behind each primary key and unique constraint of
/// `node`, in the order of [`slots`], with keys naming columns under `case`.
/// Constraints sharing an index answer the same one.
///
/// [`None`] for the rowid alias, which SQLite keys without an index.
fn autoindexes(node: &CreateTable, case: IdentifierCase) -> Vec<Option<SqliteAutoindex>> {
    let without_rowid = node.is_without_rowid();
    let mut built: Vec<(Vec<KeyEntry<'_>>, usize)> = Vec::new();
    let mut ordinals: Vec<Option<usize>> = Vec::new();
    let mut primary_key = None;
    let mut delayed = None;
    let keys = slots(node).into_iter().filter_map(|slot| key_of(node, slot, case));
    for (position, key) in keys.enumerate() {
        if key.primary {
            primary_key = Some(position);
            if keys_integer_column(node, &key.entries, case) {
                if without_rowid {
                    delayed = Some((position, key.entries));
                }
                ordinals.push(None);
                continue;
            }
        }
        ordinals.push(Some(ordinal_of(&mut built, key.entries, position)));
    }
    if let Some((position, entries)) = delayed {
        ordinals[position] = Some(ordinal_of(&mut built, entries, position));
    }
    let primary_key = primary_key.and_then(|position| ordinals[position]);
    let table = object_name_last_identifier(&node.name).map_or("", |name| name.value.as_str());
    ordinals
        .into_iter()
        .map(|ordinal| {
            ordinal.map(|ordinal| {
                SqliteAutoindex {
                    name: format!("sqlite_autoindex_{table}_{ordinal}"),
                    primary_key: primary_key == Some(ordinal),
                    first: built[ordinal - 1].1,
                }
            })
        })
        .collect()
}

/// The ordinal, counted from one, of the index among `built` whose key
/// equals `key`, building a new one for the constraint at `position` when
/// none does.
fn ordinal_of<'node>(
    built: &mut Vec<(Vec<KeyEntry<'node>>, usize)>,
    key: Vec<KeyEntry<'node>>,
    position: usize,
) -> usize {
    if let Some(held) = built.iter().position(|(held, _)| *held == key) {
        return held + 1;
    }
    built.push((key, position));
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
    columns.iter().map(|entry| key_entry(node, &entry.column.expr, case)).collect()
}

/// The key entry `expression` writes in a constraint or index of `node`,
/// naming its column under `case`.
///
/// SQLite keeps no node for parentheses. The entry compares under the
/// outermost `COLLATE`, and names a column when one is left once every
/// `COLLATE` is taken off.
pub(super) fn key_entry<'node>(
    node: &'node CreateTable,
    expression: &'node Expr,
    case: IdentifierCase,
) -> KeyEntry<'node> {
    let mut inner = expression;
    while let Expr::Nested(nested) = inner {
        inner = nested;
    }
    let written = match inner {
        Expr::Collate { collation, .. } => Some(collation_name(collation)),
        _ => None,
    };
    while let Expr::Nested(nested) | Expr::Collate { expr: nested, .. } = inner {
        inner = nested;
    }
    let column = node.columns.iter().position(|column| {
        matches!(inner, Expr::Identifier(name) if case_idents_match(&column.name, name, case))
    });
    let declared = column.map_or("BINARY", |position| declared_collation(&node.columns[position]));
    KeyEntry {
        column,
        expression: column.is_none().then_some(inner),
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
