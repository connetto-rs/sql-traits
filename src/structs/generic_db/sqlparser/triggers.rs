//! Triggers, and the copies PostgreSQL keeps of a row trigger on the
//! partitions below its table.
//!
//! A `FOR EACH ROW` trigger on a partitioned table is copied onto each of its
//! partitions under the same name, and a copy on a partitioned partition onto
//! that partition's own partitions, as `pg_trigger.tgparentid` records. A
//! table that becomes a partition receives copies of the row triggers its
//! parent holds then. A copy goes with the trigger it copies or with the
//! detaching of its partition, and is never dropped or replaced on its own.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{
    CreateTable, CreateTrigger, DropTrigger, ObjectName, TriggerObject, TriggerObjectKind,
};

use super::{
    ParserDB, ParserDBBuilder, StoredTable, bind_reference,
    inheritance::{self, ParentKind},
    object_names_match, require_named, stored_qualifier, stored_subqueries, views,
};
use crate::{
    errors::{Error, ObjectKind},
    structs::metadata::TriggerMetadata,
    traits::TableLike,
    utils::last_str,
};

/// Records a `CREATE TRIGGER` on the table its target reaches, with its
/// copies on the partitions below when it fires per row.
///
/// # Errors
///
/// Returns [`Error::RelationKindMismatch`] for a target a view holds,
/// [`Error::TableNotFoundForTrigger`] for one nothing holds,
/// [`Error::FunctionNotFoundForTrigger`] for a function the trigger cannot
/// run, [`Error::TriggerAlreadyExists`] when the table or a partition below
/// carries a trigger of the name and the statement wrote no `OR REPLACE`,
/// [`Error::ClonedTriggerReplaced`] when the trigger replaced is a copy, and
/// [`Error::ConstraintTriggerOnForeignTable`] for a constraint trigger that
/// would land on a foreign table.
pub(super) fn create_trigger(
    mut builder: ParserDBBuilder,
    mut create_trigger: CreateTrigger,
) -> Result<ParserDBBuilder, Error> {
    require_named(&create_trigger.name, ObjectKind::Trigger)?;
    let Some(table) =
        builder.resolve_table_object_name(&create_trigger.table_name)?.map(StoredTable::of)
    else {
        let table_name = last_str(&create_trigger.table_name).to_string();
        // An `INSTEAD OF` trigger on a view is not modelled.
        if let Some(actual_kind) = views::holds_view(&builder, &create_trigger.table_name) {
            return Err(Error::RelationKindMismatch {
                object_name: table_name,
                expected_kind: ObjectKind::Table,
                actual_kind,
            });
        }
        return Err(Error::TableNotFoundForTrigger {
            table_name,
            trigger_name: last_str(&create_trigger.name).to_string(),
        });
    };
    stored_subqueries::refuse_in_trigger_condition(&builder, &create_trigger.condition)?;

    if let Some(exec_body) = &mut create_trigger.exec_body {
        let Some(function) = builder.trigger_function(&exec_body.func_desc.name) else {
            return Err(Error::FunctionNotFoundForTrigger {
                function_name: last_str(&exec_body.func_desc.name).to_string(),
                trigger_name: last_str(&create_trigger.name).to_string(),
            });
        };
        bind_reference(&mut exec_body.func_desc.name, stored_qualifier(&function.name));
    }

    builder.bind_table_reference(&mut create_trigger.table_name);
    if let Some(referenced) = &mut create_trigger.referenced_table_name {
        builder.bind_table_reference(referenced);
    }
    refuse_constraint_trigger_on_foreign_table(
        &builder,
        &create_trigger,
        ParserDB::stored_node(&builder, &table)?,
    )?;

    if let Some(position) = position_on(&builder, &create_trigger.name, &table) {
        if !create_trigger.or_replace {
            return Err(Error::TriggerAlreadyExists {
                trigger_name: last_str(&create_trigger.name).to_string(),
                table_name: last_str(&create_trigger.table_name).to_string(),
            });
        }
        if builder.triggers()[position].1.is_clone() {
            return Err(Error::ClonedTriggerReplaced {
                trigger_name: last_str(&create_trigger.name).to_string(),
                table_name: last_str(&create_trigger.table_name).to_string(),
            });
        }
        builder.triggers_mut().remove(position);
    }

    let metadata = TriggerMetadata::new(builder.calls_in(&create_trigger.condition));
    // A statement trigger leaves the copies of the row trigger it replaces.
    if fires_per_row(&create_trigger) {
        clone_down(&mut builder, &create_trigger, &metadata, &table, create_trigger.or_replace)?;
    }
    Ok(builder.add_trigger(Arc::new(create_trigger), metadata))
}

/// Applies a `DROP TRIGGER`, dropping the copies below the trigger with it.
///
/// # Errors
///
/// Returns [`Error::DropTriggerNotFound`] when no trigger matches and the
/// statement wrote no `IF EXISTS`, and [`Error::ClonedTriggerDropped`] when
/// the trigger is a copy.
pub(super) fn drop_trigger(
    mut builder: ParserDBBuilder,
    mut drop_trigger: DropTrigger,
) -> Result<ParserDBBuilder, Error> {
    if let Some(table_name) = &mut drop_trigger.table_name {
        builder.bind_table_reference(table_name);
    }
    let case = builder.identifier_case();
    // Matching the table too spares a same-named trigger elsewhere, and a
    // statement naming no table means a trigger written on one, never a copy.
    let Some(position) = builder.triggers().iter().position(|(trigger, metadata)| {
        object_names_match(&trigger.name, &drop_trigger.trigger_name, case)
            && drop_trigger.table_name.as_ref().map_or(!metadata.is_clone(), |table_name| {
                super::target_tables_match(&trigger.table_name, table_name, case)
            })
    }) else {
        if drop_trigger.if_exists {
            return Ok(builder);
        }
        return Err(Error::DropTriggerNotFound {
            trigger_name: last_str(&drop_trigger.trigger_name).to_string(),
        });
    };

    let (trigger, metadata) = &builder.triggers()[position];
    let Some(table) = builder.resolve_table_object_name(&trigger.table_name)?.map(StoredTable::of)
    else {
        builder.triggers_mut().remove(position);
        return Ok(builder);
    };
    let name = trigger.name.clone();
    if metadata.is_clone() {
        let parent = ParserDB::stored_node(&builder, &table)?.partition_of.as_ref();
        return Err(Error::ClonedTriggerDropped {
            trigger_name: last_str(&name).to_string(),
            table_name: table.name,
            parent_table_name: parent
                .map(|parent| last_str(parent).to_string())
                .unwrap_or_default(),
        });
    }
    drop_with_clones(&mut builder, &name, &table);
    Ok(builder)
}

/// Copies the row triggers `parent` holds onto `partition`, which has just
/// become one of its partitions, and onto the partitions below `partition`.
///
/// # Errors
///
/// Returns [`Error::TriggerAlreadyExists`] when `partition` or a partition
/// below it already carries a trigger of a copied name, and
/// [`Error::ConstraintTriggerOnForeignTable`] for a constraint trigger that
/// would land on a foreign table.
pub(super) fn clone_onto_partition(
    builder: &mut ParserDBBuilder,
    parent: &StoredTable,
    partition: &StoredTable,
) -> Result<(), Error> {
    let row_triggers: Vec<(Arc<CreateTrigger>, TriggerMetadata)> = builder
        .triggers()
        .iter()
        .filter(|(trigger, _)| parent.named_by(&trigger.table_name) && fires_per_row(trigger))
        .map(|(trigger, metadata)| (Arc::clone(trigger), metadata.for_clone()))
        .collect();
    for (trigger, metadata) in row_triggers {
        if position_on(builder, &trigger.name, partition).is_some() {
            return Err(already_exists(&trigger, partition));
        }
        let clone = clone_onto(builder, &trigger, partition)?;
        clone_down(builder, &clone, &metadata, partition, false)?;
        builder.triggers_mut().push((Arc::new(clone), metadata));
    }
    Ok(())
}

/// Drops the copies `partition` holds of its former parent's row triggers,
/// and their copies below it, once `partition` is detached.
pub(super) fn drop_clones_on_detached(builder: &mut ParserDBBuilder, partition: &StoredTable) {
    let names: Vec<ObjectName> = builder
        .triggers()
        .iter()
        .filter(|(trigger, metadata)| {
            metadata.is_clone() && partition.named_by(&trigger.table_name)
        })
        .map(|(trigger, _)| trigger.name.clone())
        .collect();
    for name in &names {
        drop_with_clones(builder, name, partition);
    }
}

/// Whether `trigger` fires once per row, the triggers PostgreSQL copies onto
/// partitions.
fn fires_per_row(trigger: &CreateTrigger) -> bool {
    matches!(
        trigger.trigger_object,
        Some(
            TriggerObjectKind::For(TriggerObject::Row)
                | TriggerObjectKind::ForEach(TriggerObject::Row)
        )
    )
}

/// The partitions directly below `table`.
fn partitions(builder: &ParserDBBuilder, table: &StoredTable) -> Vec<StoredTable> {
    inheritance::direct_children(builder, table)
        .into_iter()
        .filter_map(|(kind, child)| (kind == ParentKind::PartitionOf).then_some(child))
        .collect()
}

/// Where the trigger named `name` on `table` is stored.
fn position_on(builder: &ParserDBBuilder, name: &ObjectName, table: &StoredTable) -> Option<usize> {
    let case = builder.identifier_case();
    builder.triggers().iter().position(|(trigger, _)| {
        table.named_by(&trigger.table_name) && object_names_match(&trigger.name, name, case)
    })
}

/// The refusal of `trigger` on `table`, which carries one of its name.
fn already_exists(trigger: &CreateTrigger, table: &StoredTable) -> Error {
    Error::TriggerAlreadyExists {
        trigger_name: last_str(&trigger.name).to_string(),
        table_name: table.name.clone(),
    }
}

/// Refuses a constraint trigger on a foreign table.
fn refuse_constraint_trigger_on_foreign_table(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
    table: &CreateTable,
) -> Result<(), Error> {
    if trigger.is_constraint && builder.is_foreign_table(table) {
        return Err(Error::ConstraintTriggerOnForeignTable {
            trigger_name: last_str(&trigger.name).to_string(),
            table_name: table.table_name().to_string(),
        });
    }
    Ok(())
}

/// `trigger` as PostgreSQL copies it onto `partition`.
fn clone_onto(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
    partition: &StoredTable,
) -> Result<CreateTrigger, Error> {
    let node = ParserDB::stored_node(builder, partition)?;
    refuse_constraint_trigger_on_foreign_table(builder, trigger, node)?;
    let mut clone = trigger.clone();
    clone.table_name.clone_from(&node.name);
    bind_reference(&mut clone.table_name, stored_qualifier(&node.name));
    Ok(clone)
}

/// Copies `trigger`, held on `table` with `metadata`, onto every partition
/// below `table`, replacing a trigger of the same name there when `replace`
/// is set and refusing it otherwise.
fn clone_down(
    builder: &mut ParserDBBuilder,
    trigger: &CreateTrigger,
    metadata: &TriggerMetadata,
    table: &StoredTable,
    replace: bool,
) -> Result<(), Error> {
    for partition in partitions(builder, table) {
        if let Some(position) = position_on(builder, &trigger.name, &partition) {
            if !replace {
                return Err(already_exists(trigger, &partition));
            }
            builder.triggers_mut().remove(position);
        }
        let clone = clone_onto(builder, trigger, &partition)?;
        let clone_metadata = metadata.for_clone();
        clone_down(builder, &clone, &clone_metadata, &partition, replace)?;
        builder.triggers_mut().push((Arc::new(clone), clone_metadata));
    }
    Ok(())
}

/// Drops the trigger named `name` on `table` and its copies below `table`.
fn drop_with_clones(builder: &mut ParserDBBuilder, name: &ObjectName, table: &StoredTable) {
    if let Some(position) = position_on(builder, name, table) {
        builder.triggers_mut().remove(position);
    }
    for partition in partitions(builder, table) {
        let copied = position_on(builder, name, &partition)
            .is_some_and(|position| builder.triggers()[position].1.is_clone());
        if copied {
            drop_with_clones(builder, name, &partition);
        }
    }
}
