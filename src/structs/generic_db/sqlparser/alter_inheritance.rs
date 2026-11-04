//! `ALTER TABLE child INHERIT parent` and `NO INHERIT parent`, which link an
//! existing table to a parent and unlink it again.
//!
//! A table linked this way must already have its parent's shape. It keeps
//! declaring every column and check it had, and receives from then on what the
//! parent passes, so the parent alone changes those. Unlinking makes what the
//! table received from that parent its own.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{
    AlterTableOperation, CheckConstraint, ColumnDef, ColumnOption, CreateTable, Ident, ObjectName,
    TableConstraint,
};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable, bind_reference, inheritance,
    states_not_null, stored_column_collation_name, stored_qualifier, views,
};
use crate::{
    errors::{ChildMismatch, Error},
    traits::TableLike,
    utils::identifier_resolution::stored_ident_name,
};

/// Applies `ALTER TABLE child INHERIT parent`.
///
/// # Errors
///
/// Returns the refusals PostgreSQL gives: a partition or partitioned table as
/// the child, an absent parent or one that is no table, a partitioned or
/// partition parent, a temporary parent of a permanent table, a parent named
/// already, a parent that is the table or inherits from it, and a table
/// without the parent's shape.
pub(super) fn inherit(
    builder: ParserDBBuilder,
    child_name: &ObjectName,
    scope: AlterTableScope,
    parent_name: &ObjectName,
) -> Result<ParserDBBuilder, Error> {
    let Some(child) = ParserDB::alter_table_target(&builder, child_name, scope)? else {
        return Ok(builder);
    };
    let child_node = ParserDB::stored_node(&builder, &child)?;
    refuse_partition_child(child_node, child_name)?;
    if child_node.partition_by.is_some() {
        return Err(Error::InheritanceChangeOnPartitioned { table_name: child_name.to_string() });
    }
    let parent_node = resolve_parent(&builder, child_name, parent_name)?;
    let (table_name, parent_text) = (child_name.to_string(), parent_name.to_string());
    if parent_node.partition_by.is_some() {
        return Err(Error::InheritsFromPartitionedTable { table_name, parent_name: parent_text });
    }
    if parent_node.partition_of.is_some() {
        return Err(Error::InheritsFromPartition { table_name, parent_name: parent_text });
    }
    if parent_node.is_temporary() && !child_node.is_temporary() {
        return Err(Error::PermanentRelationInheritsTemporary {
            table_name,
            parent_name: parent_text,
        });
    }
    let parent = StoredTable::of(parent_node);
    if inheritance::parent_names(child_node).any(|name| parent.named_by(name)) {
        return Err(Error::ParentRepeated {
            table_name,
            parent_name: parent_node.table_name().to_string(),
        });
    }
    let case = builder.identifier_case();
    if parent.matches(child_node, case)
        || inheritance::descendants(&builder, &child)
            .iter()
            .any(|descendant| descendant.matches(parent_node, case))
    {
        return Err(Error::CircularInheritance { table_name, parent_name: parent_text });
    }
    if let Some(reason) = shape_mismatch(child_node, parent_node) {
        return Err(Error::InheritanceChildMismatch {
            table_name,
            parent_name: parent_text,
            reason,
        });
    }

    let mut bound = parent_name.clone();
    bind_reference(&mut bound, stored_qualifier(&parent_node.name));
    ParserDB::replace_table_node(builder, &child, |_, node| {
        node.inherits.get_or_insert_with(Vec::new).push(bound);
        Ok(())
    })
}

/// Applies `ALTER TABLE child NO INHERIT parent`.
///
/// # Errors
///
/// Returns [`Error::InheritanceChangeOnPartition`] for a partition,
/// [`Error::ParentTableNotFound`] for an absent parent and
/// [`Error::NotAParentOf`] for a table the child does not inherit from.
pub(super) fn no_inherit(
    builder: ParserDBBuilder,
    child_name: &ObjectName,
    scope: AlterTableScope,
    parent_name: &ObjectName,
) -> Result<ParserDBBuilder, Error> {
    let Some(child) = ParserDB::alter_table_target(&builder, child_name, scope)? else {
        return Ok(builder);
    };
    refuse_partition_child(ParserDB::stored_node(&builder, &child)?, child_name)?;
    let parent = StoredTable::of(resolve_parent(&builder, child_name, parent_name)?);
    let child_node = ParserDB::stored_node(&builder, &child)?;
    if !child_node.inherits.iter().flatten().any(|name| parent.named_by(name)) {
        return Err(Error::NotAParentOf {
            table_name: child_name.to_string(),
            parent_name: parent_name.to_string(),
        });
    }

    let mut builder = ParserDB::replace_table_node(builder, &child, |_, node| {
        if let Some(parents) = &mut node.inherits {
            parents.retain(|name| !parent.named_by(name));
        }
        if node.inherits.as_ref().is_some_and(Vec::is_empty) {
            node.inherits = None;
        }
        Ok(())
    })?;
    // What no remaining parent passes is now the table's own.
    let node = ParserDB::stored_node(&builder, &child)?;
    let case = builder.identifier_case();
    let Some((_, metadata, _)) =
        builder.tables().iter().find(|(stored, _, _)| child.matches(stored, case))
    else {
        return Ok(builder);
    };
    let columns: Vec<String> = metadata
        .inherited_column_names()
        .iter()
        .filter(|column| {
            inheritance::is_inherited_column(
                &builder,
                node,
                &Ident::with_quote('"', column.as_str()),
            )
        })
        .cloned()
        .collect();
    let still_received: Vec<String> = node
        .constraints
        .iter()
        .filter(|constraint| inheritance::receives_constraint(&builder, node, constraint))
        .map(TableConstraint::to_string)
        .collect();
    let constraints: Vec<String> = metadata
        .inherited_constraints()
        .iter()
        .filter(|recorded| still_received.contains(recorded))
        .cloned()
        .collect();
    if let Some((_, metadata, _)) =
        builder.tables_mut().iter_mut().find(|(stored, _, _)| child.matches(stored, case))
    {
        metadata.set_inherited_column_names(columns);
        metadata.set_inherited_constraints(constraints);
    }
    Ok(builder)
}

/// Refuses a partition as the table whose parents change.
fn refuse_partition_child(node: &CreateTable, child_name: &ObjectName) -> Result<(), Error> {
    if node.partition_of.is_some() {
        return Err(Error::InheritanceChangeOnPartition { table_name: child_name.to_string() });
    }
    Ok(())
}

/// The table `parent_name` reaches, refusing an absent one and a relation of
/// another kind.
fn resolve_parent<'builder>(
    builder: &'builder ParserDBBuilder,
    child_name: &ObjectName,
    parent_name: &ObjectName,
) -> Result<&'builder CreateTable, Error> {
    if let Some(parent) = builder.resolve_table_object_name(parent_name)? {
        return Ok(parent);
    }
    if let Some(object_kind) = views::holds_view(builder, parent_name) {
        return Err(Error::AlterActionUnsupportedOnRelation {
            object_kind,
            relation_name: parent_name.to_string(),
            operation: AlterTableOperation::Inherit { parent: parent_name.clone() }.to_string(),
        });
    }
    Err(Error::ParentTableNotFound {
        parent_table: parent_name.to_string(),
        child_table: child_name.to_string(),
    })
}

/// Whether the column is a stored generated column.
fn is_generated(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        matches!(option.option, ColumnOption::Generated { generation_expr: Some(_), .. })
    })
}

/// The collation a column writes, as PostgreSQL stores its name.
fn collation(column: &ColumnDef) -> Option<Vec<String>> {
    stored_column_collation_name(column).map(|name| {
        name.0
            .iter()
            .filter_map(|part| part.as_ident())
            .map(|ident| stored_ident_name(ident).into_owned())
            .collect()
    })
}

/// A check the parent passes down, with its name when it has one.
struct PassedCheck {
    name: Option<String>,
    expr: String,
}

/// The checks a table holds or passes down: its table checks and the checks
/// written on its columns, each with its name and expression. `inheritable`
/// keeps only those a child receives.
fn checks(table: &CreateTable, inheritable: bool) -> Vec<PassedCheck> {
    let wanted = |check: &CheckConstraint| !inheritable || !check.no_inherit;
    let table_checks = table.constraints.iter().filter_map(|constraint| {
        match constraint {
            TableConstraint::Check(check) if wanted(check) => {
                Some(PassedCheck {
                    name: check.name.as_ref().map(|name| stored_ident_name(name).into_owned()),
                    expr: check.expr.to_string(),
                })
            }
            _ => None,
        }
    });
    let column_checks =
        table.columns.iter().flat_map(|column| &column.options).filter_map(|option| {
            match &option.option {
                ColumnOption::Check(check) if wanted(check) => {
                    Some(PassedCheck {
                        name: option
                            .name
                            .as_ref()
                            .or(check.name.as_ref())
                            .map(|name| stored_ident_name(name).into_owned()),
                        expr: check.expr.to_string(),
                    })
                }
                _ => None,
            }
        });
    table_checks.chain(column_checks).collect()
}

/// What the child lacks of the parent's shape, checked the way PostgreSQL
/// checks it: every column with its type, collation, `NOT NULL` and
/// generation, then every check the parent passes down.
fn shape_mismatch(child: &CreateTable, parent: &CreateTable) -> Option<ChildMismatch> {
    for parent_column in &parent.columns {
        let column_name = || parent_column.name.value.clone();
        let Some(column) = child
            .columns
            .iter()
            .find(|held| inheritance::same_column(&held.name, &parent_column.name))
        else {
            return Some(ChildMismatch::MissingColumn { column_name: column_name() });
        };
        if inheritance::types_conflict(&parent_column.data_type, &column.data_type) {
            return Some(ChildMismatch::ColumnType { column_name: column_name() });
        }
        if collation(parent_column) != collation(column) {
            return Some(ChildMismatch::ColumnCollation { column_name: column_name() });
        }
        if states_not_null(parent_column) && !states_not_null(column) {
            return Some(ChildMismatch::ColumnNullable { column_name: column_name() });
        }
        match (is_generated(parent_column), is_generated(column)) {
            (true, false) => {
                return Some(ChildMismatch::MustBeGenerated { column_name: column_name() });
            }
            (false, true) => {
                return Some(ChildMismatch::MustNotBeGenerated { column_name: column_name() });
            }
            _ => {}
        }
    }
    let held = checks(child, false);
    for passed in checks(parent, true) {
        match &passed.name {
            Some(name) => {
                let Some(found) = held.iter().find(|check| check.name.as_ref() == Some(name))
                else {
                    return Some(ChildMismatch::MissingConstraint {
                        constraint_name: name.clone(),
                    });
                };
                if found.expr != passed.expr {
                    return Some(ChildMismatch::ConstraintDiffers {
                        constraint_name: name.clone(),
                    });
                }
            }
            // PostgreSQL names an unnamed check after the parent, a name the
            // model does not spell, so any check of the same expression meets
            // it.
            None => {
                if !held.iter().any(|check| check.expr == passed.expr) {
                    return Some(ChildMismatch::MissingConstraint {
                        constraint_name: format!("CHECK ({})", passed.expr),
                    });
                }
            }
        }
    }
    None
}
