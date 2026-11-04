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
    ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, Expr, Ident, TableConstraint,
    visit_expressions,
};

use super::{
    ParserDBBuilder, SqlparserDialect, StoredTable, generated_names, inheritance,
    schema_qualifiers_match, table_schema_qualifier,
};
use crate::{
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
pub(super) fn names<'a>(
    node: &'a CreateTable,
    recorded: &'a [GeneratedConstraintName],
) -> Vec<(Slot<'a>, Option<Cow<'a, str>>)> {
    let slots = slots(node);
    let keys = keys(&slots);
    slots
        .into_iter()
        .zip(keys)
        .map(|(slot, key)| {
            let name = match slot.declared_name() {
                Some(declared) => Some(stored_ident_name(declared)),
                None => {
                    recorded
                        .iter()
                        .find(|generated| generated.slot == key)
                        .map(|generated| Cow::Borrowed(generated.name.as_str()))
                }
            };
            (slot, name)
        })
        .collect()
}

/// The constraint of the node answering to `name`, written or generated.
pub(super) fn locate(
    node: &CreateTable,
    recorded: &[GeneratedConstraintName],
    name: &Ident,
) -> Option<Position> {
    let wanted = stored_ident_name(name);
    names(node, recorded)
        .into_iter()
        .find(|(_, held)| held.as_deref() == Some(wanted.as_ref()))
        .map(|(slot, _)| slot.position())
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
                    _ => false,
                };
                same.then_some(name).flatten().map(Cow::into_owned)
            })
        })
}

/// Whether the table received the slot from a parent rather than declaring it,
/// as the records of what it received say.
fn is_received(metadata: &TableMetadata<CreateTable>, slot: Slot<'_>) -> bool {
    match slot {
        Slot::Column {
            def, held: ColumnOptionDef { option: ColumnOption::NotNull, .. }, ..
        } => metadata.inherited_not_null().contains(&def.name.value),
        Slot::Column { def, .. } => metadata.inherited_column_names().contains(&def.name.value),
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
    for (slot, key) in slots.into_iter().zip(keys) {
        let unnamed = slot.declared_name().is_none()
            && !recorded.iter().any(|generated| generated.slot == key);
        let Some(naming) = unnamed.then(|| slot.naming()).flatten() else {
            continue;
        };
        let received =
            is_received(metadata, slot).then(|| received_name(builder, node, slot)).flatten();
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
