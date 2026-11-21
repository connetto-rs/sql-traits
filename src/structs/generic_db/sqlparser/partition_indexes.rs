//! Standalone indexes inherited by partitions, and the indexes
//! `ALTER INDEX ... ATTACH PARTITION` makes partitions of another.
//!
//! A partitioned index is valid once every direct partition of its table,
//! a foreign one included, holds a valid index attached to it. Validating
//! one may validate the index it is attached to in turn.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use sqlparser::ast::{
    CreateIndex, CreateTable, Expr, Ident, IndexType, NullsDistinctOption, ObjectName,
    TableConstraint,
};

use super::{
    IndexSlot, ParserDB, ParserDBBuilder, StoredTable, attach_partition,
    constraint_names::{self, Position, Slot},
    inheritance::{self, ParentKind},
    object_name_last_identifier,
    partition_index_families::OPERATOR_FAMILIES,
    referenced_keys,
};
use crate::{
    errors::{AttachIndexRefusal, Error},
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
                index_name: builder.dialect().stored_name(held),
                parent_index_name: builder.dialect().stored_name(parent),
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
        referenced_keys::OnDependents::Refuse {
            key_name: &root_name,
            table_name: &table_name,
            dropping_index: true,
        }
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

/// An index a partitioned index tree may hold, either a standalone index at
/// its position in the builder or the index behind the primary key or unique
/// constraint at its position in the table node.
enum Member {
    /// The standalone index at the position.
    Standalone(usize),
    /// The index of the constraint at the position and its metadata key.
    Key(Position, String),
}

/// An index of a table, with the name it answers to.
struct NamedIndex {
    /// The table the index is on.
    table: StoredTable,
    /// The name of the index.
    name: Ident,
    /// Where the index is held.
    member: Member,
}

/// The index on `table` answering to `name`.
fn member_on(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    name: &Ident,
) -> Result<Option<Member>, Error> {
    let case = builder.identifier_case();
    if let Some(position) = builder.indices().iter().position(|(index, _)| {
        table.matches(index.table(), case)
            && index
                .attribute()
                .name
                .as_ref()
                .and_then(object_name_last_identifier)
                .is_some_and(|held| same_name(builder, held, name))
    }) {
        return Ok(Some(Member::Standalone(position)));
    }
    let node = ParserDB::stored_node(builder, table)?;
    Ok(constraint_names::answers(builder, node)
        .into_iter()
        .zip(constraint_names::keyed_slots(node))
        .find(|((slot, held_name), _)| {
            slot.is_unique_key()
                && held_name.as_ref().is_some_and(|key| {
                    case.identifiers_match(key, true, &name.value, name.quote_style.is_some())
                })
        })
        .map(|((slot, _), (_, key))| Member::Key(slot.position(), key)))
}

/// The index `name` reaches, [`None`] for an index on a materialized view.
fn resolve(builder: &ParserDBBuilder, name: &ObjectName) -> Result<Option<NamedIndex>, Error> {
    let not_found =
        || Error::AlterIndexNotFound { index_name: builder.dialect().stored_object_name(name) };
    if let Some(ident) = object_name_last_identifier(name)
        && let Some((table, _)) = constraint_names::key_index_owner(builder, name, ident)
        && let Some(member) = member_on(builder, &table, ident)?
    {
        let name = Ident::with_quote('"', stored_ident_name(ident).as_ref());
        return Ok(Some(NamedIndex { table, name, member }));
    }
    match builder.index_position(name)? {
        None => Err(not_found()),
        Some(IndexSlot::MaterializedView(_)) => Ok(None),
        Some(IndexSlot::Table(position)) => {
            let index = &builder.indices()[position].0;
            let held = index
                .attribute()
                .name
                .as_ref()
                .and_then(object_name_last_identifier)
                .ok_or_else(not_found)?;
            Ok(Some(NamedIndex {
                table: StoredTable::of(index.table()),
                name: held.clone(),
                member: Member::Standalone(position),
            }))
        }
    }
}

/// The definition of `index`, a key's as the index PostgreSQL builds for it.
fn definition<'builder>(
    builder: &'builder ParserDBBuilder,
    index: &NamedIndex,
) -> Result<Option<Cow<'builder, CreateIndex>>, Error> {
    match &index.member {
        Member::Standalone(position) => {
            Ok(Some(Cow::Borrowed(builder.indices()[*position].0.attribute())))
        }
        Member::Key(position, _) => {
            let node = ParserDB::stored_node(builder, &index.table)?;
            Ok(constraint_names::slots(node)
                .into_iter()
                .find(|slot| slot.position() == *position)
                .and_then(|slot| constraint_index(node, slot, &index.name.value))
                .map(Cow::Owned))
        }
    }
}

/// Whether the index `member` of `table` is valid.
fn is_valid(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    member: &Member,
) -> Result<bool, Error> {
    match member {
        Member::Standalone(position) => Ok(builder.indices()[*position].1.is_valid()),
        Member::Key(position, _) => {
            let node = ParserDB::stored_node(builder, table)?;
            Ok(!referenced_keys::invalid_keys(builder, node).contains(position))
        }
    }
}

/// Every index of a partition `table` records, with the index it is attached
/// to.
fn attachments<'builder>(
    builder: &'builder ParserDBBuilder,
    table: &'builder StoredTable,
) -> impl Iterator<Item = &'builder (Ident, Ident)> {
    builder
        .tables()
        .iter()
        .filter(|(node, _, _)| table.matches(node, builder.identifier_case()))
        .flat_map(|(_, metadata, _)| metadata.partition_indexes())
}

/// The table a partition's keys are received from.
fn partition_parent<'builder>(
    builder: &'builder ParserDBBuilder,
    node: &CreateTable,
) -> Option<&'builder CreateTable> {
    node.partition_of
        .as_ref()
        .and_then(|name| builder.resolve_bound_table_name(name).ok().flatten())
}

/// Whether the key in `slot` of the partition `table` is one it received,
/// which a key in the constraint list is only when the table records it.
fn received_key(builder: &ParserDBBuilder, table: &StoredTable, slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column { .. } => true,
        Slot::Table(_, constraint) => {
            inheritance::records_inherited_constraint(builder, table, constraint)
        }
    }
}

/// Every index of a partition `table` attached to an index of its parent,
/// with that index, the keys it received included.
fn links(builder: &ParserDBBuilder, table: &StoredTable) -> Result<Vec<(Ident, Ident)>, Error> {
    let mut links: Vec<(Ident, Ident)> = attachments(builder, table).cloned().collect();
    let node = ParserDB::stored_node(builder, table)?;
    let Some(parent) = partition_parent(builder, node) else {
        return Ok(links);
    };
    let case = builder.identifier_case();
    let parent_keys: Vec<(Cow<'_, TableConstraint>, Cow<'_, str>)> =
        constraint_names::answers(builder, parent)
            .into_iter()
            .filter(|(slot, _)| slot.is_unique_key())
            .filter_map(|(slot, name)| Some((constraint_form(slot)?, name?)))
            .collect();
    for (slot, name) in constraint_names::answers(builder, node) {
        let (Some(name), Some(form)) = (name, constraint_form(slot)) else {
            continue;
        };
        if !slot.is_unique_key()
            || links.iter().any(|(held, _)| {
                case.identifiers_match(&name, true, &held.value, held.quote_style.is_some())
            })
            || !received_key(builder, table, slot)
        {
            continue;
        }
        if let Some((_, parent_name)) = parent_keys
            .iter()
            .find(|(passed, _)| attach_partition::same_key(builder, &form, passed))
        {
            links.push((
                Ident::with_quote('"', name.as_ref()),
                Ident::with_quote('"', parent_name.as_ref()),
            ));
        }
    }
    Ok(links)
}

/// Whether `held`, a key of the partition `child`, was attached to a key of
/// `parent`, its partitioned table, by `ALTER INDEX ... ATTACH PARTITION`.
pub(super) fn attached_to_key(
    builder: &ParserDBBuilder,
    child: &CreateTable,
    parent: &CreateTable,
    held: &TableConstraint,
) -> bool {
    let table = StoredTable::of(child);
    if attachments(builder, &table).next().is_none() {
        return false;
    }
    let case = builder.identifier_case();
    let named = |node, wanted: &dyn Fn(Slot<'_>) -> bool| -> Vec<String> {
        constraint_names::answers(builder, node)
            .into_iter()
            .filter(|(slot, _)| slot.is_unique_key() && wanted(*slot))
            .filter_map(|(_, name)| name.map(Cow::into_owned))
            .collect()
    };
    let held_names = named(child, &|slot| constraint_form(slot).is_some_and(|form| *form == *held));
    let parent_names = named(parent, &|_| true);
    let answers_to = |names: &[String], ident: &Ident| {
        names.iter().any(|name| {
            case.identifiers_match(name, true, &ident.value, ident.quote_style.is_some())
        })
    };
    attachments(builder, &table).any(|(name, parent_name)| {
        answers_to(&held_names, name) && answers_to(&parent_names, parent_name)
    })
}

/// The key of the partition `table` received from or attached to `passed`,
/// the parent's key named `passed_name`, with its position and name.
pub(super) fn linked_key(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    passed: &TableConstraint,
    passed_name: &str,
) -> Result<Option<(Position, TableConstraint, String)>, Error> {
    let node = ParserDB::stored_node(builder, table)?;
    let case = builder.identifier_case();
    Ok(constraint_names::answers(builder, node).into_iter().find_map(|(slot, name)| {
        let form = constraint_form(slot).filter(|_| slot.is_unique_key())?;
        let name = name?;
        let attached = attachments(builder, table).any(|(held, parent)| {
            case.identifiers_match(&name, true, &held.value, held.quote_style.is_some())
                && case.identifiers_match(
                    passed_name,
                    true,
                    &parent.value,
                    parent.quote_style.is_some(),
                )
        });
        (attached
            || (received_key(builder, table, slot)
                && attach_partition::same_key(builder, &form, passed)))
        .then(|| (slot.position(), form.into_owned(), name.into_owned()))
    }))
}

/// Whether every direct partition of `table` holds a valid index attached to
/// its index `name`, which a foreign partition never does.
fn partitions_attached(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    name: &Ident,
) -> Result<bool, Error> {
    for (kind, child) in inheritance::direct_children(builder, table) {
        if kind != ParentKind::PartitionOf {
            continue;
        }
        if builder.is_foreign_table(ParserDB::stored_node(builder, &child)?) {
            return Ok(false);
        }
        let Some((held, _)) = links(builder, &child)?
            .into_iter()
            .find(|(_, parent)| same_name(builder, parent, name))
        else {
            return Ok(false);
        };
        let Some(member) = member_on(builder, &child, &held)? else {
            return Ok(false);
        };
        if !is_valid(builder, &child, &member)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Validates the invalid index `name` of `table` once each partition holds a
/// valid index attached to it, then the index it is attached to the same
/// way, up the tree.
fn revalidate(
    builder: &mut ParserDBBuilder,
    mut table: StoredTable,
    mut name: Ident,
) -> Result<(), Error> {
    loop {
        let Some(member) = member_on(builder, &table, &name)? else {
            return Ok(());
        };
        if is_valid(builder, &table, &member)? || !partitions_attached(builder, &table, &name)? {
            return Ok(());
        }
        match member {
            Member::Standalone(position) => builder.indices_mut()[position].1.set_validity(true),
            Member::Key(_, key) => {
                referenced_keys::set_key_validity(builder, &table, &key, true);
            }
        }
        let Some(parent_name) = links(builder, &table)?
            .into_iter()
            .find(|(held, _)| same_name(builder, held, &name))
            .map(|(_, parent)| parent)
        else {
            return Ok(());
        };
        let Some(parent_table) =
            partition_parent(builder, ParserDB::stored_node(builder, &table)?).map(StoredTable::of)
        else {
            return Ok(());
        };
        table = parent_table;
        name = parent_name;
    }
}

/// Marks invalid the index of `constraint`, a primary key or unique
/// constraint just added `ONLY` to `table`, when the table has a partition,
/// since no index of a partition is attached to it yet.
///
/// # Errors
///
/// Returns the error naming a table the builder does not hold.
pub(super) fn invalidate_key_above_partitions(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    constraint: &TableConstraint,
) -> Result<(), Error> {
    if !matches!(constraint, TableConstraint::PrimaryKey(_) | TableConstraint::Unique(_))
        || !inheritance::direct_children(builder, table)
            .iter()
            .any(|(kind, _)| *kind == ParentKind::PartitionOf)
    {
        return Ok(());
    }
    let Some((_, key)) =
        constraint_names::keyed_slots(ParserDB::stored_node(builder, table)?).pop()
    else {
        return Ok(());
    };
    referenced_keys::set_key_validity(builder, table, &key, false);
    Ok(())
}

/// Applies `ALTER INDEX parent ATTACH PARTITION child`, making the index
/// `child` of a partition the partition of the index `parent` of its
/// partitioned table, which it validates once every partition holds one.
///
/// # Errors
///
/// Returns [`Error::AlterIndexNotFound`] for a name reaching no index,
/// [`Error::RelationKindMismatch`] for one reaching another relation,
/// [`Error::AttachIndexNotSupported`] when `parent` is not on a partitioned
/// table, and [`Error::AttachIndexRefused`] when PostgreSQL refuses to attach
/// `child` to it.
pub(super) fn attach(
    mut builder: ParserDBBuilder,
    parent_name: &ObjectName,
    child_name: &ObjectName,
) -> Result<ParserDBBuilder, Error> {
    let dialect = *builder.dialect();
    let not_supported = || {
        Error::AttachIndexNotSupported { index_name: dialect.stored_object_name_last(parent_name) }
    };
    let parent = resolve(&builder, parent_name)?.ok_or_else(not_supported)?;
    let parent_node = ParserDB::stored_node(&builder, &parent.table)?;
    if parent_node.partition_by.is_none() {
        return Err(not_supported());
    }
    let parent_table_name = dialect.stored_object_name_last(&parent_node.name);
    let child = resolve(&builder, child_name)?;
    let refused = |index_name: String, reason: AttachIndexRefusal| {
        Error::AttachIndexRefused {
            index_name,
            parent_index_name: dialect.stored_name(&parent.name),
            reason,
        }
    };
    let Some(child) = child else {
        return Err(refused(
            dialect.stored_object_name_last(child_name),
            AttachIndexRefusal::NotOnPartition { table_name: parent_table_name },
        ));
    };
    let links = links(&builder, &child.table)?;
    let attached_to = links
        .iter()
        .find(|(held, _)| same_name(&builder, held, &child.name))
        .map(|(_, attached_to)| same_name(&builder, attached_to, &parent.name));
    if attached_to == Some(true) {
        revalidate(&mut builder, parent.table, parent.name)?;
        return Ok(builder);
    }
    let child_index_name = || dialect.stored_name(&child.name);
    if links.iter().any(|(_, attached_to)| same_name(&builder, attached_to, &parent.name)) {
        let partition_name =
            dialect.stored_object_name_last(&ParserDB::stored_node(&builder, &child.table)?.name);
        return Err(refused(
            child_index_name(),
            AttachIndexRefusal::PartitionHasIndex { partition_name },
        ));
    }
    if attached_to.is_some() {
        return Err(refused(child_index_name(), AttachIndexRefusal::AttachedElsewhere));
    }
    let child_node = ParserDB::stored_node(&builder, &child.table)?;
    let case = builder.identifier_case();
    if !inheritance::direct_children(&builder, &parent.table)
        .iter()
        .any(|(kind, table)| *kind == ParentKind::PartitionOf && table.matches(child_node, case))
    {
        return Err(refused(
            child_index_name(),
            AttachIndexRefusal::NotOnPartition { table_name: parent_table_name },
        ));
    }
    let parent_node = ParserDB::stored_node(&builder, &parent.table)?;
    let matching = definition(&builder, &parent)?.zip(definition(&builder, &child)?).is_some_and(
        |(parent_definition, child_definition)| {
            equivalent(&builder, parent_node, &parent_definition, child_node, &child_definition)
        },
    );
    if !matching {
        return Err(refused(child_index_name(), AttachIndexRefusal::DefinitionsDiffer));
    }
    if matches!(parent.member, Member::Key(..)) && matches!(child.member, Member::Standalone(_)) {
        return Err(refused(
            child_index_name(),
            AttachIndexRefusal::ConstraintMissing { table_name: parent_table_name },
        ));
    }
    if let (Member::Key(..), Member::Key(position, _)) = (&parent.member, &child.member)
        && let Some(form) = constraint_names::slots(child_node)
            .into_iter()
            .find(|slot| slot.position() == *position)
            .and_then(constraint_form)
            .map(Cow::into_owned)
    {
        inheritance::mark_inherited_constraint(&mut builder, &child.table, &form);
    }
    record(&mut builder, &child.table, child.name, parent.name.clone());
    revalidate(&mut builder, parent.table, parent.name)?;
    Ok(builder)
}
