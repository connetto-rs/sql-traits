//! Standalone indexes inherited by partitions.

use alloc::{borrow::Cow, string::ToString, sync::Arc, vec, vec::Vec};

use sqlparser::ast::{
    CreateIndex, CreateTable, Expr, Ident, IndexType, NullsDistinctOption, ObjectName,
    TableConstraint,
};

use super::{
    IndexSlot, ParserDB, ParserDBBuilder, StoredTable, attach_partition,
    constraint_names::{self, Slot},
    inheritance::{self, ParentKind},
    object_name_last_identifier,
    partition_index_families::OPERATOR_FAMILIES,
    referenced_keys,
};
use crate::{
    errors::Error,
    traits::TableLike,
    utils::identifier_resolution::{stored_ident_name, stored_identifier},
};

/// Whether two identifiers name the same stored object.
fn same_name(builder: &ParserDBBuilder, left: &Ident, right: &Ident) -> bool {
    super::case_idents_match(left, right, builder.identifier_case())
}

/// The operator family and its default status for a builtin operator class.
fn operator_family(method: &IndexType, name: &ObjectName) -> Option<(&'static str, bool)> {
    let identifier = object_name_last_identifier(name)?;
    OPERATOR_FAMILIES.iter().find_map(|(held_method, held_name, family, default)| {
        (held_method == method
            && crate::utils::identifier_resolution::identifiers_match(
                &identifier.value,
                identifier.quote_style.is_some(),
                held_name,
                false,
            ))
        .then_some((*family, *default))
    })
}

/// Whether the two operator class selections belong to the same family.
fn same_family(method: &IndexType, left: Option<&ObjectName>, right: Option<&ObjectName>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left == right
                || operator_family(method, left)
                    .zip(operator_family(method, right))
                    .is_some_and(|(left, right)| left.0 == right.0)
        }
        (Some(name), None) | (None, Some(name)) => {
            operator_family(method, name).is_some_and(|(_, default)| default)
        }
    }
}

/// Whether expressions have the same parsed meaning for index attachment.
fn same_expression(builder: &ParserDBBuilder, left: &Expr, right: &Expr) -> bool {
    match (left, right) {
        (Expr::Identifier(left), Expr::Identifier(right)) => same_name(builder, left, right),
        _ => left == right || inheritance::parsed_expr(left) == inheritance::parsed_expr(right),
    }
}

/// The indexed expression and its explicit or column-inherited collation.
fn index_expression<'table>(
    builder: &ParserDBBuilder,
    table: &'table CreateTable,
    expression: &'table Expr,
) -> (&'table Expr, Option<&'table ObjectName>) {
    match expression {
        Expr::Nested(inner) => index_expression(builder, table, inner),
        Expr::Collate { expr: inner, collation } => {
            let (expression, _) = index_expression(builder, table, inner);
            (expression, Some(collation))
        }
        Expr::Identifier(identifier) => {
            let collation = table
                .columns
                .iter()
                .find(|column| same_name(builder, &column.name, identifier))
                .and_then(super::stored_column_collation_name);
            (expression, collation)
        }
        _ => (expression, None),
    }
}

/// Whether PostgreSQL can attach one index to the other.
fn equivalent(
    builder: &ParserDBBuilder,
    left_table: &CreateTable,
    left: &CreateIndex,
    right_table: &CreateTable,
    right: &CreateIndex,
) -> bool {
    let method = left.using.as_ref().unwrap_or(&IndexType::BTree);
    left.unique == right.unique
        && left.nulls_distinct.unwrap_or(true) == right.nulls_distinct.unwrap_or(true)
        && method == right.using.as_ref().unwrap_or(&IndexType::BTree)
        && left.columns.len() == right.columns.len()
        && left.include.len() == right.include.len()
        && left
            .include
            .iter()
            .zip(&right.include)
            .all(|(left, right)| same_name(builder, left, right))
        && left.columns.iter().zip(&right.columns).all(|(left, right)| {
            let (left_expression, left_collation) =
                index_expression(builder, left_table, &left.column.expr);
            let (right_expression, right_collation) =
                index_expression(builder, right_table, &right.column.expr);
            left_collation == right_collation
                && same_expression(builder, left_expression, right_expression)
                && same_family(method, left.operator_class.as_ref(), right.operator_class.as_ref())
        })
        && match (&left.predicate, &right.predicate) {
            (None, None) => true,
            (Some(left), Some(right)) => same_expression(builder, left, right),
            _ => false,
        }
}

/// A column or table constraint in its table-constraint representation.
fn constraint_form(slot: Slot<'_>) -> Option<Cow<'_, TableConstraint>> {
    match slot {
        Slot::Column { def, held, .. } => {
            attach_partition::table_form(&def.name, held).map(Cow::Owned)
        }
        Slot::Table(_, constraint) => Some(Cow::Borrowed(constraint)),
    }
}

/// Whether a constraint index belongs to a different constraint on the parent.
fn has_other_constraint_parent(
    builder: &ParserDBBuilder,
    parent_table: &CreateTable,
    slot: Slot<'_>,
    parent_name: &Ident,
) -> bool {
    let Some(held) = constraint_form(slot) else {
        return false;
    };
    constraint_names::answers(builder, parent_table).into_iter().any(|(slot, name)| {
        name.is_some_and(|name| {
            !builder.identifier_case().identifiers_match(
                &name,
                true,
                &parent_name.value,
                parent_name.quote_style.is_some(),
            ) && constraint_form(slot).is_some_and(|passed| inheritance::is_copy_of(&held, &passed))
        })
    })
}

/// The index declaration represented by an existing primary or unique
/// constraint.
fn constraint_index(node: &CreateTable, slot: Slot<'_>, name: &str) -> Option<CreateIndex> {
    let constraint = constraint_form(slot)?;
    let (columns, include, using, nulls_distinct) = match constraint.as_ref() {
        TableConstraint::PrimaryKey(key) => (&key.columns, &key.include, &key.index_type, true),
        TableConstraint::Unique(key) => {
            (
                &key.columns,
                &key.include,
                &key.index_type,
                key.nulls_distinct != NullsDistinctOption::NotDistinct,
            )
        }
        _ => return None,
    };
    Some(CreateIndex {
        name: Some(ObjectName::from(vec![Ident::with_quote('"', name)])),
        only: false,
        table_name: node.name.clone(),
        using: using.clone(),
        columns: columns.clone(),
        unique: true,
        concurrently: false,
        r#async: false,
        if_not_exists: false,
        include: include.clone(),
        nulls_distinct: Some(nulls_distinct),
        with: Vec::new(),
        predicate: None,
        index_options: Vec::new(),
        alter_options: Vec::new(),
    })
}

/// Whether an index is already attached to a parent index.
fn attached(builder: &ParserDBBuilder, table: &StoredTable, name: &Ident) -> bool {
    builder
        .tables()
        .iter()
        .filter(|(node, _, _)| table.matches(node, builder.identifier_case()))
        .any(|(_, metadata, _)| {
            metadata.partition_indexes().iter().any(|(held, _)| same_name(builder, held, name))
        })
}

/// Records the immediate index parent on a partition.
fn record(builder: &mut ParserDBBuilder, table: &StoredTable, name: Ident, parent: Ident) {
    let case = builder.identifier_case();
    if let Some((_, metadata, _)) =
        builder.tables_mut().iter_mut().find(|(node, _, _)| table.matches(node, case))
    {
        metadata.partition_indexes_mut().push((name, parent));
    }
}

/// Creates or adopts the copy required on one partition and its descendants.
fn receive(
    mut builder: ParserDBBuilder,
    parent: &CreateIndex,
    child: &StoredTable,
) -> Result<ParserDBBuilder, Error> {
    let Some(parent_name) = parent.name.as_ref().and_then(object_name_last_identifier) else {
        return Ok(builder);
    };
    let node = ParserDB::stored_node(&builder, child)?;
    if builder.is_foreign_table(node)
        || builder
            .tables()
            .iter()
            .filter(|(node, _, _)| child.matches(node, builder.identifier_case()))
            .any(|(_, metadata, _)| {
                metadata
                    .partition_indexes()
                    .iter()
                    .any(|(_, held_parent)| same_name(&builder, held_parent, parent_name))
            })
    {
        return Ok(builder);
    }
    let parent_table = builder.index_table(parent)?;
    let candidate = builder
        .indices()
        .iter()
        .find(|(index, _)| {
            child.matches(index.table(), builder.identifier_case())
                && index
                    .attribute()
                    .name
                    .as_ref()
                    .and_then(object_name_last_identifier)
                    .is_some_and(|name| !attached(&builder, child, name))
                && equivalent(&builder, parent_table, parent, index.table(), index.attribute())
        })
        .map(|(index, _)| Arc::clone(index));
    if let Some(index) = candidate {
        if let Some(name) = index.attribute().name.as_ref().and_then(object_name_last_identifier) {
            record(&mut builder, child, name.clone(), parent_name.clone());
        }
        return propagate(builder, index.attribute());
    }
    let candidate =
        constraint_names::answers(&builder, node).into_iter().find_map(|(slot, name)| {
            let name = name?;
            let definition = constraint_index(node, slot, &name)?;
            let identifier = Ident::with_quote('"', name.as_ref());
            (!attached(&builder, child, &identifier)
                && !has_other_constraint_parent(&builder, parent_table, slot, parent_name)
                && equivalent(&builder, parent_table, parent, node, &definition))
            .then_some((definition, identifier))
        });
    if let Some((definition, name)) = candidate {
        record(&mut builder, child, name, parent_name.clone());
        return propagate(builder, &definition);
    }
    let mut copy = parent.clone();
    copy.table_name = node.name.clone();
    copy.name = builder.generated_index_name(&copy)?.map(|name| ObjectName::from(vec![name]));
    copy.if_not_exists = false;
    copy.only = false;
    let (index, metadata) = ParserDB::process_create_index(copy, &builder)?;
    let case = builder.identifier_case();
    for (_, table_metadata, _) in
        builder.tables_mut().iter_mut().filter(|(node, _, _)| child.matches(node, case))
    {
        table_metadata.add_index(Arc::clone(&index));
    }
    if let Some(name) = index.attribute().name.as_ref().and_then(object_name_last_identifier) {
        record(&mut builder, child, name.clone(), parent_name.clone());
    }
    let metadata = metadata.with_key_identity(Some(builder.draw_key_identity()));
    builder = builder.add_index(Arc::clone(&index), metadata);
    propagate(builder, index.attribute())
}

/// Passes an index to every direct partition and recursively through its tree.
pub(super) fn propagate(
    mut builder: ParserDBBuilder,
    parent: &CreateIndex,
) -> Result<ParserDBBuilder, Error> {
    let table = StoredTable::of(builder.index_table(parent)?);
    for (kind, child) in inheritance::direct_children(&builder, &table) {
        if kind == ParentKind::PartitionOf {
            builder = receive(builder, parent, &child)?;
        }
    }
    Ok(builder)
}

/// Receives the standalone indexes of a newly assigned partition parent.
pub(super) fn receive_all(
    mut builder: ParserDBBuilder,
    root: &StoredTable,
    partition: &StoredTable,
) -> Result<ParserDBBuilder, Error> {
    let indexes: Vec<_> = builder
        .indices()
        .iter()
        .filter(|(index, _)| root.matches(index.table(), builder.identifier_case()))
        .map(|(index, _)| Arc::clone(index))
        .collect();
    for index in indexes {
        builder = receive(builder, index.attribute(), partition)?;
    }
    Ok(builder)
}

/// Refuses independent removal of an attached partition index.
pub(super) fn refuse_drop(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    name: &Ident,
) -> Result<(), Error> {
    for (_, metadata, _) in builder
        .tables()
        .iter()
        .filter(|(node, _, _)| table.matches(node, builder.identifier_case()))
    {
        if let Some((held, parent)) =
            metadata.partition_indexes().iter().find(|(held, _)| same_name(builder, held, name))
        {
            return Err(Error::IndexRequiredByParent {
                index_name: held.value.clone(),
                parent_index_name: parent.value.clone(),
            });
        }
    }
    Ok(())
}

/// Follows an index rename without renaming any other index.
pub(super) fn rename(builder: &mut ParserDBBuilder, table: &StoredTable, old: &Ident, new: &Ident) {
    let children = inheritance::direct_children(builder, table);
    let case = builder.identifier_case();
    for (node, metadata, _) in builder.tables_mut() {
        let own = table.matches(node, case);
        let child = children
            .iter()
            .any(|(kind, child)| *kind == ParentKind::PartitionOf && child.matches(node, case));
        for (name, parent) in metadata.partition_indexes_mut() {
            if own && super::case_idents_match(name, old, case) {
                *name = new.clone();
            }
            if child && super::case_idents_match(parent, old, case) {
                *parent = new.clone();
            }
        }
    }
}

/// Removes attachment records whose index disappeared during a table rebuild.
pub(super) fn prune(builder: &mut ParserDBBuilder, table: &StoredTable) -> Result<(), Error> {
    let case = builder.identifier_case();
    let position = builder
        .tables()
        .iter()
        .position(|(node, _, _)| table.matches(node, case))
        .ok_or_else(|| crate::errors::ObjectKind::Table.not_in_database(&table.name))?;
    let mut attachments = core::mem::take(builder.tables_mut()[position].1.partition_indexes_mut());
    if !attachments.is_empty() {
        let keys = constraint_names::answers(builder, &builder.tables()[position].0);
        attachments.retain(|(name, _)| {
            builder.indices().iter().any(|(index, _)| {
                table.matches(index.table(), case)
                    && index
                        .attribute()
                        .name
                        .as_ref()
                        .and_then(object_name_last_identifier)
                        .is_some_and(|held| same_name(builder, held, name))
            }) || keys.iter().any(|(slot, key)| {
                slot.is_unique_key()
                    && key.as_ref().is_some_and(|key| {
                        case.identifiers_match(key, true, &name.value, name.quote_style.is_some())
                    })
            })
        });
    }
    *builder.tables_mut()[position].1.partition_indexes_mut() = attachments;
    Ok(())
}

/// The root index and its attached descendants in parent-first order.
fn index_tree(
    builder: &ParserDBBuilder,
    root: StoredTable,
    name: Ident,
) -> Vec<(StoredTable, Ident)> {
    let mut frontier = vec![(root, name)];
    let mut copies = Vec::new();
    while let Some((table, parent)) = frontier.pop() {
        for (kind, child) in inheritance::direct_children(builder, &table) {
            if kind != ParentKind::PartitionOf {
                continue;
            }
            for (_, metadata, _) in builder
                .tables()
                .iter()
                .filter(|(node, _, _)| child.matches(node, builder.identifier_case()))
            {
                for (name, _) in metadata
                    .partition_indexes()
                    .iter()
                    .filter(|(_, held)| same_name(builder, held, &parent))
                {
                    frontier.push((child.clone(), name.clone()));
                }
            }
        }
        copies.push((table, parent));
    }
    copies
}

/// Drops an index and all attached indexes, respecting adopted constraints.
pub(super) fn drop_index(
    mut builder: ParserDBBuilder,
    position: usize,
    cascade: bool,
) -> Result<ParserDBBuilder, Error> {
    let index = Arc::clone(&builder.indices()[position].0);
    let root = StoredTable::of(index.table());
    let Some(name) = index.attribute().name.as_ref().and_then(object_name_last_identifier) else {
        builder.remove_index_at(IndexSlot::Table(position));
        return Ok(builder);
    };
    refuse_drop(&builder, &root, name)?;
    let copies = index_tree(&builder, root, name.clone());
    let root_name = stored_ident_name(name);
    let table_name =
        stored_identifier(index.table().table_name(), index.table().table_name_is_quoted());
    let mut removed = Vec::new();
    for (table, name) in &copies {
        let node = ParserDB::stored_node(&builder, table)?;
        if let Some((slot, _)) =
            constraint_names::answers(&builder, node).into_iter().find(|(slot, held)| {
                slot.is_unique_key() && held.as_deref() == Some(name.value.as_str())
            })
        {
            if !cascade {
                return Err(Error::IndexHasDependentConstraints {
                    index_name: root_name.to_string(),
                });
            }
            removed.extend(referenced_keys::key_identity(&builder, node, slot));
        } else if let Some((_, metadata)) = builder.indices().iter().find(|(index, _)| {
            table.matches(index.table(), builder.identifier_case())
                && index
                    .attribute()
                    .name
                    .as_ref()
                    .and_then(object_name_last_identifier)
                    .is_some_and(|held| same_name(&builder, held, name))
        }) {
            removed.extend(metadata.key_identity());
        }
    }
    let on_dependents = if cascade {
        referenced_keys::OnDependents::Cascade
    } else {
        referenced_keys::OnDependents::Refuse { key_name: &root_name, table_name: &table_name }
    };
    ParserDB::drop_keys(builder, &removed, on_dependents, |mut builder| {
        for (table, name) in copies.into_iter().rev() {
            let node = ParserDB::stored_node(&builder, &table)?;
            let position = constraint_names::answers(&builder, node)
                .into_iter()
                .find(|(slot, held)| {
                    slot.is_unique_key() && held.as_deref() == Some(name.value.as_str())
                })
                .map(|(slot, _)| slot.position());
            if let Some(position) = position {
                builder =
                    ParserDB::replace_table_node_removing(builder, &table, position, |_, node| {
                        match position {
                            constraint_names::Position::Table(position) => {
                                node.constraints.remove(position);
                            }
                            constraint_names::Position::Column(column, option) => {
                                node.columns[column].options.remove(option);
                            }
                        }
                        Ok(())
                    })?;
            } else if let Some(position) = builder.indices().iter().position(|(index, _)| {
                table.matches(index.table(), builder.identifier_case())
                    && index
                        .attribute()
                        .name
                        .as_ref()
                        .and_then(object_name_last_identifier)
                        .is_some_and(|held| same_name(&builder, held, &name))
            }) {
                builder.remove_index_at(IndexSlot::Table(position));
            }
            prune(&mut builder, &table)?;
        }
        Ok(builder)
    })
}
