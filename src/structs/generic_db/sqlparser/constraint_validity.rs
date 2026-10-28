//! Whether each constraint a table holds has been checked against the rows the
//! table held when the constraint arrived.
//!
//! `ALTER TABLE ... ADD ... NOT VALID` adds a check or a foreign key without
//! reading the rows already present, and `ALTER TABLE ... VALIDATE CONSTRAINT`
//! reads them later. PostgreSQL records the difference in
//! `pg_constraint.convalidated`, and every rule here was measured against
//! PostgreSQL 18 by reading that column after each statement.
//!
//! A copy passed down by an `ALTER TABLE` carries the state the statement
//! asked for, since the rows already below were not read either. A table
//! created below an unvalidated check holds no rows, so its copy starts
//! validated, while a partition's copy of an unvalidated foreign key starts
//! unvalidated, as the server leaves it.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{ColumnOption, Ident, ObjectName, TableConstraint};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable, column_constraint_position,
    inheritance::{self, ParentKind},
    last_str, table_constraint_has_name,
};
use crate::errors::{Error, InheritedChange};

/// Whether the kind can be added `NOT VALID` and validated afterwards.
fn takes_validation(constraint: &TableConstraint) -> bool {
    matches!(constraint, TableConstraint::Check(_) | TableConstraint::ForeignKey(_))
}

/// Refuses `NOT VALID` on a kind PostgreSQL always checks on arrival.
///
/// PostgreSQL refuses it while reading the statement, so this runs before the
/// table is looked up and `IF EXISTS` excuses nothing.
pub(super) fn refuse_not_valid(
    table_name: &ObjectName,
    constraint: &TableConstraint,
) -> Result<(), Error> {
    if takes_validation(constraint) {
        return Ok(());
    }
    Err(Error::NotValidRefused {
        table_name: last_str(table_name).to_string(),
        constraint: constraint.to_string(),
    })
}

/// Whether the table holds `constraint` validated.
pub(super) fn is_validated(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    constraint: &TableConstraint,
) -> bool {
    let rendered = constraint.to_string();
    !builder
        .tables()
        .iter()
        .find(|(stored, _, _)| table.matches(stored, builder.identifier_case()))
        .is_some_and(|(_, metadata, _)| metadata.unvalidated_constraints().contains(&rendered))
}

/// Records whether the table holds `constraint` validated.
pub(super) fn record(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    constraint: &TableConstraint,
    validated: bool,
) {
    let case = builder.identifier_case();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(stored, _, _)| table.matches(stored, case))
    {
        metadata.set_constraint_validated(constraint.to_string(), validated);
    }
}

/// The rendering of each copy a newly created table receives unvalidated.
///
/// The new table holds no rows, so a check arrives validated. A foreign key
/// only reaches a partition, whose one parent passes down the state it holds
/// the key in.
pub(super) fn unvalidated_copies(
    builder: &ParserDBBuilder,
    parents: &[(ParentKind, ObjectName)],
    copies: &[TableConstraint],
) -> Vec<String> {
    copies
        .iter()
        .filter(|copy| matches!(copy, TableConstraint::ForeignKey(_)))
        .filter(|copy| {
            parents.iter().any(|(_, name)| {
                builder
                    .resolve_bound_table_name(name)
                    .ok()
                    .flatten()
                    .is_some_and(|parent| !is_validated(builder, &StoredTable::of(parent), copy))
            })
        })
        .map(ToString::to_string)
        .collect()
}

/// Whether a table already holding its own copy of an arriving constraint
/// holds it validated once the two merge.
///
/// A check keeps the table's own state, except that an unvalidated one cannot
/// take a validated one from a parent, which PostgreSQL refuses. A foreign key
/// is validated when either side was.
///
/// # Errors
///
/// Returns [`Error::NotValidConstraintConflict`] for the refused check.
pub(super) fn merged_copy_validated(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    held: &TableConstraint,
    arriving_validated: bool,
) -> Result<bool, Error> {
    let held_validated = is_validated(builder, table, held);
    match held {
        TableConstraint::Check(_) if arriving_validated && !held_validated => {
            Err(Error::NotValidConstraintConflict {
                table_name: table.name.clone(),
                constraint: held.to_string(),
            })
        }
        TableConstraint::ForeignKey(_) => Ok(held_validated || arriving_validated),
        _ => Ok(held_validated),
    }
}

/// Applies `ALTER TABLE ... VALIDATE CONSTRAINT`.
///
/// The name is looked up among the table's constraints first and among the
/// constraints written on its columns after, as `DROP CONSTRAINT` does. One
/// written on a column was checked when the column arrived, so it is already
/// validated, and only a key, which never carries the state at all, is
/// refused.
///
/// # Errors
///
/// Returns [`Error::AlterTableNotFound`] for an absent table without `IF
/// EXISTS`, [`Error::ValidateConstraintNotFound`] for a name the table does
/// not hold, [`Error::ConstraintNotValidatable`] for a key, and
/// [`Error::OnlyRefusedWithChildren`] when `ONLY` would leave an unvalidated
/// check's copies below unread.
pub(super) fn validate_constraint(
    builder: ParserDBBuilder,
    table_name: &ObjectName,
    scope: AlterTableScope,
    name: &Ident,
) -> Result<ParserDBBuilder, Error> {
    let Some(stored) = ParserDB::alter_table_target(&builder, table_name, scope)? else {
        return Ok(builder);
    };
    let node = ParserDB::stored_node(&builder, &stored)?;
    let not_validatable = || {
        Error::ConstraintNotValidatable {
            table_name: stored.name.clone(),
            constraint_name: name.value.clone(),
        }
    };

    if let Some(held) = node.constraints.iter().find(|held| table_constraint_has_name(held, name)) {
        if !takes_validation(held) {
            return Err(not_validatable());
        }
        if is_validated(&builder, &stored, held) {
            return Ok(builder);
        }
        // A foreign key on a partitioned table is validated on every partition
        // whatever the statement says, while a check refuses `ONLY` outright.
        if let TableConstraint::Check(check) = held
            && !check.no_inherit
        {
            ParserDB::refuse_only_with_children(
                &builder,
                &stored,
                scope,
                InheritedChange::ValidateConstraint,
            )?;
        }
        let held = held.clone();
        return Ok(validate_down(builder, stored, &held));
    }

    if let Some((column, option)) = column_constraint_position(node, name) {
        return match node.columns[column].options[option].option {
            ColumnOption::Check(_) | ColumnOption::ForeignKey(_) | ColumnOption::NotNull => {
                Ok(builder)
            }
            _ => Err(not_validatable()),
        };
    }

    Err(Error::ValidateConstraintNotFound {
        table_name: stored.name.clone(),
        constraint_name: name.value.clone(),
    })
}

/// Validates `constraint` on the table and on every table below it.
///
/// A check or foreign key reaches a table below unrenamed, so each copy is the
/// constraint itself, and PostgreSQL reads the rows of every table holding one
/// at any depth.
fn validate_down(
    mut builder: ParserDBBuilder,
    stored: StoredTable,
    constraint: &TableConstraint,
) -> ParserDBBuilder {
    record(&mut builder, &stored, constraint, true);
    let mut frontier = alloc::vec![stored];
    let mut reached: Vec<StoredTable> = Vec::new();

    while let Some(current) = frontier.pop() {
        for (kind, child) in inheritance::direct_children(&builder, &current) {
            if !inheritance::passes_down(kind, constraint) || reached.contains(&child) {
                continue;
            }
            reached.push(child.clone());
            record(&mut builder, &child, constraint, true);
            frontier.push(child);
        }
    }

    builder
}
