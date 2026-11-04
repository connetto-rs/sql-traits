//! `ALTER TABLE child INHERIT parent` and `NO INHERIT parent`, which link an
//! existing table to a parent and unlink it again.
//!
//! A table linked this way must already have its parent's shape. It keeps
//! declaring every column and check it had, and receives from then on what the
//! parent passes, so the parent alone changes those. Unlinking makes what the
//! table received from that parent its own.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{AlterTableOperation, CreateTable, Ident, ObjectName, TableConstraint};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable, bind_reference, existing_child,
    inheritance, stored_qualifier,
};
use crate::{errors::Error, traits::TableLike};

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
    existing_child::merge_into_existing(&builder, (&parent, parent_node), (&child, child_node))
        .map_err(|reason| {
            Error::InheritanceChildMismatch { table_name, parent_name: parent_text, reason }
        })?;

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
/// [`Error::ParentTableNotFound`] for an absent parent,
/// [`Error::NotAPartitionOf`] for a partitioned table and
/// [`Error::NotAParentOf`] for any other relation the child does not inherit
/// from.
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
    let not_a_parent = || {
        Error::NotAParentOf {
            table_name: child_name.to_string(),
            parent_name: parent_name.to_string(),
        }
    };
    let Some(parent_node) = builder.resolve_table_object_name(parent_name)? else {
        return Err(if builder.relation_reached(parent_name).is_some() {
            not_a_parent()
        } else {
            absent_parent(child_name, parent_name)
        });
    };
    if parent_node.partition_by.is_some() {
        return Err(Error::NotAPartitionOf {
            table_name: parent_name.to_string(),
            partition_name: child_name.to_string(),
        });
    }
    let parent = StoredTable::of(parent_node);
    let child_node = ParserDB::stored_node(&builder, &child)?;
    if !child_node.inherits.iter().flatten().any(|name| parent.named_by(name)) {
        return Err(not_a_parent());
    }

    let mut builder = ParserDB::replace_table_node(builder, &child, |_, node| {
        let kept: Vec<ObjectName> = node
            .inherits
            .take()
            .into_iter()
            .flatten()
            .filter(|name| !parent.named_by(name))
            .collect();
        node.inherits = (!kept.is_empty()).then_some(kept);
        Ok(())
    })?;
    // What no remaining parent passes is now the table's own.
    let node = ParserDB::stored_node(&builder, &child)?;
    let passed_columns: Vec<Ident> = inheritance::parent_names(node)
        .filter_map(|name| builder.resolve_bound_table_name(name).ok().flatten())
        .flat_map(|remaining| remaining.columns.iter().map(|column| column.name.clone()))
        .collect();
    let still_received: Vec<String> = node
        .constraints
        .iter()
        .filter(|constraint| inheritance::receives_constraint(&builder, node, constraint))
        .map(TableConstraint::to_string)
        .collect();
    let case = builder.identifier_case();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(stored, _, _)| child.matches(stored, case))
    {
        let columns = metadata
            .inherited_column_names()
            .iter()
            .filter(|column| {
                let column = Ident::with_quote('"', column.as_str());
                passed_columns.iter().any(|passed| inheritance::same_column(passed, &column))
            })
            .cloned()
            .collect();
        metadata.set_inherited_column_names(columns);
        let constraints = metadata
            .inherited_constraints()
            .iter()
            .filter(|recorded| still_received.contains(recorded))
            .cloned()
            .collect();
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
    match builder.relation_reached(parent_name) {
        Some((object_kind, _)) => {
            Err(Error::AlterActionUnsupportedOnRelation {
                object_kind,
                relation_name: parent_name.to_string(),
                operation: AlterTableOperation::Inherit { parent: parent_name.clone() }.to_string(),
            })
        }
        None => Err(absent_parent(child_name, parent_name)),
    }
}

fn absent_parent(child_name: &ObjectName, parent_name: &ObjectName) -> Error {
    Error::ParentTableNotFound {
        parent_table: parent_name.to_string(),
        child_table: child_name.to_string(),
    }
}
