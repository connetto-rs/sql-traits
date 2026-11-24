//! `ALTER TABLE root ATTACH PARTITION part`, which makes an existing table a
//! partition.
//!
//! Unlike `CREATE TABLE ... PARTITION OF`, the table keeps its own column
//! order and defaults, must already hold the root's columns and checks, and
//! receives only the root's keys, foreign keys and identity.

use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};

use sqlparser::ast::{
    AlterTableOperation, ColumnOption, ColumnOptionDef, CreateTable, Expr, ForValues,
    ForeignKeyConstraint, Ident, IndexColumn, ObjectName, OrderByExpr, OrderByOptions,
    TableConstraint,
};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable, constraint_validity, existing_child,
    idents_match,
    inheritance::{self, ParentKind},
    partition_bounds, stored_qualifier, triggers, views,
};
use crate::{
    errors::{Error, ObjectKind, UnattachablePartition},
    impls::foreign_keys_stored_alike,
    structs::IdentifierCase,
    traits::TableLike,
    utils::is_identity,
};

/// A key or foreign key a table passes to a partition, in the table-constraint
/// spelling.
struct Passed {
    /// Compared without names, since a partition's copy is named afresh.
    constraint: TableConstraint,
    /// The column and option holding it, when the holder writes it on a column.
    written_on: Option<(Ident, ColumnOptionDef)>,
    validated: bool,
}

/// What one table receives from the table it becomes, or already is, a
/// partition of.
#[derive(Default)]
struct Received {
    /// The root and bound the table is attached under, for the attached table
    /// alone.
    edge: Option<(ObjectName, ForValues)>,
    options: Vec<(Ident, ColumnOptionDef)>,
    /// Copies of constraints, each with whether it arrives validated.
    constraints: Vec<(TableConstraint, bool)>,
    /// Constraints the table already held that now count as received.
    merged: Vec<(TableConstraint, bool)>,
    /// What the table's own partitions receive in turn.
    passed_on: Vec<Passed>,
}

/// Applies `ALTER TABLE root ATTACH PARTITION partition`.
///
/// # Errors
///
/// Returns [`Error::AlterTableNotFound`] for an absent root without `IF
/// EXISTS`, [`Error::RelationNotFound`] for an absent partition,
/// [`Error::AlterActionUnsupportedOnRelation`] for a root that is not
/// partitioned or a partition that is a view, and every refusal of a table
/// whose shape cannot join the root's.
pub(super) fn attach_partition(
    builder: ParserDBBuilder,
    root_name: &ObjectName,
    scope: AlterTableScope,
    partition_name: &ObjectName,
    for_values: &ForValues,
) -> Result<ParserDBBuilder, Error> {
    let unsupported = |object_kind, relation_name: &ObjectName| {
        Error::AlterActionUnsupportedOnRelation {
            object_kind,
            relation_name: builder.dialect().stored_object_name_last(relation_name),
            operation: AlterTableOperation::AttachPartitionOf {
                partition: partition_name.clone(),
                for_values: for_values.clone(),
            }
            .to_string(),
            action: Some(crate::errors::RelationAlterAction::Command("ATTACH PARTITION")),
        }
    };

    let Some(root) = ParserDB::alter_table_target(&builder, root_name, scope)? else {
        return Ok(builder);
    };
    super::stored_expressions::refuse_in_bound(&builder, for_values)?;
    let root_node = ParserDB::stored_node(&builder, &root)?;
    if root_node.partition_by.is_none() {
        return Err(unsupported(ObjectKind::Table, root_name));
    }

    let Some(partition_node) = builder.resolve_table_object_name(partition_name)? else {
        if let Some(object_kind) = views::holds_view(&builder, partition_name) {
            return Err(unsupported(object_kind, partition_name));
        }
        if let Some(schema_name) =
            super::missing_schema::missing_schema_name(&builder, partition_name)
        {
            return Err(Error::SchemaNotFound { schema_name });
        }
        return Err(Error::RelationNotFound {
            object_kind: ObjectKind::Table,
            object_name: builder.dialect().stored_object_name(partition_name),
            drop_statement: false,
        });
    };
    let partition = StoredTable::of(partition_node);

    refuse_unattachable(&builder, (&root, root_node), (&partition, partition_node))?;
    partition_bounds::validate_bound(
        &builder,
        root_node,
        &builder.dialect().stored_object_name_last(&partition_node.name),
        for_values,
    )?;
    let merged_checks = existing_child::merge_into_existing(
        &builder,
        (&root, root_node),
        (&partition, partition_node),
    )
    .map_err(|mismatch| {
        Error::PartitionNotAttachable {
            table_name: builder.dialect().stored_object_name_last(&root_node.name),
            partition_name: builder.dialect().stored_object_name_last(&partition_node.name),
            reason: UnattachablePartition::Shape(mismatch),
        }
    })?;

    let passed = passed_keys(&builder, &root, root_node);
    let mut received = receive(&builder, &partition, partition_node, &passed)?;
    received.options.extend(identities_of(root_node, partition_node, builder.identifier_case()));
    received.merged.extend(merged_checks);
    refuse_foreign_under_unique_index(&builder, (&root, root_node), (&partition, partition_node))?;
    let mut bound_root = root_name.clone();
    builder.bind_relation_reference(&mut bound_root, stored_qualifier(&root_node.name));
    received.edge = Some((bound_root, for_values.clone()));

    let column_names: Vec<String> =
        partition_node.columns.iter().map(|column| column.name.value.clone()).collect();
    let mut builder = builder;
    // A partition's columns all belong to its root, as
    // `pg_attribute.attislocal` records.
    for column in &column_names {
        inheritance::mark_inherited(&mut builder, &partition, column);
    }
    triggers::clone_onto_partition(&mut builder, &root, &partition)?;

    let mut frontier = vec![(partition.clone(), received)];
    while let Some((table, mut received)) = frontier.pop() {
        builder = apply(builder, &table, &mut received)?;
        if received.passed_on.is_empty() {
            continue;
        }
        let partitions = inheritance::direct_children(&builder, &table)
            .into_iter()
            .filter_map(|(kind, child)| (kind == ParentKind::PartitionOf).then_some(child));
        for child in partitions {
            let child_node = ParserDB::stored_node(&builder, &child)?;
            let next = receive(&builder, &child, child_node, &received.passed_on)?;
            frontier.push((child, next));
        }
    }

    super::partition_indexes::receive_all(builder, &root, &partition)
}

/// Refuses a foreign table reaching a unique index of the root or of one of
/// its ancestors, either as the partition itself or among its own partitions.
fn refuse_foreign_under_unique_index(
    builder: &ParserDBBuilder,
    (root, root_node): (&StoredTable, &CreateTable),
    (partition, partition_node): (&StoredTable, &CreateTable),
) -> Result<(), Error> {
    if !holds_unique_index_above(builder, root, root_node)? {
        return Ok(());
    }
    if builder.is_foreign_table(partition_node) {
        return Err(Error::PartitionNotAttachable {
            table_name: builder.dialect().stored_object_name_last(&root_node.name),
            partition_name: builder.dialect().stored_object_name_last(&partition_node.name),
            reason: UnattachablePartition::ForeignTableUnderUniqueIndex,
        });
    }
    ParserDB::refuse_unique_index_over_foreign_partition(builder, partition)
}

/// Refuses `CREATE FOREIGN TABLE ... PARTITION OF` a table that holds a unique
/// index, or whose ancestors do, as PostgreSQL refuses to create a foreign
/// partition there. A parent that resolves to nothing is left to the parent
/// resolution that names it.
pub(super) fn refuse_foreign_partition_under_unique_index(
    builder: &ParserDBBuilder,
    partition: &CreateTable,
) -> Result<(), Error> {
    let Some(root_node) = partition
        .partition_of
        .as_ref()
        .map(|root| builder.resolve_table_object_name(root))
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    let root = StoredTable::of(root_node);
    if holds_unique_index_above(builder, &root, root_node)? {
        return Err(Error::UniqueIndexOverForeignPartition {
            table_name: builder.dialect().stored_object_name_last(&root_node.name),
            partition_name: builder.dialect().stored_object_name_last(&partition.name),
            creating_partition: true,
        });
    }
    Ok(())
}

/// Whether `table` or a table it is a partition of holds a unique index, as a
/// key or as a `CREATE UNIQUE INDEX`.
fn holds_unique_index_above(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    node: &CreateTable,
) -> Result<bool, Error> {
    let case = builder.identifier_case();
    let (mut table, mut node) = (table.clone(), node);
    loop {
        let keyed = node.constraints.iter().any(|constraint| {
            matches!(
                constraint,
                TableConstraint::PrimaryKey(_)
                    | TableConstraint::PrimaryKeyUsingIndex(_)
                    | TableConstraint::Unique(_)
                    | TableConstraint::UniqueUsingIndex(_)
            )
        }) || node.columns.iter().flat_map(|column| &column.options).any(|option| {
            matches!(option.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_))
        }) || builder
            .indices()
            .iter()
            .any(|(index, _)| index.attribute().unique && table.matches(index.table(), case));
        if keyed {
            return Ok(true);
        }
        let parent = node.partition_of.as_ref();
        let Some(parent_node) =
            parent.map(|parent| builder.resolve_table_object_name(parent)).transpose()?.flatten()
        else {
            return Ok(false);
        };
        table = StoredTable::of(parent_node);
        node = parent_node;
    }
}

/// Refuses a table PostgreSQL will not attach, in the order PostgreSQL checks.
fn refuse_unattachable(
    builder: &ParserDBBuilder,
    (root, root_node): (&StoredTable, &CreateTable),
    (partition, partition_node): (&StoredTable, &CreateTable),
) -> Result<(), Error> {
    let refuse = |reason| {
        Err(Error::PartitionNotAttachable {
            table_name: builder.dialect().stored_object_name_last(&root_node.name),
            partition_name: builder.dialect().stored_object_name_last(&partition_node.name),
            reason,
        })
    };

    if partition_node.partition_of.is_some() {
        return refuse(UnattachablePartition::AlreadyPartition);
    }
    if partition_node.of_type.is_some() {
        return refuse(UnattachablePartition::Typed);
    }
    if partition_node.inherits.as_ref().is_some_and(|parents| !parents.is_empty()) {
        return refuse(UnattachablePartition::InheritanceChild);
    }
    if inheritance::direct_children(builder, partition)
        .iter()
        .any(|(kind, _)| *kind == ParentKind::Inherits)
    {
        return refuse(UnattachablePartition::InheritanceParent);
    }
    if partition == root || inheritance::descendants(builder, partition).contains(root) {
        return refuse(UnattachablePartition::Circular);
    }

    if partition_node.is_temporary() && !root_node.is_temporary() {
        return Err(Error::TemporaryPartitionOfPermanent {
            table_name: builder.dialect().stored_object_name_last(&partition_node.name),
            parent_name: builder.dialect().stored_object_name_last(&root_node.name),
            attach: true,
        });
    }
    if !partition_node.is_temporary() && root_node.is_temporary() {
        return Err(Error::PermanentRelationInheritsTemporary {
            table_name: builder.dialect().stored_object_name_last(&partition_node.name),
            parent_name: builder.dialect().stored_object_name_last(&root_node.name),
            partition: Some(true),
        });
    }

    let case = builder.identifier_case();
    for column in &partition_node.columns {
        if !root_node
            .columns
            .iter()
            .any(|held| super::case_idents_match(&held.name, &column.name, case))
        {
            return refuse(UnattachablePartition::ExtraColumn {
                column_name: builder.dialect().stored_name(&column.name),
            });
        }
    }
    for column in &partition_node.columns {
        if column.options.iter().any(|option| is_identity(&option.option)) {
            return refuse(UnattachablePartition::IdentityColumn {
                column_name: builder.dialect().stored_name(&column.name),
            });
        }
    }

    if let Some(trigger_name) = triggers::row_transition_trigger(builder, partition) {
        return refuse(UnattachablePartition::RowTransitionTrigger { trigger_name });
    }

    Ok(())
}

/// The root's identities, which PostgreSQL gives the attached table alone and
/// not the partitions below it.
fn identities_of(
    root_node: &CreateTable,
    partition_node: &CreateTable,
    case: IdentifierCase,
) -> Vec<(Ident, ColumnOptionDef)> {
    root_node
        .columns
        .iter()
        .filter_map(|root_column| {
            let identity = root_column.options.iter().find(|option| is_identity(&option.option))?;
            let column = partition_node
                .columns
                .iter()
                .find(|held| super::case_idents_match(&held.name, &root_column.name, case))?;
            Some((column.name.clone(), identity.clone()))
        })
        .collect()
}

/// The keys and foreign keys a table passes to its partitions, in both
/// spellings.
fn passed_keys(builder: &ParserDBBuilder, table: &StoredTable, node: &CreateTable) -> Vec<Passed> {
    let written_on_columns = node.columns.iter().flat_map(|column| {
        column.options.iter().filter_map(|option| {
            Some(Passed {
                constraint: table_form(&column.name, option)?,
                written_on: Some((column.name.clone(), option.clone())),
                validated: true,
            })
        })
    });
    let written_alone =
        node.constraints.iter().filter(|constraint| is_key(constraint)).map(|constraint| {
            Passed {
                constraint: constraint.clone(),
                written_on: None,
                validated: constraint_validity::is_validated(builder, table, constraint),
            }
        });
    written_on_columns.chain(written_alone).collect()
}

fn is_key(constraint: &TableConstraint) -> bool {
    matches!(
        constraint,
        TableConstraint::PrimaryKey(_)
            | TableConstraint::Unique(_)
            | TableConstraint::ForeignKey(_)
    )
}

/// The table constraint a key or foreign key written on `column` stands for.
pub(super) fn table_form(column: &Ident, option: &ColumnOptionDef) -> Option<TableConstraint> {
    let key_column = || {
        vec![IndexColumn {
            column: OrderByExpr {
                expr: Expr::Identifier(column.clone()),
                options: OrderByOptions::default(),
                with_fill: None,
            },
            operator_class: None,
        }]
    };
    Some(match &option.option {
        ColumnOption::PrimaryKey(primary_key) => {
            let mut primary_key = primary_key.clone();
            primary_key.name = primary_key.name.or_else(|| option.name.clone());
            primary_key.columns = key_column();
            TableConstraint::PrimaryKey(primary_key)
        }
        ColumnOption::Unique(unique) => {
            let mut unique = unique.clone();
            unique.name = unique.name.or_else(|| option.name.clone());
            unique.columns = key_column();
            TableConstraint::Unique(unique)
        }
        ColumnOption::ForeignKey(foreign_key) => {
            let mut foreign_key = foreign_key.clone();
            foreign_key.name = foreign_key.name.or_else(|| option.name.clone());
            foreign_key.columns = vec![column.clone()];
            TableConstraint::ForeignKey(foreign_key)
        }
        _ => return None,
    })
}

/// Whether two keys or foreign keys enforce the same thing, whatever each is
/// called and however it spells the names it holds, which is how PostgreSQL
/// matches an index or a foreign key.
pub(super) fn same_key(
    builder: &ParserDBBuilder,
    left: &TableConstraint,
    right: &TableConstraint,
) -> bool {
    let case = builder.identifier_case();
    match (left, right) {
        (TableConstraint::ForeignKey(left), TableConstraint::ForeignKey(right)) => {
            foreign_keys_stored_alike(
                &compared_foreign_key(builder, left),
                &compared_foreign_key(builder, right),
            )
        }
        _ => inheritance::is_copy_of(&compared_key(left, case), &compared_key(right, case)),
    }
}

/// The identifier in the form the parse compares it under, so two spellings
/// of one name read alike.
fn compared_ident(ident: &Ident, case: IdentifierCase) -> Ident {
    Ident::new(case.compared_form(&ident.value, ident.quote_style.is_some()))
}

/// The key with the columns it names in their compared form.
fn compared_key(key: &TableConstraint, case: IdentifierCase) -> TableConstraint {
    let mut compared = key.clone();
    let columns = match &mut compared {
        TableConstraint::PrimaryKey(primary_key) => &mut primary_key.columns,
        TableConstraint::Unique(unique) => &mut unique.columns,
        _ => return compared,
    };
    for column in columns {
        if let Expr::Identifier(ident) = &mut column.column.expr {
            *ident = compared_ident(ident, case);
        }
    }
    compared
}

/// The foreign key with its columns in their compared form and the table it
/// references named as stored, so two spellings reaching one table read
/// alike.
fn compared_foreign_key(
    builder: &ParserDBBuilder,
    key: &ForeignKeyConstraint,
) -> ForeignKeyConstraint {
    let case = builder.identifier_case();
    let mut compared = key.clone();
    if let Ok(Some(table)) = builder.resolve_bound_table_name(&key.foreign_table) {
        compared.foreign_table = table.name.clone();
    }
    for column in compared.columns.iter_mut().chain(&mut compared.referred_columns) {
        *column = compared_ident(column, case);
    }
    compared
}

/// What `table` receives of the keys and foreign keys its parent passes down.
///
/// A table already holding an equivalent keeps its own under its own name,
/// which then counts as received.
fn receive(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    node: &CreateTable,
    passed: &[Passed],
) -> Result<Received, Error> {
    let held = passed_keys(builder, table, node);
    let mut received = Received::default();
    let mut taken = Vec::new();

    for arriving in passed {
        if let Some(own) =
            held.iter().find(|own| same_key(builder, &own.constraint, &arriving.constraint))
        {
            if own.written_on.is_none() {
                let validated = constraint_validity::merged_copy_validated(
                    builder,
                    table,
                    &own.constraint,
                    arriving.validated,
                )?;
                received.merged.push((own.constraint.clone(), validated));
            }
            continue;
        }
        if matches!(arriving.constraint, TableConstraint::PrimaryKey(_))
            && held.iter().any(|own| matches!(own.constraint, TableConstraint::PrimaryKey(_)))
        {
            return Err(Error::MultiplePrimaryKeys {
                table_name: builder.dialect().stored_spelling(&table.name, table.name_quoted),
            });
        }

        if let Some((column, option)) = &arriving.written_on {
            let mut copy = option.clone();
            inheritance::name_partition_key(builder, node, column, &mut copy, &mut taken);
            received.passed_on.push(Passed {
                constraint: arriving.constraint.clone(),
                written_on: Some((column.clone(), copy.clone())),
                validated: true,
            });
            received.options.push((column.clone(), copy));
        } else {
            let copy = inheritance::partition_copy(builder, node, &arriving.constraint, &mut taken);
            received.passed_on.push(Passed {
                constraint: copy.clone(),
                written_on: None,
                validated: arriving.validated,
            });
            received.constraints.push((copy, arriving.validated));
        }
    }

    Ok(received)
}

/// Writes what a table received into its node and its records.
fn apply(
    builder: ParserDBBuilder,
    table: &StoredTable,
    received: &mut Received,
) -> Result<ParserDBBuilder, Error> {
    let edge = received.edge.take();
    let mut options = core::mem::take(&mut received.options);
    let constraints = &received.constraints;
    let mut builder = ParserDB::replace_table_node(builder, table, |_, node| {
        if let Some((root, for_values)) = edge {
            node.partition_of = Some(root);
            node.for_values = Some(for_values);
        }
        for declared in &mut node.columns {
            declared.options.extend(
                options
                    .extract_if(.., |(column, _)| idents_match(column, &declared.name))
                    .map(|(_, option)| option),
            );
        }
        node.constraints.extend(constraints.iter().map(|(constraint, _)| constraint.clone()));
        Ok(())
    })?;

    for (constraint, validated) in &received.constraints {
        inheritance::mark_inherited_constraint(&mut builder, table, constraint);
        if !validated {
            constraint_validity::record(&mut builder, table, constraint, false);
        }
    }
    for (constraint, validated) in &received.merged {
        inheritance::mark_inherited_constraint(&mut builder, table, constraint);
        constraint_validity::record(&mut builder, table, constraint, *validated);
    }

    Ok(builder)
}
