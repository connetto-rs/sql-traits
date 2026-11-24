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
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable, existing_child, inheritance,
    stored_qualifier, triggers,
};
use crate::{errors::Error, traits::TableLike};

/// Applies `ALTER TABLE child INHERIT parent`.
///
/// # Errors
///
/// Returns the refusals PostgreSQL gives: a partition or partitioned table as
/// the child, an absent parent or one that is no table, a partitioned or
/// partition parent, a temporary parent of a permanent table, a parent named
/// already, a parent that is the table or inherits from it, a table carrying
/// a row trigger with transition tables, and a table without the parent's
/// shape.
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
    refuse_inheritance_child(&builder, child_node)?;
    let parent_node = resolve_parent(&builder, child_name, parent_name)?;
    let (table_name, parent_text) = (
        builder.dialect().stored_object_name_last(&child_node.name),
        builder.dialect().stored_object_name_last(&parent_node.name),
    );
    if parent_node.partition_by.is_some() {
        return Err(Error::InheritsFromPartitionedTable { table_name, parent_name: parent_text });
    }
    if parent_node.partition_of.is_some() {
        return Err(Error::InheritsFromPartition {
            table_name,
            parent_name: parent_text,
            altering: true,
        });
    }
    if parent_node.is_temporary() && !child_node.is_temporary() {
        return Err(Error::PermanentRelationInheritsTemporary {
            table_name,
            parent_name: parent_text,
            partition: None,
        });
    }
    let parent = StoredTable::of(parent_node);
    if inheritance::parent_names(child_node).any(|name| parent.named_by(name)) {
        return Err(Error::ParentRepeated {
            table_name,
            parent_name: builder.dialect().stored_object_name_last(&parent_node.name),
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
    if let Some(trigger_name) = triggers::row_transition_trigger(&builder, &child) {
        return Err(Error::InheritanceChildRowTransitionTrigger { table_name, trigger_name });
    }
    existing_child::merge_into_existing(&builder, (&parent, parent_node), (&child, child_node))
        .map_err(|reason| {
            Error::InheritanceChildMismatch { table_name, parent_name: parent_text, reason }
        })?;

    let mut bound = parent_name.clone();
    builder.bind_relation_reference(&mut bound, stored_qualifier(&parent_node.name));
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
    refuse_partition_child(&builder, ParserDB::stored_node(&builder, &child)?)?;
    let not_a_parent = || {
        Error::NotAParentOf {
            table_name: builder.dialect().stored_object_name_last(child_name),
            parent_name: builder.dialect().stored_object_name_last(parent_name),
        }
    };
    let Some(parent_node) = builder.resolve_table_object_name(parent_name)? else {
        return Err(if builder.relation_reached(parent_name).is_some() {
            not_a_parent()
        } else {
            absent_parent(&builder, child_name, parent_name)
        });
    };
    if parent_node.partition_by.is_some() {
        return Err(Error::NotAPartitionOf {
            table_name: builder.dialect().stored_object_name_last(parent_name),
            partition_name: builder.dialect().stored_object_name_last(child_name),
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
    let still_required: Vec<String> = node
        .columns
        .iter()
        .filter(|column| inheritance::requires_a_value(&builder, node, &column.name))
        .map(|column| column.name.value.clone())
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
                passed_columns.iter().any(|passed| super::case_idents_match(passed, &column, case))
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
        let not_null = metadata
            .inherited_not_null()
            .iter()
            .filter(|column| still_required.contains(column))
            .cloned()
            .collect();
        metadata.set_inherited_not_null(not_null);
    }
    Ok(builder)
}

/// Refuses a partition or partitioned table as a new inheritance child.
pub(super) fn refuse_inheritance_child(
    builder: &ParserDBBuilder,
    node: &CreateTable,
) -> Result<(), Error> {
    refuse_partition_child(builder, node)?;
    if node.partition_by.is_some() {
        return Err(Error::InheritanceChangeOnPartitioned {
            table_name: builder.dialect().stored_object_name_last(&node.name),
        });
    }
    Ok(())
}

/// Refuses a partition as the table whose parents change.
fn refuse_partition_child(builder: &ParserDBBuilder, node: &CreateTable) -> Result<(), Error> {
    if node.partition_of.is_some() {
        return Err(Error::InheritanceChangeOnPartition {
            table_name: builder.dialect().stored_object_name_last(&node.name),
        });
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
                relation_name: builder.dialect().stored_object_name_last(parent_name),
                operation: AlterTableOperation::Inherit { parent: parent_name.clone() }.to_string(),
                action: Some(crate::errors::RelationAlterAction::Command("INHERIT")),
            })
        }
        None => Err(absent_parent(builder, child_name, parent_name)),
    }
}

fn absent_parent(
    builder: &ParserDBBuilder,
    child_name: &ObjectName,
    parent_name: &ObjectName,
) -> Error {
    if let Some(schema_name) = super::missing_schema::missing_schema_name(builder, parent_name) {
        return Error::SchemaNotFound { schema_name };
    }
    Error::ParentTableNotFound {
        parent_table: builder.dialect().stored_object_name(parent_name),
        child_table: builder.dialect().stored_object_name_last(child_name),
    }
}
