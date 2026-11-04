//! The shape PostgreSQL requires of an existing table it makes a child, by
//! `ALTER TABLE ... INHERIT` or `ATTACH PARTITION`, following
//! `MergeAttributesIntoExisting` and `MergeConstraintsIntoExisting`.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{CheckConstraint, ColumnDef, ColumnOption, CreateTable, TableConstraint};

use super::{
    ParserDBBuilder, StoredTable, constraint_validity, idents_match, inheritance, states_not_null,
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
    if states_not_null(parent_column) && !states_not_null(column) {
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

/// Every check the table holds, whether written on a column or on its own, with
/// whether it is held validated.
fn checks_of(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    node: &CreateTable,
) -> Vec<(CheckConstraint, bool)> {
    let written_on_columns =
        node.columns.iter().flat_map(|column| &column.options).filter_map(|option| {
            let ColumnOption::Check(check) = &option.option else {
                return None;
            };
            let mut check = check.clone();
            check.name = check.name.or_else(|| option.name.clone());
            Some((check, true))
        });
    let written_alone = node.constraints.iter().filter_map(|constraint| {
        let TableConstraint::Check(check) = constraint else {
            return None;
        };
        Some((check.clone(), constraint_validity::is_validated(builder, table, constraint)))
    });
    written_on_columns.chain(written_alone).collect()
}

/// Finds the child's own copy of each check the parent passes down, refusing a
/// missing one and one that cannot merge with the parent's.
///
/// PostgreSQL matches by name. A check the parent left unnamed is matched by
/// its expression, because the model does not hold the name PostgreSQL gives
/// it.
fn merged_checks(
    builder: &ParserDBBuilder,
    (parent, parent_node): (&StoredTable, &CreateTable),
    (child, child_node): (&StoredTable, &CreateTable),
) -> Result<Vec<(TableConstraint, bool)>, ChildMismatch> {
    let held = checks_of(builder, child, child_node);
    let mut merged = Vec::new();

    for (check, validated) in checks_of(builder, parent, parent_node) {
        if check.no_inherit {
            continue;
        }
        let same_expr = |own: &CheckConstraint| {
            inheritance::stored_expr(&own.expr) == inheritance::stored_expr(&check.expr)
        };
        let found = held.iter().find(|(own, _)| {
            match &check.name {
                Some(name) => own.name.as_ref().is_some_and(|own| idents_match(own, name)),
                None => same_expr(own),
            }
        });
        let Some((own, own_validated)) = found else {
            return Err(ChildMismatch::MissingConstraint {
                constraint_name: check
                    .name
                    .as_ref()
                    .map_or_else(|| check.to_string(), |name| name.value.clone()),
            });
        };
        let constraint_name =
            || own.name.as_ref().map_or_else(|| own.to_string(), |name| name.value.clone());
        if !same_expr(own) {
            return Err(ChildMismatch::ConstraintDiffers { constraint_name: constraint_name() });
        }
        if own.no_inherit {
            return Err(ChildMismatch::ConstraintNotInherited {
                constraint_name: constraint_name(),
            });
        }
        if validated && !own_validated {
            return Err(ChildMismatch::ConstraintNotValid { constraint_name: constraint_name() });
        }

        let own = TableConstraint::Check(own.clone());
        if child_node.constraints.contains(&own) {
            merged.push((own, *own_validated));
        }
    }

    Ok(merged)
}
