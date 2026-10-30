//! Keeping stored view and function body definitions reading what they read,
//! and spelling out the wildcards and `NATURAL` joins a view's creation fixes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    AssignmentTarget, CreateFunctionBody, Delete, Expr, FromTable, Ident, Insert, JoinConstraint,
    JoinOperator, Merge, MergeAction, MergeUpdateKind, ObjectName, ObjectNamePart, Query,
    ReturnStatement, ReturnStatementValue, SelectItem, SetExpr, Statement, TableAlias,
    TableAliasColumnDef, TableFactor, TableObject, TableWithJoins, Update, UpdateTableFromKind,
    Visit, VisitMut, Visitor, VisitorMut,
};

use super::{
    BoundRelation, ParserDBBuilder, function_bodies,
    views::{relations_read_in, statements_read},
};
use crate::{
    errors::Error,
    impls::dql::{
        definition_graph::reference_sites::{QueryReferences, SiteOutcome, query_references},
        join_constraint, join_operator_constraint, query_output_names,
    },
    structs::{IdentifierCase, ParserDB, TableAttribute},
    traits::{ColumnLike, DatabaseLike, TableLike, ViewLike},
    utils::{
        identifier_resolution::identifiers_match,
        last_str,
        object_name::{
            RelationKey, TEMPORARY_SCHEMA, is_temporary_schema, overqualified_object_name,
            relation_name_is_claimed, resolve_view_on_search_path_in_iter, stored_table_key,
            stored_view_key, table_matches_key, target_key, target_name_from_object_name,
            target_name_of_object_name, view_matches_key,
        },
        with_scopes::WithScopes,
    },
};

/// Stored definitions are bound when recorded, so every name in one is read
/// the way PostgreSQL reads it.
const CASE: IdentifierCase = IdentifierCase::AsWritten;

/// What a statement renamed, as stored definitions name it.
pub(super) enum Rename {
    /// The relation stored under `key` is now called `to`, the schema part
    /// included when the statement moved it between schemas.
    Relation {
        /// The relation's key under its old name.
        key: RelationKey,
        /// The relation's new name, as the statement wrote it.
        to: ObjectName,
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

/// Where a stored definition sits in the builder.
#[derive(Clone, Copy)]
enum Slot {
    Plain(usize),
    Materialized(usize),
    Function(usize),
}

/// The stored shape of a definition the machinery rewrites, with where it
/// sits.
enum Definition {
    /// A view's definition, or the query a `RETURN` body's expression is
    /// wrapped in.
    Query { slot: Slot, query: alloc::boxed::Box<Query> },
    /// The statements of a `BEGIN ATOMIC` body.
    Statements { function: usize, statements: Vec<Statement> },
}

/// Writes a rewritten query back into the definition that holds it.
fn store_query(builder: &mut ParserDBBuilder, slot: Slot, query: Query) {
    match slot {
        Slot::Plain(position) => {
            let declaration =
                alloc::sync::Arc::make_mut(&mut builder.views_mut()[position].0).declaration_mut();
            *declaration.query_mut() = query;
        }
        Slot::Materialized(position) => {
            let declaration =
                alloc::sync::Arc::make_mut(&mut builder.materialized_views_mut()[position].0)
                    .declaration_mut();
            *declaration.query_mut() = query;
        }
        Slot::Function(position) => {
            let function = alloc::sync::Arc::make_mut(&mut builder.functions_mut()[position].0);
            function_bodies::write_definition(&mut function.function_body, query);
        }
    }
}

/// Every stored definition whose body names one of `keys`.
fn definitions_reading(builder: &ParserDBBuilder, keys: &[&RelationKey]) -> Vec<Definition> {
    let names = |reads: &[RelationKey]| reads.iter().any(|key| keys.contains(&key));
    let plain = builder.views().iter().enumerate().filter_map(|(position, (view, _, _))| {
        let definition = view.definition();
        names(&relations_read_in(definition)).then_some(Definition::Query {
            slot: Slot::Plain(position),
            query: alloc::boxed::Box::new(definition.clone()),
        })
    });
    let materialized =
        builder.materialized_views().iter().enumerate().filter_map(|(position, (view, _, _))| {
            let definition = view.definition();
            names(&relations_read_in(definition)).then_some(Definition::Query {
                slot: Slot::Materialized(position),
                query: alloc::boxed::Box::new(definition.clone()),
            })
        });
    let functions =
        builder.functions().iter().enumerate().filter_map(|(position, (function, _))| {
            let definition = match function.function_body.as_ref()? {
                CreateFunctionBody::Return(expr) => {
                    Definition::Query {
                        slot: Slot::Function(position),
                        query: alloc::boxed::Box::new(function_bodies::definition(expr)),
                    }
                }
                CreateFunctionBody::BeginAtomic(block) => {
                    Definition::Statements {
                        function: position,
                        statements: block.statements.clone(),
                    }
                }
                // A body written as a string binds nothing to follow.
                _ => return None,
            };
            names(&definition_read(builder, &definition)).then_some(definition)
        });
    plain.chain(materialized).chain(functions).collect()
}

/// Every relation a stored definition reads or writes.
fn definition_read(builder: &ParserDBBuilder, definition: &Definition) -> Vec<RelationKey> {
    match definition {
        Definition::Query { query, .. } => relations_read_in(query),
        Definition::Statements { statements, .. } => statements_read(builder, statements),
    }
}

/// The part of the catalog the stored `definitions` read, which is every
/// relation they name and, transitively, every relation a view among those
/// names.
fn catalog_definitions<'q>(
    builder: &ParserDBBuilder,
    definitions: impl IntoIterator<Item = &'q Definition>,
) -> ParserDB {
    let reads: Vec<RelationKey> = definitions
        .into_iter()
        .flat_map(|definition| definition_read(builder, definition))
        .collect();
    catalog_reads(builder, reads)
}

/// The part of the catalog `definitions` read, which is every relation they
/// name and, transitively, every relation a view among those names.
pub(super) fn catalog<'q>(
    builder: &ParserDBBuilder,
    definitions: impl IntoIterator<Item = &'q Query>,
) -> ParserDB {
    let reads: Vec<RelationKey> = definitions.into_iter().flat_map(relations_read_in).collect();
    catalog_reads(builder, reads)
}

/// The part of the catalog `reads` name, which is every relation among them
/// and, transitively, every relation a view among those names.
fn catalog_reads(builder: &ParserDBBuilder, reads: Vec<RelationKey>) -> ParserDB {
    let mut pending = reads;
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

/// The stored definitions reading what a statement is about to rename, with
/// the part of the catalog they read as it stands before the statement.
pub(super) struct Readers {
    definitions: Vec<Definition>,
    before: ParserDB,
}

/// The stored definitions reading a relation one of `keys` names, or `None`
/// when none does and a rename of them has nothing to follow.
pub(super) fn readers(builder: &ParserDBBuilder, keys: &[&RelationKey]) -> Option<Readers> {
    let definitions = definitions_reading(builder, keys);
    if definitions.is_empty() {
        return None;
    }
    let before = catalog_definitions(builder, definitions.iter());
    Some(Readers { definitions, before })
}

/// How a block statement holds the query part it holds.
#[derive(Clone, Copy)]
enum PartShape {
    /// The statement is the query itself.
    Statement,
    /// The statement returns the query as its expression.
    Return,
}

/// A query part of a stored definition spelled the way PostgreSQL spells it
/// after the renames, waiting to be checked against the renamed catalog.
struct Part {
    /// Where the part's definition sits.
    slot: Slot,
    /// The statement the part is a part of and how it holds the query, which
    /// is `None` for a definition that is a query of its own.
    statement: Option<(usize, PartShape)>,
    /// The part before the renames.
    original: Query,
    /// The part `follow_renames` put in place, which the check reads back.
    stored: Query,
    /// What every reference of the original reached before the renames.
    expected: Vec<SiteOutcome>,
    /// How many `FROM` items the original holds.
    factors: usize,
}

/// A respelled query part of a block statement, awaiting its check.
struct StatementPart {
    /// The statement the part is a part of.
    statement: usize,
    /// How the statement holds the part.
    shape: PartShape,
    original: Query,
    stored: Query,
    expected: Vec<SiteOutcome>,
    factors: usize,
}

/// Rewrites every stored definition `readers` found so it keeps reading what
/// `renames` renamed, once the builder holds the renamed catalog.
///
/// Definitions are only rewritten in place, never added or removed, so the
/// positions `readers` recorded before the renames still hold.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches the write target of a `BEGIN ATOMIC` statement the model could not
/// bind.
pub(super) fn follow_renames(
    builder: &mut ParserDBBuilder,
    readers: Readers,
    renames: &[Rename],
) -> Result<(), Error> {
    let Readers { definitions, before } = readers;
    let mut parts: Vec<Part> = Vec::new();
    for definition in definitions {
        match definition {
            Definition::Query { slot, query } => {
                let original = *query;
                let mut respelled = original.clone();
                match respell(&mut respelled, &before, renames) {
                    Some(references) => {
                        parts.push(Part {
                            slot,
                            statement: None,
                            original,
                            stored: respelled.clone(),
                            expected: references.sites.iter().map(|site| site.outcome).collect(),
                            factors: references.factors.len(),
                        });
                        store_query(builder, slot, respelled);
                    }
                    None => store_query(builder, slot, shielded(&original, renames)),
                }
            }
            Definition::Statements { function, statements } => {
                let function_name = last_str(&builder.functions()[function].0.name).to_string();
                let (rewritten, statement_parts) =
                    respell_statements(&statements, &before, renames, &function_name)?;
                for part in statement_parts {
                    parts.push(Part {
                        slot: Slot::Function(function),
                        statement: Some((part.statement, part.shape)),
                        original: part.original,
                        stored: part.stored,
                        expected: part.expected,
                        factors: part.factors,
                    });
                }
                let entry = alloc::sync::Arc::make_mut(&mut builder.functions_mut()[function].0);
                function_bodies::write_statements(&mut entry.function_body, rewritten);
            }
        }
    }
    if parts.is_empty() {
        return Ok(());
    }

    // Every respelled part is in place before any is checked.
    let after = {
        let reads: Vec<RelationKey> =
            parts.iter().flat_map(|part| relations_read_in(&part.stored)).collect();
        catalog_reads(builder, reads)
    };
    for part in parts {
        if query_references(&part.stored, &after, CASE)
            .is_some_and(|references| reaches(&references, &part.expected, part.factors))
        {
            continue;
        }
        // Qualifications are keyed by address, so resolve the copy edited.
        let mut qualified = part.stored.clone();
        let settled = query_references(&qualified, &after, CASE).is_some_and(|references| {
            qualify_captured(&mut qualified, &references, &part.expected)
        }) && query_references(&qualified, &after, CASE)
            .is_some_and(|references| reaches(&references, &part.expected, part.factors));
        store_part(
            builder,
            &part,
            if settled { qualified } else { shielded(&part.original, renames) },
        );
    }
    Ok(())
}

/// Writes a checked query part back into the definition that holds it.
fn store_part(builder: &mut ParserDBBuilder, part: &Part, query: Query) {
    match part.slot {
        Slot::Plain(position) => {
            let declaration =
                alloc::sync::Arc::make_mut(&mut builder.views_mut()[position].0).declaration_mut();
            *declaration.query_mut() = query;
        }
        Slot::Materialized(position) => {
            let declaration =
                alloc::sync::Arc::make_mut(&mut builder.materialized_views_mut()[position].0)
                    .declaration_mut();
            *declaration.query_mut() = query;
        }
        Slot::Function(position) => {
            let function = alloc::sync::Arc::make_mut(&mut builder.functions_mut()[position].0);
            if let (Some((statement, shape)), Some(CreateFunctionBody::BeginAtomic(block))) =
                (part.statement, function.function_body.as_mut())
            {
                block.statements[statement] = statement_with_query_part(shape, query);
            } else {
                function_bodies::write_definition(&mut function.function_body, query);
            }
        }
    }
}

/// A statement holding `query` as the part `shape` spells it.
fn statement_with_query_part(shape: PartShape, query: Query) -> Statement {
    match shape {
        PartShape::Statement => Statement::Query(alloc::boxed::Box::new(query)),
        PartShape::Return => {
            Statement::Return(ReturnStatement {
                value: Some(ReturnStatementValue::Expr(Expr::Subquery(alloc::boxed::Box::new(
                    query,
                )))),
            })
        }
    }
}

/// The query part of a statement, which is the statement itself when it is a
/// query and the subquery a `RETURN` statement returns.
fn part_of(statement: &Statement) -> Option<&Query> {
    match statement {
        Statement::Query(query) => Some(query),
        Statement::Return(statement) => {
            match &statement.value {
                Some(ReturnStatementValue::Expr(Expr::Subquery(query))) => Some(query.as_ref()),
                _ => None,
            }
        }
        _ => None,
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
    join_operator_constraint!(operator)
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
                        let target = target_name_of_object_name(name);
                        identifiers_match(
                            target.name(),
                            target.name_is_quoted(),
                            &old.value,
                            old.quote_style.is_some(),
                        )
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
    /// A `FROM` item's new relation name, the schema part included when the
    /// statement moved it between schemas.
    names: BTreeMap<usize, ObjectName>,
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
            if let Some(to) = self.names.get(&at) {
                // A new name written with its schema moves the relation, so it
                // replaces the qualifier too.
                let kept = if to.0.len() > 1 { 0 } else { name.0.len().saturating_sub(1) };
                name.0.truncate(kept);
                name.0.extend(to.0.iter().cloned());
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
                    FactorShape { relation: Some(last_part_as_ident(name)), alias: alias.clone() }
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
                    edits.qualifiers.insert(site.expression, last_part_as_ident(to));
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

/// Respells a block's statements the way PostgreSQL respells them after the
/// renames: query statements through the query path, and `INSERT`, `UPDATE`,
/// `DELETE` and `MERGE` through their own clauses. Answers the rewritten
/// statements and the query parts among them, each with what its references
/// reached before the renames.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches the write target of a statement the model could not bind.
fn respell_statements(
    statements: &[Statement],
    before: &ParserDB,
    renames: &[Rename],
    function_name: &str,
) -> Result<(Vec<Statement>, Vec<StatementPart>), Error> {
    let mut rewritten: Vec<Statement> = Vec::with_capacity(statements.len());
    let mut parts: Vec<StatementPart> = Vec::new();
    for (position, statement) in statements.iter().enumerate() {
        if let Some(query) = part_of(statement) {
            let shape = match statement {
                Statement::Query(_) => PartShape::Statement,
                _ => PartShape::Return,
            };
            let original = query.clone();
            let mut respelled = original.clone();
            let stored = match respell(&mut respelled, before, renames) {
                Some(references) => {
                    parts.push(StatementPart {
                        statement: position,
                        shape,
                        original,
                        stored: respelled.clone(),
                        expected: references.sites.iter().map(|site| site.outcome).collect(),
                        factors: references.factors.len(),
                    });
                    respelled
                }
                None => shielded(&original, renames),
            };
            rewritten.push(statement_with_query_part(shape, stored));
            continue;
        }
        let mut statement = statement.clone();
        // Every statement's own clauses respell or shield the same way, which
        // a statement without clause factors answers with no edits at all.
        respell_embedded(&mut statement, before, renames);
        if respell_shell(&mut statement, before, renames).is_none() {
            shield_shell(&mut statement, before, renames, function_name)?;
        }
        rewritten.push(statement);
    }
    Ok((rewritten, parts))
}

/// Applies, at ingestion, the analysis PostgreSQL does of a `BEGIN ATOMIC`
/// body's statements: the catalog qualifier a factor name spells is dropped
/// when it names this catalog, and a clause reference carrying more parts
/// than the relation it names is dropped to the parts it keeps or refused
/// when its parts name no relation the statement's own clauses scope.
pub(super) fn normalize_block_references(
    builder: &ParserDBBuilder,
    statements: &mut [Statement],
) -> Result<(), Error> {
    let catalog = builder.catalog_name();
    for statement in statements.iter_mut() {
        strip_catalog_qualifier(statement, catalog)?;
        normalize_statement_references(builder, statement, catalog)?;
    }
    Ok(())
}

/// The catalog qualifier a factor name spells, dropped when it names this
/// catalog, the way PostgreSQL drops it before binding the name.
fn strip_catalog_qualifier(statement: &mut Statement, catalog: &str) -> Result<(), Error> {
    let strip_factor = |factor: &mut TableFactor| -> Result<(), Error> {
        if let Some(name) = factor_name(factor) {
            strip_factor_name(name, catalog)?;
        }
        Ok(())
    };
    match statement {
        Statement::Insert(Insert { table: TableObject::TableName(name), .. }) => {
            strip_factor_name(name, catalog)
        }
        Statement::Update(Update { table, from, .. }) => {
            strip_factor(&mut table.relation)?;
            if let Some(from) = from.as_mut() {
                let tables = match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => tables,
                };
                for table in tables {
                    strip_factor(&mut table.relation)?;
                    for join in &mut table.joins {
                        strip_factor(&mut join.relation)?;
                    }
                }
            }
            Ok(())
        }
        Statement::Delete(Delete { from, using, .. }) => {
            let tables = match from {
                FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
            };
            for table in tables {
                strip_factor(&mut table.relation)?;
                for join in &mut table.joins {
                    strip_factor(&mut join.relation)?;
                }
            }
            if let Some(using) = using {
                for table in using {
                    strip_factor(&mut table.relation)?;
                    for join in &mut table.joins {
                        strip_factor(&mut join.relation)?;
                    }
                }
            }
            Ok(())
        }
        Statement::Merge(Merge { table, source, .. }) => {
            strip_factor(table)?;
            strip_factor(source)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The name one clause factor spells, which a table function does not hold.
fn factor_name(factor: &mut TableFactor) -> Option<&mut ObjectName> {
    match factor {
        TableFactor::Table { name, args: None, .. } => Some(name),
        _ => None,
    }
}

/// Drops the catalog qualifier one factor name spells when it names this
/// catalog, refusing any other catalog it may name.
fn strip_factor_name(name: &mut ObjectName, catalog: &str) -> Result<(), Error> {
    if name.0.len() != 3 {
        return Ok(());
    }
    match name.0.first() {
        Some(ObjectNamePart::Identifier(first))
            if identifiers_match(&first.value, first.quote_style.is_some(), catalog, false) =>
        {
            name.0.remove(0);
            Ok(())
        }
        _ => Err(refused_object_name(name)),
    }
}

/// The refusal PostgreSQL gives a name or a reference it will not analyze,
/// named as the statement wrote it.
fn refused_object_name(name: &ObjectName) -> Error {
    overqualified_object_name(name).into()
}

/// The part a clause reference keeps: its database qualifier is dropped when
/// it names this catalog, the way PostgreSQL drops it, a reference naming any
/// other catalog or carrying more parts than a database, a schema and a
/// column is refused outright.
fn keep_reference_parts(parts: Vec<Ident>, catalog: &str) -> Result<Vec<Ident>, Error> {
    if parts.len() >= 5 {
        return Err(refused_reference(&parts));
    }
    if parts.len() == 4 {
        let matches_catalog = parts.first().is_some_and(|first| {
            identifiers_match(&first.value, first.quote_style.is_some(), catalog, false)
        });
        if !matches_catalog {
            return Err(refused_reference(&parts));
        }
        return Ok(parts.into_iter().skip(1).collect());
    }
    Ok(parts)
}

/// The refusal PostgreSQL gives a clause reference it will not analyze.
fn refused_reference(parts: &[Ident]) -> Error {
    let name =
        ObjectName(parts.iter().map(|part| ObjectNamePart::Identifier(part.clone())).collect());
    refused_object_name(&name)
}

/// One statement's clause references, PostgreSQL's analysis applied: a
/// reference naming a relation the statement's own clauses scope under a
/// matching schema is dropped to its last two parts, every other reference
/// with more parts than that is refused.
fn normalize_statement_references(
    builder: &ParserDBBuilder,
    statement: &mut Statement,
    catalog: &str,
) -> Result<(), Error> {
    let keys = own_clause_factor_names(statement)
        .iter()
        .filter_map(|name| builder.bound_relation(name).map(|bound| bound_factor_key(&bound, name)))
        .collect::<Vec<_>>();
    let slice = catalog_reads(builder, keys);
    let factors = shell_factors(statement, &slice);
    let mut rewrites = BTreeMap::new();
    for (at, parts) in clause_references(statement) {
        let parts = keep_reference_parts(parts, catalog)?;
        if let [schema, table, column] = parts.as_slice()
            && let Some(factor) =
                factors.iter().find(|factor| same_identifier(&factor.exposed, table))
            && identifiers_match(
                &factor.key.schema,
                false,
                &schema.value,
                schema.quote_style.is_some(),
            )
        {
            let matches_column = factor.columns.as_ref().is_none_or(|columns| {
                columns.iter().any(|(name, quoted)| {
                    identifiers_match(&column.value, column.quote_style.is_some(), name, *quoted)
                })
            });
            if matches_column {
                rewrites.insert(at, alloc::vec![table.clone(), column.clone()]);
            } else {
                return Err(refused_reference(&parts));
            }
        } else if parts.len() == 3 {
            return Err(refused_reference(&parts));
        }
    }
    if rewrites.is_empty() {
        return Ok(());
    }
    let mut references = ReferenceRewrites { rewrites };
    let walked = VisitMut::visit(statement, &mut references);
    debug_assert!(walked.is_continue(), "the rewriter never breaks");
    Ok(())
}

/// The names a statement's own clauses factor, the queries nested in it
/// excluded, which the reference analysis resolves.
fn own_clause_factor_names(statement: &Statement) -> Vec<&ObjectName> {
    let mut names = Vec::new();
    match statement {
        Statement::Insert(Insert { table: TableObject::TableName(name), .. }) => {
            names.push(name);
        }
        Statement::Update(Update { table, from, .. }) => {
            push_factor_name(&table.relation, &mut names);
            if let Some(tables) = from.as_ref().map(|from| {
                match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => tables,
                }
            }) {
                for table in tables {
                    push_factor_name(&table.relation, &mut names);
                    for join in &table.joins {
                        push_factor_name(&join.relation, &mut names);
                    }
                }
            }
        }
        Statement::Delete(Delete { from, using, .. }) => {
            let tables = match from {
                FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
            };
            for table in tables {
                push_factor_name(&table.relation, &mut names);
                for join in &table.joins {
                    push_factor_name(&join.relation, &mut names);
                }
            }
            if let Some(using) = using {
                for table in using {
                    push_factor_name(&table.relation, &mut names);
                    for join in &table.joins {
                        push_factor_name(&join.relation, &mut names);
                    }
                }
            }
        }
        Statement::Merge(Merge { table, source, .. }) => {
            push_factor_name(table, &mut names);
            push_factor_name(source, &mut names);
        }
        _ => {}
    }
    names
}

/// One clause factor's name, which a table function does not hold.
fn push_factor_name<'a>(factor: &'a TableFactor, names: &mut Vec<&'a ObjectName>) {
    if let TableFactor::Table { name, args: None, .. } = factor {
        names.push(name);
    }
}

/// The key a factor name reaches its relation by, the relation it bound
/// under at ingestion, which is the temporary schema for a temporary
/// relation.
fn bound_factor_key(bound: &BoundRelation, name: &ObjectName) -> RelationKey {
    let target = target_name_of_object_name(name);
    let schema = if bound.temporary {
        CASE.compared_form(TEMPORARY_SCHEMA, false)
    } else {
        match &bound.qualifier {
            Some(qualifier) => {
                CASE.compared_form(&qualifier.value, qualifier.quote_style.is_some())
            }
            None => CASE.compared_form("public", false),
        }
    };
    RelationKey {
        schema: schema.into_owned(),
        name: CASE.compared_form(target.name(), target.name_is_quoted()).into_owned(),
    }
}

/// The rewrites one statement's clause references take, dropping an
/// analyzed reference to the parts it keeps.
struct ReferenceRewrites {
    rewrites: BTreeMap<usize, Vec<Ident>>,
}

impl VisitorMut for ReferenceRewrites {
    type Break = ();

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        if let Some(to) = self.rewrites.get(&address(expression))
            && let Expr::CompoundIdentifier(parts) = expression
        {
            parts.clone_from(to);
        }
        ControlFlow::Continue(())
    }
}

/// A relation a statement's own clauses read, with the name its factor
/// reaches it by.
struct ShellFactor {
    /// The name the statement reaches the relation by, which is its alias
    /// when it carries one.
    exposed: Ident,
    /// Whether the factor carries an alias.
    has_alias: bool,
    /// The alias a clause factor carries, which a shield extends to bring
    /// back the old column names.
    alias: Option<TableAlias>,
    /// The key the relation resolves to in the catalog the statement read,
    /// which a rename follows it by.
    key: RelationKey,
    /// The address a rename reaches the factor by, which is the clause
    /// factor's or the write target's name's for an `INSERT` target.
    address: usize,
    /// Whether the factor is an `INSERT` write target.
    insert_target: bool,
    /// The relation's column names before the renames, which the respelling
    /// resolves the statement's own references against.
    columns: Option<Vec<(String, bool)>>,
}

/// Every relation a statement's own clauses read, in the order PostgreSQL
/// scopes them.
fn shell_factors(statement: &Statement, before: &ParserDB) -> Vec<ShellFactor> {
    let mut factors = Vec::new();
    match statement {
        Statement::Insert(Insert { table: TableObject::TableName(name), table_alias, .. }) => {
            let key = resolved_factor_key(before, name);
            let columns = relation_columns(before, &key);
            factors.push(ShellFactor {
                exposed: table_alias
                    .as_ref()
                    .map_or_else(|| last_part_as_ident(name), |alias| alias.alias.clone()),
                has_alias: table_alias.is_some(),
                alias: None,
                key,
                address: address(name),
                insert_target: true,
                columns,
            });
        }
        Statement::Update(Update { table, from, .. }) => {
            push_shell_factor(&mut factors, &table.relation, before);
            if let Some(tables) = from.as_ref().map(|from| {
                match from {
                    UpdateTableFromKind::BeforeSet(tables)
                    | UpdateTableFromKind::AfterSet(tables) => tables,
                }
            }) {
                for table in tables {
                    push_shell_factor(&mut factors, &table.relation, before);
                    for join in &table.joins {
                        push_shell_factor(&mut factors, &join.relation, before);
                    }
                }
            }
        }
        Statement::Delete(Delete { from, using, .. }) => {
            let tables = match from {
                FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
            };
            for table in tables {
                push_shell_factor(&mut factors, &table.relation, before);
                for join in &table.joins {
                    push_shell_factor(&mut factors, &join.relation, before);
                }
            }
            if let Some(using) = using {
                for table in using {
                    push_shell_factor(&mut factors, &table.relation, before);
                    for join in &table.joins {
                        push_shell_factor(&mut factors, &join.relation, before);
                    }
                }
            }
        }
        Statement::Merge(Merge { table, source, .. }) => {
            push_shell_factor(&mut factors, table, before);
            push_shell_factor(&mut factors, source, before);
        }
        _ => {}
    }
    factors
}

/// Adds one clause factor of a statement to the list.
fn push_shell_factor(factors: &mut Vec<ShellFactor>, factor: &TableFactor, before: &ParserDB) {
    let TableFactor::Table { name, alias, args: None, .. } = factor else {
        return;
    };
    let key = resolved_factor_key(before, name);
    let columns = relation_columns(before, &key);
    let alias = alias.clone();
    factors.push(ShellFactor {
        exposed: alias
            .as_ref()
            .map_or_else(|| last_part_as_ident(name), |alias| alias.name.clone()),
        has_alias: alias.is_some(),
        alias,
        key,
        address: address(factor),
        insert_target: false,
        columns,
    });
}

/// The key a stored factor name reaches the relation by in `catalog`, the
/// relation it resolves to under the catalog's search path, temporary schema
/// searched first.
///
/// Ingestion binds every factor of a statement's own clauses, so the name
/// always reaches a relation, and the key the name spells stands in otherwise.
fn resolved_factor_key(catalog: &ParserDB, name: &ObjectName) -> RelationKey {
    let target = target_name_of_object_name(name);
    if name.0.len() == 2 {
        return target_key(&target, CASE);
    }
    if let Ok(Some(table)) = catalog.resolve_table_object_name_on_search_path(name) {
        return stored_table_key(table, CASE);
    }
    let claimed = |key: &RelationKey| relation_name_is_claimed(catalog, key, CASE);
    let path = || catalog.search_path();
    let plain = resolve_view_on_search_path_in_iter(catalog.views(), &target, path, CASE, claimed);
    let materialized = resolve_view_on_search_path_in_iter(
        catalog.materialized_views(),
        &target,
        path,
        CASE,
        claimed,
    );
    let plain = plain.ok().flatten().map(|view| stored_view_key(view, CASE));
    let materialized = materialized.ok().flatten().map(|view| stored_view_key(view, CASE));
    plain.or(materialized).unwrap_or(target_key(&target, CASE))
}

/// The identifier a stored factor name's last part spells, which a static
/// name always holds.
fn last_part_as_ident(name: &ObjectName) -> Ident {
    let target = target_name_of_object_name(name);
    if target.name_is_quoted() {
        Ident::with_quote('"', target.name().to_string())
    } else {
        Ident::new(target.name().to_string())
    }
}

/// The name a respelled or shielded clause spells a renamed factor under, the
/// temporary schema PostgreSQL stores it without dropped.
fn respelled_factor_name(to: &ObjectName) -> ObjectName {
    ObjectName(
        to.0.iter()
            .filter(|part| {
                !matches!(
                    part,
                    ObjectNamePart::Identifier(ident)
                        if is_temporary_schema(&ident.value, ident.quote_style.is_some())
                )
            })
            .cloned()
            .collect(),
    )
}

/// The names a relation's output columns had before the renames, in order,
/// or `None` when some output cannot be named.
fn relation_columns(before: &ParserDB, key: &RelationKey) -> Option<Vec<(String, bool)>> {
    let table = before.tables().find(|table| table_matches_key(*table, key, CASE));
    if let Some(table) = table {
        let columns = table.columns(before).ok()?;
        return Some(
            columns
                .map(|column| (column.column_name().to_string(), column.column_name_is_quoted()))
                .collect(),
        );
    }
    // The key came from a resolved factor, so a view of one kind holds it.
    before.views().find(|view| view_matches_key(*view, key, CASE)).map_or_else(
        || {
            before
                .materialized_views()
                .find(|view| view_matches_key(*view, key, CASE))
                .and_then(|view| view_output_names(view, before))
        },
        |view| view_output_names(view, before),
    )
}

/// The names a view's outputs had, declared or derived from its definition.
fn view_output_names<V: ViewLike>(view: &V, before: &ParserDB) -> Option<Vec<(String, bool)>> {
    if let Some(declared) = view.declared_output_names() {
        return Some(declared);
    }
    query_output_names(view.definition(), before, CASE).ok().flatten()
}

/// What a reference of a statement's own clauses reached before the renames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellOutcome {
    /// Output column `ordinal` of the factor at position `factor`.
    Entry { factor: usize, ordinal: usize },
    /// Nothing in reach exposes the name, as for a parameter.
    Absent,
    /// Nothing certain, because an opaque relation could have answered or
    /// the reference was ambiguous.
    Unknown,
}

struct ShellSite {
    /// The address of the reference's expression.
    address: usize,
    /// The reference's name, which is `None` for a qualified one.
    name: Option<Ident>,
    /// What the reference reached before the renames.
    outcome: ShellOutcome,
}

/// Every column reference of a statement's own clauses, the queries nested
/// in it excluded, with what each reached before the renames.
fn shell_sites(statement: &Statement, factors: &[ShellFactor]) -> Vec<ShellSite> {
    clause_references(statement)
        .into_iter()
        .map(|(at, parts)| {
            ShellSite {
                address: at,
                name: (parts.len() == 1).then(|| parts[0].clone()),
                outcome: shell_outcome(&parts, factors),
            }
        })
        .collect()
}

/// Every reference of a statement's own clauses, the queries nested in it
/// excluded, each as the parts it wrote and the address it holds them at.
fn clause_references(statement: &Statement) -> Vec<(usize, Vec<Ident>)> {
    struct References {
        depth: usize,
        found: Vec<(usize, Vec<Ident>)>,
    }

    impl Visitor for References {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.depth += 1;
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
            if self.depth == 0 {
                match expression {
                    Expr::Identifier(name) => {
                        self.found.push((address(expression), alloc::vec![name.clone()]));
                    }
                    Expr::CompoundIdentifier(parts) => {
                        self.found.push((address(expression), parts.clone()));
                    }
                    _ => {}
                }
            }
            ControlFlow::Continue(())
        }
    }

    let mut finder = References { depth: 0, found: Vec::new() };
    let walked = statement.visit(&mut finder);
    debug_assert!(walked.is_continue(), "the finder never breaks");
    finder.found
}

/// What one reference of a statement's own clauses reached before the
/// renames, resolving its written parts against the factors the statement
/// scopes them.
///
/// A reference held at ingestion names its column in its last part and its
/// relation in the part before, so its first part is the relation's name
/// when it carries one.
fn shell_outcome(parts: &[Ident], factors: &[ShellFactor]) -> ShellOutcome {
    let (qualifier, column) = if parts.len() == 1 {
        (None, &parts[0])
    } else {
        (Some(&parts[0]), &parts[parts.len() - 1])
    };
    let mut answers: Vec<(usize, usize)> = Vec::new();
    for (index, factor) in factors.iter().enumerate() {
        let matches_name = match qualifier {
            Some(qualifier) => same_identifier(qualifier, &factor.exposed),
            None => true,
        };
        if !matches_name {
            continue;
        }
        let Some(columns) = exposed_columns(factor) else {
            return ShellOutcome::Unknown;
        };
        for (ordinal, (name, quoted)) in columns.iter().enumerate() {
            if identifiers_match(&column.value, column.quote_style.is_some(), name, *quoted) {
                answers.push((index, ordinal));
                break;
            }
        }
    }
    match answers.as_slice() {
        [] => ShellOutcome::Absent,
        [(factor, ordinal)] => ShellOutcome::Entry { factor: *factor, ordinal: *ordinal },
        _ => ShellOutcome::Unknown,
    }
}

/// The names a factor's outputs had before the renames, in order, which an
/// alias column list renames the first of them.
fn exposed_columns(factor: &ShellFactor) -> Option<Vec<(String, bool)>> {
    let columns = factor.columns.as_ref()?;
    Some(
        columns
            .iter()
            .enumerate()
            .map(|(ordinal, column)| {
                factor.alias.as_ref().and_then(|alias| alias.columns.get(ordinal)).map_or_else(
                    || column.clone(),
                    |alias| (alias.name.value.clone(), alias.name.quote_style.is_some()),
                )
            })
            .collect(),
    )
}

/// Respells the queries nested in a statement, an `INSERT`'s source and
/// every subquery in its clauses, each on its own the way a view's definition
/// is. A query that cannot be respelled by is left as written.
fn respell_embedded(statement: &mut Statement, before: &ParserDB, renames: &[Rename]) {
    struct Embedded<'a> {
        before: &'a ParserDB,
        renames: &'a [Rename],
    }

    impl VisitorMut for Embedded<'_> {
        type Break = ();

        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
            let _ = respell(query, self.before, self.renames);
            ControlFlow::Continue(())
        }
    }

    let mut embedded = Embedded { before, renames };
    let walked = VisitMut::visit(statement, &mut embedded);
    debug_assert!(walked.is_continue(), "the walker never breaks");
}

/// Respells a statement's own clauses the way PostgreSQL respells them after
/// the renames, or [`None`] when a reference is not certain enough to
/// respell by. The queries nested in the statement are respelled on their
/// own, so only the clauses owning them are rewritten here.
fn respell_shell(statement: &mut Statement, before: &ParserDB, renames: &[Rename]) -> Option<()> {
    let factors = shell_factors(statement, before);
    let sites = shell_sites(statement, &factors);
    if sites.iter().any(|site| site.outcome == ShellOutcome::Unknown) {
        return None;
    }
    // A renamed relation captures a bare reference that names it, which read
    // the whole row.
    for factor in &factors {
        if factor.has_alias || factor.insert_target {
            continue;
        }
        let relation_renamed = renames.iter().any(|candidate| {
            match candidate {
                Rename::Relation { key: rename_key, .. } => rename_key == &factor.key,
                Rename::Column { .. } => false,
            }
        });
        if !relation_renamed {
            continue;
        }
        if sites.iter().any(|site| {
            site.outcome == ShellOutcome::Absent
                && site.name.as_ref().is_some_and(|name| same_identifier(name, &factor.exposed))
        }) {
            return None;
        }
    }
    let mut edits = shell_edits(&factors, &sites, renames);
    rename_lists(statement, &factors, renames);
    let walked = VisitMut::visit(statement, &mut edits);
    debug_assert!(walked.is_continue(), "the edits never break");
    Some(())
}

/// The edits that respell a statement's own clauses, one block per factor
/// the renames touch.
fn shell_edits(factors: &[ShellFactor], sites: &[ShellSite], renames: &[Rename]) -> ShellEdits {
    let mut edits = ShellEdits::default();
    for (index, factor) in factors.iter().enumerate() {
        let key = &factor.key;
        let relation_to = renames.iter().find_map(|candidate| {
            match candidate {
                Rename::Relation { key: rename_key, to } if rename_key == key => Some(to),
                _ => None,
            }
        });
        let relation_to_name = relation_to.as_ref().map(|to| last_part_as_ident(to));
        let column_renames: Vec<(usize, &Ident)> = renames
            .iter()
            .filter(|candidate| candidate.key() == key)
            .filter_map(|candidate| {
                match candidate {
                    Rename::Column { ordinal, to, .. } => Some((*ordinal, to)),
                    Rename::Relation { .. } => None,
                }
            })
            .collect();
        if let Some(to) = relation_to {
            let to = respelled_factor_name(to);
            if factor.insert_target {
                edits.names.insert(factor.address, to);
            } else {
                edits.factor_names.insert(factor.address, to);
            }
        }
        // A qualified reference carries the relation's old name, the new
        // name replaces the qualifier, a write target included: PostgreSQL
        // respells it under the target's new name as well.
        if let Some(qualifier) = relation_to_name.as_ref()
            && !factor.has_alias
        {
            for site in sites {
                let ShellOutcome::Entry { factor: target, .. } = site.outcome else {
                    continue;
                };
                if target == index && site.name.is_none() {
                    edits.qualifiers.insert(site.address, qualifier.clone());
                }
            }
        }
        for (ordinal, to) in column_renames {
            let pinned = factor.alias.as_ref().is_some_and(|alias| alias.columns.len() > ordinal);
            if pinned {
                continue;
            }
            let qualifier = if factor.has_alias {
                factor.exposed.clone()
            } else {
                relation_to_name.clone().unwrap_or_else(|| factor.exposed.clone())
            };
            for site in sites {
                let ShellOutcome::Entry { factor: target, ordinal: at } = site.outcome else {
                    continue;
                };
                if target != index || at != ordinal {
                    continue;
                }
                if site.name.is_some() {
                    edits.qualified.insert(site.address, (qualifier.clone(), to.clone()));
                } else {
                    edits.columns.insert(site.address, to.clone());
                }
            }
        }
    }
    edits
}

/// Shields a statement's own clauses from the renames, each renamed relation
/// the statement reads taking the new name under an alias that brings back
/// the old one and the old column names. An `INSERT` target cannot reach
/// here: its statement's own clauses hold one factor whose columns the
/// catalog always answers, so the statement always respells by.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches the statement's write target, the first factor, whose alias
/// PostgreSQL accepts with no column list.
fn shield_shell(
    statement: &mut Statement,
    before: &ParserDB,
    renames: &[Rename],
    function_name: &str,
) -> Result<(), Error> {
    let factors = shell_factors(statement, before);
    let mut edits = ShellEdits::default();
    for (position, factor) in factors.iter().enumerate() {
        let key = &factor.key;
        for rename in renames.iter().filter(|rename| rename.key() == key) {
            match rename {
                Rename::Relation { to, .. } => {
                    if !factor.has_alias {
                        edits.factor_names.insert(factor.address, respelled_factor_name(to));
                        edits.aliases.insert(
                            factor.address,
                            TableAlias {
                                explicit: true,
                                name: factor.exposed.clone(),
                                columns: Vec::new(),
                                at: None,
                            },
                        );
                    }
                }
                Rename::Column { ordinal, columns, .. } => {
                    if position == 0 {
                        return Err(Error::FunctionBodyBlocksColumnRename {
                            column_name: columns[*ordinal].value.clone(),
                            relation_name: key.name.clone(),
                            function_name: function_name.to_string(),
                        });
                    }
                    if factor.alias.as_ref().is_some_and(|alias| alias.columns.len() > *ordinal) {
                        continue;
                    }
                    edits.aliases.insert(
                        factor.address,
                        restoring_alias(factor.alias.as_ref(), factor.exposed.clone(), columns),
                    );
                }
            }
        }
    }
    let walked = VisitMut::visit(statement, &mut edits);
    debug_assert!(walked.is_continue(), "the edits never break");
    Ok(())
}

/// Renames the columns a write target lists, which name the target's own
/// columns rather than any alias's outputs.
fn rename_lists(statement: &mut Statement, factors: &[ShellFactor], renames: &[Rename]) {
    let Some(target) = factors.first() else {
        return;
    };
    let key = &target.key;
    let mut entries: Vec<&mut ObjectName> = Vec::new();
    match statement {
        Statement::Insert(Insert { columns, .. }) => entries.extend(columns.iter_mut()),
        Statement::Update(Update { assignments, .. }) => {
            for assignment in assignments {
                match &mut assignment.target {
                    AssignmentTarget::ColumnName(name) => entries.push(name),
                    // A tuple target lists the target's columns by name.
                    AssignmentTarget::Tuple(names) => entries.extend(names.iter_mut()),
                }
            }
        }
        Statement::Merge(Merge { clauses, .. }) => {
            for clause in clauses {
                match &mut clause.action {
                    MergeAction::Insert(insert) => entries.extend(insert.columns.iter_mut()),
                    MergeAction::Update(update) => {
                        match &mut update.kind {
                            MergeUpdateKind::Set(assignments) => {
                                for assignment in assignments {
                                    match &mut assignment.target {
                                        AssignmentTarget::ColumnName(name) => entries.push(name),
                                        AssignmentTarget::Tuple(names) => {
                                            entries.extend(names.iter_mut());
                                        }
                                    }
                                }
                            }
                            // A wildcard update lists no column to follow.
                            MergeUpdateKind::Wildcard => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    // An entry's first part names the column, and later parts name fields of
    // a composite column, which a column rename leaves alone.
    for part in entries.into_iter().filter_map(|entry| entry.0.first_mut()) {
        for rename in renames.iter().filter(|rename| rename.key() == key) {
            let Rename::Column { ordinal, to, columns, .. } = rename else {
                continue;
            };
            // `ordinal` is the position the renamed column held among
            // `columns`, so it is in range.
            let old = &columns[*ordinal];
            let matches_old = part.as_ident().is_some_and(|written| {
                identifiers_match(
                    &old.value,
                    old.quote_style.is_some(),
                    &written.value,
                    written.quote_style.is_some(),
                )
            });
            if matches_old {
                *part = ObjectNamePart::Identifier(to.clone());
            }
        }
    }
}

/// The edits respelling a statement's own clauses, keyed by the address of
/// what they change.
#[derive(Default)]
struct ShellEdits {
    /// A write target's or column list entry's new name.
    names: BTreeMap<usize, ObjectName>,
    /// A clause factor's new relation name.
    factor_names: BTreeMap<usize, ObjectName>,
    /// A clause factor's new alias.
    aliases: BTreeMap<usize, TableAlias>,
    /// A qualified reference's new column identifier.
    columns: BTreeMap<usize, Ident>,
    /// A qualified reference's new relation identifier.
    qualifiers: BTreeMap<usize, Ident>,
    /// A bare reference's new compound form, which a renamed column of an
    /// unaliased or aliased factor calls for.
    qualified: BTreeMap<usize, (Ident, Ident)>,
}

impl VisitorMut for ShellEdits {
    type Break = ();

    fn pre_visit_relation(&mut self, name: &mut ObjectName) -> ControlFlow<Self::Break> {
        if let Some(to) = self.names.get(&address(&*name)) {
            *name = to.clone();
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        let at = address(&*factor);
        if let TableFactor::Table { name, alias, .. } = factor {
            if let Some(to) = self.factor_names.get(&at) {
                // A new name written with its schema moves the relation, so it
                // replaces the qualifier too.
                let kept = if to.0.len() > 1 { 0 } else { name.0.len().saturating_sub(1) };
                name.0.truncate(kept);
                name.0.extend(to.0.iter().cloned());
            }
            if let Some(replacement) = self.aliases.get(&at) {
                *alias = Some(replacement.clone());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        let at = address(&*expression);
        if let Some((qualifier, column)) = self.qualified.get(&at)
            && let Expr::Identifier(_) = expression
        {
            *expression = Expr::CompoundIdentifier(alloc::vec![qualifier.clone(), column.clone()]);
            return ControlFlow::Continue(());
        }
        // A reference the columns edit rewrites wrote its column qualified.
        if let Some(to) = self.columns.get(&at)
            && let Expr::CompoundIdentifier(parts) = expression
            && let [.., last] = parts.as_mut_slice()
        {
            *last = to.clone();
        }
        if let Some(to) = self.qualifiers.get(&at)
            && let Expr::CompoundIdentifier(parts) = expression
            && let Some(relation) = parts.len().checked_sub(2).and_then(|at| parts.get_mut(at))
        {
            *relation = to.clone();
        }
        ControlFlow::Continue(())
    }
}
