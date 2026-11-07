//! The shape PostgreSQL requires of an existing table it makes a child, by
//! `ALTER TABLE ... INHERIT` or `ATTACH PARTITION`, following
//! `MergeAttributesIntoExisting` and `MergeConstraintsIntoExisting`.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{CheckConstraint, ColumnDef, ColumnOption, CreateTable, TableConstraint};

use super::{
    ParserDBBuilder, StoredTable,
    constraint_names::{self, Slot},
    constraint_validity, inheritance, states_not_null,
};
use crate::errors::ChildMismatch;

/// Refuses a child lacking the parent's shape, its columns first and then the
/// checks it passes down, and answers the child's own table checks that now
/// count as the parent's, each with whether it is held validated.
///
/// # Errors
///
/// Returns the first [`ChildMismatch`] in the order PostgreSQL checks.
pub(super) fn merge_into_existing(
    builder: &ParserDBBuilder,
    (parent, parent_node): (&StoredTable, &CreateTable),
    (child, child_node): (&StoredTable, &CreateTable),
) -> Result<Vec<(TableConstraint, bool)>, ChildMismatch> {
    for parent_column in &parent_node.columns {
        refuse_column(builder, (parent_node, parent_column), child_node)?;
    }
    merged_checks(builder, (parent, parent_node), (child, child_node))
}

/// Refuses the child's copy of one parent column.
fn refuse_column(
    builder: &ParserDBBuilder,
    (parent_node, parent_column): (&CreateTable, &ColumnDef),
    child_node: &CreateTable,
) -> Result<(), ChildMismatch> {
    let column_name = parent_column.name.value.clone();
    let Some(column) = child_node
        .columns
        .iter()
        .find(|held| inheritance::same_column(&held.name, &parent_column.name))
    else {
        return Err(ChildMismatch::MissingColumn { column_name });
    };
    if inheritance::types_conflict(builder, &parent_column.data_type, &column.data_type) {
        return Err(ChildMismatch::ColumnType { column_name });
    }
    if inheritance::collations_conflict(builder, (parent_node, parent_column), (child_node, column))
    {
        return Err(ChildMismatch::ColumnCollation { column_name });
    }
    // A `NO INHERIT` table-level `NOT NULL` states nothing the child must
    // hold, so only a requirement that passes down shapes the child.
    let parent_requires = states_not_null(parent_column)
        || super::table_not_null(parent_node, &parent_column.name)
            .is_some_and(|held| !held.no_inherit);
    if parent_requires && !super::requires_value(child_node, &column.name) {
        return Err(ChildMismatch::ColumnNullable { column_name });
    }
    match (is_stored_generated(parent_column), is_stored_generated(column)) {
        (true, false) => Err(ChildMismatch::MustBeGenerated { column_name }),
        (false, true) => Err(ChildMismatch::MustNotBeGenerated { column_name }),
        _ => Ok(()),
    }
}

fn is_stored_generated(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        matches!(option.option, ColumnOption::Generated { generation_expr: Some(_), .. })
    })
}

/// A check a table holds, written on a column or on its own.
struct HeldCheck {
    check: CheckConstraint,
    /// Whether the rows held when it arrived were checked.
    validated: bool,
    /// The name it answers to, written or given by PostgreSQL.
    name: Option<String>,
    /// Whether it sits in the table's constraint list.
    in_constraint_list: bool,
}

/// Every check the table holds.
fn checks_of(builder: &ParserDBBuilder, table: &StoredTable, node: &CreateTable) -> Vec<HeldCheck> {
    constraint_names::names(node, super::generated_constraint_names(builder, table))
        .into_iter()
        .filter_map(|(slot, name)| {
            let name = name.map(Cow::into_owned);
            match slot {
                Slot::Column { held, .. } => {
                    let ColumnOption::Check(check) = &held.option else {
                        return None;
                    };
                    let mut check = check.clone();
                    check.name = check.name.or_else(|| held.name.clone());
                    Some(HeldCheck { check, validated: true, name, in_constraint_list: false })
                }
                Slot::Table(_, constraint) => {
                    let TableConstraint::Check(check) = constraint else {
                        return None;
                    };
                    Some(HeldCheck {
                        check: check.clone(),
                        validated: constraint_validity::is_validated(builder, table, constraint),
                        name,
                        in_constraint_list: true,
                    })
                }
            }
        })
        .collect()
}

/// Finds the child's own copy of each check the parent passes down, refusing a
/// missing one and one that cannot merge with the parent's.
///
/// PostgreSQL matches by name, the one PostgreSQL gave a check written without
/// one included. Only where no name is known, outside PostgreSQL, is a check
/// matched by its expression.
fn merged_checks(
    builder: &ParserDBBuilder,
    (parent, parent_node): (&StoredTable, &CreateTable),
    (child, child_node): (&StoredTable, &CreateTable),
) -> Result<Vec<(TableConstraint, bool)>, ChildMismatch> {
    let held = checks_of(builder, child, child_node);
    let mut merged = Vec::new();

    for passed in checks_of(builder, parent, parent_node) {
        if passed.check.no_inherit {
            continue;
        }
        let same_expr = |own: &CheckConstraint| {
            inheritance::stored_expr(&own.expr) == inheritance::stored_expr(&passed.check.expr)
        };
        let found = held.iter().find(|own| {
            match &passed.name {
                Some(name) => own.name.as_ref() == Some(name),
                None => same_expr(&own.check),
            }
        });
        let Some(own) = found else {
            return Err(ChildMismatch::MissingConstraint {
                constraint_name: passed.name.clone().unwrap_or_else(|| passed.check.to_string()),
            });
        };
        let constraint_name = || own.name.clone().unwrap_or_else(|| own.check.to_string());
        if !same_expr(&own.check) {
            return Err(ChildMismatch::ConstraintDiffers { constraint_name: constraint_name() });
        }
        if own.check.no_inherit {
            return Err(ChildMismatch::ConstraintNotInherited {
                constraint_name: constraint_name(),
            });
        }
        if passed.validated && !own.validated {
            return Err(ChildMismatch::ConstraintNotValid { constraint_name: constraint_name() });
        }
        if own.in_constraint_list {
            merged.push((TableConstraint::Check(own.check.clone()), own.validated));
        }
    }

    Ok(merged)
}
