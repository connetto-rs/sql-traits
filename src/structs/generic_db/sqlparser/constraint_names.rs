//! The names PostgreSQL gives the constraints of a table it is not told a name
//! for.
//!
//! A name is chosen when the constraint arrives and never changes after, not
//! when the table or a column is renamed. The model keeps the statement as
//! written, so the names live in the table's metadata, keyed by the slot each
//! constraint fills in the node.

use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CheckConstraint, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, Expr, Ident,
    ObjectName, TableConstraint, visit_expressions,
};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, SqlparserDialect, StoredTable, generated_names,
    inheritance, schema_qualifiers_match, table_schema_qualifier,
};
use crate::{
    errors::{Error, InheritedChange, ObjectKind},
    structs::{TableMetadata, metadata::GeneratedConstraintName},
    utils::identifier_resolution::stored_ident_name,
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
    let _: ControlFlow<()> = visit_expressions(expr, |expr| {
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
/// slots of the pair answer alike to the pair's name.
pub(super) fn names<'a>(
    node: &'a CreateTable,
    recorded: &'a [GeneratedConstraintName],
) -> Vec<(Slot<'a>, Option<Cow<'a, str>>)> {
    let slots = slots(node);
    let keys = keys(&slots);
    let mut named: Vec<(Slot<'a>, Option<Cow<'a, str>>)> = slots
        .iter()
        .zip(&keys)
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
        .collect();
    for (index, partner) in slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| matches!(slot, &Slot::Table(_, TableConstraint::NotNull(_))))
        .filter_map(|(index, _)| {
            not_null_partner_index(&slots, index).map(|partner| (index, partner))
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

/// The slot that holds the name of the requirement the slot at `index` is a
/// spelling of: the column's `NOT NULL` option, an earlier entry of the same
/// requirement, or a table entry the column spells.
fn not_null_partner_index(slots: &[Slot<'_>], index: usize) -> Option<usize> {
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
                    inheritance::same_column(&held.column, column)
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
                    !is_table && inheritance::same_column(&not_null.column, column)
                }
                Slot::Column {
                    def,
                    held: ColumnOptionDef { option: ColumnOption::NotNull, .. },
                    ..
                } => is_table && inheritance::same_column(&def.name, column),
                _ => false,
            }
        })
        .map(|(other, _)| other)
}

/// The constraint of the node answering to `name`, written or generated.
pub(super) fn locate(
    node: &CreateTable,
    recorded: &[GeneratedConstraintName],
    name: &Ident,
) -> Option<Position> {
    let wanted = stored_ident_name(name);
    // The two spellings of one `NOT NULL` are a single constraint, and the
    // table's slot answers for the pair.
    let answering: Vec<Slot<'_>> = names(node, recorded)
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

/// Carries the generated names across a rebuild of the node.
///
/// An edit that rewrites constraints in place, which is what a column rename
/// does, keeps the number of slots, so the names follow position. An edit that
/// adds or removes one leaves the survivors' renderings alone, so those are
/// recognised by key.
pub(super) fn follow(
    recorded: &[GeneratedConstraintName],
    previous: &CreateTable,
    replacement: &CreateTable,
) -> Vec<GeneratedConstraintName> {
    let before = keys(&slots(previous));
    let after = keys(&slots(replacement));
    if before.len() == after.len() {
        before
            .iter()
            .zip(&after)
            .filter_map(|(before, after)| {
                recorded.iter().find(|generated| generated.slot == *before).map(|generated| {
                    GeneratedConstraintName { slot: after.clone(), ..generated.clone() }
                })
            })
            .collect()
    } else {
        recorded.iter().filter(|generated| after.contains(&generated.slot)).cloned().collect()
    }
}

/// Whether a constraint of the schema `node` sits in, other than those still
/// being named, holds `name`.
fn constraint_name_held(builder: &ParserDBBuilder, node: &CreateTable, name: &str) -> bool {
    let schema = table_schema_qualifier(node);
    let case = builder.identifier_case();
    builder.tables().iter().any(|(table, metadata, _)| {
        schema_qualifiers_match(table_schema_qualifier(table), schema, case)
            && names(table, metadata.generated_constraint_names())
                .iter()
                .any(|(_, held)| held.as_deref() == Some(name))
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
        schema_qualifiers_match(table_schema_qualifier(table), schema, case)
            && metadata
                .generated_constraint_names()
                .iter()
                .any(|generated| generated.names_index && generated.name == wanted)
    })
}

/// The name a parent gave the constraint the table received as `slot`.
fn received_name(builder: &ParserDBBuilder, node: &CreateTable, slot: Slot<'_>) -> Option<String> {
    inheritance::parent_names(node)
        .filter_map(|name| builder.resolve_bound_table_name(name).ok().flatten())
        .find_map(|parent| {
            let recorded = builder
                .tables()
                .iter()
                .find(|(table, _, _)| core::ptr::eq(table.as_ref(), parent))
                .map_or(&[][..], |(_, metadata, _)| metadata.generated_constraint_names());
            names(parent, recorded).into_iter().find_map(|(held, name)| {
                let same = match (slot, held) {
                    (
                        Slot::Column { def, held: option, .. },
                        Slot::Column { def: parent_def, held: parent_option, .. },
                    ) => {
                        inheritance::same_column(&def.name, &parent_def.name)
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
                    ) => inheritance::same_column(&not_null.column, &def.name),
                    (
                        Slot::Column {
                            def,
                            held: ColumnOptionDef { option: ColumnOption::NotNull, .. },
                            ..
                        },
                        Slot::Table(_, TableConstraint::NotNull(not_null)),
                    ) => inheritance::same_column(&def.name, &not_null.column),
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

/// Gives every constraint that arrived since the last statement the name
/// PostgreSQL gives it, parents before the tables receiving from them so a
/// copy can take the name of its original.
pub(super) fn name_pending(builder: &mut ParserDBBuilder) {
    if !matches!(*builder.dialect(), SqlparserDialect::PostgreSql) {
        return;
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
            return;
        };
        let names: Vec<Vec<GeneratedConstraintName>> = builder
            .tables()
            .iter()
            .filter(|(table, _, _)| next.matches(table, case))
            .map(|(table, metadata, _)| new_names(builder, table, metadata))
            .collect();
        for ((_, metadata, _), names) in builder
            .tables_mut()
            .iter_mut()
            .filter(|(table, _, _)| next.matches(table, case))
            .zip(names)
        {
            metadata.set_generated_constraint_names(names);
            metadata.set_constraint_names_pending(false);
        }
    }
}

/// The table's generated names once every constraint without one has one.
fn new_names(
    builder: &ParserDBBuilder,
    node: &CreateTable,
    metadata: &TableMetadata<CreateTable>,
) -> Vec<GeneratedConstraintName> {
    let mut recorded = metadata.generated_constraint_names().to_vec();
    let slots = slots(node);
    let keys = keys(&slots);
    for ((index, slot), key) in slots.iter().enumerate().zip(&keys) {
        let slot = *slot;
        // The requirement's name is one: the column's option answers the
        // table's slot, a table slot a named option spells answers the
        // option's, and a repeated entry answers the first one's.
        if let Some(partner) = not_null_partner_index(&slots, index) {
            let partner_is_table =
                matches!(slots[partner], Slot::Table(_, TableConstraint::NotNull(_)));
            if matches!(slot, Slot::Column { .. })
                || slots[partner].declared_name().is_some()
                || partner_is_table
            {
                continue;
            }
        }
        let key = key.clone();
        let unnamed = slot.declared_name().is_none()
            && !recorded.iter().any(|generated| generated.slot == key);
        let Some(naming) = unnamed.then(|| slot.naming()).flatten() else {
            continue;
        };
        let received = is_received(builder, node, metadata, slot)
            .then(|| received_name(builder, node, slot))
            .flatten();
        let name = received.unwrap_or_else(|| {
            let own =
                |candidate: &str| recorded.iter().any(|generated| generated.name == candidate);
            if naming.names_index {
                generated_names::index_name_on(
                    builder,
                    node,
                    naming.columns.as_deref(),
                    naming.label,
                    |candidate| own(&candidate.value),
                )
                .value
            } else {
                generated_names::constraint_name_on(
                    node,
                    naming.columns.as_deref(),
                    naming.label,
                    |candidate| own(candidate) || constraint_name_held(builder, node, candidate),
                )
            }
        });
        recorded.push(GeneratedConstraintName { slot: key, name, names_index: naming.names_index });
    }
    recorded
}

/// The names the slots of a node the builder holds answer to.
fn answers<'a>(
    builder: &'a ParserDBBuilder,
    node: &'a CreateTable,
) -> Vec<(Slot<'a>, Option<Cow<'a, str>>)> {
    let recorded = builder
        .tables()
        .iter()
        .find(|(table, _, _)| core::ptr::eq(table.as_ref(), node))
        .map_or(&[][..], |(_, metadata, _)| metadata.generated_constraint_names());
    names(node, recorded)
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
fn not_null_partner_position(node: &CreateTable, position: Position) -> Option<Position> {
    let slots = slots(node);
    slots.iter().enumerate().filter(|(_, slot)| slot.position() == position).find_map(
        |(index, _)| not_null_partner_index(&slots, index).map(|partner| slots[partner].position()),
    )
}

/// The slot at `position` of the node.
fn slot_at(node: &CreateTable, position: Position) -> Option<(Slot<'_>, String)> {
    let slots = slots(node);
    let keys = keys(&slots);
    slots.into_iter().zip(keys).find(|(slot, _)| slot.position() == position)
}

/// Whether the table receives the constraint in `slot` from a parent, which
/// then alone may rename it.
fn received_from_parent(builder: &ParserDBBuilder, node: &CreateTable, slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column {
            def, held: ColumnOptionDef { option: ColumnOption::NotNull, .. }, ..
        } => inheritance::requires_a_value(builder, node, &def.name.value),
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
fn backed_by_index(slot: Slot<'_>) -> bool {
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
/// hold, [`Error::InheritedConstraintNotRenamable`] for a constraint received
/// from a parent, [`Error::OnlyRefusedWithChildren`] when `ONLY` would leave
/// copies below under the old name, [`Error::ConstraintAlreadyExists`] for a
/// name another constraint of the table holds, and
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
    let node = ParserDB::stored_node(&builder, &stored)?;
    let recorded = super::generated_constraint_names(&builder, &stored);
    let Some((slot, _)) = locate(node, recorded, old).and_then(|position| slot_at(node, position))
    else {
        return Err(Error::RenameConstraintNotFound {
            table_name: stored.name.clone(),
            constraint_name: old.value.clone(),
        });
    };
    if received_from_parent(&builder, node, slot) {
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
    if locate(node, recorded, new).is_some() {
        return Err(Error::ConstraintAlreadyExists {
            table_name: stored.name.clone(),
            constraint_name: new.value.clone(),
        });
    }
    if backed_by_index(slot)
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
    let recorded = super::generated_constraint_names(&builder, table);
    let Some((slot, key)) =
        locate(node, recorded, old).and_then(|position| slot_at(node, position))
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
        not_null_partner_position(node, slot.position())
            .and_then(|position| slot_at(node, position))
            .map_or(slot, |(partner, _)| partner)
    };
    if slot.declared_name().is_some() {
        let position = slot.position();
        return ParserDB::replace_table_node(builder, table, |_, node| {
            rename_declared(node, position, new);
            Ok(())
        });
    }
    let mut builder = builder;
    let case = builder.identifier_case();
    let name = stored_ident_name(new).into_owned();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(stored, _, _)| table.matches(stored, case))
    {
        let names = metadata
            .generated_constraint_names()
            .iter()
            .map(|generated| {
                if generated.slot == key {
                    GeneratedConstraintName { name: name.clone(), ..generated.clone() }
                } else {
                    generated.clone()
                }
            })
            .collect();
        metadata.set_generated_constraint_names(names);
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
