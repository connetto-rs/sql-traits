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
    case_idents_match,
    constraint_names::{Slot, slots},
    key_nullability::is_integer_primary_key,
    object_name_last_identifier,
};
use crate::{structs::IdentifierCase, traits::TableLike};

/// One entry of a key as SQLite compares it: the column, and the collation
/// it is compared under.
struct KeyEntry<'node> {
    /// The position of the column among those of the table, or the written
    /// expression when it names none.
    column: Result<usize, &'node Expr>,
    /// The collation written on the entry, else the one declared on the
    /// column, else `BINARY`.
    collation: &'node str,
}

impl PartialEq for KeyEntry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.column == other.column && self.collation.eq_ignore_ascii_case(other.collation)
    }
}

/// The name of the index SQLite builds behind each primary key and unique
/// constraint of `node`, in the order of [`slots`], with keys naming columns
/// under `case`. Constraints sharing an index answer the same name.
///
/// [`None`] for the rowid alias, which SQLite keys without an index.
pub(super) fn autoindex_names(node: &CreateTable, case: IdentifierCase) -> Vec<Option<String>> {
    let without_rowid = node.is_without_rowid();
    let mut built: Vec<Vec<KeyEntry<'_>>> = Vec::new();
    let mut ordinals: Vec<Option<usize>> = Vec::new();
    let mut delayed = None;
    for slot in slots(node).into_iter().filter(|slot| slot.is_unique_key()) {
        let key = key_entries(node, slot, case);
        if integer_primary_key(node, slot, case) {
            if without_rowid {
                delayed = Some((ordinals.len(), key));
            }
            ordinals.push(None);
            continue;
        }
        ordinals.push(Some(ordinal_of(&mut built, key)));
    }
    if let Some((position, key)) = delayed {
        ordinals[position] = Some(ordinal_of(&mut built, key));
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

/// Whether `slot` holds the primary key of `node` on the one integer column
/// that would make it the rowid alias, with keys naming columns under `case`.
fn integer_primary_key(node: &CreateTable, slot: Slot<'_>, case: IdentifierCase) -> bool {
    match slot {
        Slot::Column { def, held, .. } => {
            matches!(held.option, ColumnOption::PrimaryKey(_))
                && is_integer_primary_key(node, def, case)
        }
        Slot::Table(_, TableConstraint::PrimaryKey(key)) => {
            let [only] = key.columns.as_slice() else {
                return false;
            };
            let Expr::Identifier(name) = &only.column.expr else {
                return false;
            };
            node.columns.iter().any(|column| {
                case_idents_match(&column.name, name, case)
                    && is_integer_primary_key(node, column, case)
            })
        }
        Slot::Table(..) => false,
    }
}

/// The entries of the key `slot` holds, a constraint on a column keying that
/// column alone, with keys naming columns under `case`.
fn key_entries<'node>(
    node: &'node CreateTable,
    slot: Slot<'node>,
    case: IdentifierCase,
) -> Vec<KeyEntry<'node>> {
    match slot {
        Slot::Column { column, def, .. } => {
            Vec::from([KeyEntry { column: Ok(column), collation: declared_collation(def) }])
        }
        Slot::Table(_, constraint) => {
            let columns: &[IndexColumn] = match constraint {
                TableConstraint::PrimaryKey(key) => &key.columns,
                TableConstraint::Unique(key) => &key.columns,
                _ => &[],
            };
            columns.iter().map(|entry| key_entry(node, &entry.column.expr, case)).collect()
        }
    }
}

/// The key entry `expression` writes in a constraint of `node`, naming its
/// column under `case`.
fn key_entry<'node>(
    node: &'node CreateTable,
    expression: &'node Expr,
    case: IdentifierCase,
) -> KeyEntry<'node> {
    let (expression, written) = match expression {
        Expr::Collate { expr, collation } => (&**expr, Some(collation_name(collation))),
        _ => (expression, None),
    };
    let declared = match expression {
        Expr::Identifier(name) => {
            node.columns
                .iter()
                .enumerate()
                .find(|(_, column)| case_idents_match(&column.name, name, case))
        }
        _ => None,
    };
    match declared {
        Some((position, column)) => {
            KeyEntry {
                column: Ok(position),
                collation: written.unwrap_or_else(|| declared_collation(column)),
            }
        }
        None => KeyEntry { column: Err(expression), collation: written.unwrap_or("BINARY") },
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
