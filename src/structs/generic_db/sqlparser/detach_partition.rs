//! `ALTER TABLE root DETACH PARTITION part`, which turns a partition back into
//! a table of its own.
//!
//! The detached table keeps its columns, defaults, keys, foreign keys and
//! checks, all now its own, and loses the identity its root gave it, along
//! with every partition below it. Its own partitions stay its partitions.

use alloc::{string::ToString, vec, vec::Vec};

use sqlparser::ast::{AlterTableOperation, DetachPartitionMode, ForValues, ObjectName};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable,
    inheritance::{self, ParentKind},
};
use crate::{
    errors::{Error, ObjectKind},
    utils::is_identity,
};

/// Applies `ALTER TABLE root DETACH PARTITION partition`.
///
/// # Errors
///
/// Returns [`Error::AlterTableNotFound`] for an absent root without `IF
/// EXISTS`, [`Error::AlterActionUnsupportedOnRelation`] for a root that is not
/// partitioned, [`Error::RelationNotFound`] for an absent partition,
/// [`Error::NotAPartitionOf`] for a relation that is not one of the root's
/// partitions, [`Error::NoDetachPending`] for `FINALIZE`, and for
/// `CONCURRENTLY` [`Error::DetachConcurrentlyInTransaction`] inside a
/// transaction block and [`Error::DetachConcurrentlyWithDefault`] while the
/// root has a default partition.
pub(super) fn detach_partition(
    builder: ParserDBBuilder,
    root_name: &ObjectName,
    scope: AlterTableScope,
    partition_name: &ObjectName,
    mode: Option<DetachPartitionMode>,
    in_transaction: bool,
) -> Result<ParserDBBuilder, Error> {
    let Some(root) = ParserDB::alter_table_target(&builder, root_name, scope)? else {
        return Ok(builder);
    };
    if ParserDB::stored_node(&builder, &root)?.partition_by.is_none() {
        return Err(Error::AlterActionUnsupportedOnRelation {
            object_kind: ObjectKind::Table,
            relation_name: root_name.to_string(),
            operation: AlterTableOperation::DetachPartitionOf {
                partition: partition_name.clone(),
                mode,
            }
            .to_string(),
        });
    }
    let partition = partition_of(&builder, &root, root_name, partition_name)?;
    let (table_name, detached_name) = (root.name.clone(), partition.name.clone());
    match mode {
        Some(DetachPartitionMode::Finalize) => {
            return Err(Error::NoDetachPending { table_name, partition_name: detached_name });
        }
        Some(DetachPartitionMode::Concurrently) if in_transaction => {
            return Err(Error::DetachConcurrentlyInTransaction {
                table_name,
                partition_name: detached_name,
            });
        }
        Some(DetachPartitionMode::Concurrently) if has_default_partition(&builder, &root)? => {
            return Err(Error::DetachConcurrentlyWithDefault {
                table_name,
                partition_name: detached_name,
            });
        }
        Some(DetachPartitionMode::Concurrently) | None => {}
    }

    let mut builder = ParserDB::replace_table_node(builder, &partition, |_, node| {
        node.partition_of = None;
        node.for_values = None;
        Ok(())
    })?;
    // A partition takes no other parent, so once detached it has received
    // nothing.
    let case = builder.identifier_case();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(stored, _, _)| partition.matches(stored, case))
    {
        metadata.set_inherited_column_names(Vec::new());
        metadata.set_inherited_constraints(Vec::new());
    }
    // The identity stays with the root, so it leaves the whole detached tree.
    let mut frontier = vec![partition];
    while let Some(table) = frontier.pop() {
        builder = ParserDB::replace_table_node(builder, &table, |_, node| {
            for column in &mut node.columns {
                column.options.retain(|option| !is_identity(&option.option));
            }
            Ok(())
        })?;
        frontier.extend(
            inheritance::direct_children(&builder, &table)
                .into_iter()
                .filter_map(|(kind, child)| (kind == ParentKind::PartitionOf).then_some(child)),
        );
    }
    Ok(builder)
}

/// The partition of `root` that `partition_name` reaches.
fn partition_of(
    builder: &ParserDBBuilder,
    root: &StoredTable,
    root_name: &ObjectName,
    partition_name: &ObjectName,
) -> Result<StoredTable, Error> {
    let not_a_partition = || {
        Error::NotAPartitionOf {
            table_name: root_name.to_string(),
            partition_name: partition_name.to_string(),
        }
    };
    let Some(node) = builder.resolve_table_object_name(partition_name)? else {
        if builder.relation_reached(partition_name).is_some() {
            return Err(not_a_partition());
        }
        return Err(Error::RelationNotFound {
            object_kind: ObjectKind::Table,
            object_name: partition_name.to_string(),
        });
    };
    let case = builder.identifier_case();
    inheritance::direct_children(builder, root)
        .into_iter()
        .find(|(kind, child)| *kind == ParentKind::PartitionOf && child.matches(node, case))
        .map(|(_, child)| child)
        .ok_or_else(not_a_partition)
}

/// Whether `root` has a default partition.
fn has_default_partition(builder: &ParserDBBuilder, root: &StoredTable) -> Result<bool, Error> {
    for (kind, child) in inheritance::direct_children(builder, root) {
        if kind == ParentKind::PartitionOf
            && matches!(
                ParserDB::stored_node(builder, &child)?.for_values,
                Some(ForValues::Default)
            )
        {
            return Ok(true);
        }
    }
    Ok(false)
}
