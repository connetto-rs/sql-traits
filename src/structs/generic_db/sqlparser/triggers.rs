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

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CreateTable, CreateTrigger, DropTrigger, Expr, Ident, ObjectName, ObjectNamePart, TriggerEvent,
    TriggerObject, TriggerObjectKind, TriggerPeriod, TriggerReferencingType,
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
    utils::{
        identifier_resolution::stored_identifier, object_name::object_name_last_part,
        walk::walk_expressions,
    },
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
/// [`Error::ClonedTriggerReplaced`] when the trigger replaced is a copy,
/// [`Error::ConstraintTriggerOnForeignTable`] for a constraint trigger that
/// would land on a foreign table, and under PostgreSQL
/// [`Error::ConstraintTriggerTransitionTables`] and
/// [`Error::QualifiedTransitionTableName`] for a `REFERENCING` clause its
/// grammar refuses.
pub(super) fn create_trigger(
    builder: ParserDBBuilder,
    create_trigger: CreateTrigger,
) -> Result<ParserDBBuilder, Error> {
    require_named(&create_trigger.name, ObjectKind::Trigger)?;
    refuse_transition_table_syntax(&builder, &create_trigger)?;
    if let Some(table) = builder.resolve_table_object_name(&create_trigger.table_name)? {
        builder.refuse_table_kinds(
            table,
            &create_trigger.table_name,
            &[ObjectKind::VirtualTable],
            crate::errors::RelationKindContext::Trigger,
        )?;
        let host = FiringHost::of_table(&builder, table);
        let table = StoredTable::of(table);
        refuse_firing(&builder, &create_trigger, host)?;
        stored_expressions::refuse_in_trigger_condition(&builder, &create_trigger.condition)?;
        let mut create_trigger = create_trigger;
        builder.bind_table_reference(&mut create_trigger.table_name);
        column_references::refuse_in_expressions(
            &builder,
            &create_trigger.table_name,
            &[Some("new"), Some("old")],
            &create_trigger.condition,
        )?;
        refuse_before_generated(
            &builder,
            &create_trigger,
            ParserDB::stored_node(&builder, &table)?,
        )?;
        let create_trigger = bind_parts(&builder, create_trigger)?;
        return create_on_table(builder, create_trigger, &table);
    }
    if let Some(view) = builder.resolve_view_object_name(&create_trigger.table_name)? {
        let host = ViewHost::of_view(view);
        let qualifier = stored_view_qualifier(view);
        refuse_firing(&builder, &create_trigger, FiringHost::View)?;
        stored_expressions::refuse_in_trigger_condition(&builder, &create_trigger.condition)?;
        let mut create_trigger = bind_parts(&builder, create_trigger)?;
        builder.bind_relation_reference(&mut create_trigger.table_name, qualifier);
        return create_on_view(builder, create_trigger, &host);
    }
    if let Some(actual_kind) = views::holds_view(&builder, &create_trigger.table_name) {
        return Err(Error::RelationKindMismatch {
            object_name: builder.dialect().stored_object_name_last(&create_trigger.table_name),
            expected_kind: ObjectKind::Table,
            actual_kind,
            context: crate::errors::RelationKindContext::Trigger,
        });
    }
    if let Some(schema_name) =
        super::missing_schema::missing_schema_name(&builder, &create_trigger.table_name)
    {
        return Err(Error::SchemaNotFound { schema_name });
    }
    Err(Error::RelationNotFoundForTrigger {
        relation_name: builder.dialect().stored_object_name(&create_trigger.table_name),
        trigger_name: builder.dialect().stored_object_name_last(&create_trigger.name),
    })
}

/// Refuses generated values and `tableoid` in `NEW` before the row is formed.
fn refuse_before_generated(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
    table: &CreateTable,
) -> Result<(), Error> {
    if !matches!(builder.dialect(), SqlparserDialect::PostgreSql)
        || trigger.period != Some(TriggerPeriod::Before)
    {
        return Ok(());
    }
    let names_new = |name: &Ident| {
        builder.identifier_case().identifiers_match(
            &name.value,
            name.quote_style.is_some(),
            "new",
            false,
        )
    };
    let generated = |column: &sqlparser::ast::ColumnDef| {
        crate::impls::generation_storage(column, *builder.dialect()).is_some()
    };
    let result = walk_expressions(&trigger.condition, |expression| {
        let column = match expression {
            Expr::CompoundIdentifier(parts) if parts.len() == 2 && names_new(&parts[0]) => {
                Some(&parts[1])
            }
            Expr::Identifier(name) if names_new(name) => None,
            Expr::QualifiedWildcard(name, _)
                if name.0.len() == 1 && name.0[0].as_ident().is_some_and(names_new) =>
            {
                None
            }
            _ => return ControlFlow::Continue(()),
        };
        if let Some(column) = column {
            if builder.identifier_case().identifiers_match(
                &column.value,
                column.quote_style.is_some(),
                "tableoid",
                false,
            ) {
                return ControlFlow::Break(Error::BeforeTriggerSystemColumn {
                    table_name: builder.dialect().stored_object_name_last(&table.name),
                    column_name: builder.dialect().stored_name(column),
                });
            }
            if !table.columns.iter().any(|declared| {
                super::case_idents_match(&declared.name, column, builder.identifier_case())
                    && generated(declared)
            }) {
                return ControlFlow::Continue(());
            }
        } else if !table.columns.iter().any(generated) {
            return ControlFlow::Continue(());
        }
        ControlFlow::Break(Error::BeforeTriggerGeneratedColumn {
            table_name: builder.dialect().stored_object_name_last(&table.name),
            column_name: column.map(|column| builder.dialect().stored_name(column)),
        })
    });
    match result {
        ControlFlow::Break(error) => Err(error),
        ControlFlow::Continue(()) => Ok(()),
    }
}

/// The kind of relation a trigger is created on, as the engine's checks of a
/// new trigger tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FiringHost {
    /// A plain table, inheriting from no other.
    Table,
    /// A partitioned table, a partition or not.
    PartitionedTable,
    /// A partition that is not itself partitioned.
    Partition,
    /// A table inheriting from another through `INHERITS`.
    InheritanceChild,
    /// A foreign table, a partition or not.
    ForeignTable,
    /// A plain view.
    View,
}

impl FiringHost {
    /// The kind of the table `node` declares.
    fn of_table(builder: &ParserDBBuilder, node: &CreateTable) -> Self {
        if builder.is_foreign_table(node) {
            Self::ForeignTable
        } else if node.partition_by.is_some() {
            Self::PartitionedTable
        } else if node.partition_of.is_some() {
            Self::Partition
        } else if node.inherits.as_ref().is_some_and(|parents| !parents.is_empty()) {
            Self::InheritanceChild
        } else {
            Self::Table
        }
    }
}

/// Refuses `trigger` on its relation, of the kind `host`, when the engine of
/// the dialect refuses its timing, level, events or clauses there. Other
/// dialects refuse none.
fn refuse_firing(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
    host: FiringHost,
) -> Result<(), Error> {
    let refusal = match builder.dialect() {
        SqlparserDialect::PostgreSql => postgres_firing_refusal(trigger, host),
        SqlparserDialect::SQLite => sqlite_firing_refusal(trigger, host == FiringHost::View),
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
fn postgres_firing_refusal(
    trigger: &CreateTrigger,
    host: FiringHost,
) -> Option<TriggerFiringRefusal> {
    let instead = trigger.period == Some(TriggerPeriod::InsteadOf);
    if host == FiringHost::View {
        if !instead && fires_per_row(trigger) {
            return Some(TriggerFiringRefusal::RowLevelBeforeOrAfterOnView);
        }
        if trigger.events.contains(&TriggerEvent::Truncate) {
            return Some(TriggerFiringRefusal::TruncateOnView);
        }
    } else if instead {
        return Some(TriggerFiringRefusal::InsteadOfOnTable);
    }
    if host == FiringHost::PartitionedTable
        && fires_per_row(trigger)
        && !trigger.referencing.is_empty()
    {
        return Some(TriggerFiringRefusal::RowTransitionTablesOnPartitionedTable);
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
    transition_table_refusal(trigger, host)
}

/// What PostgreSQL refuses in the transition tables `trigger` declares on a
/// relation of the kind `host`, checking the relation, then the trigger's
/// timing, events and column list, then each table in the order written,
/// then the two names, as PostgreSQL does.
fn transition_table_refusal(
    trigger: &CreateTrigger,
    host: FiringHost,
) -> Option<TriggerFiringRefusal> {
    if trigger.referencing.is_empty() {
        return None;
    }
    match host {
        FiringHost::ForeignTable => {
            return Some(TriggerFiringRefusal::TransitionTablesOnForeignTable);
        }
        FiringHost::View => return Some(TriggerFiringRefusal::TransitionTablesOnView),
        FiringHost::Partition if fires_per_row(trigger) => {
            return Some(TriggerFiringRefusal::RowTransitionTablesOnPartition);
        }
        FiringHost::InheritanceChild if fires_per_row(trigger) => {
            return Some(TriggerFiringRefusal::RowTransitionTablesOnInheritanceChild);
        }
        _ => {}
    }
    if trigger.period != Some(TriggerPeriod::After) {
        return Some(TriggerFiringRefusal::TransitionTablesNotAfter);
    }
    if trigger.events.contains(&TriggerEvent::Truncate) {
        return Some(TriggerFiringRefusal::TransitionTablesOnTruncate);
    }
    let fires_on = |kind: fn(&TriggerEvent) -> bool| trigger.events.iter().any(kind);
    let insert = fires_on(|event| matches!(event, TriggerEvent::Insert));
    let update = fires_on(|event| matches!(event, TriggerEvent::Update(_)));
    let delete = fires_on(|event| matches!(event, TriggerEvent::Delete));
    if usize::from(insert) + usize::from(update) + usize::from(delete) != 1 {
        return Some(TriggerFiringRefusal::TransitionTablesWithSeveralEvents);
    }
    if fires_on(|event| matches!(event, TriggerEvent::Update(columns) if !columns.is_empty())) {
        return Some(TriggerFiringRefusal::TransitionTablesWithColumnList);
    }
    let (mut new_name, mut old_name) = (None, None);
    for referencing in &trigger.referencing {
        let (fires, name, unfit, repeated) = match referencing.refer_type {
            TriggerReferencingType::NewTable => {
                (
                    insert || update,
                    &mut new_name,
                    TriggerFiringRefusal::NewTableWithoutInsertOrUpdate,
                    TriggerFiringRefusal::RepeatedNewTable,
                )
            }
            TriggerReferencingType::OldTable => {
                (
                    delete || update,
                    &mut old_name,
                    TriggerFiringRefusal::OldTableWithoutDeleteOrUpdate,
                    TriggerFiringRefusal::RepeatedOldTable,
                )
            }
        };
        if !fires {
            return Some(unfit);
        }
        if name.replace(&referencing.transition_relation_name).is_some() {
            return Some(repeated);
        }
    }
    match (new_name.and_then(stored_transition_name), old_name.and_then(stored_transition_name)) {
        (Some(new_name), Some(old_name)) if new_name == old_name => {
            Some(TriggerFiringRefusal::SameTransitionTableNames)
        }
        _ => None,
    }
}

/// The name PostgreSQL stores for the transition table `name`.
fn stored_transition_name(name: &ObjectName) -> Option<Cow<'_, str>> {
    object_name_last_part(name).map(|(value, quoted)| stored_identifier(value, quoted))
}

/// Refuses under PostgreSQL a `REFERENCING` clause its grammar does not
/// accept, which is one on a constraint trigger or one naming a transition
/// table by more than one identifier.
fn refuse_transition_table_syntax(
    builder: &ParserDBBuilder,
    trigger: &CreateTrigger,
) -> Result<(), Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql || trigger.referencing.is_empty() {
        return Ok(());
    }
    let trigger_name = || builder.dialect().stored_object_name_last(&trigger.name);
    if trigger.is_constraint {
        return Err(Error::ConstraintTriggerTransitionTables {
            trigger_name: trigger_name(),
            written_token: "REFERENCING".to_string(),
        });
    }
    if trigger.referencing.iter().any(|referencing| {
        !matches!(
            referencing.transition_relation_name.0.as_slice(),
            [ObjectNamePart::Identifier(_)]
        )
    }) {
        return Err(Error::QualifiedTransitionTableName { trigger_name: trigger_name() });
    }
    Ok(())
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

/// Binds the trigger function and its referenced table.
///
/// # Errors
///
/// Returns [`Error::FunctionNotFoundForTrigger`] for a function the trigger
/// cannot run.
fn bind_parts(
    builder: &ParserDBBuilder,
    mut create_trigger: CreateTrigger,
) -> Result<CreateTrigger, Error> {
    if let Some(exec_body) = &mut create_trigger.exec_body {
        let Some(function) = builder.trigger_function(&exec_body.func_desc.name) else {
            if let Some(schema_name) =
                super::missing_schema::missing_schema_name(builder, &exec_body.func_desc.name)
            {
                return Err(Error::SchemaNotFound { schema_name });
            }
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
            builder.bind_relation_reference(relation_name, qualifier);
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
        if let Some(schema_name) = drop_trigger
            .table_name
            .as_ref()
            .and_then(|name| super::missing_schema::missing_schema_name(&builder, name))
        {
            return Err(Error::SchemaNotFound { schema_name });
        }
        return Err(Error::DropTriggerNotFound {
            trigger_name: builder.dialect().stored_object_name_last(&drop_trigger.trigger_name),
            table_name: drop_trigger.table_name.as_ref().map(|name| {
                if builder.relation_reached(name).is_some() {
                    builder.dialect().stored_object_name_last(name)
                } else {
                    builder.dialect().stored_object_name(name)
                }
            }),
            table_exists: drop_trigger
                .table_name
                .as_ref()
                .is_some_and(|name| builder.relation_reached(name).is_some()),
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
        let key = crate::utils::object_name::stored_table_key(
            builder.namespaces(),
            node,
            builder.identifier_case(),
        );
        let parent = node.partition_of.as_ref();
        let parent_key = parent.map(|parent| {
            let schema = crate::utils::object_name::qualifier_of(parent).named().map_or_else(
                || builder.namespaces().default_schema().to_string(),
                |(schema, quoted)| builder.dialect().stored_spelling(schema, quoted),
            );
            crate::utils::object_name::RelationKey {
                schema,
                name: builder.dialect().stored_object_name_last(parent),
            }
        });
        return Err(Error::ClonedTriggerDropped {
            trigger_name: builder.dialect().stored_object_name_last(&name),
            table_name: builder.dialect().stored_spelling(&table.name, table.name_quoted),
            parent_table_name: parent
                .map(|parent| builder.dialect().stored_object_name_last(parent))
                .unwrap_or_default(),
            table_schema: views::relation_schema_for_error(&builder, &key),
            parent_table_schema: parent_key
                .as_ref()
                .and_then(|key| views::relation_schema_for_error(&builder, key)),
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

/// The stored name of the first `FOR EACH ROW` trigger with a transition
/// table that `table` carries, in the byte order of names PostgreSQL reads a
/// table's triggers in, which keeps `table` from becoming a partition or an
/// inheritance child.
pub(super) fn row_transition_trigger(
    builder: &ParserDBBuilder,
    table: &StoredTable,
) -> Option<String> {
    builder
        .triggers()
        .iter()
        .filter(|(trigger, _)| {
            table.named_by(&trigger.table_name)
                && fires_per_row(trigger)
                && !trigger.referencing.is_empty()
        })
        .map(|(trigger, _)| builder.dialect().stored_object_name_last(&trigger.name))
        .min()
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
