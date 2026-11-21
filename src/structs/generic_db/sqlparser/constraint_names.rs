//! PostgreSQL constraint names and name reservations in statement execution
//! order.

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CheckConstraint, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, ExcludeConstraint,
    Expr, Ident, ObjectName, TableConstraint,
};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, SqlparserDialect, StoredTable, generated_names,
    inheritance, schema_qualifiers_match, table_schema_qualifier,
};
use crate::{
    errors::{Error, InheritedChange, ObjectKind},
    structs::{
        IdentifierCase, TableMetadata,
        metadata::{KeyCreation, RecordedConstraintName},
    },
    traits::TableLike,
    utils::{identifier_resolution::stored_ident_name, walk::walk_expressions},
};

/// One constraint a table node holds: an option written on a column, or an
/// entry of its constraint list.
#[derive(Clone, Copy)]
pub(super) enum Slot<'node> {
    /// The option at `option` of the column at `column`.
    Column { column: usize, option: usize, def: &'node ColumnDef, held: &'node ColumnOptionDef },
    /// The entry at the position of the constraint list.
    Table(usize, &'node TableConstraint),
}

/// Where a constraint sits in its table node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Position {
    /// The option at `.1` of the column at `.0`.
    Column(usize, usize),
    /// The entry at the position of the constraint list.
    Table(usize),
}

/// The PostgreSQL statement whose constraint installation passes apply.
#[derive(Clone, Copy)]
pub(super) enum CreationOrder {
    /// Checks, not-null constraints, primary keys, other indexes, then foreign
    /// keys.
    CreateTable,
    /// Not-null constraints, indexes, then checks and foreign keys.
    AlterTable,
}

/// How PostgreSQL builds a constraint's name: the column part, the label, and
/// whether the name is its index's.
struct Naming {
    columns: Option<String>,
    label: &'static str,
    names_index: bool,
}

impl<'node> Slot<'node> {
    /// Where the constraint sits.
    pub(super) fn position(self) -> Position {
        match self {
            Self::Column { column, option, .. } => Position::Column(column, option),
            Self::Table(position, _) => Position::Table(position),
        }
    }

    /// The name the statement wrote for it.
    pub(super) fn declared_name(self) -> Option<&'node Ident> {
        match self {
            Self::Column { held, .. } => {
                held.name.as_ref().or(match &held.option {
                    ColumnOption::Check(check) => check.name.as_ref(),
                    ColumnOption::ForeignKey(foreign_key) => foreign_key.name.as_ref(),
                    ColumnOption::PrimaryKey(primary_key) => primary_key.name.as_ref(),
                    ColumnOption::Unique(unique) => unique.name.as_ref(),
                    _ => None,
                })
            }
            Self::Table(_, constraint) => super::table_constraint_name(constraint),
        }
    }

    /// The slot as its key starts, before it is numbered among equal ones.
    fn rendering(self) -> String {
        match self {
            Self::Column { def, held, .. } => format!("{} {held}", def.name),
            Self::Table(_, constraint) => constraint.to_string(),
        }
    }

    /// Whether the slot holds a check.
    pub(super) fn is_check(self) -> bool {
        matches!(
            self,
            Self::Column { held: ColumnOptionDef { option: ColumnOption::Check(_), .. }, .. }
        ) || matches!(self, Self::Table(_, TableConstraint::Check(_)))
    }

    /// Whether the slot holds a primary key or a unique constraint.
    pub(super) fn is_unique_key(self) -> bool {
        matches!(
            self,
            Self::Column {
                held: ColumnOptionDef {
                    option: ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_),
                    ..
                },
                ..
            }
        ) || matches!(
            self,
            Self::Table(_, TableConstraint::PrimaryKey(_) | TableConstraint::Unique(_))
        )
    }

    /// Whether the slot holds a foreign key.
    pub(super) fn is_foreign_key(self) -> bool {
        matches!(
            self,
            Self::Column { held: ColumnOptionDef { option: ColumnOption::ForeignKey(_), .. }, .. }
        ) || matches!(self, Self::Table(_, TableConstraint::ForeignKey(_)))
    }

    /// The column whose `NOT NULL` the slot holds.
    pub(super) fn not_null_of(self) -> Option<&'node Ident> {
        match self {
            Self::Column {
                def,
                held: ColumnOptionDef { option: ColumnOption::NotNull, .. },
                ..
            } => Some(&def.name),
            Self::Table(_, TableConstraint::NotNull(not_null)) => Some(&not_null.column),
            _ => None,
        }
    }

    /// The order PostgreSQL installs newly declared constraint kinds.
    fn creation_order(self, order: CreationOrder) -> u8 {
        if matches!(order, CreationOrder::AlterTable) {
            return if self.not_null_of().is_some() {
                0
            } else if backed_by_index(self) {
                1
            } else {
                2
            };
        }
        if self.is_check() {
            0
        } else if self.not_null_of().is_some() {
            1
        } else if matches!(
            self,
            Self::Column { held: ColumnOptionDef { option: ColumnOption::PrimaryKey(_), .. }, .. }
                | Self::Table(_, TableConstraint::PrimaryKey(_))
        ) {
            2
        } else if self.is_foreign_key() {
            4
        } else {
            3
        }
    }

    /// How PostgreSQL names the constraint, or [`None`] for a kind it does not
    /// name or an index expression whose name is not modeled.
    fn naming(self) -> Option<Naming> {
        let constraint = |columns, label| Some(Naming { columns, label, names_index: false });
        let index = |columns, label| Some(Naming { columns, label, names_index: true });
        match self {
            Self::Column { def, held, .. } => {
                let column = || Some(generated_names::column_part(&def.name));
                match &held.option {
                    ColumnOption::Check(check) => constraint(only_column(&check.expr), "check"),
                    ColumnOption::ForeignKey(_) => constraint(column(), "fkey"),
                    ColumnOption::PrimaryKey(_) => index(None, "pkey"),
                    ColumnOption::Unique(_) => index(column(), "key"),
                    // A slot on a column holds a constraint, and `NOT NULL` is
                    // the one left.
                    _ => constraint(column(), "not_null"),
                }
            }
            Self::Table(_, table_constraint) => {
                match table_constraint {
                    TableConstraint::Check(check) => constraint(only_column(&check.expr), "check"),
                    TableConstraint::ForeignKey(foreign_key) => {
                        let columns: Vec<String> =
                            foreign_key.columns.iter().map(generated_names::column_part).collect();
                        constraint(Some(columns.join("_")), "fkey")
                    }
                    TableConstraint::PrimaryKey(_) => index(None, "pkey"),
                    TableConstraint::Unique(unique) => {
                        index(
                            Some(generated_names::index_columns_part(
                                &unique.columns,
                                &unique.include,
                            )?),
                            "key",
                        )
                    }
                    TableConstraint::Exclude(exclude) => {
                        let columns: Vec<_> =
                            exclude.elements.iter().map(|element| element.column.clone()).collect();
                        index(
                            Some(generated_names::index_columns_part(&columns, &exclude.include)?),
                            "excl",
                        )
                    }
                    TableConstraint::NotNull(not_null) => {
                        constraint(Some(generated_names::column_part(&not_null.column)), "not_null")
                    }
                    _ => None,
                }
            }
        }
    }
}

/// The column a check reads, when it reads exactly one, which PostgreSQL then
/// puts in the check's name.
fn only_column(expr: &Expr) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    let _: ControlFlow<()> = walk_expressions(expr, |expr| {
        let column = match expr {
            Expr::Identifier(ident) => Some(ident),
            Expr::CompoundIdentifier(parts) => parts.last(),
            _ => None,
        };
        if let Some(column) = column.map(generated_names::column_part)
            && !found.contains(&column)
        {
            found.push(column);
        }
        ControlFlow::Continue(())
    });
    match found.as_slice() {
        [column] => Some(column.clone()),
        _ => None,
    }
}

/// Every constraint the node holds, those written on its columns first in
/// column order and then its constraint list, the order PostgreSQL names them
/// in and the order the model builds their objects in.
pub(super) fn slots(node: &CreateTable) -> Vec<Slot<'_>> {
    let written_on_columns = node.columns.iter().enumerate().flat_map(|(column, def)| {
        def.options
            .iter()
            .enumerate()
            .filter(|(_, held)| super::column_option_is_constraint(&held.option))
            .map(move |(option, held)| Slot::Column { column, option, def, held })
    });
    let written_alone = node
        .constraints
        .iter()
        .enumerate()
        .map(|(position, constraint)| Slot::Table(position, constraint));
    written_on_columns.chain(written_alone).collect()
}

/// The key of each slot: its rendering, numbered among the equal ones before
/// it so that two constraints written alike keep apart.
fn keys(slots: &[Slot<'_>]) -> Vec<String> {
    let renderings: Vec<String> = slots.iter().map(|slot| slot.rendering()).collect();
    renderings
        .iter()
        .enumerate()
        .map(|(index, rendering)| {
            let earlier = renderings[..index].iter().filter(|held| *held == rendering).count();
            format!("{earlier} {rendering}")
        })
        .collect()
}

/// The name each slot of the node answers to: the one written, else the one
/// PostgreSQL gave it, as PostgreSQL stores either.
///
/// A `NOT NULL` written on a column and on the table is one constraint, so the
/// slots of the pair, naming the column under `case`, answer alike to the
/// pair's name.
pub(super) fn names<'a>(
    node: &'a CreateTable,
    recorded: &'a [RecordedConstraintName],
    case: IdentifierCase,
) -> Vec<(Slot<'a>, Option<Cow<'a, str>>)> {
    let slots = slots(node);
    let mut named = slot_names(&slots, recorded);
    for (index, partner) in slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot, &Slot::Table(_, TableConstraint::NotNull(_))))
        .filter_map(|(index, _)| {
            not_null_partner_index(&slots, index, case).map(|partner| (index, partner))
        })
    {
        // The table's spelling holds the pair's name, the option's spelling
        // only where the table's carries none.
        let pair = named[index].1.clone().or_else(|| named[partner].1.clone());
        named[index].1.clone_from(&pair);
        named[partner].1 = pair;
    }
    named
}

/// The name each slot answers to on its own, before the two spellings of a
/// `NOT NULL` share one, which is all a check or an exclusion constraint
/// ever answers to.
fn slot_names<'a>(
    slots: &[Slot<'a>],
    recorded: &'a [RecordedConstraintName],
) -> Vec<(Slot<'a>, Option<Cow<'a, str>>)> {
    slots
        .iter()
        .zip(&keys(slots))
        .map(|(slot, key)| {
            let name = match slot.declared_name() {
                Some(declared) => Some(stored_ident_name(declared)),
                None => {
                    recorded
                        .iter()
                        .find(|generated| generated.slot == *key)
                        .map(|generated| Cow::Borrowed(generated.name.as_str()))
                }
            };
            (*slot, name)
        })
        .collect()
}

/// The slot that holds the name of the requirement the slot at `index` is a
/// spelling of: the column's `NOT NULL` option, an earlier entry of the same
/// requirement, or a table entry the column spells, the column named under
/// `case`.
fn not_null_partner_index(slots: &[Slot<'_>], index: usize, case: IdentifierCase) -> Option<usize> {
    let (column, is_table) = match &slots[index] {
        Slot::Table(_, TableConstraint::NotNull(not_null)) => (&not_null.column, true),
        Slot::Column {
            def, held: ColumnOptionDef { option: ColumnOption::NotNull, .. }, ..
        } => (&def.name, false),
        _ => return None,
    };
    // A requirement spelled twice in the list is one constraint, and the
    // first entry holds its name.
    if is_table
        && let Some(earlier) = slots.iter().take(index).enumerate().find(|(_, other)| {
            match other {
                Slot::Table(_, TableConstraint::NotNull(held)) => {
                    super::case_idents_match(&held.column, column, case)
                }
                _ => false,
            }
        })
    {
        return Some(earlier.0);
    }
    slots
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index)
        .find(|(_, other)| {
            match other {
                Slot::Table(_, TableConstraint::NotNull(not_null)) => {
                    !is_table && super::case_idents_match(&not_null.column, column, case)
                }
                Slot::Column {
                    def,
                    held: ColumnOptionDef { option: ColumnOption::NotNull, .. },
                    ..
                } => is_table && super::case_idents_match(&def.name, column, case),
                _ => false,
            }
        })
        .map(|(other, _)| other)
}

/// The constraint of the node answering to `name`, written or generated, a
/// `NOT NULL` naming its column under `case`.
pub(super) fn locate(
    node: &CreateTable,
    recorded: &[RecordedConstraintName],
    name: &Ident,
    case: IdentifierCase,
) -> Option<Position> {
    let wanted = stored_ident_name(name);
    // The two spellings of one `NOT NULL` are a single constraint, and the
    // table's slot answers for the pair.
    let answering: Vec<Slot<'_>> = names(node, recorded, case)
        .into_iter()
        .filter(|(_, held)| held.as_deref() == Some(wanted.as_ref()))
        .map(|(slot, _)| slot)
        .collect();
    answering
        .iter()
        .find(|slot| matches!(slot, Slot::Table(_, TableConstraint::NotNull(_))))
        .or(answering.first())
        .copied()
        .map(Slot::position)
}

/// A record kept per constraint of a table node, keyed by the slot the
/// constraint fills.
pub(super) trait SlotRecord: Clone {
    /// The key of the slot the record belongs to.
    fn slot(&self) -> &str;

    /// The record moved to `slot`, retaining its current declared name.
    fn moved_to(&self, slot: String, declared_name: Option<&Ident>) -> Self;
}

impl SlotRecord for RecordedConstraintName {
    fn slot(&self) -> &str {
        &self.slot
    }

    fn moved_to(&self, slot: String, declared_name: Option<&Ident>) -> Self {
        Self {
            slot,
            name: declared_name.map_or_else(
                || self.name.clone(),
                |declared| stored_ident_name(declared).into_owned(),
            ),
            names_index: self.names_index,
        }
    }
}

impl SlotRecord for KeyCreation {
    fn slot(&self) -> &str {
        &self.slot
    }

    fn moved_to(&self, slot: String, _declared_name: Option<&Ident>) -> Self {
        Self { slot, identity: self.identity }
    }
}

impl SlotRecord for String {
    fn slot(&self) -> &str {
        self
    }

    fn moved_to(&self, slot: String, _declared_name: Option<&Ident>) -> Self {
        slot
    }
}

/// Carries the records kept per constraint across a rebuild of the node.
///
/// An edit that rewrites constraints in place, which is what a column rename
/// does, keeps the number of slots, so the records follow position, and so do
/// the survivors of an edit removing the slot at `removed`, which leaves them
/// in order. An edit that otherwise adds or removes one leaves the
/// survivors' renderings alone, so those are recognised by key.
pub(super) fn follow<R: SlotRecord>(
    recorded: &[R],
    previous: &CreateTable,
    replacement: &CreateTable,
    removed: Option<Position>,
) -> Vec<R> {
    let previous_slots = slots(previous);
    let gone = removed
        .and_then(|removed| previous_slots.iter().position(|slot| slot.position() == removed));
    let before: Vec<String> = keys(&previous_slots)
        .into_iter()
        .enumerate()
        .filter(|(position, _)| Some(*position) != gone)
        .map(|(_, key)| key)
        .collect();
    let replacement_slots = slots(replacement);
    let after = keys(&replacement_slots);
    if before.len() == after.len() {
        before
            .iter()
            .zip(after.iter().zip(&replacement_slots))
            .filter_map(|(before, (after, slot))| {
                recorded
                    .iter()
                    .find(|record| record.slot() == before)
                    .map(|record| record.moved_to(after.clone(), slot.declared_name()))
            })
            .collect()
    } else {
        recorded
            .iter()
            .filter(|record| after.iter().any(|key| key == record.slot()))
            .cloned()
            .collect()
    }
}

/// Every constraint the node holds, in the order of [`slots`], with the key
/// the records kept per constraint name it by.
pub(super) fn keyed_slots(node: &CreateTable) -> Vec<(Slot<'_>, String)> {
    let slots = slots(node);
    let keys = keys(&slots);
    slots.into_iter().zip(keys).collect()
}

/// Whether an installed constraint in the table's schema holds the name.
pub(super) fn constraint_name_held(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    name: &str,
) -> bool {
    let schema = table_schema_qualifier(node);
    let case = builder.identifier_case();
    builder.tables().iter().any(|(table, metadata, _)| {
        schema_qualifiers_match(builder.namespaces(), table_schema_qualifier(table), schema, case)
            && metadata.recorded_constraint_names().iter().any(|held| held.name == name)
    })
}

/// Whether a constraint of the schema has `name` as the name of its index.
pub(super) fn index_name_held(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: super::SchemaQualifier<'_>,
) -> bool {
    let wanted = stored_ident_name(name);
    let case = builder.identifier_case();
    builder.tables().iter().any(|(table, metadata, _)| {
        schema_qualifiers_match(builder.namespaces(), table_schema_qualifier(table), schema, case)
            && metadata
                .recorded_constraint_names()
                .iter()
                .any(|generated| generated.names_index && generated.name == wanted)
    })
}

/// The name a parent gave the constraint the table received as `slot`.
fn received_name(builder: &ParserDBBuilder, node: &CreateTable, slot: Slot<'_>) -> Option<String> {
    let case = builder.identifier_case();
    inheritance::parent_names(node)
        .filter_map(|name| builder.resolve_bound_table_name(name).ok().flatten())
        .find_map(|parent| {
            let recorded = builder
                .tables()
                .iter()
                .find(|(table, _, _)| core::ptr::eq(table.as_ref(), parent))
                .map_or(&[][..], |(_, metadata, _)| metadata.recorded_constraint_names());
            names(parent, recorded, case).into_iter().find_map(|(held, name)| {
                let same = match (slot, held) {
                    (
                        Slot::Column { def, held: option, .. },
                        Slot::Column { def: parent_def, held: parent_option, .. },
                    ) => {
                        super::case_idents_match(&def.name, &parent_def.name, case)
                            && inheritance::is_copy_of_option(option, parent_option)
                    }
                    (Slot::Table(_, constraint), Slot::Table(_, passed)) => {
                        inheritance::is_copy_of(constraint, passed)
                    }
                    // A `NOT NULL` and its copy are one constraint whether the
                    // parent writes it on a column or on the table.
                    (
                        Slot::Table(_, TableConstraint::NotNull(not_null)),
                        Slot::Column {
                            def,
                            held: ColumnOptionDef { option: ColumnOption::NotNull, .. },
                            ..
                        },
                    ) => super::case_idents_match(&not_null.column, &def.name, case),
                    (
                        Slot::Column {
                            def,
                            held: ColumnOptionDef { option: ColumnOption::NotNull, .. },
                            ..
                        },
                        Slot::Table(_, TableConstraint::NotNull(not_null)),
                    ) => super::case_idents_match(&def.name, &not_null.column, case),
                    _ => false,
                };
                same.then_some(name).flatten().map(Cow::into_owned)
            })
        })
}

/// Whether the table received the slot from a parent rather than declaring it,
/// as the records of what it received say.
fn is_received(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    metadata: &TableMetadata<CreateTable>,
    slot: Slot<'_>,
) -> bool {
    match slot {
        Slot::Column {
            def, held: ColumnOptionDef { option: ColumnOption::NotNull, .. }, ..
        } => metadata.inherited_not_null().contains(&def.name.value),
        // A copy can reach a column the table declares itself too.
        Slot::Column { def, held, .. } => {
            metadata.inherited_column_names().contains(&def.name.value)
                || inheritance::receives_column_constraint(builder, node, &def.name, held)
        }
        Slot::Table(_, constraint) => {
            metadata.inherited_constraints().contains(&constraint.to_string())
        }
    }
}

/// Installs pending constraint names in PostgreSQL creation order.
///
/// # Errors
///
/// Returns a name-collision error when a declared name is already installed.
pub(super) fn name_pending(
    builder: &mut ParserDBBuilder,
    order: CreationOrder,
) -> Result<(), Error> {
    if !matches!(*builder.dialect(), SqlparserDialect::PostgreSql) {
        return Ok(());
    }
    let case = builder.identifier_case();
    loop {
        let pending: Vec<StoredTable> = builder
            .tables()
            .iter()
            .filter(|(_, metadata, _)| metadata.constraint_names_pending())
            .map(|(table, _, _)| StoredTable::of(table))
            .collect();
        let Some(next) = pending
            .iter()
            .find(|table| {
                builder.tables().iter().any(|(node, _, _)| {
                    table.matches(node, case)
                        && !inheritance::parent_names(node)
                            .filter_map(|name| {
                                builder.resolve_bound_table_name(name).ok().flatten()
                            })
                            .any(|parent| pending.iter().any(|held| held.matches(parent, case)))
                })
            })
            .or(pending.first())
        else {
            return Ok(());
        };
        let names: Vec<Vec<RecordedConstraintName>> = builder
            .tables()
            .iter()
            .filter(|(table, _, _)| next.matches(table, case))
            .map(|(table, metadata, _)| new_names(builder, table, metadata, order))
            .collect::<Result<_, _>>()?;
        for ((_, metadata, _), names) in builder
            .tables_mut()
            .iter_mut()
            .filter(|(table, _, _)| next.matches(table, case))
            .zip(names)
        {
            metadata.set_recorded_constraint_names(names);
            metadata.set_constraint_names_pending(false);
        }
    }
}

/// The installed names after assigning every pending constraint.
fn new_names(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    metadata: &TableMetadata<CreateTable>,
    order: CreationOrder,
) -> Result<Vec<RecordedConstraintName>, Error> {
    let mut recorded = metadata.recorded_constraint_names().to_vec();
    let mut slots = slots(node);
    slots.sort_by_key(|slot| slot.creation_order(order));
    let keys = keys(&slots);
    for ((index, slot), key) in slots.iter().copied().enumerate().zip(keys) {
        if let Some(partner) = not_null_partner_index(&slots, index, builder.identifier_case()) {
            let partner_is_table =
                matches!(slots[partner], Slot::Table(_, TableConstraint::NotNull(_)));
            if matches!(slot, Slot::Column { .. })
                || slots[partner].declared_name().is_some()
                || partner_is_table
            {
                continue;
            }
        }
        if recorded.iter().any(|generated| generated.slot == key) {
            continue;
        }
        let Some(naming) = slot.naming() else {
            continue;
        };
        let received = is_received(builder, node, metadata, slot)
            .then(|| received_name(builder, node, slot))
            .flatten();
        let own = |candidate: &str| recorded.iter().find(|held| held.name == candidate);
        let name = if let Some(declared) = slot.declared_name() {
            let name = stored_ident_name(declared).into_owned();
            if let Some(held) = own(&name) {
                return Err(if naming.names_index && held.names_index {
                    Error::RelationNameAlreadyTaken {
                        object_kind: ObjectKind::UniqueIndex,
                        conflicting_kind: ObjectKind::UniqueIndex,
                        object_name: name,
                    }
                } else {
                    Error::ConstraintAlreadyExists {
                        table_name: node.table_name().to_string(),
                        constraint_name: name,
                    }
                });
            }
            name
        } else {
            received.unwrap_or_else(|| {
                generated_names::constraint_name_on(
                    node,
                    naming.columns.as_deref(),
                    naming.label,
                    |candidate| {
                        own(&candidate.value).is_some()
                            || constraint_name_held(builder, node, &candidate.value)
                            || naming.names_index
                                && super::non_constraint_relation_name_holder(
                                    builder,
                                    candidate,
                                    table_schema_qualifier(node),
                                )
                                .is_some()
                    },
                )
                .value
            })
        };
        recorded.push(RecordedConstraintName { slot: key, name, names_index: naming.names_index });
    }
    Ok(recorded)
}

/// The names the slots of a node the builder holds answer to.
pub(super) fn answers<'a>(
    builder: &'a ParserDBBuilder,
    node: &'a CreateTable,
) -> Vec<(Slot<'a>, Option<Cow<'a, str>>)> {
    let recorded = builder
        .tables()
        .iter()
        .find(|(table, _, _)| core::ptr::eq(table.as_ref(), node))
        .map_or(&[][..], |(_, metadata, _)| metadata.recorded_constraint_names());
    names(node, recorded, builder.identifier_case())
}

/// The name the check `check` of `node`, a table the builder holds, answers
/// to, the one written or else the one PostgreSQL gave it.
pub(super) fn check_name(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    check: &CheckConstraint,
) -> Option<String> {
    answers(builder, node)
        .into_iter()
        .find(|(slot, _)| check_of(*slot) == Some(check))
        .and_then(|(_, name)| name.map(Cow::into_owned))
}

/// The name the exclusion constraint `exclusion`, borrowed from `node`, a
/// table the builder holds, answers to, the one written or else the one
/// PostgreSQL gave it.
pub(super) fn exclusion_name(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    exclusion: &ExcludeConstraint,
) -> Option<String> {
    answers(builder, node)
        .into_iter()
        .find(|(slot, _)| {
            matches!(
                slot,
                Slot::Table(_, TableConstraint::Exclude(held)) if core::ptr::eq(held, exclusion)
            )
        })
        .and_then(|(_, name)| name.map(Cow::into_owned))
}

/// The names the checks of `node` answer to, in the order of its slots,
/// `recorded` holding the names PostgreSQL gave those written without one.
pub(super) fn check_names(
    node: &CreateTable,
    recorded: &[RecordedConstraintName],
) -> Vec<Option<String>> {
    slot_names(&slots(node), recorded)
        .into_iter()
        .filter(|(slot, _)| check_of(*slot).is_some())
        .map(|(_, name)| name.map(Cow::into_owned))
        .collect()
}

/// The names the exclusion constraints of `node` answer to, in the order it
/// lists them, `recorded` holding the names PostgreSQL gave those written
/// without one.
pub(super) fn exclusion_names(
    node: &CreateTable,
    recorded: &[RecordedConstraintName],
) -> Vec<Option<String>> {
    slot_names(&slots(node), recorded)
        .into_iter()
        .filter(|(slot, _)| matches!(slot, Slot::Table(_, TableConstraint::Exclude(_))))
        .map(|(_, name)| name.map(Cow::into_owned))
        .collect()
}

/// The exclusion constraints of `node`, in the order it lists them.
pub(super) fn exclusions(node: &CreateTable) -> impl Iterator<Item = &ExcludeConstraint> {
    node.constraints.iter().filter_map(|constraint| {
        match constraint {
            TableConstraint::Exclude(exclusion) => Some(exclusion),
            _ => None,
        }
    })
}

/// The slot of `node` holding the check `check`, written on a column or on
/// its own.
pub(super) fn check_slot<'node>(
    node: &'node CreateTable,
    check: &CheckConstraint,
) -> Option<Slot<'node>> {
    slots(node).into_iter().find(|slot| check_of(*slot) == Some(check))
}

/// The check a slot holds, with the name written on its column option folded
/// in.
fn check_of(slot: Slot<'_>) -> Option<&CheckConstraint> {
    match slot {
        Slot::Column {
            held: ColumnOptionDef { option: ColumnOption::Check(check), .. }, ..
        }
        | Slot::Table(_, TableConstraint::Check(check)) => Some(check),
        _ => None,
    }
}

/// Whether a parent passes `node` the check in `slot`, matched the way
/// PostgreSQL matches an inherited check: by the name both answer to and
/// their expression, whether either is written on a column or on its own.
pub(super) fn receives_check(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    slot: Slot<'_>,
) -> bool {
    let Some(check) = check_of(slot) else {
        return false;
    };
    let Some(name) = answers(builder, node)
        .into_iter()
        .find(|(held, _)| held.position() == slot.position())
        .and_then(|(_, name)| name)
    else {
        return false;
    };
    inheritance::parent_names(node)
        .filter_map(|parent| builder.resolve_bound_table_name(parent).ok().flatten())
        .any(|parent| {
            answers(builder, parent).into_iter().any(|(passed, passed_name)| {
                check_of(passed).is_some_and(|passed| {
                    !passed.no_inherit
                        && passed_name.as_deref() == Some(name.as_ref())
                        && inheritance::stored_expr(&passed.expr)
                            == inheritance::stored_expr(&check.expr)
                })
            })
        })
}

/// Removes from a table being created the unnamed copies of parent checks it
/// declares itself under the name the parent's check answers to, since the
/// two are one constraint, both its own and received. Answers the renderings
/// of the copies removed from its constraint list.
pub(super) fn merge_named_copies(builder: &ParserDBBuilder, node: &mut CreateTable) -> Vec<String> {
    let declared: Vec<(String, Expr)> = slots(node)
        .into_iter()
        .filter_map(|slot| {
            let name = slot.declared_name()?;
            Some((
                stored_ident_name(name).into_owned(),
                inheritance::stored_expr(&check_of(slot)?.expr),
            ))
        })
        .collect();
    let passed: Vec<Expr> = inheritance::parent_names(node)
        .filter_map(|parent| builder.resolve_bound_table_name(parent).ok().flatten())
        .flat_map(|parent| {
            answers(builder, parent).into_iter().filter_map(|(slot, name)| {
                let check = check_of(slot)?;
                let expr = inheritance::stored_expr(&check.expr);
                (slot.declared_name().is_none()
                    && !check.no_inherit
                    && declared.iter().any(|(held, held_expr)| {
                        Some(held.as_str()) == name.as_deref() && *held_expr == expr
                    }))
                .then_some(expr)
            })
        })
        .collect();
    if passed.is_empty() {
        return Vec::new();
    }
    let is_merged_copy = |check: &CheckConstraint| {
        check.name.is_none() && passed.contains(&inheritance::stored_expr(&check.expr))
    };
    for column in &mut node.columns {
        column.options.retain(|held| {
            !(held.name.is_none()
                && matches!(&held.option, ColumnOption::Check(check) if is_merged_copy(check)))
        });
    }
    let mut removed = Vec::new();
    node.constraints.retain(|constraint| {
        let copy = matches!(constraint, TableConstraint::Check(check) if is_merged_copy(check));
        if copy {
            removed.push(constraint.to_string());
        }
        !copy
    });
    removed
}

/// The position of the other spelling of a paired `NOT NULL`.
fn not_null_partner_position(
    node: &CreateTable,
    position: Position,
    case: IdentifierCase,
) -> Option<Position> {
    let slots = slots(node);
    slots.iter().enumerate().filter(|(_, slot)| slot.position() == position).find_map(
        |(index, _)| {
            not_null_partner_index(&slots, index, case).map(|partner| slots[partner].position())
        },
    )
}

/// The slot at `position` of the node.
fn slot_at(node: &CreateTable, position: Position) -> Option<(Slot<'_>, String)> {
    let slots = slots(node);
    let keys = keys(&slots);
    slots.into_iter().zip(keys).find(|(slot, _)| slot.position() == position)
}

/// Whether the table receives the constraint in `slot` from a parent.
fn received_from_parent(builder: &ParserDBBuilder, node: &CreateTable, slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column {
            def, held: ColumnOptionDef { option: ColumnOption::NotNull, .. }, ..
        } => inheritance::requires_a_value(builder, node, &def.name),
        Slot::Column { def, held, .. } => {
            inheritance::receives_column_constraint(builder, node, &def.name, held)
        }
        Slot::Table(_, constraint) => inheritance::receives_constraint(builder, node, constraint),
    }
}

/// Whether the tables below hold copies of the constraint in `slot`.
fn passes_down(slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column { held, .. } => {
            matches!(&held.option, ColumnOption::NotNull)
                || matches!(&held.option, ColumnOption::Check(check) if !check.no_inherit)
        }
        Slot::Table(_, constraint) => {
            matches!(constraint, TableConstraint::Check(check) if !check.no_inherit)
                || matches!(constraint, TableConstraint::NotNull(not_null) if !not_null.no_inherit)
        }
    }
}

/// Whether the constraint in `slot` is backed by an index, whose name it
/// shares.
pub(super) fn backed_by_index(slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column { held, .. } => {
            matches!(held.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_))
        }
        Slot::Table(_, constraint) => {
            matches!(
                constraint,
                TableConstraint::PrimaryKey(_)
                    | TableConstraint::Unique(_)
                    | TableConstraint::Exclude(_)
                    | TableConstraint::PrimaryKeyUsingIndex(_)
                    | TableConstraint::UniqueUsingIndex(_)
            )
        }
    }
}

/// Applies `ALTER TABLE ... RENAME CONSTRAINT old TO new`.
///
/// The constraint is found by the name it answers to, written or generated,
/// and renamed on the table and on every copy below it, which carries the
/// same name. A written name is rewritten where it is written, and a
/// generated one in the record holding it.
///
/// # Errors
///
/// Returns [`Error::RenameConstraintNotFound`] for a name the table does not
/// hold, [`Error::InheritedConstraintNotRenamable`] for a check or `NOT NULL`
/// received from a parent, [`Error::OnlyRefusedWithChildren`] when `ONLY`
/// would leave copies below under the old name,
/// [`Error::ConstraintAlreadyExists`] for a name another constraint of the
/// table holds, and
/// [`Error::RelationNameAlreadyTaken`] when a key's new name, which its index
/// takes too, belongs to another relation of the schema.
pub(super) fn rename_constraint(
    builder: ParserDBBuilder,
    table_name: &ObjectName,
    scope: AlterTableScope,
    old: &Ident,
    new: &Ident,
) -> Result<ParserDBBuilder, Error> {
    let Some(stored) = ParserDB::alter_table_target(&builder, table_name, scope)? else {
        return Ok(builder);
    };
    rename_constraint_of(builder, stored, scope, old, new)
}

/// The table holding the primary key, unique or exclusion constraint whose
/// index `name` reaches, or [`None`] when `name` reaches an index of its own
/// or nothing at all.
///
/// PostgreSQL renames such an index together with its constraint, and checks
/// the schema's relation names before the table's constraint names, so a
/// clash with a relation is reported here.
///
/// # Errors
///
/// Returns [`Error::RelationNameAlreadyTaken`] when another relation of the
/// schema holds `new`.
pub(super) fn key_index_table(
    builder: &ParserDBBuilder,
    name: &ObjectName,
    old: &Ident,
    new: &Ident,
) -> Result<Option<StoredTable>, Error> {
    let Some((stored, schema)) = key_index_owner(builder, name, old) else {
        return Ok(None);
    };
    match super::relation_name_holder(builder, new, Some(schema)) {
        Some(conflicting_kind) => {
            Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::UniqueIndex,
                conflicting_kind,
                object_name: new.value.clone(),
            })
        }
        None => Ok(Some(stored)),
    }
}

/// The table and reached schema whose constraint owns the index named by
/// `name`.
pub(super) fn key_index_owner<'name>(
    builder: &'name ParserDBBuilder,
    name: &'name ObjectName,
    index_name: &Ident,
) -> Option<(StoredTable, (&'name str, bool))> {
    let case = builder.identifier_case();
    builder
        .relation_reached(name)
        .filter(|(kind, _)| matches!(kind, ObjectKind::UniqueIndex))
        .and_then(|(_, schema)| {
            builder
                .tables()
                .iter()
                .find(|(table, metadata, _)| {
                    schema_qualifiers_match(
                        builder.namespaces(),
                        table_schema_qualifier(table),
                        Some(schema),
                        case,
                    ) && locate(table, metadata.recorded_constraint_names(), index_name, case)
                        .and_then(|position| slot_at(table, position))
                        .is_some_and(|(slot, _)| backed_by_index(slot))
                })
                .map(|(table, _, _)| (StoredTable::of(table), schema))
        })
}

/// Applies `ALTER INDEX ... RENAME TO new` to the key of `stored` answering to
/// `old`, which [`key_index_table`] found. The rename reaches that one
/// relation, never the partitions or children below it.
///
/// # Errors
///
/// Returns the errors of [`rename_constraint`].
pub(super) fn rename_key_index(
    builder: ParserDBBuilder,
    stored: StoredTable,
    old: &Ident,
    new: &Ident,
) -> Result<ParserDBBuilder, Error> {
    rename_constraint_of(
        builder,
        stored,
        AlterTableScope { if_exists: false, only: true },
        old,
        new,
    )
}

/// Renames the constraint of `stored` answering to `old`, as
/// [`rename_constraint`] describes.
fn rename_constraint_of(
    builder: ParserDBBuilder,
    stored: StoredTable,
    scope: AlterTableScope,
    old: &Ident,
    new: &Ident,
) -> Result<ParserDBBuilder, Error> {
    let node = ParserDB::stored_node(&builder, &stored)?;
    let recorded = super::recorded_constraint_names(&builder, &stored);
    let case = builder.identifier_case();
    let Some((slot, _)) =
        locate(node, recorded, old, case).and_then(|position| slot_at(node, position))
    else {
        return Err(Error::RenameConstraintNotFound {
            table_name: stored.name.clone(),
            constraint_name: old.value.clone(),
        });
    };
    // PostgreSQL leaves a received check or `NOT NULL` to the parent to
    // rename, and lets a table rename a key or foreign key it received.
    if passes_down(slot) && received_from_parent(&builder, node, slot) {
        return Err(Error::InheritedConstraintNotRenamable {
            table_name: stored.name.clone(),
            constraint_name: old.value.clone(),
        });
    }
    if passes_down(slot) {
        ParserDB::refuse_only_with_children(
            &builder,
            &stored,
            scope,
            InheritedChange::RenameConstraint,
        )?;
    }
    if locate(node, recorded, new, case).is_some() {
        return Err(Error::ConstraintAlreadyExists {
            table_name: stored.name.clone(),
            constraint_name: new.value.clone(),
        });
    }
    let index_backed = backed_by_index(slot);
    if index_backed
        && let Some(conflicting_kind) =
            super::relation_name_holder(&builder, new, table_schema_qualifier(node))
    {
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::UniqueIndex,
            conflicting_kind,
            object_name: new.value.clone(),
        });
    }

    let below = if scope.only { Vec::new() } else { inheritance::descendants(&builder, &stored) };
    let mut builder = builder;
    for table in core::iter::once(stored).chain(below) {
        if index_backed {
            super::partition_indexes::rename(&mut builder, &table, old, new);
        }
        builder = rename_on(builder, &table, old, new)?;
    }
    Ok(builder)
}

/// Renames the constraint of `table` answering to `old`, if it holds one.
fn rename_on(
    builder: ParserDBBuilder,
    table: &StoredTable,
    old: &Ident,
    new: &Ident,
) -> Result<ParserDBBuilder, Error> {
    let node = ParserDB::stored_node(&builder, table)?;
    let recorded = super::recorded_constraint_names(&builder, table);
    let case = builder.identifier_case();
    let Some((slot, key)) =
        locate(node, recorded, old, case).and_then(|position| slot_at(node, position))
    else {
        return Ok(builder);
    };
    // A paired `NOT NULL` writes its name on the spelling that carries it,
    // and the table's slot answers for the pair.
    let slot = if slot.declared_name().is_some()
        || recorded.iter().any(|generated| generated.slot == key)
    {
        slot
    } else {
        not_null_partner_position(node, slot.position(), case)
            .and_then(|position| slot_at(node, position))
            .map_or(slot, |(partner, _)| partner)
    };
    let declared = slot.declared_name().is_some();
    let position = slot.position();
    let index_renamed = backed_by_index(slot);
    let mut builder = builder;
    let case = builder.identifier_case();
    if index_renamed {
        let (old, new) = (stored_ident_name(old), stored_ident_name(new));
        for (_, metadata, _) in
            builder.tables_mut().iter_mut().filter(|(stored, _, _)| table.matches(stored, case))
        {
            super::replica_identity::follow_rename(metadata, &old, &new);
        }
    }
    if declared {
        return ParserDB::replace_table_node(builder, table, |_, node| {
            rename_declared(node, position, new);
            Ok(())
        });
    }
    let name = stored_ident_name(new).into_owned();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(stored, _, _)| table.matches(stored, case))
    {
        let names = metadata
            .recorded_constraint_names()
            .iter()
            .map(|generated| {
                if generated.slot == key {
                    RecordedConstraintName { name: name.clone(), ..generated.clone() }
                } else {
                    generated.clone()
                }
            })
            .collect();
        metadata.set_recorded_constraint_names(names);
    }
    Ok(builder)
}

/// Rewrites the written name of the constraint at `position`, in the field
/// its written name is read from first.
fn rename_declared(node: &mut CreateTable, position: Position, new: &Ident) {
    let new = Some(new.clone());
    match position {
        Position::Column(column, option) => node.columns[column].options[option].name = new,
        // An index entry is no constraint, and ingestion keeps none in the
        // list.
        Position::Table(index) => {
            let constraint = &mut node.constraints[index];
            if let TableConstraint::Check(check) = constraint {
                check.name = new;
            } else if let TableConstraint::Unique(unique) = constraint {
                unique.name = new;
            } else if let TableConstraint::PrimaryKey(key) = constraint {
                key.name = new;
            } else if let TableConstraint::ForeignKey(foreign_key) = constraint {
                foreign_key.name = new;
            } else if let TableConstraint::Exclude(exclude) = constraint {
                exclude.name = new;
            } else if let TableConstraint::NotNull(not_null) = constraint {
                not_null.name = new;
            } else if let TableConstraint::PrimaryKeyUsingIndex(using_index)
            | TableConstraint::UniqueUsingIndex(using_index) = constraint
            {
                using_index.name = new;
            }
        }
    }
}
