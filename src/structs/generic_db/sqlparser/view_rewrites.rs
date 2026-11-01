//! Keeping stored view and function body definitions reading what they read,
//! and spelling out the wildcards and `NATURAL` joins a view's creation fixes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    AssignmentTarget, ConflictTarget, CreateFunction, CreateFunctionBody, Delete, DoUpdate, Expr,
    FromTable, Ident, Insert, JoinConstraint, JoinOperator, Merge, MergeAction, MergeUpdateExpr,
    MergeUpdateKind, ObjectName, ObjectNamePart, OnConflict, OnConflictAction, OnInsert,
    OutputClause, Query, ReturnStatement, ReturnStatementValue, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Statement, TableAlias, TableAliasColumnDef,
    TableAliasWithoutColumns, TableFactor, TableObject, TableWithJoins, Update,
    UpdateTableFromKind, Visit, VisitMut, Visitor, VisitorMut,
    helpers::attached_token::AttachedToken,
};

use super::{
    BoundRelation, ParserDBBuilder, function_bodies,
    views::{relations_read_in, statements_read},
};
use crate::{
    errors::Error,
    impls::dql::{
        definition_graph::reference_sites::{QueryReferences, SiteOutcome, query_references},
        join_constraint, join_operator_constraint, query_output_names, wildcard_reshapes_output,
    },
    structs::{IdentifierCase, ParserDB, TableAttribute},
    traits::{ColumnLike, DatabaseLike, TableLike, ViewLike},
    utils::{
        identifier_resolution::{identifiers_match, is_temporary_schema},
        last_str,
        object_name::{
            RelationKey, TEMPORARY_SCHEMA, overqualified_object_name, relation_name_is_claimed,
            resolve_view_on_search_path_in_iter, stored_table_key, stored_view_key,
            table_matches_key, target_key, target_name_from_object_name,
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
            edit_function(builder, position, |function| {
                function_bodies::write_definition(&mut function.function_body, query);
            });
        }
    }
}

/// Applies `edit` to the function stored at `position`, re-pointing every
/// check constraint and policy holding the node at its edited copy.
fn edit_function(
    builder: &mut ParserDBBuilder,
    position: usize,
    edit: impl FnOnce(&mut CreateFunction),
) {
    let slot = &mut builder.functions_mut()[position].0;
    // An unshared node is edited in place, with nothing holding it to update.
    let stale = (alloc::sync::Arc::strong_count(slot) > 1).then(|| alloc::sync::Arc::clone(slot));
    edit(alloc::sync::Arc::make_mut(slot));
    let fresh = alloc::sync::Arc::clone(slot);
    if let Some(stale) = stale {
        builder.repoint_function(&stale, &fresh);
    }
}

/// Every stored definition whose body names one of `keys`, with the union
/// of every relation those definitions read.
fn definitions_reading(
    builder: &ParserDBBuilder,
    keys: &[&RelationKey],
) -> (Vec<Definition>, Vec<RelationKey>) {
    let names = |reads: &[RelationKey]| reads.iter().any(|key| keys.contains(&key));
    let mut definitions = Vec::new();
    let mut reads: Vec<RelationKey> = Vec::new();
    for (position, (view, _, _)) in builder.views().iter().enumerate() {
        let definition = view.definition();
        let read = relations_read_in(definition);
        if names(&read) {
            definitions.push(Definition::Query {
                slot: Slot::Plain(position),
                query: alloc::boxed::Box::new(definition.clone()),
            });
            reads.extend(read);
        }
    }
    for (position, (view, _, _)) in builder.materialized_views().iter().enumerate() {
        let definition = view.definition();
        let read = relations_read_in(definition);
        if names(&read) {
            definitions.push(Definition::Query {
                slot: Slot::Materialized(position),
                query: alloc::boxed::Box::new(definition.clone()),
            });
            reads.extend(read);
        }
    }
    for (position, (function, _)) in builder.functions().iter().enumerate() {
        let Some((definition, read)) =
            function_definition_reads(builder, position, function.function_body.as_ref())
        else {
            continue;
        };
        if names(&read) {
            definitions.push(definition);
            reads.extend(read);
        }
    }
    (definitions, reads)
}

/// The stored form of one function's body with every relation it reads,
/// or [`None`] for a body PostgreSQL does not bind.
fn function_definition_reads(
    builder: &ParserDBBuilder,
    position: usize,
    body: Option<&CreateFunctionBody>,
) -> Option<(Definition, Vec<RelationKey>)> {
    match body {
        Some(CreateFunctionBody::Return(expr)) => {
            let query = alloc::boxed::Box::new(function_bodies::definition(expr));
            let read = relations_read_in(&query);
            Some((Definition::Query { slot: Slot::Function(position), query }, read))
        }
        Some(CreateFunctionBody::BeginAtomic(block)) => {
            Some((
                Definition::Statements { function: position, statements: block.statements.clone() },
                statements_read(builder, &block.statements),
            ))
        }
        _ => None,
    }
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
    let (definitions, reads) = definitions_reading(builder, keys);
    if definitions.is_empty() {
        return None;
    }
    Some(Readers { definitions, before: catalog_reads(builder, reads) })
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
                edit_function(builder, function, |entry| {
                    function_bodies::write_statements(&mut entry.function_body, rewritten);
                });
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
        let fallback = {
            let mut fallback = shielded(&part.original, renames);
            // The original holds unprocessed data-modifying bodies, which the
            // shield left as written, so the copy shields takes them through
            // their own clauses.
            if let Slot::Function(function) = part.slot {
                let function_name = last_str(&builder.functions()[function].0.name).to_string();
                dml_shell(&mut fallback, &before, renames, &function_name)?;
            }
            fallback
        };
        store_part(builder, &part, if settled { qualified } else { fallback });
    }
    Ok(())
}

/// Writes a checked query part back into the definition that holds it.
fn store_part(builder: &mut ParserDBBuilder, part: &Part, query: Query) {
    let (Slot::Function(position), Some((statement, shape))) = (part.slot, part.statement) else {
        return store_query(builder, part.slot, query);
    };
    edit_function(builder, position, |function| {
        if let Some(CreateFunctionBody::BeginAtomic(block)) = &mut function.function_body {
            block.statements[statement] = statement_with_query_part(shape, query);
        }
    });
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
        /// The data-modifying statements the walk sits in, whose factors the
        /// query path never reads.
        dml_depth: usize,
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

        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            self.dml_depth += usize::from(statement_dml(statement).is_some());
            ControlFlow::Continue(())
        }

        fn post_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            self.dml_depth -= usize::from(statement_dml(statement).is_some());
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            if self.dml_depth == 0
                && let TableFactor::Table { name, args: None, .. } = factor
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

    let mut finder =
        Finder { renames, scopes: WithScopes::default(), dml_depth: 0, found: BTreeMap::new() };
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

/// The new relation names and aliases `FROM` items take, keyed by the
/// factor's address.
#[derive(Default)]
struct FactorEdits {
    /// A factor's new relation name, the schema part included when the
    /// relation moved between schemas.
    names: BTreeMap<usize, ObjectName>,
    /// A factor's new alias.
    aliases: BTreeMap<usize, TableAlias>,
}

impl FactorEdits {
    /// Rewrites one factor, a schema-carrying new name replacing the
    /// qualifier too.
    fn apply(&self, factor: &mut TableFactor) {
        let at = address(&*factor);
        if let TableFactor::Table { name, alias, .. } = factor {
            if let Some(to) = self.names.get(&at) {
                let kept = if to.0.len() > 1 { 0 } else { name.0.len().saturating_sub(1) };
                name.0.truncate(kept);
                name.0.extend(to.0.iter().cloned());
            }
            if let Some(replacement) = self.aliases.get(&at) {
                *alias = Some(replacement.clone());
            }
        }
    }
}

/// The relation part of a qualified reference, which a reference of fewer
/// than two parts lacks.
fn relation_part(parts: &mut [Ident]) -> Option<&mut Ident> {
    parts.len().checked_sub(2).and_then(|at| parts.get_mut(at))
}

/// The edits spelling a definition the way PostgreSQL spells it after the
/// renames, keyed by the address of what they change.
#[derive(Default)]
struct Edits {
    /// A column reference's new last identifier.
    columns: BTreeMap<usize, Ident>,
    /// A qualified reference's new relation identifier.
    qualifiers: BTreeMap<usize, Ident>,
    /// The `FROM` items' new names and aliases.
    factors: FactorEdits,
    /// A select-list item's output name, which a renamed column reference
    /// written alone would otherwise change.
    outputs: BTreeMap<usize, Ident>,
}

impl VisitorMut for Edits {
    type Break = ();

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        self.factors.apply(factor);
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
            && let Some(relation) = relation_part(parts)
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
        /// The data-modifying statements the walk sits in, whose factors the
        /// query path never reads.
        dml_depth: usize,
        found: BTreeMap<usize, FactorShape>,
    }

    impl Visitor for Shapes {
        type Break = ();

        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            self.dml_depth += usize::from(statement_dml(statement).is_some());
            ControlFlow::Continue(())
        }

        fn post_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            self.dml_depth -= usize::from(statement_dml(statement).is_some());
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            if self.dml_depth == 0 {
                let shape = match factor {
                    TableFactor::Table { name, alias, .. } => {
                        FactorShape {
                            relation: Some(last_part_as_ident(name)),
                            alias: alias.clone(),
                        }
                    }
                    TableFactor::Derived { alias, .. } => {
                        FactorShape { relation: None, alias: alias.clone() }
                    }
                    _ => FactorShape { relation: None, alias: None },
                };
                self.found.insert(address(factor), shape);
            }
            ControlFlow::Continue(())
        }
    }

    let mut shapes = Shapes { dml_depth: 0, found: BTreeMap::new() };
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
                edits.factors.names.insert(*factor, to.clone());
            }
            Rename::Column { columns, .. } if through_using.contains(factor) => {
                let exposed = shape.exposed()?.clone();
                edits
                    .factors
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
                edits.factors.names.insert(*factor, to.clone());
                if shape.alias.is_none() {
                    edits.factors.aliases.insert(
                        *factor,
                        TableAlias { explicit: true, name: exposed, columns: Vec::new(), at: None },
                    );
                }
            }
            Rename::Column { ordinal, columns, .. } => {
                if !shape.renames_column(*ordinal) {
                    edits
                        .factors
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

/// Respells the data-modifying statements a query holds, each through its
/// own clauses under the `WITH` items it sees, and shields one the
/// respelling cannot take on.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches a statement's write target through a whole-row reference the
/// statement's `RETURNING` list holds.
fn dml_shell(
    query: &mut Query,
    before: &ParserDB,
    renames: &[Rename],
    function_name: &str,
) -> Result<(), Error> {
    struct Shells<'a> {
        before: &'a ParserDB,
        renames: &'a [Rename],
        function_name: &'a str,
        scopes: CteScopes,
    }

    impl VisitorMut for Shells<'_> {
        type Break = Error;

        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
            self.scopes.enter_query(query, Some(self.before));
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.scopes.leave_query();
            ControlFlow::Continue(())
        }

        fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<Self::Break> {
            if statement_dml(statement).is_some()
                && let Err(error) = self.shell_dml(statement)
            {
                return ControlFlow::Break(error);
            }
            ControlFlow::Continue(())
        }
    }

    impl Shells<'_> {
        /// Respells a data-modifying statement's own clauses, shielding them
        /// when the respelling reaches a reference it cannot follow.
        ///
        /// # Errors
        ///
        /// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column
        /// rename reaches the statement's write target.
        fn shell_dml(&mut self, statement: &mut Statement) -> Result<(), Error> {
            let ctes = self.scopes.visible();
            let respelled =
                respell_shell(statement, &ctes, self.before, self.renames, self.function_name)?;
            if respelled.is_none() {
                shield_shell(statement, &ctes, self.before, self.renames, self.function_name)?;
            }
            Ok(())
        }
    }

    let mut shells = Shells { before, renames, function_name, scopes: CteScopes::default() };
    match query.visit(&mut shells) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

/// Respells a block's statements the way PostgreSQL respells them after the
/// renames: query statements through the query path, and `INSERT`, `UPDATE`,
/// `DELETE` and `MERGE` through their own clauses, under a `WITH` clause as
/// well. Answers the rewritten statements and the query parts among them,
/// each with what its references reached before the renames.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches a statement's write target through a whole-row reference the
/// statement's `RETURNING` list holds or a statement the model could not
/// bind.
fn respell_statements(
    statements: &[Statement],
    before: &ParserDB,
    renames: &[Rename],
    function_name: &str,
) -> Result<(Vec<Statement>, Vec<StatementPart>), Error> {
    let mut rewritten: Vec<Statement> = Vec::with_capacity(statements.len());
    let mut parts: Vec<StatementPart> = Vec::new();
    for (position, statement) in statements.iter().enumerate() {
        if let Some(query) = part_of(statement)
            && dml_ref(&query.body).is_none()
        {
            let shape = match statement {
                Statement::Query(_) => PartShape::Statement,
                _ => PartShape::Return,
            };
            let original = query.clone();
            let mut respelled = original.clone();
            let mut stored = match respell(&mut respelled, before, renames) {
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
            // The queries the respelling left untouched, which are the ones
            // nested in data-modifying bodies, respell on their own, and the
            // data-modifying statements respell through their own clauses.
            respell_embedded(&mut stored, before, renames, true);
            dml_shell(&mut stored, before, renames, function_name)?;
            rewritten.push(statement_with_query_part(shape, stored));
            continue;
        }
        let mut statement = statement.clone();
        // Every statement's own clauses respell or shield the same way, which
        // a statement without clause factors answers with no edits at all.
        respell_embedded(&mut statement, before, renames, false);
        if let Statement::Query(query) = &mut statement
            && dml_ref(&query.body).is_some()
        {
            dml_shell(query, before, renames, function_name)?;
        } else if respell_shell(&mut statement, &[], before, renames, function_name)?.is_none() {
            shield_shell(&mut statement, &[], before, renames, function_name)?;
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
    struct NormalizeDmls<'a> {
        builder: &'a ParserDBBuilder,
        catalog: &'a str,
        scopes: CteScopes,
    }

    impl VisitorMut for NormalizeDmls<'_> {
        type Break = Error;

        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
            self.scopes.enter_query(query, None);
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
            self.scopes.leave_query();
            ControlFlow::Continue(())
        }

        fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<Self::Break> {
            if let Some(mut dml) = statement_dml_mut(statement) {
                let ctes = self.scopes.visible();
                if let Err(error) = strip_catalog_qualifier(&mut dml, self.catalog) {
                    return ControlFlow::Break(error);
                }
                let names = own_clause_factor_names(&dml);
                if let Err(error) = normalize_statement_references(
                    self.builder,
                    statement,
                    &names,
                    &ctes,
                    self.catalog,
                ) {
                    return ControlFlow::Break(error);
                }
            }
            ControlFlow::Continue(())
        }
    }

    let catalog = builder.catalog_name();
    let mut normalizer = NormalizeDmls { builder, catalog, scopes: CteScopes::default() };
    for statement in statements.iter_mut() {
        match statement.visit(&mut normalizer) {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(error) => return Err(error),
        }
    }
    Ok(())
}

/// The catalog qualifier a factor name spells, dropped when it names this
/// catalog, the way PostgreSQL drops it before binding the name.
fn strip_catalog_qualifier(dml: &mut DmlMut<'_>, catalog: &str) -> Result<(), Error> {
    let strip_factor = |factor: &mut TableFactor| -> Result<(), Error> {
        if let Some(name) = factor_name(factor) {
            strip_factor_name(name, catalog)?;
        }
        Ok(())
    };
    if let Some(name) = dml.insert_target_mut() {
        strip_factor_name(name, catalog)?;
    }
    dml.for_each_clause_factor(|factor| strip_factor(factor))?;
    Ok(())
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
    names: &[ObjectName],
    ctes: &[ScopeFactor],
    catalog: &str,
) -> Result<(), Error> {
    let keys = names
        .iter()
        .filter_map(|name| builder.bound_relation(name).map(|bound| bound_factor_key(&bound, name)))
        .collect::<Vec<_>>();
    let slice = catalog_reads(builder, keys);
    let factors = shell_factors(statement, ctes, &slice);
    let mut rewrites = BTreeMap::new();
    for (at, parts) in clause_references(statement) {
        let parts = keep_reference_parts(parts, catalog)?;
        // A reference a `WITH` item's name would have to reach names no
        // schema of a relation, so it is refused with the others.
        if let [schema, table, column] = parts.as_slice()
            && let Some(factor) = factors.iter().find(|factor| {
                !factor.has_alias && factor.key.is_some() && same_identifier(&factor.exposed, table)
            })
            && let Some(key) = &factor.key
            && identifiers_match(&key.schema, false, &schema.value, schema.quote_style.is_some())
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
fn own_clause_factor_names(dml: &DmlMut<'_>) -> Vec<ObjectName> {
    let mut names = Vec::new();
    let dml = dml.reference();
    if let Some(name) = dml.insert_target() {
        names.push(name.clone());
    }
    dml.for_each_clause_factor(|factor| push_factor_name(factor, &mut names));
    names
}

/// One clause factor's name, which a table function does not hold.
fn push_factor_name(factor: &TableFactor, names: &mut Vec<ObjectName>) {
    if let TableFactor::Table { name, args: None, .. } = factor {
        names.push(name.clone());
    }
}

/// The key a factor name reaches its relation by, the relation it bound
/// under at ingestion, which is the temporary schema for a temporary
/// relation.
pub(super) fn bound_factor_key(bound: &BoundRelation, name: &ObjectName) -> RelationKey {
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

/// The refusal PostgreSQL's parser gives a clause shape it does not accept
/// in an unquoted SQL function body, which is a join on an `UPDATE` or
/// `DELETE` target, a wildcard `MERGE UPDATE SET`, a `MERGE` `OUTPUT`
/// clause, or a `RETURN` statement that returns a data-modifying subquery.
pub(super) fn shape_of_block_statement(
    statement: &Statement,
    function_name: &str,
) -> Result<(), Error> {
    let refused = match statement {
        Statement::Query(query) => {
            dml_body_has_refused_shape(query.body.as_ref())
                || query.with.as_ref().is_some_and(|with| {
                    with.cte_tables.iter().any(|cte| cte_has_refused_shape(&cte.query))
                })
        }
        Statement::Return(_) => {
            part_of(statement).is_some_and(|query| dml_ref(&query.body).is_some())
        }
        _ => statement_dml(statement).is_some_and(|dml| dml.has_refused_shape()),
    };
    if refused {
        return Err(Error::FunctionBodyStatementUnsupported {
            statement: statement.to_string(),
            function_name: function_name.to_string(),
        });
    }
    Ok(())
}

/// Whether a `DELETE` names a join on its target.
fn dml_target_has_join(delete: &Delete) -> bool {
    delete_from_tables(&delete.from).iter().any(|table| !table.joins.is_empty())
}

/// Whether a `MERGE` writes a clause shape the body's kind cannot hold,
/// which is a wildcard update or an `OUTPUT` clause.
fn merge_has_refused_shape(merge: &Merge) -> bool {
    matches!(merge.output.as_ref(), Some(OutputClause::Output { .. }))
        || merge.clauses.iter().any(|clause| {
            matches!(
                &clause.action,
                MergeAction::Update(update) if matches!(update.kind, MergeUpdateKind::Wildcard)
            )
        })
}

/// Whether a `WITH` item's query holds a clause shape the body's own kind
/// refuses, the items the item's own `WITH` clause writes included.
fn cte_has_refused_shape(query: &Query) -> bool {
    dml_body_has_refused_shape(query.body.as_ref())
        || query
            .with
            .as_ref()
            .is_some_and(|with| with.cte_tables.iter().any(|cte| cte_has_refused_shape(&cte.query)))
}

/// Whether a query body holds a clause shape the body's own kind refuses.
fn dml_body_has_refused_shape(body: &SetExpr) -> bool {
    dml_ref(body).is_some_and(|dml| dml.has_refused_shape())
}

/// A relation a bare reference can reach in one scope, by the name it
/// reaches it under, or `None` when its outputs cannot be named.
#[derive(Clone)]
struct ScopeFactor {
    /// The name the reference reaches the relation by.
    exposed: Ident,
    /// The names its outputs had, in order.
    columns: Option<Vec<(String, bool)>>,
}

/// The refusal PostgreSQL gives a bare column reference that more than one
/// relation in the same scope exposes, which it refuses at `CREATE FUNCTION`
/// time.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyReferenceAmbiguous`] naming the first
/// reference that two relations expose.
pub(super) fn refuse_ambiguous_block_reference(
    builder: &ParserDBBuilder,
    statement: &Statement,
    function_name: &str,
) -> Result<(), Error> {
    struct Scopes<'a> {
        slice: &'a ParserDB,
        scopes: CteScopes,
    }

    impl Visitor for Scopes<'_> {
        type Break = ();

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            self.scopes.enter_query(query, Some(self.slice));
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.scopes.leave_query();
            ControlFlow::Continue(())
        }
    }

    struct Ambiguous<'a> {
        slice: &'a ParserDB,
        scopes: CteScopes,
        chain: Vec<Vec<ScopeFactor>>,
    }

    impl Visitor for Ambiguous<'_> {
        type Break = Ident;

        fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            // A data-modifying statement scopes its own clauses, whose
            // factors a visible `WITH` item answers for by name.
            if let Some(dml) = statement_dml(statement) {
                let ctes = self.scopes.visible();
                self.chain.push(dml.scope_factors(self.slice, &ctes));
            }
            ControlFlow::Continue(())
        }

        fn post_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
            if statement_dml(statement).is_some() {
                self.chain.pop();
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            self.scopes.enter_query(query, Some(self.slice));
            let ctes = self.scopes.visible();
            self.chain.push(select_scope_factors(query, self.slice, &ctes));
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            self.scopes.leave_query();
            self.chain.pop();
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
            if let Expr::Identifier(column) = expression
                && reference_is_ambiguous(column, &self.chain)
            {
                return ControlFlow::Break(column.clone());
            }
            ControlFlow::Continue(())
        }
    }

    let reads = statements_read(builder, core::iter::once(statement));
    let slice = catalog_reads(builder, reads);
    let mut scopes = Scopes { slice: &slice, scopes: CteScopes::default() };
    let walked = statement.visit(&mut scopes);
    debug_assert!(walked.is_continue(), "the walker never breaks");
    let Scopes { scopes, .. } = scopes;
    let mut ambiguous = Ambiguous { slice: &slice, scopes, chain: Vec::new() };
    match statement.visit(&mut ambiguous) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(column) => {
            Err(Error::FunctionBodyReferenceAmbiguous {
                column_name: column.value,
                function_name: function_name.to_string(),
            })
        }
    }
}

/// The names a `WITH` item's outputs had, which a data-modifying item names
/// through its `RETURNING` list.
fn cte_output_columns(query: &Query, slice: &ParserDB) -> Option<Vec<(String, bool)>> {
    if let Some(items) = dml_returning_items(query.body.as_ref()) {
        return items.iter().map(select_item_output_name).collect::<Option<_>>();
    }
    query_output_names(query, slice, CASE).ok().flatten()
}

/// The names a select item's output had, or `None` when the item writes no
/// name.
fn select_item_output_name(item: &SelectItem) -> Option<(String, bool)> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => {
            Some((alias.value.clone(), alias.quote_style.is_some()))
        }
        SelectItem::UnnamedExpr(expression) => simple_reference_name(expression),
        _ => None,
    }
}

/// The name a simple reference writes, which a compound one writes as its
/// last part.
fn simple_reference_name(expression: &Expr) -> Option<(String, bool)> {
    match innermost(expression) {
        Expr::Identifier(name) => Some((name.value.clone(), name.quote_style.is_some())),
        Expr::CompoundIdentifier(parts) => {
            parts.last().map(|part| (part.value.clone(), part.quote_style.is_some()))
        }
        _ => None,
    }
}

/// A data-modifying statement, wherever a statement holds it.
pub(super) enum DmlRef<'a> {
    /// An `INSERT` statement.
    Insert(&'a Insert),
    /// An `UPDATE` statement.
    Update(&'a Update),
    /// A `DELETE` statement.
    Delete(&'a Delete),
    /// A `MERGE` statement.
    Merge(&'a Merge),
}

impl<'a> DmlRef<'a> {
    /// The name an `INSERT` statement writes, which sits outside its queries.
    pub(super) fn insert_target(&self) -> Option<&'a ObjectName> {
        match self {
            DmlRef::Insert(Insert { table: TableObject::TableName(name), .. }) => Some(name),
            _ => None,
        }
    }

    /// Calls `visit` on the statement's clause factors, in the order
    /// PostgreSQL scopes them, which an `INSERT` target is not among.
    fn for_each_clause_factor(&self, mut visit: impl FnMut(&TableFactor)) {
        match self {
            DmlRef::Insert(_) => {}
            DmlRef::Update(Update { table, from, .. }) => {
                visit(&table.relation);
                visit_tables(from.as_ref().map_or(&[], update_from_tables), &mut visit);
            }
            DmlRef::Delete(Delete { from, using, .. }) => {
                visit_tables(delete_from_tables(from), &mut visit);
                visit_tables(using.as_deref().unwrap_or_default(), &mut visit);
            }
            DmlRef::Merge(Merge { table, source, .. }) => {
                visit(table);
                visit(source);
            }
        }
    }

    /// The clause factors the statement scopes, in the order PostgreSQL
    /// scopes them.
    fn scope_factors(&self, slice: &ParserDB, ctes: &[ScopeFactor]) -> Vec<ScopeFactor> {
        let mut factors = Vec::new();
        if let DmlRef::Insert(Insert { table: TableObject::TableName(name), table_alias, .. }) =
            self
        {
            factors.push(ScopeFactor {
                exposed: table_alias
                    .as_ref()
                    .map_or_else(|| last_part_as_ident(name), |alias| alias.alias.clone()),
                columns: factor_key_and_columns(name, ctes, slice).1,
            });
        }
        self.for_each_clause_factor(|factor| {
            factors.extend(table_factors(factor, slice, ctes));
        });
        factors
    }

    /// Whether the statement's clause shape is one the body's kind refuses.
    fn has_refused_shape(&self) -> bool {
        match self {
            DmlRef::Insert(_) => false,
            DmlRef::Update(update) => !update.table.joins.is_empty(),
            DmlRef::Delete(delete) => dml_target_has_join(delete),
            DmlRef::Merge(merge) => merge_has_refused_shape(merge),
        }
    }

    /// The `RETURNING` list the statement writes, if any.
    pub(super) fn returning(&self) -> Option<&'a [SelectItem]> {
        match self {
            DmlRef::Insert(insert) => insert.returning.as_deref(),
            DmlRef::Update(update) => update.returning.as_deref(),
            DmlRef::Delete(delete) => delete.returning.as_deref(),
            DmlRef::Merge(merge) => {
                match merge.output.as_ref() {
                    Some(OutputClause::Returning { select_items, .. }) => {
                        Some(select_items.as_slice())
                    }
                    _ => None,
                }
            }
        }
    }
}

/// A data-modifying statement held for its creation rewrites.
enum DmlMut<'a> {
    /// An `INSERT` statement.
    Insert(&'a mut Insert),
    /// An `UPDATE` statement.
    Update(&'a mut Update),
    /// An `DELETE` statement.
    Delete(&'a mut Delete),
    /// An `MERGE` statement.
    Merge(&'a mut Merge),
}

impl<'a> DmlMut<'a> {
    /// The statement as its read-only view.
    fn reference(&self) -> DmlRef<'_> {
        match self {
            DmlMut::Insert(insert) => DmlRef::Insert(insert),
            DmlMut::Update(update) => DmlRef::Update(update),
            DmlMut::Delete(delete) => DmlRef::Delete(delete),
            DmlMut::Merge(merge) => DmlRef::Merge(merge),
        }
    }

    /// The name an `INSERT` statement writes, held for its rewrites.
    fn insert_target_mut(&mut self) -> Option<&mut ObjectName> {
        match self {
            DmlMut::Insert(Insert { table: TableObject::TableName(name), .. }) => Some(name),
            _ => None,
        }
    }

    /// Calls `visit` on the statement's clause factors, held for their
    /// rewrites, in the order PostgreSQL scopes them, which an `INSERT`
    /// target is not among.
    ///
    /// # Errors
    ///
    /// Returns the first error `visit` gives a factor.
    fn for_each_clause_factor(
        &mut self,
        mut visit: impl FnMut(&mut TableFactor) -> Result<(), Error>,
    ) -> Result<(), Error> {
        match self {
            DmlMut::Insert(_) => Ok(()),
            DmlMut::Update(Update { table, from, .. }) => {
                visit(&mut table.relation)?;
                visit_tables_mut(from.as_mut().map_or(&mut [], update_from_tables_mut), &mut visit)
            }
            DmlMut::Delete(Delete { from, using, .. }) => {
                visit_tables_mut(delete_from_tables_mut(from), &mut visit)?;
                visit_tables_mut(using.as_deref_mut().unwrap_or_default(), &mut visit)
            }
            DmlMut::Merge(Merge { table, source, .. }) => {
                visit(table)?;
                visit(source)
            }
        }
    }

    /// The clause factors the statement scopes, in the order PostgreSQL
    /// scopes them.
    fn scope_factors(&self, slice: &ParserDB, ctes: &[ScopeFactor]) -> Vec<ScopeFactor> {
        self.reference().scope_factors(slice, ctes)
    }

    /// The rewrites the statement takes at creation, which spell its
    /// `RETURNING` wildcards and write the column list an `INSERT` without
    /// one holds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::FunctionBodyInsertTooWide`] when the `INSERT` offers
    /// more expressions than its target has columns.
    fn spell(self, slice: &ParserDB, function_name: &str) -> Result<(), Error> {
        let factors = self.scope_factors(slice, &[]);
        match self {
            DmlMut::Insert(insert) => {
                let wildcard: &[ScopeFactor] = factors.first().map_or(&[], alloc::slice::from_ref);
                spell_returning_items(insert.returning.as_mut(), wildcard, &factors);
                spell_insert_columns(insert, slice, function_name)
            }
            DmlMut::Update(update) => {
                spell_returning_items(update.returning.as_mut(), &factors[..], &factors);
                Ok(())
            }
            DmlMut::Delete(delete) => {
                spell_returning_items(delete.returning.as_mut(), &factors[..], &factors);
                Ok(())
            }
            DmlMut::Merge(merge) => {
                let factors = factors.into_iter().rev().collect::<Vec<_>>();
                if let Some(OutputClause::Returning { select_items, .. }) = merge.output.as_mut() {
                    spell_returning_items(Some(select_items), &factors[..], &factors);
                }
                Ok(())
            }
        }
    }

    /// The `RETURNING` list the statement writes, held for its rewrites.
    fn returning_mut(self) -> Option<&'a mut [SelectItem]> {
        match self {
            DmlMut::Insert(insert) => insert.returning.as_deref_mut(),
            DmlMut::Update(update) => update.returning.as_deref_mut(),
            DmlMut::Delete(delete) => delete.returning.as_deref_mut(),
            DmlMut::Merge(merge) => {
                match merge.output.as_mut() {
                    Some(OutputClause::Returning { select_items, .. }) => {
                        Some(select_items.as_mut_slice())
                    }
                    _ => None,
                }
            }
        }
    }
}

/// The tables a `DELETE`'s `FROM` clause writes, whichever keyword spells it.
fn delete_from_tables(from: &FromTable) -> &[TableWithJoins] {
    match from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
    }
}

/// The tables a `DELETE`'s `FROM` clause writes, held for their rewrites.
fn delete_from_tables_mut(from: &mut FromTable) -> &mut [TableWithJoins] {
    match from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
    }
}

/// The tables an `UPDATE`'s `FROM` clause writes, whichever position holds.
fn update_from_tables(from: &UpdateTableFromKind) -> &[TableWithJoins] {
    match from {
        UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => tables,
    }
}

/// The tables an `UPDATE`'s `FROM` clause writes, held for their rewrites.
fn update_from_tables_mut(from: &mut UpdateTableFromKind) -> &mut [TableWithJoins] {
    match from {
        UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => tables,
    }
}

/// The data-modifying statement a query body holds, if any.
pub(super) fn dml_ref(body: &SetExpr) -> Option<DmlRef<'_>> {
    match body {
        SetExpr::Insert(statement)
        | SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Merge(statement) => statement_dml(statement),
        _ => None,
    }
}

/// The data-modifying statement a query body holds, held for its rewrites.
fn dml_ref_mut(body: &mut SetExpr) -> Option<DmlMut<'_>> {
    match body {
        SetExpr::Insert(statement)
        | SetExpr::Update(statement)
        | SetExpr::Delete(statement)
        | SetExpr::Merge(statement) => statement_dml_mut(statement),
        _ => None,
    }
}

/// The `RETURNING` list a data-modifying statement writes, if any.
fn dml_returning_items(body: &SetExpr) -> Option<&[SelectItem]> {
    dml_ref(body).and_then(|dml| dml.returning())
}

/// The data-modifying statement one statement holds, if any.
pub(super) fn statement_dml(statement: &Statement) -> Option<DmlRef<'_>> {
    match statement {
        Statement::Insert(insert) => Some(DmlRef::Insert(insert)),
        Statement::Update(update) => Some(DmlRef::Update(update)),
        Statement::Delete(delete) => Some(DmlRef::Delete(delete)),
        Statement::Merge(merge) => Some(DmlRef::Merge(merge)),
        _ => None,
    }
}

/// The data-modifying statement one statement holds, held for its rewrites.
fn statement_dml_mut(statement: &mut Statement) -> Option<DmlMut<'_>> {
    match statement {
        Statement::Insert(insert) => Some(DmlMut::Insert(insert)),
        Statement::Update(update) => Some(DmlMut::Update(update)),
        Statement::Delete(delete) => Some(DmlMut::Delete(delete)),
        Statement::Merge(merge) => Some(DmlMut::Merge(merge)),
        _ => None,
    }
}

/// The factors a query's own level scopes, which is its `FROM` items.
fn select_scope_factors(query: &Query, slice: &ParserDB, ctes: &[ScopeFactor]) -> Vec<ScopeFactor> {
    let mut factors = Vec::new();
    if let SetExpr::Select(select) = query.body.as_ref() {
        visit_tables(&select.from, &mut |factor| {
            factors.extend(table_factors(factor, slice, ctes));
        });
    }
    factors
}

/// Calls `visit` on each table's relation and then on its joins' relations.
fn visit_tables(tables: &[TableWithJoins], visit: &mut impl FnMut(&TableFactor)) {
    for table in tables {
        visit(&table.relation);
        for join in &table.joins {
            visit(&join.relation);
        }
    }
}

/// Calls `visit` on each table's relation and then on its joins' relations,
/// for their rewrites, stopping at the first error.
///
/// # Errors
///
/// Returns the first error `visit` gives a factor.
fn visit_tables_mut(
    tables: &mut [TableWithJoins],
    visit: &mut impl FnMut(&mut TableFactor) -> Result<(), Error>,
) -> Result<(), Error> {
    for table in tables {
        visit(&mut table.relation)?;
        for join in &mut table.joins {
            visit(&mut join.relation)?;
        }
    }
    Ok(())
}

/// The factors one `FROM` item contributes, which a nameless or unnameable
/// one contributes none.
fn table_factors(factor: &TableFactor, slice: &ParserDB, ctes: &[ScopeFactor]) -> Vec<ScopeFactor> {
    match factor {
        TableFactor::NestedJoin { table_with_joins, .. } => {
            let mut factors = Vec::new();
            factors.extend(table_factors(&table_with_joins.relation, slice, ctes));
            for join in &table_with_joins.joins {
                factors.extend(table_factors(&join.relation, slice, ctes));
            }
            factors
        }
        TableFactor::Table { name, alias, args: None, .. } => {
            alloc::vec![ScopeFactor {
                exposed: alias
                    .as_ref()
                    .map_or_else(|| last_part_as_ident(name), |alias| alias.name.clone()),
                columns: factor_key_and_columns(name, ctes, slice).1,
            }]
        }
        TableFactor::Derived { subquery, alias: Some(alias), .. } => {
            alloc::vec![ScopeFactor {
                exposed: alias.name.clone(),
                columns: query_output_names(subquery, slice, CASE).ok().flatten(),
            }]
        }
        _ => Vec::new(),
    }
}

/// Whether a factor's name is the one a visible `WITH` item answers.
fn factor_name_answers_cte(name: &ObjectName, cte: &ScopeFactor) -> bool {
    let target = target_name_of_object_name(name);
    identifiers_match(
        &cte.exposed.value,
        cte.exposed.quote_style.is_some(),
        target.name(),
        target.name_is_quoted(),
    )
}

/// The queries a statement walk sits in, with the `WITH` items each of
/// them scopes.
#[derive(Default)]
struct CteScopes {
    /// The addresses of the queries the walk sits in, innermost last.
    queries: Vec<usize>,
    /// A `WITH` item's query by its own address, naming the query that
    /// holds the item.
    cte_owners: BTreeMap<usize, usize>,
    /// A query's own `WITH` items by their address, in the order the
    /// query's `WITH` clause writes them.
    query_ctes: BTreeMap<usize, Vec<(usize, ScopeFactor)>>,
}

impl CteScopes {
    /// The query the walk enters, which its own `WITH` items join the
    /// scope, their outputs named when `slice` can name them.
    fn enter_query(&mut self, query: &Query, slice: Option<&ParserDB>) {
        if let Some(with) = &query.with {
            let factors = with
                .cte_tables
                .iter()
                .map(|cte| {
                    (
                        address(&*cte.query),
                        ScopeFactor {
                            exposed: cte.alias.name.clone(),
                            columns: slice.and_then(|slice| cte_output_columns(&cte.query, slice)),
                        },
                    )
                })
                .collect();
            for cte in &with.cte_tables {
                self.cte_owners.insert(address(&*cte.query), address(query));
            }
            self.query_ctes.insert(address(query), factors);
        }
        self.queries.push(address(query));
    }

    /// The query the walk leaves.
    fn leave_query(&mut self) {
        self.queries.pop();
    }

    /// The `WITH` items visible from the query the walk sits in.
    fn visible(&self) -> Vec<ScopeFactor> {
        self.queries.last().copied().map_or_else(Vec::new, |own| {
            visible_ctes(&self.queries, &self.cte_owners, &self.query_ctes, own)
        })
    }
}

/// The `WITH` items visible from one query's own level, which the items of
/// every query it sits in join, minus the item a `WITH` definition writes
/// from inside itself.
fn visible_ctes(
    queries: &[usize],
    cte_owners: &BTreeMap<usize, usize>,
    query_ctes: &BTreeMap<usize, Vec<(usize, ScopeFactor)>>,
    own: usize,
) -> Vec<ScopeFactor> {
    let mut ctes = Vec::new();
    for query in queries {
        if let Some(items) = query_ctes.get(query) {
            ctes.extend(
                items
                    .iter()
                    .filter(|(cte, _)| {
                        *query == own
                            || cte_owners.get(&own).is_none_or(|owner| *owner != *query)
                            || *cte != own
                    })
                    .map(|(_, cte)| cte.clone()),
            );
        }
    }
    ctes
}

/// Whether a bare reference is ambiguous in the scope chain, walked from the
/// innermost scope outward until a scope answers the name or holds a
/// relation whose columns are unknown.
fn reference_is_ambiguous(column: &Ident, chain: &[Vec<ScopeFactor>]) -> bool {
    for scope in chain.iter().rev() {
        let exposing = scope
            .iter()
            .filter_map(|factor| factor.columns.as_ref())
            .filter(|columns| {
                columns.iter().any(|(name, quoted)| {
                    identifiers_match(&column.value, column.quote_style.is_some(), name, *quoted)
                })
            })
            .count();
        if exposing > 0 || scope.iter().any(|factor| factor.columns.is_none()) {
            return exposing >= 2;
        }
    }
    false
}

/// Spells a block statement the way PostgreSQL spells it at creation, so a
/// later rename follows written names. That covers the wildcards of its query
/// parts, the `RETURNING` wildcards of its data-modifying statements, and the
/// column list an `INSERT` without one holds.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyInsertTooWide`] when an `INSERT` offers more
/// expressions than its target has columns.
pub(super) fn spell_block_statement(
    builder: &ParserDBBuilder,
    statement: &mut Statement,
    function_name: &str,
) -> Result<(), Error> {
    struct Wildcards<'a> {
        builder: &'a ParserDBBuilder,
    }

    impl VisitorMut for Wildcards<'_> {
        type Break = ();

        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
            spell_out(self.builder, query);
            ControlFlow::Continue(())
        }
    }

    let mut wildcards = Wildcards { builder };
    let walked = statement.visit(&mut wildcards);
    debug_assert!(walked.is_continue(), "the speller never breaks");

    let reads = statements_read(builder, core::iter::once(&*statement));
    let slice = catalog_reads(builder, reads);
    let mut returning = ReturningSpeller { slice: &slice, function_name };
    match statement_dml_mut(statement) {
        Some(dml) => dml.spell(&slice, function_name),
        None => {
            match statement.visit(&mut returning) {
                ControlFlow::Continue(()) => Ok(()),
                ControlFlow::Break(error) => Err(error),
            }
        }
    }
}

/// The `RETURNING` wildcards and the missing `INSERT` column list one
/// data-modifying statement spells at creation.
struct ReturningSpeller<'a> {
    slice: &'a ParserDB,
    function_name: &'a str,
}

impl VisitorMut for ReturningSpeller<'_> {
    type Break = Error;

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        if let Some(dml) = dml_ref_mut(&mut query.body)
            && let Err(error) = dml.spell(self.slice, self.function_name)
        {
            return ControlFlow::Break(error);
        }
        ControlFlow::Continue(())
    }
}

/// Spells a `RETURNING` list's wildcards as the qualified columns they
/// reach, which a wildcard whose columns cannot all be named keeps as
/// written.
fn spell_returning_items(
    returning: Option<&mut Vec<SelectItem>>,
    wildcard: &[ScopeFactor],
    factors: &[ScopeFactor],
) {
    let Some(returning) = returning else {
        return;
    };
    let written = core::mem::take(returning);
    let items: Vec<SelectItem> = written
        .into_iter()
        .flat_map(|item| spell_returning_item(item, wildcard, factors))
        .collect();
    *returning = items;
}

/// The items one `RETURNING` item spells, which a wildcard the factors
/// cannot all name keeps as written.
fn spell_returning_item(
    item: SelectItem,
    wildcard: &[ScopeFactor],
    factors: &[ScopeFactor],
) -> Vec<SelectItem> {
    let factor = match &item {
        SelectItem::Wildcard(options) if !wildcard_reshapes_output(options) => {
            return match wildcard_items(wildcard) {
                Some(items) if !items.is_empty() => items,
                _ => alloc::vec![item],
            };
        }
        SelectItem::QualifiedWildcard(
            SelectItemQualifiedWildcardKind::ObjectName(name),
            options,
        ) if !wildcard_reshapes_output(options) => {
            name.0.last().and_then(|part| part.as_ident()).and_then(|exposed| {
                factors.iter().find(|factor| {
                    identifiers_match(
                        &exposed.value,
                        exposed.quote_style.is_some(),
                        &factor.exposed.value,
                        factor.exposed.quote_style.is_some(),
                    )
                })
            })
        }
        _ => return alloc::vec![item],
    };
    match factor.and_then(qualified_returning_items) {
        Some(items) => items,
        None => alloc::vec![item],
    }
}

/// The items a `RETURNING` wildcard stands for, which a wildcard the factors
/// cannot all name keeps as written.
fn wildcard_items(factors: &[ScopeFactor]) -> Option<Vec<SelectItem>> {
    let mut items = Vec::new();
    for factor in factors {
        items.extend(qualified_returning_items(factor)?);
    }
    Some(items)
}

/// The qualified items a `RETURNING` wildcard over one factor stands for, or
/// [`None`] when the factor's columns cannot all be named.
fn qualified_returning_items(factor: &ScopeFactor) -> Option<Vec<SelectItem>> {
    let columns = factor.columns.as_ref()?;
    Some(
        columns
            .iter()
            .map(|(name, quoted)| {
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(alloc::vec![
                    factor.exposed.clone(),
                    quoted_identifier(name, *quoted),
                ]))
            })
            .collect(),
    )
}

/// The identifier one name writes, quoted when the name is.
fn quoted_identifier(name: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', name.to_string()) } else { Ident::new(name.to_string()) }
}

/// Writes an `INSERT` without a column list the first columns of its target
/// it writes, which PostgreSQL stores, and refuses an `INSERT` whose row the
/// target cannot hold, the way PostgreSQL refuses it.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyInsertTooWide`] when the `INSERT` offers more
/// expressions than its target has columns.
fn spell_insert_columns(
    insert: &mut Insert,
    slice: &ParserDB,
    function_name: &str,
) -> Result<(), Error> {
    let TableObject::TableName(name) = &insert.table else { return Ok(()) };
    let width = insert_source_width(insert);
    let Some(columns) = relation_columns(slice, &resolved_factor_key(slice, name)) else {
        return Ok(());
    };
    if width.is_some_and(|width| width > columns.len()) {
        return Err(Error::FunctionBodyInsertTooWide { function_name: function_name.to_string() });
    }
    if !insert.columns.is_empty() || insert.by_name {
        return Ok(());
    }
    let Some(width) = width else {
        return Ok(());
    };
    insert.columns = columns
        .into_iter()
        .take(width)
        .map(|(name, quoted)| {
            ObjectName(alloc::vec![ObjectNamePart::Identifier(quoted_identifier(&name, quoted))])
        })
        .collect();
    Ok(())
}

/// The row width an `INSERT` source writes, which its first `VALUES` row,
/// its projection names, or a set operation's first operand writes.
fn insert_source_width(insert: &Insert) -> Option<usize> {
    insert.source.as_ref().and_then(|query| insert_source_width_of(query.body.as_ref()))
}

/// The row width one query body writes, which its first `VALUES` row, its
/// projection names, or a set operation's first operand writes.
fn insert_source_width_of(body: &SetExpr) -> Option<usize> {
    match body {
        SetExpr::Values(values) => values.rows.first().map(|row| row.len()),
        SetExpr::Select(select) => {
            (!select.projection.is_empty()).then_some(select.projection.len())
        }
        SetExpr::SetOperation { left, .. } => insert_source_width_of(left),
        _ => None,
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
/// reaches it by, or a `WITH` item the factor answers for by name.
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
    /// which a rename follows it by, or `None` for a `WITH` item's factor.
    key: Option<RelationKey>,
    /// The address a rename reaches the factor by, which is the clause
    /// factor's or the write target's name's for an `INSERT` target.
    address: usize,
    /// Whether the factor is an `INSERT` write target.
    insert_target: bool,
    /// The relation's column names before the renames, which the respelling
    /// resolves the statement's own references against.
    columns: Option<Vec<(String, bool)>>,
}

impl ShellFactor {
    /// Whether its own alias column list renames output column `ordinal`.
    fn renames_column(&self, ordinal: usize) -> bool {
        self.alias.as_ref().is_some_and(|alias| alias.columns.len() > ordinal)
    }
}

/// Every relation a statement's own clauses read, in the order PostgreSQL
/// scopes them, a visible `WITH` item answering a factor's name by its
/// outputs.
fn shell_factors(
    statement: &Statement,
    ctes: &[ScopeFactor],
    before: &ParserDB,
) -> Vec<ShellFactor> {
    let mut factors = Vec::new();
    match statement {
        Statement::Insert(Insert { table: TableObject::TableName(name), table_alias, .. }) => {
            let (key, columns) = factor_key_and_columns(name, ctes, before);
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
        _ => {
            if let Some(dml) = statement_dml(statement) {
                dml.for_each_clause_factor(|factor| {
                    push_shell_factor(&mut factors, factor, ctes, before);
                });
            }
        }
    }
    factors
}

/// The key and column names one factor name reaches, a visible `WITH` item
/// answering the name before a relation of it.
fn factor_key_and_columns(
    name: &ObjectName,
    ctes: &[ScopeFactor],
    before: &ParserDB,
) -> (Option<RelationKey>, Option<Vec<(String, bool)>>) {
    if let Some(cte) = ctes.iter().find(|cte| factor_name_answers_cte(name, cte)) {
        (None, cte.columns.clone())
    } else {
        let key = resolved_factor_key(before, name);
        let columns = relation_columns(before, &key);
        (Some(key), columns)
    }
}

/// Adds one clause factor of a statement to the list.
fn push_shell_factor(
    factors: &mut Vec<ShellFactor>,
    factor: &TableFactor,
    ctes: &[ScopeFactor],
    before: &ParserDB,
) {
    let TableFactor::Table { name, alias, args: None, .. } = factor else {
        return;
    };
    let (key, columns) = factor_key_and_columns(name, ctes, before);
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
    /// The reference's column, which is its last part.
    column: Ident,
    /// Whether the reference's qualifier is one of the pseudo names a
    /// data-modifying statement's own clauses scope.
    pseudo: bool,
    /// What the reference reached before the renames.
    outcome: ShellOutcome,
}

/// Every column reference of a statement's own clauses, the queries nested
/// in it excluded, with what each reached before the renames.
fn shell_sites(statement: &Statement, factors: &[ShellFactor]) -> Vec<ShellSite> {
    clause_references(statement)
        .into_iter()
        .map(|(at, parts)| {
            let (column, qualifier, name) = if parts.len() == 1 {
                (parts[0].clone(), None, Some(parts[0].clone()))
            } else {
                (parts[parts.len() - 1].clone(), Some(parts[0].clone()), None)
            };
            ShellSite {
                address: at,
                name,
                column,
                pseudo: qualifier.as_ref().is_some_and(is_pseudo_qualifier),
                outcome: shell_outcome(&parts, factors),
            }
        })
        .collect()
}

/// Whether a qualifier is one of the pseudo names a data-modifying
/// statement's own clauses scope, which always name the write target's
/// columns.
fn is_pseudo_qualifier(qualifier: &Ident) -> bool {
    qualifier.quote_style.is_none()
        && (identifiers_match(&qualifier.value, false, "old", false)
            || identifiers_match(&qualifier.value, false, "new", false)
            || identifiers_match(&qualifier.value, false, "excluded", false))
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
    if let Some(qualifier) = qualifier
        && is_pseudo_qualifier(qualifier)
    {
        // A pseudo qualifier names the write target's columns, never an
        // alias list's outputs.
        return pseudo_outcome(column, factors);
    }
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

/// The outcome a reference through a pseudo qualifier takes, which names the
/// write target's columns, never an alias list's outputs.
fn pseudo_outcome(column: &Ident, factors: &[ShellFactor]) -> ShellOutcome {
    let Some(target) = factors.first() else { return ShellOutcome::Absent };
    let Some(columns) = target.columns.as_ref() else {
        return ShellOutcome::Unknown;
    };
    for (ordinal, (name, quoted)) in columns.iter().enumerate() {
        if identifiers_match(&column.value, column.quote_style.is_some(), name, *quoted) {
            return ShellOutcome::Entry { factor: 0, ordinal };
        }
    }
    ShellOutcome::Absent
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

/// Respells the queries nested in `node`, an `INSERT`'s source and every
/// subquery in its clauses, each on its own the way a view's definition is.
/// A query that cannot be respelled by is left as written, and one the
/// walk's root already respelled is left as it stands.
fn respell_embedded(
    node: &mut impl VisitMut,
    before: &ParserDB,
    renames: &[Rename],
    root_walked: bool,
) {
    struct Embedded<'a> {
        before: &'a ParserDB,
        renames: &'a [Rename],
        /// The data-modifying statements the walk sits in, whose bodies a
        /// root walk leaves untouched.
        dml_depth: usize,
        /// Whether the walk's root respelled the queries it holds on its
        /// own, which the ones outside data-modifying bodies then keep as
        /// respelled.
        root_walked: bool,
    }

    impl VisitorMut for Embedded<'_> {
        type Break = ();

        fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<Self::Break> {
            if statement_dml(statement).is_some() {
                self.dml_depth += 1;
            }
            ControlFlow::Continue(())
        }

        fn post_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<Self::Break> {
            if statement_dml(statement).is_some() {
                self.dml_depth -= 1;
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
            if dml_ref(&query.body).is_some() {
                return ControlFlow::Continue(());
            }
            if self.root_walked && self.dml_depth == 0 {
                return ControlFlow::Continue(());
            }
            let original = query.clone();
            if respell(query, self.before, self.renames).is_none() {
                *query = shielded(&original, self.renames);
            }
            ControlFlow::Continue(())
        }
    }

    let mut embedded = Embedded { before, renames, dml_depth: 0, root_walked };
    let walked = VisitMut::visit(node, &mut embedded);
    debug_assert!(walked.is_continue(), "the walker never breaks");
}

/// Respells a statement's own clauses the way PostgreSQL respells them after
/// the renames, or [`None`] when a reference is not certain enough to
/// respell by. The queries nested in the statement are respelled on their
/// own, so only the clauses owning them are rewritten here.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches the statement's write target through a whole-row reference the
/// statement's `RETURNING` list holds.
fn respell_shell(
    statement: &mut Statement,
    ctes: &[ScopeFactor],
    before: &ParserDB,
    renames: &[Rename],
    function_name: &str,
) -> Result<Option<()>, Error> {
    let factors = shell_factors(statement, ctes, before);
    let sites = shell_sites(statement, &factors);
    if sites.iter().any(|site| site.outcome == ShellOutcome::Unknown) {
        return Ok(None);
    }
    let returning = returning_reference_addresses(statement);
    let on_conflict = on_conflict_addresses(statement);
    let mut edits = shell_edits(&factors, &sites, renames, &on_conflict, &returning);
    if let Some(mut dml) = statement_dml_mut(statement) {
        rename_lists(&mut dml, &factors, renames);
    }
    if whole_row_outcomes(&mut edits, &factors, &sites, renames, &returning, function_name)? {
        return Ok(None);
    }
    captured_sites(&mut edits, &sites, &factors, renames, function_name);
    let walked = VisitMut::visit(statement, &mut edits);
    debug_assert!(walked.is_continue(), "the edits never break");
    if let Some(dml) = statement_dml_mut(statement) {
        apply_returning_aliases(dml, &edits.outputs);
    }
    Ok(Some(()))
}

/// The outcome a whole-row reference the renames capture takes, and whether
/// a rename calls for the shield instead of the respelling.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches the statement's write target through a whole-row reference the
/// statement's `RETURNING` list holds.
fn whole_row_outcomes(
    edits: &mut ShellEdits,
    factors: &[ShellFactor],
    sites: &[ShellSite],
    renames: &[Rename],
    returning: &BTreeSet<usize>,
    function_name: &str,
) -> Result<bool, Error> {
    for (index, factor) in factors.iter().enumerate() {
        let whole_row_sites = sites
            .iter()
            .filter(|site| {
                site.outcome == ShellOutcome::Absent
                    && site.name.as_ref().is_some_and(|name| same_identifier(name, &factor.exposed))
            })
            .collect::<Vec<_>>();
        if whole_row_sites.is_empty() {
            continue;
        }
        let Some(key) = &factor.key else {
            continue;
        };
        if !factor.has_alias
            && renames.iter().any(|candidate| {
                matches!(candidate, Rename::Relation { key: rename_key, .. } if rename_key == key)
            })
        {
            return Ok(true);
        }
        for rename in renames.iter().filter(|candidate| candidate.key() == key) {
            let Rename::Column { ordinal, to, columns, .. } = rename else {
                continue;
            };
            if !identifiers_match(
                &to.value,
                to.quote_style.is_some(),
                &factor.exposed.value,
                factor.exposed.quote_style.is_some(),
            ) || factor.renames_column(*ordinal)
            {
                continue;
            }
            if index != 0 {
                // The other factor shields through its alias column list.
                return Ok(true);
            }
            if whole_row_sites.iter().any(|site| returning.contains(&site.address)) {
                return Err(Error::FunctionBodyBlocksColumnRename {
                    column_name: columns[*ordinal].value.clone(),
                    relation_name: key.name.clone(),
                    function_name: function_name.to_string(),
                });
            }
            for site in &whole_row_sites {
                edits.wildcards.insert(
                    site.address,
                    ObjectName(alloc::vec![ObjectNamePart::Identifier(factor.exposed.clone())]),
                );
            }
        }
    }
    Ok(false)
}

/// The bare references the renames capture, each taking the qualifying name
/// that keeps it reaching what it reached.
fn captured_sites(
    edits: &mut ShellEdits,
    sites: &[ShellSite],
    factors: &[ShellFactor],
    renames: &[Rename],
    function_name: &str,
) {
    for site in sites {
        let Some(written) = &site.name else {
            continue;
        };
        if let ShellOutcome::Entry { factor, .. } = site.outcome {
            let factor = &factors[factor];
            let column_new = edits
                .qualified
                .get(&site.address)
                .map(|(_, to)| to.clone())
                .or_else(|| edits.bare_columns.get(&site.address).cloned())
                .unwrap_or_else(|| written.clone());
            if factors_exposing(factors, renames, &column_new) >= 2 {
                edits.captured.insert(site.address, (factor.exposed.clone(), column_new));
                edits.qualified.remove(&site.address);
                edits.bare_columns.remove(&site.address);
            }
        }
        if matches!(site.outcome, ShellOutcome::Absent) {
            // A whole-row reference the rename already turned into a
            // wildcard keeps its wildcard form.
            if factors_exposing(factors, renames, written) >= 1
                && !edits.wildcards.contains_key(&site.address)
            {
                edits
                    .captured
                    .insert(site.address, (Ident::new(function_name.to_string()), written.clone()));
            }
        }
    }
}

/// The factors that expose one column name after the renames.
fn factors_exposing(factors: &[ShellFactor], renames: &[Rename], column: &Ident) -> usize {
    factors
        .iter()
        .filter(|factor| {
            post_columns(factor, renames).is_some_and(|columns| {
                columns.iter().any(|(name, quoted)| {
                    identifiers_match(&column.value, column.quote_style.is_some(), name, *quoted)
                })
            })
        })
        .count()
}

/// Renames a `RETURNING` list's output names a column rename took, each
/// renamed reference taking an alias of the name it held before.
fn apply_returning_aliases(dml: DmlMut<'_>, outputs: &BTreeMap<usize, Ident>) {
    let Some(returning) = dml.returning_mut() else {
        return;
    };
    if outputs.is_empty() {
        return;
    }
    for item in returning {
        let SelectItem::UnnamedExpr(expression) = item else {
            continue;
        };
        let at = address(innermost(expression));
        if let Some(name) = outputs.get(&at) {
            *item = SelectItem::ExprWithAlias { expr: expression.clone(), alias: name.clone() };
        }
    }
}

/// The addresses of the column references a statement's `RETURNING` list
/// holds, which a rename keeps the list's output names through.
fn returning_reference_addresses(statement: &Statement) -> BTreeSet<usize> {
    let mut addresses = BTreeSet::new();
    if let Some(dml) = statement_dml(statement)
        && let Some(returning) = dml.returning()
    {
        for item in returning {
            let (SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias { expr: expression, .. }) = item
            else {
                continue;
            };
            if simple_reference_name(expression).is_some() {
                addresses.insert(address(innermost(expression)));
            }
        }
    }
    addresses
}

/// The addresses of the column references an `ON CONFLICT` predicate holds,
/// which a rename keeps bare the way PostgreSQL respells it.
fn on_conflict_addresses(statement: &Statement) -> BTreeSet<usize> {
    struct References<'a> {
        addresses: &'a mut BTreeSet<usize>,
    }

    impl Visitor for References<'_> {
        type Break = ();

        fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
            if matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                self.addresses.insert(address(expression));
            }
            ControlFlow::Continue(())
        }
    }

    let predicate = match statement {
        Statement::Insert(Insert { on, .. }) => {
            let Some(OnInsert::OnConflict(OnConflict { conflict_target, .. })) = on else {
                return BTreeSet::new();
            };
            let Some(ConflictTarget::Columns { predicate: Some(predicate), .. }) = conflict_target
            else {
                return BTreeSet::new();
            };
            Some(predicate)
        }
        _ => None,
    };
    let Some(predicate) = predicate else {
        return BTreeSet::new();
    };
    let mut addresses = BTreeSet::new();
    let mut finder = References { addresses: &mut addresses };
    let walked = predicate.visit(&mut finder);
    debug_assert!(walked.is_continue(), "the finder never breaks");
    addresses
}

/// The names a factor's outputs will hold after the renames, in order, the
/// alias-pinned ordinals keeping the alias names they pin.
fn post_columns(factor: &ShellFactor, renames: &[Rename]) -> Option<Vec<(String, bool)>> {
    let mut columns = exposed_columns(factor)?;
    let Some(key) = &factor.key else {
        return Some(columns);
    };
    for rename in renames.iter().filter(|candidate| candidate.key() == key) {
        let Rename::Column { ordinal, to, .. } = rename else {
            continue;
        };
        if factor.renames_column(*ordinal) {
            continue;
        }
        let Some(column) = columns.get_mut(*ordinal) else { continue };
        column.0.clone_from(&to.value);
        column.1 = to.quote_style.is_some();
    }
    Some(columns)
}

/// The edits that respell a statement's own clauses, one block per factor
/// the renames touch.
fn shell_edits(
    factors: &[ShellFactor],
    sites: &[ShellSite],
    renames: &[Rename],
    on_conflict: &BTreeSet<usize>,
    returning: &BTreeSet<usize>,
) -> ShellEdits {
    let mut edits = ShellEdits::default();
    for (index, factor) in factors.iter().enumerate() {
        let Some(key) = &factor.key else {
            continue;
        };
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
                edits.factors.names.insert(factor.address, to);
            }
        }
        // PostgreSQL respells a target's qualified references under the
        // target's new name as well.
        if let Some(qualifier) = relation_to_name.as_ref()
            && !factor.has_alias
        {
            for site in sites {
                let ShellOutcome::Entry { factor: target, .. } = site.outcome else {
                    continue;
                };
                if target == index && site.name.is_none() && !site.pseudo {
                    edits.qualifiers.insert(site.address, qualifier.clone());
                }
            }
        }
        for (ordinal, to) in column_renames {
            let pinned = factor.renames_column(ordinal);
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
                    if on_conflict.contains(&site.address) {
                        edits.bare_columns.insert(site.address, to.clone());
                    } else {
                        edits.qualified.insert(site.address, (qualifier.clone(), to.clone()));
                    }
                } else {
                    edits.columns.insert(site.address, to.clone());
                }
                if returning.contains(&site.address) {
                    edits.outputs.insert(site.address, site.column.clone());
                }
            }
        }
    }
    edits
}

/// Shields a statement's own clauses from the renames, each renamed relation
/// the statement reads taking the new name under an alias that brings back
/// the old one and the old column names. An `INSERT` target can reach here
/// through a whole-row reference it names in its `RETURNING` list, and the
/// alias PostgreSQL accepts on it carries no column list.
///
/// # Errors
///
/// Returns [`Error::FunctionBodyBlocksColumnRename`] when a column rename
/// reaches the statement's write target, the first factor, whose alias
/// PostgreSQL accepts with no column list.
fn shield_shell(
    statement: &mut Statement,
    ctes: &[ScopeFactor],
    before: &ParserDB,
    renames: &[Rename],
    function_name: &str,
) -> Result<(), Error> {
    let factors = shell_factors(statement, ctes, before);
    let mut edits = ShellEdits::default();
    let mut insert_alias = None;
    for (position, factor) in factors.iter().enumerate() {
        let Some(key) = &factor.key else {
            continue;
        };
        for rename in renames.iter().filter(|rename| rename.key() == key) {
            match rename {
                Rename::Relation { to, .. } => {
                    let to = respelled_factor_name(to);
                    if factor.insert_target {
                        edits.names.insert(factor.address, to);
                        insert_alias = Some(factor.exposed.clone());
                    } else {
                        edits.factors.names.insert(factor.address, to);
                        if !factor.has_alias {
                            edits.factors.aliases.insert(
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
                }
                Rename::Column { ordinal, columns, .. } => {
                    if position == 0 {
                        return Err(Error::FunctionBodyBlocksColumnRename {
                            column_name: columns[*ordinal].value.clone(),
                            relation_name: key.name.clone(),
                            function_name: function_name.to_string(),
                        });
                    }
                    if factor.renames_column(*ordinal) {
                        continue;
                    }
                    edits.factors.aliases.insert(
                        factor.address,
                        restoring_alias(factor.alias.as_ref(), factor.exposed.clone(), columns),
                    );
                }
            }
        }
    }
    let walked = VisitMut::visit(statement, &mut edits);
    debug_assert!(walked.is_continue(), "the edits never break");
    if let Some(alias) = insert_alias
        && let Statement::Insert(Insert { table_alias, .. }) = statement
    {
        *table_alias = Some(TableAliasWithoutColumns { explicit: true, alias });
    }
    Ok(())
}

/// The column names one assignment target writes, a tuple target listing
/// the target's columns by name.
fn assignment_target_entries<'a>(
    target: &'a mut AssignmentTarget,
    entries: &mut Vec<&'a mut ObjectName>,
) {
    match target {
        AssignmentTarget::ColumnName(name) => entries.push(name),
        AssignmentTarget::Tuple(names) => entries.extend(names.iter_mut()),
    }
}

/// The new name a column rename writes over the part it still names, which
/// the rename's old name must hold.
fn renamed_part(rename: &Rename, written: &Ident) -> Option<Ident> {
    let Rename::Column { ordinal, to, columns, .. } = rename else {
        return None;
    };
    // `ordinal` is the position the renamed column held among `columns`,
    // so it is in range.
    let old = &columns[*ordinal];
    identifiers_match(
        &old.value,
        old.quote_style.is_some(),
        &written.value,
        written.quote_style.is_some(),
    )
    .then(|| to.clone())
}

/// Renames the columns a write target lists, which name the target's own
/// columns rather than any alias's outputs.
fn rename_lists(dml: &mut DmlMut<'_>, factors: &[ShellFactor], renames: &[Rename]) {
    let Some(target) = factors.first() else { return };
    let Some(key) = &target.key else {
        return;
    };
    let mut entries: Vec<&mut ObjectName> = Vec::new();
    let mut conflict_columns: Vec<&mut Ident> = Vec::new();
    match dml {
        DmlMut::Insert(Insert { columns, on, .. }) => {
            entries.extend(columns.iter_mut());
            if let Some(OnInsert::OnConflict(OnConflict {
                conflict_target: Some(ConflictTarget::Columns { columns: conflict, .. }),
                action: OnConflictAction::DoUpdate(DoUpdate { assignments, .. }),
                ..
            })) = on
            {
                conflict_columns.extend(conflict.iter_mut());
                for assignment in assignments {
                    assignment_target_entries(&mut assignment.target, &mut entries);
                }
            }
        }
        DmlMut::Update(Update { assignments, .. }) => {
            for assignment in assignments {
                assignment_target_entries(&mut assignment.target, &mut entries);
            }
        }
        DmlMut::Merge(Merge { clauses, .. }) => {
            for clause in clauses {
                match &mut clause.action {
                    MergeAction::Insert(insert) => entries.extend(insert.columns.iter_mut()),
                    MergeAction::Update(MergeUpdateExpr {
                        kind: MergeUpdateKind::Set(assignments),
                        ..
                    }) => {
                        for assignment in assignments {
                            assignment_target_entries(&mut assignment.target, &mut entries);
                        }
                    }
                    _ => {}
                }
            }
        }
        DmlMut::Delete(_) => {}
    }
    // An entry's first part names the column, and later parts name fields of
    // a composite column, which a column rename leaves alone.
    for part in entries.into_iter().filter_map(|entry| entry.0.first_mut()) {
        for rename in renames.iter().filter(|rename| rename.key() == key) {
            if let Some(to) = part.as_ident().and_then(|written| renamed_part(rename, written)) {
                *part = ObjectNamePart::Identifier(to);
            }
        }
    }
    for column in conflict_columns {
        for rename in renames.iter().filter(|rename| rename.key() == key) {
            if let Some(to) = renamed_part(rename, column) {
                *column = to;
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
    /// The clause factors' new names and aliases.
    factors: FactorEdits,
    /// A qualified reference's new column identifier.
    columns: BTreeMap<usize, Ident>,
    /// A qualified reference's new relation identifier.
    qualifiers: BTreeMap<usize, Ident>,
    /// A bare reference's new compound form, which a renamed column of an
    /// unaliased or aliased factor calls for.
    qualified: BTreeMap<usize, (Ident, Ident)>,
    /// A bare reference a rename captures, which takes the qualifying name
    /// that keeps it reaching what it reached.
    captured: BTreeMap<usize, (Ident, Ident)>,
    /// A bare reference's new column, which the `ON CONFLICT` predicate
    /// keeps bare.
    bare_columns: BTreeMap<usize, Ident>,
    /// A whole-row reference's new qualified wildcard.
    wildcards: BTreeMap<usize, ObjectName>,
    /// A `RETURNING` list reference's written name, which the list's output
    /// name keeps.
    outputs: BTreeMap<usize, Ident>,
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
        self.factors.apply(factor);
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        let at = address(&*expression);
        let compound = self.captured.get(&at).or_else(|| self.qualified.get(&at));
        if let Some((qualifier, column)) = compound
            && let Expr::Identifier(_) = expression
        {
            *expression = Expr::CompoundIdentifier(alloc::vec![qualifier.clone(), column.clone()]);
            return ControlFlow::Continue(());
        }
        if let Some(to) = self.bare_columns.get(&at)
            && let Expr::Identifier(written) = expression
        {
            *written = to.clone();
            return ControlFlow::Continue(());
        }
        if let Some(name) = self.wildcards.get(&at)
            && let Expr::Identifier(_) = expression
        {
            *expression = Expr::QualifiedWildcard(name.clone(), AttachedToken::empty());
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
            && let Some(relation) = relation_part(parts)
        {
            *relation = to.clone();
        }
        ControlFlow::Continue(())
    }
}
