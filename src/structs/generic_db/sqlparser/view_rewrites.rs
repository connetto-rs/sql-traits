//! Keeping stored view definitions reading what they read, and spelling out
//! the wildcards and `NATURAL` joins a view's creation fixes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, Ident, JoinConstraint, JoinOperator, ObjectName, ObjectNamePart, Query, SelectItem,
    SetExpr, TableAlias, TableAliasColumnDef, TableFactor, TableWithJoins, Visit, VisitMut,
    Visitor, VisitorMut,
};

use super::{
    ParserDBBuilder,
    views::{WithScopes, relations_read_in},
};
use crate::{
    impls::dql::{
        definition_graph::reference_sites::{QueryReferences, SiteOutcome, query_references},
        join_constraint,
    },
    structs::{IdentifierCase, ParserDB, TableAttribute},
    traits::ViewLike,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{RelationKey, target_key, target_name_from_object_name, view_matches_key},
    },
};

/// Stored definitions are bound when recorded, so every name in one is read
/// the way PostgreSQL reads it.
const CASE: IdentifierCase = IdentifierCase::AsWritten;

/// What a statement renamed, as stored definitions name it.
pub(super) enum Rename {
    /// The relation stored under `key` is now called `to`.
    Relation {
        /// The relation's key under its old name.
        key: RelationKey,
        /// The relation's new name.
        to: Ident,
    },
    /// Output column `ordinal` of the relation stored under `key` is now
    /// called `to`.
    Column {
        /// The relation's key.
        key: RelationKey,
        /// The position of the renamed column among the relation's columns.
        ordinal: usize,
        /// The column's new name.
        to: Ident,
        /// Every column name the relation exposed before the rename, in
        /// order, which an alias column list brings back.
        columns: Vec<Ident>,
    },
}

impl Rename {
    pub(super) fn key(&self) -> &RelationKey {
        match self {
            Self::Relation { key, .. } | Self::Column { key, .. } => key,
        }
    }
}

/// Where a stored view sits in the builder.
#[derive(Clone, Copy)]
enum Slot {
    Plain(usize),
    Materialized(usize),
}

fn definition_at(builder: &ParserDBBuilder, slot: Slot) -> &Query {
    match slot {
        Slot::Plain(position) => builder.views()[position].0.definition(),
        Slot::Materialized(position) => builder.materialized_views()[position].0.definition(),
    }
}

fn store(builder: &mut ParserDBBuilder, slot: Slot, query: Query) {
    let declaration = match slot {
        Slot::Plain(position) => {
            alloc::sync::Arc::make_mut(&mut builder.views_mut()[position].0).declaration_mut()
        }
        Slot::Materialized(position) => {
            alloc::sync::Arc::make_mut(&mut builder.materialized_views_mut()[position].0)
                .declaration_mut()
        }
    };
    *declaration.query_mut() = query;
}

/// Every stored view whose definition names one of `keys`.
fn views_reading(builder: &ParserDBBuilder, keys: &[&RelationKey]) -> Vec<Slot> {
    let reads =
        |definition: &Query| relations_read_in(definition).iter().any(|key| keys.contains(&key));
    let plain = builder
        .views()
        .iter()
        .enumerate()
        .filter(|(_, (view, _, _))| reads(view.definition()))
        .map(|(position, _)| Slot::Plain(position));
    let materialized = builder
        .materialized_views()
        .iter()
        .enumerate()
        .filter(|(_, (view, _, _))| reads(view.definition()))
        .map(|(position, _)| Slot::Materialized(position));
    plain.chain(materialized).collect()
}

/// The part of the catalog `definitions` read, which is every relation they
/// name and, transitively, every relation a view among those names.
pub(super) fn catalog<'q>(
    builder: &ParserDBBuilder,
    definitions: impl IntoIterator<Item = &'q Query>,
) -> ParserDB {
    let mut pending: Vec<RelationKey> =
        definitions.into_iter().flat_map(relations_read_in).collect();
    let mut keys: Vec<RelationKey> = Vec::new();
    while let Some(key) = pending.pop() {
        if keys.contains(&key) {
            continue;
        }
        let plain = builder.views().iter().map(|(view, _, _)| view.as_ref());
        for view in plain.filter(|view| view_matches_key(*view, &key, CASE)) {
            pending.extend(relations_read_in(view.definition()));
        }
        let materialized = builder.materialized_views().iter().map(|(view, _, _)| view.as_ref());
        for view in materialized.filter(|view| view_matches_key(*view, &key, CASE)) {
            pending.extend(relations_read_in(view.definition()));
        }
        keys.push(key);
    }
    builder.relations_database(&keys, TableAttribute::table)
}

/// The stored views reading what a statement is about to rename, with the
/// part of the catalog they read as it stands before the statement.
pub(super) struct Readers {
    slots: Vec<Slot>,
    before: ParserDB,
}

/// The stored views reading a relation one of `keys` names, or `None` when
/// none does and a rename of them has nothing to follow.
pub(super) fn readers(builder: &ParserDBBuilder, keys: &[&RelationKey]) -> Option<Readers> {
    let slots = views_reading(builder, keys);
    if slots.is_empty() {
        return None;
    }
    let before = catalog(builder, slots.iter().map(|slot| definition_at(builder, *slot)));
    Some(Readers { slots, before })
}

/// A definition spelled the way PostgreSQL spells it after the renames,
/// waiting to be checked against the renamed catalog.
struct Respelled {
    slot: Slot,
    original: Query,
    /// What every reference of the original reached before the renames.
    expected: Vec<SiteOutcome>,
    /// How many `FROM` items the original holds.
    factors: usize,
}

/// Rewrites every stored view `readers` found so it keeps reading what
/// `renames` renamed, once the builder holds the renamed catalog.
///
/// Views are only rewritten in place, never added or removed, so the
/// positions `readers` recorded before the renames still hold.
pub(super) fn follow_renames(builder: &mut ParserDBBuilder, readers: Readers, renames: &[Rename]) {
    let Readers { slots, before } = readers;
    let mut respelled = Vec::new();
    for slot in slots {
        let original = definition_at(builder, slot).clone();
        let mut query = original.clone();
        match respell(&mut query, &before, renames) {
            Some(references) => {
                respelled.push(Respelled {
                    slot,
                    original,
                    expected: references.sites.iter().map(|site| site.outcome).collect(),
                    factors: references.factors.len(),
                });
                store(builder, slot, query);
            }
            None => store(builder, slot, shielded(&original, renames)),
        }
    }
    if respelled.is_empty() {
        return;
    }

    // Every respelled definition is in place before any is checked.
    let after = catalog(builder, respelled.iter().map(|view| definition_at(builder, view.slot)));
    for Respelled { slot, original, expected, factors } in respelled {
        let current = definition_at(builder, slot);
        if query_references(current, &after, CASE)
            .is_some_and(|references| reaches(&references, &expected, factors))
        {
            continue;
        }
        // Qualifications are keyed by address, so resolve the copy edited.
        let mut qualified = current.clone();
        let settled = query_references(&qualified, &after, CASE)
            .is_some_and(|references| qualify_captured(&mut qualified, &references, &expected))
            && query_references(&qualified, &after, CASE)
                .is_some_and(|references| reaches(&references, &expected, factors));
        store(builder, slot, if settled { qualified } else { shielded(&original, renames) });
    }
}

/// Whether every reference reached what `expected` says, item for item.
fn reaches(references: &QueryReferences, expected: &[SiteOutcome], factors: usize) -> bool {
    references.factors.len() == factors
        && references.sites.len() == expected.len()
        && references.sites.iter().zip(expected).all(|(site, expected)| site.outcome == *expected)
}

/// Spells out the wildcards and `NATURAL` joins of a definition being
/// recorded, which PostgreSQL resolves into the columns they reach when the
/// view is created, so a column added to a relation later never joins the
/// view and a rename has written names to follow. A wildcard or join over
/// columns that cannot all be named stays as written.
pub(super) fn spell_out(builder: &ParserDBBuilder, query: &mut Query) {
    struct Spell {
        wildcards: BTreeMap<usize, Vec<SelectItem>>,
        naturals: BTreeMap<usize, Vec<Ident>>,
    }

    impl Spell {
        fn joins(&mut self, table: &mut TableWithJoins) {
            for join in &mut table.joins {
                if let Some(names) = self.naturals.remove(&address(&*join))
                    && let Some(constraint) = constraint_mut(&mut join.join_operator)
                {
                    *constraint = JoinConstraint::Using(
                        names
                            .into_iter()
                            .map(|name| ObjectName(alloc::vec![ObjectNamePart::Identifier(name)]))
                            .collect(),
                    );
                }
            }
        }
    }

    impl VisitorMut for Spell {
        type Break = ();

        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
            for_each_select_mut(&mut query.body, &mut |select| {
                let spelled: Vec<Option<Vec<SelectItem>>> = select
                    .projection
                    .iter()
                    .map(|item| self.wildcards.remove(&address(item)))
                    .collect();
                if spelled.iter().any(Option::is_some) {
                    let written = core::mem::take(&mut select.projection);
                    select.projection = written
                        .into_iter()
                        .zip(spelled)
                        .flat_map(|(item, spelled)| spelled.unwrap_or_else(|| alloc::vec![item]))
                        .collect();
                }
                for table in &mut select.from {
                    self.joins(table);
                }
            });
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
            if let TableFactor::NestedJoin { table_with_joins, .. } = factor {
                self.joins(table_with_joins);
            }
            ControlFlow::Continue(())
        }
    }

    if !spells_anything(query) {
        return;
    }
    let database = catalog(builder, core::iter::once(&*query));
    let Some(references) = query_references(query, &database, CASE) else {
        return;
    };
    let mut spell = Spell {
        wildcards: references.wildcards.into_iter().collect(),
        naturals: references.natural_joins.into_iter().collect(),
    };
    let walked = VisitMut::visit(query, &mut spell);
    debug_assert!(walked.is_continue(), "the visitor never breaks");
}

/// Whether a definition holds a wildcard or a `NATURAL` join, which is when
/// [`spell_out`] has to read the catalog.
fn spells_anything(query: &Query) -> bool {
    struct Spellable;

    impl Spellable {
        fn natural(table: &TableWithJoins) -> bool {
            table.joins.iter().any(|join| {
                matches!(join_constraint(&join.join_operator), Some(JoinConstraint::Natural))
            })
        }
    }

    impl Visitor for Spellable {
        type Break = ();

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            let mut found = false;
            for_each_select(&query.body, &mut |select| {
                found |= select.projection.iter().any(|item| {
                    matches!(item, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..))
                }) || select.from.iter().any(Self::natural);
            });
            if found { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            match factor {
                TableFactor::NestedJoin { table_with_joins, .. }
                    if Self::natural(table_with_joins) =>
                {
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    }

    query.visit(&mut Spellable).is_break()
}

/// The constraint a join operator carries, for a rewrite that respells it.
fn constraint_mut(operator: &mut JoinOperator) -> Option<&mut JoinConstraint> {
    match operator {
        JoinOperator::Join(constraint)
        | JoinOperator::Inner(constraint)
        | JoinOperator::Left(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::Right(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::CrossJoin(constraint)
        | JoinOperator::Semi(constraint)
        | JoinOperator::LeftSemi(constraint)
        | JoinOperator::RightSemi(constraint)
        | JoinOperator::Anti(constraint)
        | JoinOperator::LeftAnti(constraint)
        | JoinOperator::RightAnti(constraint) => Some(constraint),
        _ => None,
    }
}

/// Finds every `FROM` item of `query` reading a relation `renames` renamed,
/// by address. A name a visible `WITH` item answers reads that item instead.
fn renamed_factors(query: &Query, renames: &[Rename]) -> BTreeMap<usize, usize> {
    struct Finder<'r> {
        renames: &'r [Rename],
        scopes: WithScopes,
        found: BTreeMap<usize, usize>,
    }

    impl Visitor for Finder<'_> {
        type Break = ();

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            self.scopes.enter(query);
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            self.scopes.leave(query);
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            if let TableFactor::Table { name, args: None, .. } = factor
                && !self.scopes.reads_item(name)
                && let Some(target) = target_name_from_object_name(name)
            {
                let key = target_key(&target, CASE);
                if let Some(rename) = self.renames.iter().position(|rename| *rename.key() == key) {
                    self.found.insert(address(factor), rename);
                }
            }
            ControlFlow::Continue(())
        }
    }

    let mut finder = Finder { renames, scopes: WithScopes::default(), found: BTreeMap::new() };
    let walked = query.visit(&mut finder);
    debug_assert!(walked.is_continue(), "the finder never breaks");
    finder.found
}

fn address<T>(node: &T) -> usize {
    core::ptr::from_ref(node).addr()
}

fn last_identifier(name: &ObjectName) -> Option<&Ident> {
    match name.0.last()? {
        ObjectNamePart::Identifier(ident) => Some(ident),
        ObjectNamePart::Function(_) => None,
    }
}

/// The `FROM` items a renamed column of theirs is merged through by a `USING`
/// list, which keep the column under its old name so the list still reads.
fn merged_through_using(
    query: &Query,
    reading: &BTreeMap<usize, usize>,
    renames: &[Rename],
) -> BTreeSet<usize> {
    struct Using<'a> {
        reading: &'a BTreeMap<usize, usize>,
        renames: &'a [Rename],
        found: BTreeSet<usize>,
    }

    impl Using<'_> {
        fn chain(&mut self, table: &TableWithJoins) {
            let mut seen = Vec::new();
            collect_factors(&table.relation, &mut seen);
            for join in &table.joins {
                collect_factors(&join.relation, &mut seen);
                let Some(names) = using_names(&join.join_operator) else {
                    continue;
                };
                for factor in &seen {
                    let Some(rename) = self.reading.get(factor) else {
                        continue;
                    };
                    let Rename::Column { ordinal, columns, .. } = &self.renames[*rename] else {
                        continue;
                    };
                    let Some(old) = columns.get(*ordinal) else {
                        continue;
                    };
                    if names.iter().any(|name| {
                        last_identifier(name).is_some_and(|name| {
                            identifiers_match(
                                &name.value,
                                name.quote_style.is_some(),
                                &old.value,
                                old.quote_style.is_some(),
                            )
                        })
                    }) {
                        self.found.insert(*factor);
                    }
                }
            }
        }
    }

    fn collect_factors(factor: &TableFactor, seen: &mut Vec<usize>) {
        seen.push(address(factor));
        if let TableFactor::NestedJoin { table_with_joins, .. } = factor {
            collect_factors(&table_with_joins.relation, seen);
            for join in &table_with_joins.joins {
                collect_factors(&join.relation, seen);
            }
        }
    }

    impl Visitor for Using<'_> {
        type Break = ();

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            for_each_select(&query.body, &mut |select| {
                for table in &select.from {
                    self.chain(table);
                }
            });
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            if let TableFactor::NestedJoin { table_with_joins, .. } = factor {
                self.chain(table_with_joins);
            }
            ControlFlow::Continue(())
        }
    }

    let mut using = Using { reading, renames, found: BTreeSet::new() };
    let walked = query.visit(&mut using);
    debug_assert!(walked.is_continue(), "the visitor never breaks");
    using.found
}

fn using_names(operator: &JoinOperator) -> Option<&[ObjectName]> {
    match join_constraint(operator)? {
        JoinConstraint::Using(names) | JoinConstraint::UsingAlias(names, _) => Some(names),
        _ => None,
    }
}

/// Calls `visit` on every `SELECT` a query body is built from, outside the
/// queries nested in it.
fn for_each_select(body: &SetExpr, visit: &mut impl FnMut(&sqlparser::ast::Select)) {
    match body {
        SetExpr::Select(select) => visit(select),
        SetExpr::SetOperation { left, right, .. } => {
            for_each_select(left, visit);
            for_each_select(right, visit);
        }
        _ => {}
    }
}

fn for_each_select_mut(body: &mut SetExpr, visit: &mut impl FnMut(&mut sqlparser::ast::Select)) {
    match body {
        SetExpr::Select(select) => visit(select),
        SetExpr::SetOperation { left, right, .. } => {
            for_each_select_mut(left, visit);
            for_each_select_mut(right, visit);
        }
        _ => {}
    }
}

/// An alias column list bringing back every column name a renamed relation
/// exposed before the rename, keeping the names an existing list wrote.
fn restoring_alias(existing: Option<&TableAlias>, exposed: Ident, columns: &[Ident]) -> TableAlias {
    let mut alias = existing.cloned().unwrap_or(TableAlias {
        explicit: true,
        name: exposed,
        columns: Vec::new(),
        at: None,
    });
    let kept = alias.columns.len();
    alias.columns.extend(
        columns
            .iter()
            .skip(kept)
            .map(|name| TableAliasColumnDef { name: name.clone(), data_type: None }),
    );
    alias
}

/// The edits spelling a definition the way PostgreSQL spells it after the
/// renames, keyed by the address of what they change.
#[derive(Default)]
struct Edits {
    /// A column reference's new last identifier.
    columns: BTreeMap<usize, Ident>,
    /// A qualified reference's new relation identifier.
    qualifiers: BTreeMap<usize, Ident>,
    /// A `FROM` item's new relation name.
    names: BTreeMap<usize, Ident>,
    /// A `FROM` item's new alias.
    aliases: BTreeMap<usize, TableAlias>,
    /// A select-list item's output name, which a renamed column reference
    /// written alone would otherwise change.
    outputs: BTreeMap<usize, Ident>,
}

impl VisitorMut for Edits {
    type Break = ();

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        let at = address(&*factor);
        if let TableFactor::Table { name, alias, .. } = factor {
            if let Some(to) = self.names.get(&at)
                && let Some(ObjectNamePart::Identifier(last)) = name.0.last_mut()
            {
                *last = to.clone();
            }
            if let Some(replacement) = self.aliases.get(&at) {
                *alias = Some(replacement.clone());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        let at = address(&*expression);
        if let Some(to) = self.columns.get(&at) {
            let written = match expression {
                Expr::Identifier(ident) => Some(core::mem::replace(ident, to.clone())),
                Expr::CompoundIdentifier(parts) => {
                    parts.last_mut().map(|last| core::mem::replace(last, to.clone()))
                }
                _ => None,
            };
            if let Some(written) = written {
                self.outputs.insert(at, written);
            }
        }
        if let Some(to) = self.qualifiers.get(&at)
            && let Expr::CompoundIdentifier(parts) = expression
            && let Some(relation) = parts.len().checked_sub(2).and_then(|at| parts.get_mut(at))
        {
            *relation = to.clone();
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        let outputs = &self.outputs;
        for_each_select_mut(&mut query.body, &mut |select| {
            for item in &mut select.projection {
                if let SelectItem::UnnamedExpr(expression) = item
                    && let Some(written) = outputs.get(&address(innermost(expression)))
                {
                    let alias = written.clone();
                    let expr = core::mem::replace(expression, Expr::Identifier(Ident::new("")));
                    *item = SelectItem::ExprWithAlias { expr, alias };
                }
            }
        });
        ControlFlow::Continue(())
    }
}

fn innermost(mut expression: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expression {
        expression = inner;
    }
    expression
}

/// What a rewrite needs to know about one `FROM` item.
struct FactorShape {
    /// The last part of the relation name it reads, for a stored relation.
    relation: Option<Ident>,
    /// The alias it carries.
    alias: Option<TableAlias>,
}

impl FactorShape {
    /// The name a qualified reference reaches the item by.
    fn exposed(&self) -> Option<&Ident> {
        self.alias.as_ref().map(|alias| &alias.name).or(self.relation.as_ref())
    }

    /// Whether its own alias column list renames output column `ordinal`, so
    /// a rename of that column does not reach references through it.
    fn renames_column(&self, ordinal: usize) -> bool {
        self.alias.as_ref().is_some_and(|alias| alias.columns.len() > ordinal)
    }
}

/// Every `FROM` item of `query`, by address.
fn factor_shapes(query: &Query) -> BTreeMap<usize, FactorShape> {
    struct Shapes {
        found: BTreeMap<usize, FactorShape>,
    }

    impl Visitor for Shapes {
        type Break = ();

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            let shape = match factor {
                TableFactor::Table { name, alias, .. } => {
                    FactorShape { relation: last_identifier(name).cloned(), alias: alias.clone() }
                }
                TableFactor::Derived { alias, .. } => {
                    FactorShape { relation: None, alias: alias.clone() }
                }
                _ => FactorShape { relation: None, alias: None },
            };
            self.found.insert(address(factor), shape);
            ControlFlow::Continue(())
        }
    }

    let mut shapes = Shapes { found: BTreeMap::new() };
    let walked = query.visit(&mut shapes);
    debug_assert!(walked.is_continue(), "the visitor never breaks");
    shapes.found
}

/// Every bare column reference of `query`, by address.
fn bare_names(query: &Query) -> BTreeMap<usize, Ident> {
    struct Bare {
        found: BTreeMap<usize, Ident>,
    }

    impl Visitor for Bare {
        type Break = ();

        fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Identifier(ident) = expression {
                self.found.insert(address(expression), ident.clone());
            }
            ControlFlow::Continue(())
        }
    }

    let mut bare = Bare { found: BTreeMap::new() };
    let walked = query.visit(&mut bare);
    debug_assert!(walked.is_continue(), "the visitor never breaks");
    bare.found
}

fn same_identifier(left: &Ident, right: &Ident) -> bool {
    identifiers_match(
        &left.value,
        left.quote_style.is_some(),
        &right.value,
        right.quote_style.is_some(),
    )
}

/// Spells `query` the way PostgreSQL spells it after the renames, answering
/// what every reference of it reached before them, or `None` when that is not
/// certain enough to respell by.
fn respell(query: &mut Query, before: &ParserDB, renames: &[Rename]) -> Option<QueryReferences> {
    let references = query_references(query, before, CASE)?;
    if !references.certain() {
        return None;
    }
    let reading = renamed_factors(query, renames);
    let through_using = merged_through_using(query, &reading, renames);
    let shapes = factor_shapes(query);
    let bare = bare_names(query);
    let mut edits = Edits::default();

    for (factor, rename) in &reading {
        let shape = shapes.get(factor)?;
        match &renames[*rename] {
            Rename::Relation { to, .. } => {
                // A bare relation name reads the whole row.
                if shape.alias.is_none()
                    && let Some(exposed) = shape.exposed()
                    && references.sites.iter().any(|site| {
                        site.outcome == SiteOutcome::Absent
                            && bare
                                .get(&site.expression)
                                .is_some_and(|name| same_identifier(name, exposed))
                    })
                {
                    return None;
                }
                edits.names.insert(*factor, to.clone());
            }
            Rename::Column { columns, .. } if through_using.contains(factor) => {
                let exposed = shape.exposed()?.clone();
                edits
                    .aliases
                    .insert(*factor, restoring_alias(shape.alias.as_ref(), exposed, columns));
            }
            Rename::Column { .. } => {}
        }
    }

    for site in &references.sites {
        let SiteOutcome::Entry { factor, ordinal } = site.outcome else {
            continue;
        };
        let address = references.factors[factor];
        let (Some(rename), Some(shape)) = (reading.get(&address), shapes.get(&address)) else {
            continue;
        };
        match &renames[*rename] {
            Rename::Relation { to, .. } => {
                if shape.alias.is_none() {
                    edits.qualifiers.insert(site.expression, to.clone());
                }
            }
            Rename::Column { ordinal: target, to, .. } => {
                if ordinal == *target
                    && !shape.renames_column(ordinal)
                    && !through_using.contains(&address)
                {
                    edits.columns.insert(site.expression, to.clone());
                }
            }
        }
    }

    let walked = VisitMut::visit(query, &mut edits);
    debug_assert!(walked.is_continue(), "the edits never break");
    Some(references)
}

/// Qualifies every bare reference that reaches something other than
/// `expected` with the name its expected `FROM` item is reached by, which is
/// what a relation capturing it under a new name calls for. Answers `false`
/// when some reference reaching the wrong thing cannot be fixed that way.
fn qualify_captured(
    query: &mut Query,
    references: &QueryReferences,
    expected: &[SiteOutcome],
) -> bool {
    struct Qualify {
        qualifiers: BTreeMap<usize, Ident>,
    }

    impl VisitorMut for Qualify {
        type Break = ();

        fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
            if let Some(qualifier) = self.qualifiers.get(&address(&*expression))
                && let Expr::Identifier(column) = expression
            {
                let column = core::mem::replace(column, Ident::new(""));
                *expression = Expr::CompoundIdentifier(alloc::vec![qualifier.clone(), column]);
            }
            ControlFlow::Continue(())
        }
    }

    let shapes = factor_shapes(query);
    let bare = bare_names(query);
    let mut qualifiers = BTreeMap::new();
    for (site, expected) in references.sites.iter().zip(expected) {
        if site.outcome == *expected {
            continue;
        }
        let SiteOutcome::Entry { factor, .. } = expected else {
            return false;
        };
        let Some(exposed) = references
            .factors
            .get(*factor)
            .and_then(|factor| shapes.get(factor))
            .and_then(FactorShape::exposed)
        else {
            return false;
        };
        if !bare.contains_key(&site.expression) {
            return false;
        }
        qualifiers.insert(site.expression, exposed.clone());
    }
    let walked = VisitMut::visit(query, &mut Qualify { qualifiers });
    debug_assert!(walked.is_continue(), "the visitor never breaks");
    true
}

/// The definition rewritten so that no reference changes, each `FROM` item
/// reading a renamed relation taking the new name under an alias bringing
/// back the old one, or an alias column list bringing back the old column
/// names.
fn shielded(original: &Query, renames: &[Rename]) -> Query {
    let mut query = original.clone();
    let reading = renamed_factors(&query, renames);
    let shapes = factor_shapes(&query);
    let mut edits = Edits::default();
    for (factor, rename) in &reading {
        let Some(shape) = shapes.get(factor) else {
            continue;
        };
        let Some(exposed) = shape.exposed().cloned() else {
            continue;
        };
        match &renames[*rename] {
            Rename::Relation { to, .. } => {
                edits.names.insert(*factor, to.clone());
                if shape.alias.is_none() {
                    edits.aliases.insert(
                        *factor,
                        TableAlias { explicit: true, name: exposed, columns: Vec::new(), at: None },
                    );
                }
            }
            Rename::Column { ordinal, columns, .. } => {
                if !shape.renames_column(*ordinal) {
                    edits
                        .aliases
                        .insert(*factor, restoring_alias(shape.alias.as_ref(), exposed, columns));
                }
            }
        }
    }
    let walked = VisitMut::visit(&mut query, &mut edits);
    debug_assert!(walked.is_continue(), "the edits never break");
    query
}
