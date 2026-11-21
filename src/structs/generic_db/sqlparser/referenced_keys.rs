//! The key of its referenced table each foreign key depends on, which
//! PostgreSQL records as `pg_constraint.conindid`, and what dropping that key
//! meets.
//!
//! A foreign key is bound when it is declared, to the primary key when it
//! names no referenced columns and otherwise to the oldest primary key,
//! unique constraint or unique index over its columns. The model keeps the
//! identity each key was created with and which foreign keys named no
//! columns, and reads the binding from those, since nothing that orders the
//! keys or that a binding reads changes while the foreign key stands.

use alloc::{boxed::Box, string::String, vec::Vec};

use sqlparser::ast::{
    ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, ForeignKeyConstraint, Ident,
    TableConstraint,
};

use super::{
    ParserDB, ParserDBBuilder, StoredTable,
    constraint_names::{self, Position, Slot},
    inheritance::{self, ParentKind},
    key_columns_match, plain_key_columns,
};
use crate::{
    errors::Error,
    structs::{IdentifierCase, KeyIdentity, TableMetadata, metadata::KeyCreation},
    utils::object_name::table_matches_object_name,
};

/// What a statement dropping a key says of the foreign keys depending on it.
#[derive(Clone, Copy)]
pub(super) enum OnDependents<'statement> {
    /// Refuse the drop, naming the key and its table as the statement does.
    Refuse {
        /// The name of the key or unique index.
        key_name: &'statement str,
        /// The name of the table holding it.
        table_name: &'statement str,
    },
    /// Take each of them along, which is what `CASCADE` does.
    Cascade,
}

/// A foreign key of the schema, where it is held, and the key it depends on.
struct Dependence {
    /// The table declaring the foreign key.
    host: StoredTable,
    /// The foreign key as the host's node holds it.
    held: HeldConstraint,
    /// The key of the referenced table it depends on.
    key: KeyIdentity,
}

/// A constraint as a table node holds it and where it sat, so it is found
/// again after an edit moved it.
struct HeldConstraint {
    /// The constraint, which a rebuild leaves equal.
    written: Written,
    /// Where it sat when it was read.
    position: Position,
}

/// A constraint as written in a table node.
#[derive(PartialEq)]
enum Written {
    /// An entry of the constraint list.
    Table(Box<TableConstraint>),
    /// An option written on the column it names.
    Column(Ident, Box<ColumnOptionDef>),
}

impl Written {
    /// The constraint the slot holds.
    fn of(slot: Slot<'_>) -> Self {
        match slot {
            Slot::Column { def, held, .. } => {
                Self::Column(def.name.clone(), Box::new(held.clone()))
            }
            Slot::Table(_, constraint) => Self::Table(Box::new(constraint.clone())),
        }
    }

    /// Whether the slot holds this constraint.
    fn is_held_at(&self, slot: Slot<'_>) -> bool {
        match (self, slot) {
            (Self::Table(written), Slot::Table(_, held)) => **written == *held,
            (Self::Column(written_on, written), Slot::Column { def, held, .. }) => {
                def.name == *written_on && *held == **written
            }
            _ => false,
        }
    }
}

impl HeldConstraint {
    /// The constraint the slot holds.
    fn of(slot: Slot<'_>) -> Self {
        Self { written: Written::of(slot), position: slot.position() }
    }

    /// Where the constraint sits in `node`, if it still does.
    ///
    /// One still at the position it was read from is the one there, which
    /// tells it from an equal one written earlier. One that moved is the
    /// first equal one.
    fn position_in(&self, node: &CreateTable) -> Option<Position> {
        let holding: Vec<Position> = constraint_names::slots(node)
            .into_iter()
            .filter(|slot| self.written.is_held_at(*slot))
            .map(Slot::position)
            .collect();
        holding
            .iter()
            .copied()
            .find(|position| *position == self.position)
            .or(holding.first().copied())
    }
}

/// The foreign key the slot holds.
fn foreign_key_of(slot: Slot<'_>) -> Option<&ForeignKeyConstraint> {
    match slot {
        Slot::Column {
            held: ColumnOptionDef { option: ColumnOption::ForeignKey(foreign_key), .. },
            ..
        }
        | Slot::Table(_, TableConstraint::ForeignKey(foreign_key)) => Some(foreign_key),
        _ => None,
    }
}

/// The columns of the key the slot holds, and whether it is the primary key,
/// for a key over plain columns.
fn key_columns(slot: Slot<'_>) -> Option<(Vec<&Ident>, bool)> {
    match slot {
        Slot::Column { def, held, .. } => {
            match held.option {
                ColumnOption::PrimaryKey(_) => Some((alloc::vec![&def.name], true)),
                ColumnOption::Unique(_) => Some((alloc::vec![&def.name], false)),
                _ => None,
            }
        }
        Slot::Table(_, TableConstraint::PrimaryKey(primary_key)) => {
            plain_key_columns(&primary_key.columns).map(|columns| (columns, true))
        }
        Slot::Table(_, TableConstraint::Unique(unique)) => {
            plain_key_columns(&unique.columns).map(|columns| (columns, false))
        }
        Slot::Table(..) => None,
    }
}

/// Whether a parent passes `node` the foreign key at `slot`, which then
/// falls with the parent's.
fn received(builder: &ParserDBBuilder, node: &CreateTable, slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column { def, held, .. } => {
            inheritance::receives_column_constraint(builder, node, &def.name, held)
        }
        Slot::Table(_, constraint) => inheritance::receives_constraint(builder, node, constraint),
    }
}

/// Whether the foreign key at `child` is a copy of the one at `parent`, which
/// a parent linked by `kind` passed down.
fn copies(kind: ParentKind, child: Slot<'_>, parent: Slot<'_>, case: IdentifierCase) -> bool {
    match (child, parent) {
        (Slot::Table(_, held), Slot::Table(_, passed)) => {
            inheritance::passes_down(kind, passed) && inheritance::is_copy_of(held, passed)
        }
        (
            Slot::Column { def: child_def, held, .. },
            Slot::Column { def: parent_def, held: written, .. },
        ) => {
            inheritance::option_passes_down(kind, &written.option)
                && super::case_idents_match(&child_def.name, &parent_def.name, case)
                && inheritance::is_copy_of_option(held, written)
        }
        _ => false,
    }
}

/// The metadata of the stored table `node` is.
fn metadata_of<'builder>(
    builder: &'builder ParserDBBuilder,
    node: &CreateTable,
) -> Option<&'builder TableMetadata<CreateTable>> {
    builder
        .tables()
        .iter()
        .find(|(table, _, _)| core::ptr::eq(table.as_ref(), node))
        .map(|(_, metadata, _)| metadata)
}

/// The positions of the keys of `node`, a table the builder holds, whose
/// index is invalid.
pub(super) fn invalid_keys(builder: &ParserDBBuilder, node: &CreateTable) -> Vec<Position> {
    let Some(metadata) = metadata_of(builder, node) else {
        return Vec::new();
    };
    constraint_names::keyed_slots(node)
        .into_iter()
        .filter(|(_, key)| {
            metadata.key_creations().iter().any(|created| created.slot == *key && !created.valid)
        })
        .map(|(slot, _)| slot.position())
        .collect()
}

/// Whether the index of the primary key of `node`, a table the builder
/// holds, is valid, which it is when the table holds no primary key.
pub(super) fn primary_key_is_valid(builder: &ParserDBBuilder, node: &CreateTable) -> bool {
    let invalid = invalid_keys(builder, node);
    invalid.is_empty()
        || constraint_names::keyed_slots(node).into_iter().all(|(slot, _)| {
            !key_columns(slot).is_some_and(|(_, primary)| primary)
                || !invalid.contains(&slot.position())
        })
}

/// Marks the index of the recorded key of `table` valid or invalid.
pub(super) fn set_key_validity(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    key: &str,
    valid: bool,
) {
    let case = builder.identifier_case();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(node, _, _)| table.matches(node, case))
    {
        metadata.set_key_validity(key, valid);
    }
}

/// Whether the foreign key at `slot` of `node`, keyed `key`, depends on the
/// primary key of the table it references because it named no columns, or
/// is the copy of one that does.
fn names_primary_key(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    slot: Slot<'_>,
    key: &str,
) -> bool {
    if metadata_of(builder, node)
        .is_some_and(|metadata| metadata.primary_key_references().iter().any(|held| held == key))
    {
        return true;
    }
    let case = builder.identifier_case();
    inheritance::parents_with_kind(node).any(|(kind, name)| {
        builder.resolve_bound_table_name(name).ok().flatten().is_some_and(|parent| {
            constraint_names::keyed_slots(parent).into_iter().any(|(passed, passed_key)| {
                copies(kind, slot, passed, case)
                    && names_primary_key(builder, parent, passed, &passed_key)
            })
        })
    })
}

/// The key the foreign key depends on: the primary key of the table it
/// references when `names_primary_key`, else the oldest key over its columns.
fn bound_key(
    builder: &ParserDBBuilder,
    foreign_key: &ForeignKeyConstraint,
    names_primary_key: bool,
) -> Option<KeyIdentity> {
    let target = builder.resolve_bound_table_name(&foreign_key.foreign_table).ok().flatten()?;
    let metadata = metadata_of(builder, target)?;
    let case = builder.identifier_case();
    let referred: Vec<&Ident> = foreign_key.referred_columns.iter().collect();
    let mut keys: Vec<(KeyIdentity, bool)> = Vec::new();
    for (slot, key) in constraint_names::keyed_slots(target) {
        let Some((columns, primary)) = key_columns(slot) else {
            continue;
        };
        if key_columns_match(&referred, &columns, case)
            && let Some(created) =
                metadata.key_creations().iter().find(|held| held.slot == key && held.valid)
        {
            keys.push((created.identity, primary));
        }
    }
    for (index, index_metadata) in builder.indices() {
        let index = index.attribute();
        if index.unique
            && index_metadata.is_valid()
            && table_matches_object_name(builder.namespaces(), target, &index.table_name)
            && plain_key_columns(&index.columns)
                .is_some_and(|columns| key_columns_match(&referred, &columns, case))
            && let Some(identity) = index_metadata.key_identity()
        {
            keys.push((identity, false));
        }
    }
    if names_primary_key && let Some((identity, _)) = keys.iter().find(|(_, primary)| *primary) {
        return Some(*identity);
    }
    keys.into_iter().map(|(identity, _)| identity).min()
}

/// Every foreign key of the schema with the key it depends on. None under a
/// dialect binding keys on use, where a key depends on nothing.
fn dependences(builder: &ParserDBBuilder) -> Vec<Dependence> {
    if builder.dialect().binds_foreign_keys_on_use() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (node, _, _) in builder.tables() {
        for (slot, key) in constraint_names::keyed_slots(node) {
            let Some(foreign_key) = foreign_key_of(slot) else {
                continue;
            };
            let primary = names_primary_key(builder, node, slot, &key);
            found.extend(bound_key(builder, foreign_key, primary).map(|bound| {
                Dependence {
                    host: StoredTable::of(node),
                    held: HeldConstraint::of(slot),
                    key: bound,
                }
            }));
        }
    }
    found
}

/// The stored identity of the constraint key at `slot`.
pub(super) fn key_identity(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    slot: Slot<'_>,
) -> Option<KeyIdentity> {
    let key = constraint_names::keyed_slots(node)
        .into_iter()
        .find(|(held, _)| held.position() == slot.position())
        .map(|(_, key)| key);
    metadata_of(builder, node)?
        .key_creations()
        .iter()
        .find(|created| Some(&created.slot) == key.as_ref())
        .map(|created| created.identity)
}

/// The identity of the key at `slot` of `node`, the stored table `table`,
/// with those of the copies the partitions below hold of it unless `only`
/// keeps them.
///
/// # Errors
///
/// Returns the error reaching a table below `table` meets.
pub(super) fn removed_keys(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    slot: Slot<'_>,
    only: bool,
) -> Result<Vec<KeyIdentity>, Error> {
    let case = builder.identifier_case();
    let node = ParserDB::stored_node(builder, table)?;
    let mut removed: Vec<KeyIdentity> = key_identity(builder, node, slot).into_iter().collect();
    if only {
        return Ok(removed);
    }
    let mut frontier = alloc::vec![(table.clone(), slot)];
    while let Some((current, passed)) = frontier.pop() {
        for (kind, child) in inheritance::direct_children(builder, &current) {
            let child_node = ParserDB::stored_node(builder, &child)?;
            let Some(copy) = constraint_names::slots(child_node)
                .into_iter()
                .find(|held| held.is_unique_key() && copies(kind, *held, passed, case))
            else {
                continue;
            };
            removed.extend(key_identity(builder, child_node, copy));
            frontier.push((child, copy));
        }
    }
    Ok(removed)
}

/// Gives each primary key and unique constraint of `node` holding no identity
/// yet a fresh one, the primary key before the others, which is the order
/// PostgreSQL creates the indexes of one statement in.
pub(super) fn settle_key_creations(
    builder: &mut ParserDBBuilder,
    node: &CreateTable,
    metadata: &mut TableMetadata<CreateTable>,
) {
    let mut creations = metadata.key_creations().to_vec();
    let mut arriving: Vec<(bool, String)> = constraint_names::keyed_slots(node)
        .into_iter()
        .filter_map(|(slot, key)| {
            let primary = matches!(
                slot,
                Slot::Column {
                    held: ColumnOptionDef { option: ColumnOption::PrimaryKey(_), .. },
                    ..
                } | Slot::Table(_, TableConstraint::PrimaryKey(_))
            );
            (slot.is_unique_key() && !creations.iter().any(|held| held.slot == key))
                .then_some((primary, key))
        })
        .collect();
    arriving.sort_by_key(|(primary, _)| !primary);
    for (_, slot) in arriving {
        creations.push(KeyCreation { slot, identity: builder.draw_key_identity(), valid: true });
    }
    metadata.set_key_creations(creations);
}

/// Runs `bind`, which spells out the referenced columns of the foreign keys
/// `of` reads in `target`, and returns those that named none, which depend on
/// the primary key.
///
/// # Errors
///
/// Returns the error `bind` meets.
pub(super) fn bind_noting_primary_key<T, BindingError>(
    target: &mut T,
    of: for<'target> fn(&'target T) -> Vec<&'target ForeignKeyConstraint>,
    bind: impl FnOnce(&mut T) -> Result<(), BindingError>,
) -> Result<Vec<ForeignKeyConstraint>, BindingError> {
    let named_none: Vec<bool> =
        of(target).iter().map(|foreign_key| foreign_key.referred_columns.is_empty()).collect();
    bind(target)?;
    Ok(of(target)
        .into_iter()
        .zip(named_none)
        .filter(|(foreign_key, named_none)| *named_none && !foreign_key.referred_columns.is_empty())
        .map(|(foreign_key, _)| foreign_key.clone())
        .collect())
}

/// The foreign keys a table node declares.
pub(super) fn table_foreign_keys(node: &CreateTable) -> Vec<&ForeignKeyConstraint> {
    super::foreign_keys_of(node).collect()
}

/// The foreign key the constraint is.
pub(super) fn constraint_foreign_keys(constraint: &TableConstraint) -> Vec<&ForeignKeyConstraint> {
    match constraint {
        TableConstraint::ForeignKey(foreign_key) => alloc::vec![foreign_key],
        _ => Vec::new(),
    }
}

/// The foreign key written in a column option.
pub(super) fn option_foreign_keys(option: &super::ColumnOptionDef) -> Vec<&ForeignKeyConstraint> {
    match &option.option {
        ColumnOption::ForeignKey(foreign_key) => alloc::vec![foreign_key],
        _ => Vec::new(),
    }
}

/// The foreign keys written on the column.
pub(super) fn column_foreign_keys(column: &ColumnDef) -> Vec<&ForeignKeyConstraint> {
    column
        .options
        .iter()
        .filter_map(|option| {
            match &option.option {
                ColumnOption::ForeignKey(foreign_key) => Some(foreign_key),
                _ => None,
            }
        })
        .collect()
}

/// Records that the foreign keys of `table` equal to one of `bound` named no
/// referenced columns, and so depend on the primary key.
pub(super) fn record_primary_key_references(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    bound: &[ForeignKeyConstraint],
) {
    add_primary_key_references(builder, table, |_, _, slot, _| {
        foreign_key_of(slot).is_some_and(|foreign_key| bound.contains(foreign_key))
    });
}

/// Records on `table` each foreign key it receives that depends on the
/// primary key, so the record outlives the link to the parent passing it.
pub(super) fn keep_received_primary_key_references(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
) {
    add_primary_key_references(builder, table, |builder, node, slot, key| {
        foreign_key_of(slot).is_some() && names_primary_key(builder, node, slot, key)
    });
}

/// Records on `table` that each foreign key `names` picks depends on the
/// primary key.
fn add_primary_key_references(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    names: impl Fn(&ParserDBBuilder, &CreateTable, Slot<'_>, &str) -> bool,
) {
    let case = builder.identifier_case();
    let reader: &ParserDBBuilder = builder;
    let picked: Vec<String> = reader
        .tables()
        .iter()
        .filter(|(stored, _, _)| table.matches(stored, case))
        .flat_map(|(node, _, _)| {
            constraint_names::keyed_slots(node)
                .into_iter()
                .filter(|(slot, key)| names(reader, node, *slot, key))
                .map(|(_, key)| key)
        })
        .collect();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(stored, _, _)| table.matches(stored, case))
    {
        let mut references = metadata.primary_key_references().to_vec();
        for key in &picked {
            if !references.contains(key) {
                references.push(key.clone());
            }
        }
        metadata.set_primary_key_references(references);
    }
}

/// Forgets that the foreign keys reading `column` of `table`, on either side,
/// named no referenced columns, since a type change has PostgreSQL add each
/// again with its columns spelled out, bound to the oldest key over them.
pub(super) fn forget_primary_key_references(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    column: &Ident,
) {
    let case = builder.identifier_case();
    let reads_column =
        |columns: &[Ident]| columns.iter().any(|held| super::case_idents_match(held, column, case));
    let reader: &ParserDBBuilder = builder;
    let forgotten: Vec<(StoredTable, String)> = reader
        .tables()
        .iter()
        .flat_map(|(node, _, _)| {
            let on_table = table.matches(node, case);
            constraint_names::keyed_slots(node).into_iter().filter_map(move |(slot, key)| {
                let foreign_key = foreign_key_of(slot)?;
                let host_reads = on_table
                    && match slot {
                        Slot::Column { def, .. } => {
                            super::case_idents_match(&def.name, column, case)
                        }
                        Slot::Table(..) => reads_column(&foreign_key.columns),
                    };
                let referenced_reads = reader
                    .resolve_bound_table_name(&foreign_key.foreign_table)
                    .ok()
                    .flatten()
                    .is_some_and(|target| table.matches(target, case))
                    && reads_column(&foreign_key.referred_columns);
                (host_reads || referenced_reads).then(|| (StoredTable::of(node), key))
            })
        })
        .collect();
    for (host, key) in forgotten {
        for (_, metadata, _) in
            builder.tables_mut().iter_mut().filter(|(stored, _, _)| host.matches(stored, case))
        {
            let references = metadata
                .primary_key_references()
                .iter()
                .filter(|held| **held != key)
                .cloned()
                .collect();
            metadata.set_primary_key_references(references);
        }
    }
}

impl ParserDB {
    /// Settles the foreign keys depending on a key in `removed` as
    /// `on_dependents` says, then runs `drop`, which removes those keys.
    ///
    /// The foreign keys go first, so a table rebuilt without its key never
    /// holds one left without a target.
    ///
    /// # Errors
    ///
    /// Returns [`Error::KeyReferenced`] when a foreign key depends on a
    /// removed key and the statement does not say `CASCADE`, the errors
    /// taking a foreign key along meets, and the error `drop` meets.
    pub(super) fn drop_keys(
        mut builder: ParserDBBuilder,
        removed: &[KeyIdentity],
        on_dependents: OnDependents<'_>,
        drop: impl FnOnce(ParserDBBuilder) -> Result<ParserDBBuilder, Error>,
    ) -> Result<ParserDBBuilder, Error> {
        let dependents: Vec<Dependence> = dependences(&builder)
            .into_iter()
            .filter(|dependence| removed.contains(&dependence.key))
            .collect();
        if let OnDependents::Refuse { key_name, table_name } = on_dependents {
            if let Some(dependence) = dependents.first() {
                return Err(Error::KeyReferenced {
                    key_name: key_name.into(),
                    table_name: table_name.into(),
                    referencing_table: dependence.host.name.clone(),
                });
            }
            return drop(builder);
        }
        // A copy falls with the foreign key passing it down, so only keys
        // nothing passes down are dropped, until none is left.
        while let Some((host, position, name)) = dependents.iter().find_map(|dependence| {
            let node = Self::stored_node(&builder, &dependence.host).ok()?;
            let position = dependence.held.position_in(node)?;
            let slot = constraint_names::slots(node)
                .into_iter()
                .find(|slot| slot.position() == position)?;
            (!received(&builder, node, slot)).then(|| {
                let name = slot.declared_name().map(|name| name.value.clone()).unwrap_or_default();
                (dependence.host.clone(), position, name)
            })
        }) {
            builder = Self::drop_cascaded_constraint(builder, &host, position, &name)?;
        }
        drop(builder)
    }
}
