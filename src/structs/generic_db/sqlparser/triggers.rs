//! Triggers, on tables and on plain views, and the copies PostgreSQL keeps of
//! a row trigger on the partitions below its table.
//!
//! A `FOR EACH ROW` trigger on a partitioned table is copied onto each of its
//! partitions under the same name, and a copy on a partitioned partition onto
//! that partition's own partitions, as `pg_trigger.tgparentid` records. A
//! table that becomes a partition receives copies of the row triggers its
//! parent holds then. A copy goes with the trigger it copies or with the
//! detaching of its partition, and is never dropped or replaced on its own.
//!
//! Each trigger, copies included, carries its own enablement. An `ALTER
//! TABLE` without `ONLY` that changes a row trigger's enablement changes
//! every copy below it as well.
//!
//! A trigger on a view fires `INSTEAD OF` the write to the view, or under
//! PostgreSQL also `BEFORE` or `AFTER` it once per statement. It goes with
//! the view, follows its renames and outlives its replacement.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::{
    CreateTable, CreateTrigger, DropTrigger, Ident, ObjectName, TriggerEvent, TriggerObject,
    TriggerObjectKind, TriggerPeriod,
};

use super::{
    AlterTableScope, NamedColumn, ParserDB, ParserDBBuilder, StoredTable, bind_reference,
    column_references,
    inheritance::{self, ParentKind},
    object_names_match, rename_column_idents, require_named, rules, stored_expressions,
    stored_qualifier, stored_view_qualifier,
    views::{self, ViewHost},
};
use crate::{
    errors::{Error, ObjectKind, TriggerFiringRefusal},
    impls::SqlparserDialect,
    structs::{TriggerEnablement, metadata::TriggerMetadata},
};

/// Records a `CREATE TRIGGER` on the table or plain view its target reaches,
/// with its copies on the partitions below a table when it fires per row.
///
/// # Errors
///
/// Returns [`Error::TriggerFiringRefused`] for a timing, level, event or
/// clause the engine of the dialect refuses on the relation,
/// [`Error::RelationKindMismatch`] for a target a materialized view or a
/// virtual table holds,
/// [`Error::RelationNotFoundForTrigger`] for one nothing holds,
/// [`Error::FunctionNotFoundForTrigger`] for a function the trigger cannot
/// run, [`Error::UnknownColumn`] for a column reference of the `WHEN`
/// condition that reaches nothing in `NEW` and `OLD`,
/// [`Error::TriggerAlreadyExists`] when the relation or a partition below
/// carries a trigger of the name and the statement wrote no `OR REPLACE`,
/// [`Error::ClonedTriggerReplaced`] when the trigger replaced is a copy, and
/// [`Error::ConstraintTriggerOnForeignTable`] for a constraint trigger that
/// would land on a foreign table.
pub(super) fn create_trigger(
    builder: ParserDBBuilder,
    create_trigger: CreateTrigger,
) -> Result<ParserDBBuilder, Error> {
    require_named(&create_trigger.name, ObjectKind::Trigger)?;
    if let Some(table) = builder.resolve_table_object_name(&create_trigger.table_name)? {
        builder.refuse_table_kinds(
            table,
            &create_trigger.table_name,
            &[ObjectKind::VirtualTable],
        )?;
        let table = StoredTable::of(table);
        refuse_firing(&builder, &create_trigger, false)?;
        let mut create_trigger = bind_parts(&builder, create_trigger)?;
        builder.bind_table_reference(&mut create_trigger.table_name);
        column_references::refuse_in_expressions(
            &builder,
            &create_trigger.table_name,
            &[Some("new"), Some("old")],
            &create_trigger.condition,
        )?;
        return create_on_table(builder, create_trigger, &table);
    }
    if let Some(view) = builder.resolve_view_object_name(&create_trigger.table_name)? {
        let host = ViewHost::of_view(view);
        let qualifier = stored_view_qualifier(view);
        refuse_firing(&builder, &create_trigger, true)?;
        let mut create_trigger = bind_parts(&builder, create_trigger)?;
        bind_reference(&mut create_trigger.table_name, qualifier);
        return create_on_view(builder, create_trigger, &host);
    }
    if let Some(actual_kind) = views::holds_view(&builder, &create_trigger.table_name) {
        return Err(Error::RelationKindMismatch {
            object_name: builder.dialect().stored_object_name_last(&create_trigger.table_name),
            expected_kind: ObjectKind::Table,
            actual_kind,
        });
    }
    Err(Error::RelationNotFoundForTrigger {
        relation_name: builder.dialect().stored_object_name(&create_trigger.table_name),
        trigger_name: builder.dialect().stored_object_name_last(&create_trigger.name),
    })
}

/// Refuses `trigger` on its table, or on its view when `on_view` is set, when
/// the engine of the dialect refuses its timing, level, events or clauses
/// there. Other dialects refuse none.
fn refuse_firing(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
    on_view: bool,
) -> Result<(), Error> {
    let refusal = match builder.dialect() {
        SqlparserDialect::PostgreSql => postgres_firing_refusal(trigger, on_view),
        SqlparserDialect::SQLite => sqlite_firing_refusal(trigger, on_view),
        _ => None,
    };
    refusal.map_or(Ok(()), |reason| {
        Err(Error::TriggerFiringRefused {
            trigger_name: builder.dialect().stored_object_name_last(&trigger.name),
            relation_name: builder.dialect().stored_object_name_last(&trigger.table_name),
            reason,
        })
    })
}

/// What PostgreSQL refuses in `trigger`, checking the relation's kind, then
/// the shape of an `INSTEAD OF` trigger, then the transition tables, as
/// PostgreSQL does.
fn postgres_firing_refusal(trigger: &CreateTrigger, on_view: bool) -> Option<TriggerFiringRefusal> {
    let instead = trigger.period == Some(TriggerPeriod::InsteadOf);
    if on_view {
        if !instead && fires_per_row(trigger) {
            return Some(TriggerFiringRefusal::RowLevelBeforeOrAfterOnView);
        }
        if trigger.events.contains(&TriggerEvent::Truncate) {
            return Some(TriggerFiringRefusal::TruncateOnView);
        }
    } else if instead {
        return Some(TriggerFiringRefusal::InsteadOfOnTable);
    }
    if instead {
        if !fires_per_row(trigger) {
            return Some(TriggerFiringRefusal::InsteadOfPerStatement);
        }
        if trigger.condition.is_some() {
            return Some(TriggerFiringRefusal::InsteadOfCondition);
        }
        if trigger
            .events
            .iter()
            .any(|event| matches!(event, TriggerEvent::Update(columns) if !columns.is_empty()))
        {
            return Some(TriggerFiringRefusal::InsteadOfColumnList);
        }
    }
    (on_view && !trigger.referencing.is_empty())
        .then_some(TriggerFiringRefusal::TransitionTablesOnView)
}

/// What SQLite refuses in `trigger`: an `INSTEAD OF` trigger fires only on a
/// view, and a view takes no other timing, a written-out one or the `BEFORE`
/// a trigger writing none fires at.
fn sqlite_firing_refusal(trigger: &CreateTrigger, on_view: bool) -> Option<TriggerFiringRefusal> {
    match (on_view, trigger.period == Some(TriggerPeriod::InsteadOf)) {
        (true, false) => Some(TriggerFiringRefusal::BeforeOrAfterOnView),
        (false, true) => Some(TriggerFiringRefusal::InsteadOfOnTable),
        _ => None,
    }
}

/// `trigger` with its `WHEN` condition checked and the function it runs and
/// the table its `FROM` names bound, which a trigger on a table and one on a
/// view take alike.
///
/// # Errors
///
/// Returns the refusal of a subquery in the condition and
/// [`Error::FunctionNotFoundForTrigger`] for a function the trigger cannot
/// run.
fn bind_parts(
    builder: &ParserDBBuilder,
    mut create_trigger: CreateTrigger,
) -> Result<CreateTrigger, Error> {
    stored_expressions::refuse_in_trigger_condition(builder, &create_trigger.condition)?;
    if let Some(exec_body) = &mut create_trigger.exec_body {
        let Some(function) = builder.trigger_function(&exec_body.func_desc.name) else {
            return Err(Error::FunctionNotFoundForTrigger {
                function_name: builder.dialect().stored_object_name(&exec_body.func_desc.name),
                trigger_name: builder.dialect().stored_object_name_last(&create_trigger.name),
            });
        };
        bind_reference(&mut exec_body.func_desc.name, stored_qualifier(&function.name));
    }
    if let Some(referenced) = &mut create_trigger.referenced_table_name {
        builder.bind_table_reference(referenced);
    }
    Ok(create_trigger)
}

/// Records `create_trigger`, bound to `table`, with its copies on the
/// partitions below `table` when it fires per row.
fn create_on_table(
    mut builder: ParserDBBuilder,
    create_trigger: CreateTrigger,
    table: &StoredTable,
) -> Result<ParserDBBuilder, Error> {
    refuse_constraint_trigger_on_foreign_table(
        &builder,
        &create_trigger,
        ParserDB::stored_node(&builder, table)?,
    )?;

    if let Some(position) = position_on(&builder, &create_trigger.name, table) {
        if !create_trigger.or_replace {
            return Err(already_exists(&builder, &create_trigger, table));
        }
        if builder.triggers()[position].1.is_clone() {
            return Err(Error::ClonedTriggerReplaced {
                trigger_name: builder.dialect().stored_object_name_last(&create_trigger.name),
                table_name: builder.dialect().stored_spelling(&table.name, table.name_quoted),
            });
        }
        builder.triggers_mut().remove(position);
    }

    let metadata = TriggerMetadata::new(builder.calls_in(&create_trigger.condition));
    // A statement trigger leaves the copies of the row trigger it replaces.
    if fires_per_row(&create_trigger) {
        clone_down(&mut builder, &create_trigger, &metadata, table, create_trigger.or_replace)?;
    }
    Ok(builder.add_trigger(Arc::new(create_trigger), metadata))
}

/// Records `create_trigger`, bound to the view `host`.
fn create_on_view(
    mut builder: ParserDBBuilder,
    create_trigger: CreateTrigger,
    host: &ViewHost,
) -> Result<ParserDBBuilder, Error> {
    let existing = position_on(&builder, &create_trigger.name, host.stored());
    if let Some(position) = existing {
        if !create_trigger.or_replace {
            return Err(already_exists(&builder, &create_trigger, host.stored()));
        }
        builder.triggers_mut().remove(position);
    }
    let metadata = TriggerMetadata::new(builder.calls_in(&create_trigger.condition));
    Ok(builder.add_trigger(Arc::new(create_trigger), metadata))
}

/// Applies a `DROP TRIGGER`, dropping the copies below a trigger on a table
/// with it.
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
    if let Some(relation_name) = &mut drop_trigger.table_name {
        if let Ok(Some(view)) = builder.resolve_view_object_name(relation_name) {
            let qualifier = stored_view_qualifier(view);
            bind_reference(relation_name, qualifier);
        } else {
            builder.bind_table_reference(relation_name);
        }
    }
    let case = builder.identifier_case();
    // Matching the relation too spares a same-named trigger elsewhere, and a
    // statement naming no relation means a trigger written on one, never a
    // copy.
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
            trigger_name: builder.dialect().stored_object_name_last(&drop_trigger.trigger_name),
        });
    };

    let (trigger, metadata) = &builder.triggers()[position];
    // A trigger no table holds is on a view, which carries no copies.
    let Some(table) = holder(&builder, trigger) else {
        builder.triggers_mut().remove(position);
        return Ok(builder);
    };
    let name = trigger.name.clone();
    if metadata.is_clone() {
        let node = ParserDB::stored_node(&builder, &table)?;
        let parent = node.partition_of.as_ref();
        return Err(Error::ClonedTriggerDropped {
            trigger_name: builder.dialect().stored_object_name_last(&name),
            table_name: builder.dialect().stored_spelling(&table.name, table.name_quoted),
            parent_table_name: parent
                .map(|parent| builder.dialect().stored_object_name_last(parent))
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
            return Err(already_exists(builder, &trigger, partition));
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

/// Applies `ALTER TABLE ... { ENABLE | DISABLE } [ REPLICA | ALWAYS ] TRIGGER
/// name`, where an unquoted `ALL` or `USER` names every trigger of the table.
///
/// `USER` leaves out PostgreSQL's internal triggers, which the model does not
/// hold, so both reach the same triggers here.
///
/// # Errors
///
/// Returns [`Error::AlterTableNotFound`] for an absent table without `IF
/// EXISTS`, and [`Error::AlterTableTriggerNotFound`] when the table carries
/// no trigger of the name.
pub(super) fn alter_enablement(
    mut builder: ParserDBBuilder,
    table_name: &ObjectName,
    scope: AlterTableScope,
    trigger_name: &Ident,
    enablement: TriggerEnablement,
) -> Result<ParserDBBuilder, Error> {
    let Some(table) = ParserDB::alter_table_target(&builder, table_name, scope)? else {
        return Ok(builder);
    };
    let every = trigger_name.quote_style.is_none()
        && ["ALL", "USER"].iter().any(|keyword| trigger_name.value.eq_ignore_ascii_case(keyword));
    let wanted = ObjectName::from(alloc::vec![trigger_name.clone()]);
    let case = builder.identifier_case();
    let named: Vec<(ObjectName, bool)> = builder
        .triggers()
        .iter()
        .filter(|(trigger, _)| {
            table.named_by(&trigger.table_name)
                && (every || object_names_match(&trigger.name, &wanted, case))
        })
        .map(|(trigger, _)| (trigger.name.clone(), fires_per_row(trigger)))
        .collect();
    if named.is_empty() && !every {
        return Err(Error::AlterTableTriggerNotFound {
            trigger_name: builder.dialect().stored_name(trigger_name),
            table_name: builder.dialect().stored_spelling(&table.name, table.name_quoted),
        });
    }
    for (name, per_row) in &named {
        set_enablement(&mut builder, name, &table, enablement, *per_row && !scope.only);
    }
    Ok(builder)
}

/// Sets the enablement of the trigger named `name` on `table`, and of its
/// copies below `table` when `down` is set.
fn set_enablement(
    builder: &mut ParserDBBuilder,
    name: &ObjectName,
    table: &StoredTable,
    enablement: TriggerEnablement,
    down: bool,
) {
    let case = builder.identifier_case();
    let below = if down { inheritance::descendants(builder, table) } else { Vec::new() };
    for (trigger, metadata) in builder.triggers_mut() {
        let reached = table.named_by(&trigger.table_name)
            || (metadata.is_clone()
                && below.iter().any(|partition| partition.named_by(&trigger.table_name)));
        if reached && object_names_match(&trigger.name, name, case) {
            metadata.set_enablement(enablement);
        }
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

/// The stored table holding `trigger`, found by the identity its stored name
/// carries whatever the session's path has become since, or `None` for a
/// trigger on a view.
fn holder(builder: &ParserDBBuilder, trigger: &CreateTrigger) -> Option<StoredTable> {
    builder
        .tables()
        .iter()
        .map(|(table, _, _)| StoredTable::of(table))
        .find(|table| table.named_by(&trigger.table_name))
}

/// Where the trigger named `name` on `table` is stored.
fn position_on(builder: &ParserDBBuilder, name: &ObjectName, table: &StoredTable) -> Option<usize> {
    let case = builder.identifier_case();
    builder.triggers().iter().position(|(trigger, _)| {
        table.named_by(&trigger.table_name) && object_names_match(&trigger.name, name, case)
    })
}

/// The refusal of a trigger name already held on `relation`.
fn already_exists(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
    relation: &StoredTable,
) -> Error {
    Error::TriggerAlreadyExists {
        trigger_name: builder.dialect().stored_object_name_last(&trigger.name),
        relation_name: builder.dialect().stored_spelling(&relation.name, relation.name_quoted),
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
            trigger_name: builder.dialect().stored_object_name_last(&trigger.name),
            table_name: builder.dialect().stored_object_name_last(&table.name),
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
                return Err(already_exists(builder, trigger, &partition));
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
    let case = builder.identifier_case();
    builder.triggers_mut().retain(|(trigger, _)| {
        !(table.named_by(&trigger.table_name) && object_names_match(&trigger.name, name, case))
    });
    for partition in partitions(builder, table) {
        let copied = position_on(builder, name, &partition)
            .is_some_and(|position| builder.triggers()[position].1.is_clone());
        if copied {
            drop_with_clones(builder, name, &partition);
        }
    }
}

/// Drops the triggers on the view `host`, which is going away.
pub(super) fn drop_on_view(builder: &mut ParserDBBuilder, host: &ViewHost) {
    builder.triggers_mut().retain(|(trigger, _)| !host.named_by(&trigger.table_name));
}

/// Points the triggers on the renamed or moved view `host` at its stored name
/// `new_name`.
pub(super) fn follow_view_rename(
    builder: &mut ParserDBBuilder,
    host: &ViewHost,
    new_name: &ObjectName,
) {
    for (trigger, _) in builder.triggers_mut() {
        if host.named_by(&trigger.table_name) {
            Arc::make_mut(trigger).table_name = new_name.clone();
        }
    }
}

/// Carries the rename of the column `from` of the view `host` to `to` into
/// the triggers on the view.
pub(super) fn follow_view_column_rename(
    builder: &mut ParserDBBuilder,
    host: &ViewHost,
    from: &NamedColumn,
    to: &Ident,
) {
    for (trigger, _) in builder.triggers_mut() {
        if host.named_by(&trigger.table_name) {
            rename_column(Arc::make_mut(trigger), from, to);
        }
    }
}

/// Renames the column `from` of the trigger's relation to `to` in the
/// `UPDATE OF` lists and the `WHEN` condition of `trigger`.
pub(super) fn rename_column(trigger: &mut CreateTrigger, from: &NamedColumn, to: &Ident) {
    for event in &mut trigger.events {
        if let TriggerEvent::Update(columns) = event {
            rename_column_idents(columns, from, to);
        }
    }
    if let Some(condition) = trigger.condition.as_mut() {
        rules::rename_row_column_in_expression(condition, from, to);
    }
}
