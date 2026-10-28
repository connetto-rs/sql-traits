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
    AlterTableOperation, CheckConstraint, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable,
    Expr, ForValues, Ident, IndexColumn, ObjectName, OrderByExpr, OrderByOptions, TableConstraint,
};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, StoredTable, bind_reference, constraint_validity,
    idents_match,
    inheritance::{self, ParentKind},
    states_not_null, stored_column_collation_name, stored_qualifier, views,
};
use crate::{
    errors::{Error, ObjectKind, UnattachablePartition},
    traits::TableLike,
    utils::is_identity,
};

/// A key or foreign key a table passes to a partition, in the table-constraint
/// spelling.
struct Passed {
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
            relation_name: relation_name.to_string(),
            operation: AlterTableOperation::AttachPartitionOf {
                partition: partition_name.clone(),
                for_values: for_values.clone(),
            }
            .to_string(),
        }
    };

    let Some(root) = ParserDB::alter_table_target(&builder, root_name, scope)? else {
        return Ok(builder);
    };
    let root_node = ParserDB::stored_node(&builder, &root)?;
    if root_node.partition_by.is_none() {
        return Err(unsupported(ObjectKind::Table, root_name));
    }

    let Some(partition_node) = builder.resolve_table_object_name(partition_name)? else {
        if let Some(object_kind) = views::holds_view(&builder, partition_name) {
            return Err(unsupported(object_kind, partition_name));
        }
        return Err(Error::RelationNotFound {
            object_kind: ObjectKind::Table,
            object_name: partition_name.to_string(),
        });
    };
    let partition = StoredTable::of(partition_node);

    refuse_unattachable(&builder, (&root, root_node), (&partition, partition_node))?;

    let passed = passed_keys(&builder, &root, root_node);
    let mut received = receive(&builder, root_node, &partition, partition_node, &passed)?;
    received.options.extend(identities_of(root_node, partition_node));
    received.merged.extend(held_checks(
        &builder,
        (&root, root_node),
        (&partition, partition_node),
    )?);
    let mut bound_root = root_name.clone();
    bind_reference(&mut bound_root, stored_qualifier(&root_node.name));
    received.edge = Some((bound_root, for_values.clone()));

    let column_names: Vec<String> =
        partition_node.columns.iter().map(|column| column.name.value.clone()).collect();
    let mut builder = builder;
    // A partition's columns all belong to its root, as
    // `pg_attribute.attislocal` records.
    for column in &column_names {
        inheritance::mark_inherited(&mut builder, &partition, column);
    }

    let mut frontier = vec![(partition, received)];
    while let Some((table, mut received)) = frontier.pop() {
        builder = apply(builder, &table, &mut received)?;
        if received.passed_on.is_empty() {
            continue;
        }
        for (kind, child) in inheritance::direct_children(&builder, &table) {
            if kind != ParentKind::PartitionOf {
                continue;
            }
            let parent_node = ParserDB::stored_node(&builder, &table)?;
            let child_node = ParserDB::stored_node(&builder, &child)?;
            let next = receive(&builder, parent_node, &child, child_node, &received.passed_on)?;
            frontier.push((child, next));
        }
    }

    Ok(builder)
}

/// Refuses a table PostgreSQL will not attach, in the order PostgreSQL checks.
fn refuse_unattachable(
    builder: &ParserDBBuilder,
    (root, root_node): (&StoredTable, &CreateTable),
    (partition, partition_node): (&StoredTable, &CreateTable),
) -> Result<(), Error> {
    let refuse = |reason| {
        Err(Error::PartitionNotAttachable {
            table_name: root.name.clone(),
            partition_name: partition.name.clone(),
            reason,
        })
    };

    if partition_node.partition_of.is_some() {
        return refuse(UnattachablePartition::AlreadyPartition);
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
            table_name: partition.name.clone(),
            parent_name: root.name.clone(),
        });
    }
    if !partition_node.is_temporary() && root_node.is_temporary() {
        return Err(Error::PermanentRelationInheritsTemporary {
            table_name: partition.name.clone(),
            parent_name: root.name.clone(),
        });
    }

    for column in &partition_node.columns {
        if !root_node.columns.iter().any(|held| inheritance::same_column(&held.name, &column.name))
        {
            return refuse(UnattachablePartition::ExtraColumn {
                column_name: column.name.value.clone(),
            });
        }
    }
    for column in &partition_node.columns {
        if column.options.iter().any(|option| is_identity(&option.option)) {
            return refuse(UnattachablePartition::IdentityColumn {
                column_name: column.name.value.clone(),
            });
        }
    }

    for root_column in &root_node.columns {
        let Some(column) = partition_node
            .columns
            .iter()
            .find(|held| inheritance::same_column(&held.name, &root_column.name))
        else {
            return refuse(UnattachablePartition::MissingColumn {
                column_name: root_column.name.value.clone(),
            });
        };
        if inheritance::types_conflict(&root_column.data_type, &column.data_type) {
            return Err(Error::InheritedColumnTypeConflict {
                column_name: column.name.value.clone(),
                child_table: partition.name.clone(),
                child_type: column.data_type.to_string(),
                parent_table: root.name.clone(),
                parent_type: root_column.data_type.to_string(),
            });
        }
        if inheritance::collations_conflict(
            builder,
            (root_node, root_column),
            (partition_node, column),
        ) {
            let collation = |column: &ColumnDef| {
                stored_column_collation_name(column)
                    .map_or_else(|| "default".to_string(), ObjectName::to_string)
                    .into_boxed_str()
            };
            return Err(Error::InheritedColumnCollationConflict {
                column_name: column.name.value.clone().into_boxed_str(),
                child_table: partition.name.clone().into_boxed_str(),
                child_collation: collation(column),
                parent_table: root.name.clone().into_boxed_str(),
                parent_collation: collation(root_column),
            });
        }
        if states_not_null(root_column) && !states_not_null(column) {
            return refuse(UnattachablePartition::NullableColumn {
                column_name: column.name.value.clone(),
            });
        }
        let generated_in_root = is_stored_generated(root_column);
        if generated_in_root != is_stored_generated(column) {
            return refuse(UnattachablePartition::GenerationMismatch {
                column_name: column.name.value.clone(),
                generated_in_root,
            });
        }
    }

    Ok(())
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

/// Finds the partition's own copy of each check the root passes down, refusing
/// a missing one and one that cannot merge with the root's.
///
/// PostgreSQL matches by name. A check the root left unnamed is matched by its
/// expression, because the model does not hold the name PostgreSQL would give
/// it.
fn held_checks(
    builder: &ParserDBBuilder,
    (root, root_node): (&StoredTable, &CreateTable),
    (partition, partition_node): (&StoredTable, &CreateTable),
) -> Result<Vec<(TableConstraint, bool)>, Error> {
    let held = checks_of(builder, partition, partition_node);
    let mut merged = Vec::new();

    for (check, validated) in checks_of(builder, root, root_node) {
        if check.no_inherit {
            continue;
        }
        let found = held.iter().find(|(own, _)| {
            match &check.name {
                Some(name) => own.name.as_ref().is_some_and(|own| idents_match(own, name)),
                None => own.expr == check.expr,
            }
        });
        let Some((own, _)) = found else {
            return Err(Error::PartitionNotAttachable {
                table_name: root.name.clone(),
                partition_name: partition.name.clone(),
                reason: UnattachablePartition::MissingCheck { constraint: check.to_string() },
            });
        };
        if own.expr != check.expr || own.no_inherit {
            return Err(Error::InheritedConstraintConflict {
                table_name: partition.name.clone(),
                constraint_name: own
                    .name
                    .as_ref()
                    .map_or_else(|| own.to_string(), |name| name.value.clone()),
            });
        }

        let own = TableConstraint::Check(own.clone());
        if partition_node.constraints.contains(&own) {
            let validated =
                constraint_validity::merged_copy_validated(builder, partition, &own, validated)?;
            merged.push((own, validated));
        }
    }

    Ok(merged)
}

/// The root's identities, which PostgreSQL gives the attached table alone and
/// not the partitions below it.
fn identities_of(
    root_node: &CreateTable,
    partition_node: &CreateTable,
) -> Vec<(Ident, ColumnOptionDef)> {
    root_node
        .columns
        .iter()
        .filter_map(|root_column| {
            let identity = root_column.options.iter().find(|option| is_identity(&option.option))?;
            let column = partition_node
                .columns
                .iter()
                .find(|held| inheritance::same_column(&held.name, &root_column.name))?;
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
fn table_form(column: &Ident, option: &ColumnOptionDef) -> Option<TableConstraint> {
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

/// The constraint with every name stripped, since PostgreSQL matches an index
/// or a foreign key by what it enforces rather than by what it is called.
fn nameless(constraint: &TableConstraint) -> TableConstraint {
    let mut bare = constraint.clone();
    match &mut bare {
        TableConstraint::PrimaryKey(primary_key) => {
            primary_key.name = None;
            primary_key.index_name = None;
        }
        TableConstraint::Unique(unique) => {
            unique.name = None;
            unique.index_name = None;
        }
        TableConstraint::ForeignKey(foreign_key) => {
            foreign_key.name = None;
            foreign_key.index_name = None;
        }
        _ => {}
    }
    bare
}

/// What `table` receives of the keys and foreign keys its parent passes down.
///
/// A table already holding an equivalent keeps its own under its own name,
/// which then counts as received.
fn receive(
    builder: &ParserDBBuilder,
    parent_node: &CreateTable,
    table: &StoredTable,
    node: &CreateTable,
    passed: &[Passed],
) -> Result<Received, Error> {
    let held = passed_keys(builder, table, node);
    let mut received = Received::default();
    let mut taken = Vec::new();

    for arriving in passed {
        let bare = nameless(&arriving.constraint);
        if let Some(own) = held.iter().find(|own| nameless(&own.constraint) == bare) {
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
            return Err(Error::PartitionNotAttachable {
                table_name: StoredTable::of(parent_node).name,
                partition_name: table.name.clone(),
                reason: UnattachablePartition::OtherPrimaryKey,
            });
        }

        if let Some((column, option)) = &arriving.written_on {
            let mut copy = option.clone();
            inheritance::name_partition_key(builder, node, column, &mut copy, &mut taken);
            if let Some(constraint) = table_form(column, &copy) {
                received.passed_on.push(Passed {
                    constraint,
                    written_on: Some((column.clone(), copy.clone())),
                    validated: true,
                });
            }
            received.options.push((column.clone(), copy));
        } else if let Some(copy) = inheritance::received_constraint(
            builder,
            ParentKind::PartitionOf,
            node,
            &arriving.constraint,
            &mut taken,
        ) {
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
    let options = core::mem::take(&mut received.options);
    let constraints = &received.constraints;
    let mut builder = ParserDB::replace_table_node(builder, table, |_, node| {
        if let Some((root, for_values)) = edge {
            node.partition_of = Some(root);
            node.for_values = Some(for_values);
        }
        for (column, option) in options {
            if let Some(declared) =
                node.columns.iter_mut().find(|declared| idents_match(&declared.name, &column))
            {
                declared.options.push(option);
            }
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
        if !inheritance::records_inherited_constraint(&builder, table, constraint) {
            inheritance::mark_inherited_constraint(&mut builder, table, constraint);
        }
        constraint_validity::record(&mut builder, table, constraint, *validated);
    }

    Ok(builder)
}
