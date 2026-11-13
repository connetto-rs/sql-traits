//! The column references of a query, each with what it reaches, and how its
//! wildcards and `NATURAL` joins would be spelled out.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::{
    Delete, Distinct, DoUpdate, Expr, FromTable, GroupByExpr, Ident, Insert, Merge, MergeAction,
    MergeClause, MergeInsertExpr, MergeUpdateExpr, MergeUpdateKind, ObjectName, ObjectNamePart,
    OnConflict, OnConflictAction, OnInsert, OrderByKind, OutputClause, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor, TableWithJoins, Update,
    UpdateTableFromKind, Visit, Visitor,
};

use super::{
    super::{
        ColumnOrigin, FromScope, RelationKey, WildcardEntry, build_definition_graph,
        expand_qualified_wildcard, factor_address, join_constraint, subsumed_exposure,
        wildcard_origins, wildcard_reshapes_output,
    },
    Bound, DefinitionGraph, DefinitionId, ScopeCursor, ScopeId,
    output_columns::{QueryId, unwrap_nested},
};
use crate::{
    errors::LookupError, impls::dql::scoped_roots::ScopedRoot, structs::IdentifierCase,
    traits::DatabaseLike, utils::walk::walk,
};

/// What one column reference reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SiteOutcome {
    /// Output column `ordinal` of the `FROM` item at position `factor` of
    /// [`QueryReferences::factors`], counted before any alias column list.
    Entry {
        /// The position of the `FROM` item among the query's items.
        factor: usize,
        /// The position of the column among the item's output columns.
        ordinal: usize,
    },
    /// A column a `USING` or `NATURAL` join merged.
    Merged,
    /// A select-list output a clause key named.
    Output,
    /// Nothing in reach exposes the name, as for a parameter.
    Absent,
    /// Nothing certain, because an opaque relation could have answered, the
    /// reference was ambiguous, or it sits where no scope was derived.
    Unknown,
}

/// Where one column reference sits and what it reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReferenceSite {
    /// The address of the reference's expression in the walked query.
    pub(crate) expression: usize,
    /// What the reference reached.
    pub(crate) outcome: SiteOutcome,
}

/// Every column reference of a query and how its wildcards and `NATURAL`
/// joins would be spelled out.
#[derive(Debug, Default)]
pub(crate) struct QueryReferences {
    /// The address of every `FROM` item, nested queries included, in walk
    /// order.
    pub(crate) factors: Vec<usize>,
    /// Every column reference, in walk order.
    pub(crate) sites: Vec<ReferenceSite>,
    /// Each select-list wildcard whose columns could be enumerated, by the
    /// address of its item, with the items listing them.
    pub(crate) wildcards: Vec<(usize, Vec<SelectItem>)>,
    /// Each `NATURAL` join whose sides' columns could be enumerated, by the
    /// address of the join, with the names it merges.
    pub(crate) natural_joins: Vec<(usize, Vec<Ident>)>,
    /// Each parenthesized join under an alias whose columns could be
    /// enumerated, by its position in [`QueryReferences::factors`], with what
    /// each of its output columns reads.
    pub(crate) joins: Vec<(usize, Vec<SiteOutcome>)>,
}

impl QueryReferences {
    /// Whether every reference reached something certain.
    pub(crate) fn certain(&self) -> bool {
        self.sites.iter().all(|site| site.outcome != SiteOutcome::Unknown)
    }

    /// What output column `ordinal` of the `FROM` item at position `factor`
    /// reads, when the item is a parenthesized join under an alias.
    pub(crate) fn join_column(&self, factor: usize, ordinal: usize) -> Option<SiteOutcome> {
        self.joins
            .iter()
            .find(|(join, _)| *join == factor)
            .and_then(|(_, origins)| origins.get(ordinal).copied())
    }
}

/// How the clause holding a column reference resolves it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SiteRule {
    /// Through the scope the cursor names, as an ordinary reference.
    Scope(ScopeCursor),
    /// As an `ORDER BY` key of the query, a select-list output first.
    OrderBy(QueryId),
    /// A select-list output first, then the input, as a `DISTINCT ON` key.
    OutputFirst(ScopeCursor),
    /// The input first, then a select-list output, as a `GROUP BY` key.
    InputFirst(ScopeCursor),
}

/// The rule resolving each column reference, by the address of its
/// expression, `None` for one sitting where no scope was derived.
pub(crate) type SiteRules = Vec<(usize, Option<SiteRule>)>;

impl<'query, 'db, DB: DatabaseLike> DefinitionGraph<'query, 'db, DB> {
    /// Resolves `reference` as `rule` says its clause resolves it.
    pub(crate) fn resolve_site<'scope>(
        &'scope self,
        rule: SiteRule,
        reference: &Expr,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        match rule {
            SiteRule::Scope(cursor) => self.resolve_binding(cursor, reference),
            SiteRule::OrderBy(query) => self.resolve_order_by(query, reference),
            SiteRule::OutputFirst(cursor) => self.resolve_output_first(cursor, reference),
            SiteRule::InputFirst(cursor) => self.resolve_input_first(cursor, reference),
        }
    }

    /// The rule resolving each column reference of `query`, by the address of
    /// its expression, `None` for one sitting where no scope was derived.
    pub(crate) fn site_rules(&self, query: &Query) -> Vec<(usize, Option<SiteRule>)> {
        let mut rules = Vec::new();
        let mut sink = |expression: &Expr, rule| rules.push((expression_address(expression), rule));
        Walk {
            graph: self,
            references: QueryReferences::default(),
            sink: &mut sink,
            modifying_bodies: true,
        }
        .query(query);
        rules
    }

    /// The rule resolving each column reference of `root`, by the address of
    /// its expression, `None` for one sitting where no scope was derived,
    /// `outer` naming the scope [`scoped_graph`] filled with what the root
    /// reaches.
    ///
    /// [`scoped_graph`]: crate::impls::dql::scoped_roots::scoped_graph
    pub(crate) fn root_site_rules(
        &self,
        root: ScopedRoot<'_>,
        outer: ScopeCursor,
    ) -> Vec<(usize, Option<SiteRule>)> {
        let mut rules = Vec::new();
        let mut sink = |expression: &Expr, rule| rules.push((expression_address(expression), rule));
        let mut walk = Walk {
            graph: self,
            references: QueryReferences::default(),
            sink: &mut sink,
            modifying_bodies: true,
        };
        match root {
            ScopedRoot::Expression(expression) => walk.node(expression, Some(outer)),
            ScopedRoot::Statement(Statement::Query(query)) => walk.query(query),
            ScopedRoot::Statement(statement) => walk.modifying(statement),
        }
        rules
    }
}

/// Walks `query` against `database`, or answers `None` when the query's
/// scopes cannot be derived at all.
pub(crate) fn query_references<DB: DatabaseLike>(
    query: &Query,
    database: &DB,
    case: IdentifierCase,
) -> Option<QueryReferences> {
    let (graph, _) = build_definition_graph(query, database, case).ok()?;
    let mut origins = Vec::new();
    let mut sink = |expression: &Expr, rule: Option<SiteRule>| {
        let reached = match rule {
            Some(rule) => Reached::of(&graph.resolve_site(rule, expression)),
            None => Reached::Unknown,
        };
        origins.push((expression_address(expression), reached));
    };
    // A data-modifying body respells through the statement holding it, so
    // its clauses stay out of the references a respelling reads.
    let mut walk = Walk {
        graph: &graph,
        references: QueryReferences::default(),
        sink: &mut sink,
        modifying_bodies: false,
    };
    walk.query(query);
    let mut references = walk.references;
    let outcome = |origin: Reached, factors: &[usize]| {
        match origin {
            Reached::Origin(ColumnOrigin::Entry { factor, ordinal }) => {
                factors
                    .iter()
                    .position(|address| *address == factor)
                    .map_or(SiteOutcome::Unknown, |factor| SiteOutcome::Entry { factor, ordinal })
            }
            Reached::Origin(ColumnOrigin::Merged) => SiteOutcome::Merged,
            Reached::Origin(ColumnOrigin::Output) => SiteOutcome::Output,
            Reached::Origin(ColumnOrigin::Unknown) | Reached::Unknown => SiteOutcome::Unknown,
            Reached::Absent => SiteOutcome::Absent,
        }
    };
    references.sites = origins
        .into_iter()
        .map(|(expression, origin)| {
            ReferenceSite { expression, outcome: outcome(origin, &references.factors) }
        })
        .collect();
    references.joins = references
        .factors
        .iter()
        .enumerate()
        .filter_map(|(position, address)| {
            let (_, scope) = graph.joins.iter().find(|(held, _)| held == address)?;
            let data = &graph.scopes[scope.0].data;
            (!data.has_opaque()).then(|| {
                let columns = wildcard_origins(data)
                    .into_iter()
                    .map(|origin| outcome(Reached::Origin(origin), &references.factors))
                    .collect();
                (position, columns)
            })
        })
        .collect();
    Some(references)
}

/// What a resolver answered for one reference, before `FROM` item addresses
/// are turned into positions.
#[derive(Clone, Copy)]
enum Reached {
    Origin(ColumnOrigin),
    Absent,
    Unknown,
}

impl Reached {
    fn of<DB: DatabaseLike>(resolved: &Result<Option<Bound<'_, '_, '_, DB>>, LookupError>) -> Self {
        match resolved {
            Ok(Some((_, binding))) => Self::Origin(binding.origin()),
            Ok(None) => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }
}

struct Walk<'graph, 'sink, 'query, 'db, DB: DatabaseLike> {
    graph: &'graph DefinitionGraph<'query, 'db, DB>,
    references: QueryReferences,
    /// Takes each column reference with the rule its clause resolves it by.
    sink: &'sink mut dyn FnMut(&Expr, Option<SiteRule>),
    /// Whether the clauses of a data-modifying statement a query body holds
    /// are walked.
    modifying_bodies: bool,
}

fn expression_address(expression: &Expr) -> usize {
    core::ptr::from_ref(expression).addr()
}

fn item_address(item: &SelectItem) -> usize {
    core::ptr::from_ref(item).addr()
}

/// Whether an expression names a column by itself, which the clause-key
/// resolvers read whole.
fn is_column_name(expression: &Expr) -> bool {
    matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

impl<DB: DatabaseLike> Walk<'_, '_, '_, '_, DB> {
    fn record(&mut self, expression: &Expr, rule: Option<SiteRule>) {
        (self.sink)(expression, rule);
    }

    fn cursor(&self, scope: Option<ScopeId>) -> Option<ScopeCursor> {
        scope.map(|scope| self.graph.select_cursor(scope))
    }

    /// Walks the references of any node with a select's scope, entering the
    /// queries nested in it on their own terms.
    fn node<N: Visit + ?Sized>(&mut self, node: &N, cursor: Option<ScopeCursor>) {
        let mut sites = Sites { walk: self, cursor, depth: 0 };
        let walked = walk(node, &mut sites);
        debug_assert!(walked.is_continue(), "the site visitor never breaks");
    }

    fn query(&mut self, query: &Query) {
        let Query {
            with,
            body,
            order_by,
            limit_clause,
            fetch,
            locks,
            for_clause,
            settings,
            format_clause,
            pipe_operators,
        } = query;
        if let Some(with) = with {
            for cte in &with.cte_tables {
                self.query(&cte.query);
            }
        }
        self.set_expr(body);
        let body_cursor = match body.as_ref() {
            SetExpr::Select(select) => self.cursor(self.graph.select_scope(select)),
            _ => None,
        };
        if let Some(order_by) = order_by {
            if let OrderByKind::Expressions(keys) = &order_by.kind {
                let query_id = self.graph.query_id(query);
                for key in keys {
                    let bare = unwrap_nested(&key.expr);
                    match query_id {
                        Some(query_id) if is_column_name(bare) => {
                            self.record(bare, Some(SiteRule::OrderBy(query_id)));
                        }
                        _ => self.node(&key.expr, body_cursor),
                    }
                    self.node(&key.with_fill, body_cursor);
                }
            }
            self.node(&order_by.interpolate, body_cursor);
        }
        self.node(limit_clause, None);
        self.node(fetch, None);
        self.node(locks, None);
        self.node(for_clause, None);
        self.node(settings, None);
        self.node(format_clause, None);
        self.node(pipe_operators, None);
    }

    fn set_expr(&mut self, body: &SetExpr) {
        match body {
            SetExpr::Select(select) => self.select(select),
            SetExpr::Query(query) => self.query(query),
            SetExpr::SetOperation { left, right, .. } => {
                self.set_expr(left);
                self.set_expr(right);
            }
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => {
                if self.modifying_bodies {
                    self.modifying(statement);
                }
            }
            other => self.node(other, None),
        }
    }

    fn select(&mut self, select: &Select) {
        let Select {
            select_token: _,
            flavor: _,
            distinct,
            select_modifiers: _,
            optimizer_hints: _,
            top,
            top_before_distinct: _,
            projection,
            exclude: _,
            into: _,
            from,
            lateral_views,
            prewhere,
            selection,
            connect_by,
            group_by,
            cluster_by,
            distribute_by,
            sort_by,
            having,
            named_window,
            qualify,
            window_before_qualify: _,
            value_table_mode: _,
        } = select;
        let scope = self.graph.select_scope(select);
        let cursor = self.cursor(scope);

        self.relations(&[from], scope);

        if let Some(Distinct::On(keys)) = distinct {
            for key in keys {
                let bare = unwrap_nested(key);
                match cursor {
                    Some(cursor) if is_column_name(bare) => {
                        self.record(bare, Some(SiteRule::OutputFirst(cursor)));
                    }
                    _ => self.node(key, cursor),
                }
            }
        }
        self.node(top, cursor);
        for item in projection {
            self.node(item, cursor);
        }
        if let Some(scope) = scope {
            let graph = self.graph;
            self.spell_out(projection, &graph.scopes[scope.0].data);
        }
        self.node(lateral_views, cursor);
        self.node(prewhere, cursor);
        self.node(selection, cursor);
        self.node(connect_by, cursor);
        if let GroupByExpr::Expressions(keys, modifiers) = group_by {
            for key in keys {
                let bare = unwrap_nested(key);
                match cursor {
                    Some(cursor) if is_column_name(bare) => {
                        self.record(bare, Some(SiteRule::InputFirst(cursor)));
                    }
                    _ => self.node(key, cursor),
                }
            }
            self.node(modifiers, cursor);
        }
        self.node(cluster_by, cursor);
        self.node(distribute_by, cursor);
        self.node(sort_by, cursor);
        self.node(having, cursor);
        self.node(named_window, cursor);
        self.node(qualify, cursor);
    }

    /// Records a `FROM` item and walks what it holds as the entries of
    /// `scope` from `entries` on, counting them into `entries`. The items
    /// before it are what a function argument may read.
    fn factor(&mut self, factor: &TableFactor, scope: Option<ScopeId>, entries: &mut usize) {
        self.references.factors.push(factor_address(factor));
        match factor {
            TableFactor::NestedJoin { table_with_joins, alias: None } => {
                self.joined(table_with_joins, scope, entries);
                return;
            }
            TableFactor::NestedJoin { table_with_joins, alias: Some(_) } => {
                let inner = scope.and_then(|scope| self.graph.join_scope(factor, scope));
                self.joined(table_with_joins, inner, &mut 0);
                let graph = self.graph;
                if let Some(inner) = inner
                    && !graph.scopes[inner.0].data.has_opaque()
                {
                    self.natural_joins(&graph.scopes[inner.0].data);
                }
            }
            TableFactor::Derived { subquery, .. } => self.query(subquery),
            other => {
                let preceding = scope.map(|scope| ScopeCursor { scope, visible_entries: *entries });
                self.node(other, preceding);
            }
        }
        *entries += 1;
    }

    /// Walks `table` and the relations it joins, with their join
    /// constraints, as the entries of `scope` from `entries` on.
    fn joined(&mut self, table: &TableWithJoins, scope: Option<ScopeId>, entries: &mut usize) {
        self.factor(&table.relation, scope, entries);
        for join in &table.joins {
            self.factor(&join.relation, scope, entries);
            if let Some(constraint) = join_constraint(&join.join_operator) {
                let joined = scope.map(|scope| ScopeCursor { scope, visible_entries: *entries });
                self.node(constraint, joined);
            }
        }
    }

    /// Records the `FROM` items `lists` name, in order, as the entries of
    /// `scope`, and walks what they and their join constraints hold.
    fn relations(&mut self, lists: &[&[TableWithJoins]], scope: Option<ScopeId>) {
        let mut entries = 0;
        for table in lists.iter().copied().flatten() {
            self.joined(table, scope, &mut entries);
        }
    }

    /// Walks a data-modifying statement, each clause in the scope it reads,
    /// and anything else with no scope.
    fn modifying(&mut self, statement: &Statement) {
        let scopes = self.graph.modifying_scopes(statement).unwrap_or_default();
        let cursor = scopes.relations;
        let scope = cursor.map(|cursor| cursor.scope);
        match statement {
            Statement::Update(Update {
                table, from, assignments, selection, returning, ..
            }) => {
                let from = match from {
                    Some(
                        UpdateTableFromKind::BeforeSet(from) | UpdateTableFromKind::AfterSet(from),
                    ) => from.as_slice(),
                    None => &[],
                };
                self.relations(&[core::slice::from_ref(table), from], scope);
                for assignment in assignments {
                    self.node(&assignment.value, cursor);
                }
                self.node(selection, cursor);
                self.node(returning, cursor);
            }
            Statement::Delete(Delete { from, using, selection, returning, .. }) => {
                let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = from;
                self.relations(&[from, using.as_deref().unwrap_or_default()], scope);
                self.node(selection, cursor);
                self.node(returning, cursor);
            }
            Statement::Insert(Insert { source, returning, on, .. }) => {
                if let Some(source) = source {
                    self.query(source);
                }
                self.node(returning, cursor);
                if let Some(OnInsert::OnConflict(OnConflict {
                    action: OnConflictAction::DoUpdate(DoUpdate { assignments, selection }),
                    ..
                })) = on
                {
                    for assignment in assignments {
                        self.node(&assignment.value, scopes.conflict);
                    }
                    self.node(selection, scopes.conflict);
                }
            }
            Statement::Merge(Merge { table, source, on, clauses, output, .. }) => {
                let mut entries = 0;
                self.factor(source, scope, &mut entries);
                self.factor(table, scope, &mut entries);
                self.node(on, cursor);
                for MergeClause { clause_kind, predicate, action, .. } in clauses {
                    let clause = scopes.clause(*clause_kind);
                    self.node(predicate, clause);
                    self.merge_action(action, clause);
                }
                match output {
                    Some(OutputClause::Returning { select_items, .. }) => {
                        self.node(select_items, cursor);
                    }
                    other => self.node(other, None),
                }
            }
            other => self.node(other, None),
        }
    }

    /// Walks what a `MERGE` clause's action computes in the scope `cursor`
    /// names.
    fn merge_action(&mut self, action: &MergeAction, cursor: Option<ScopeCursor>) {
        match action {
            MergeAction::Insert(MergeInsertExpr { kind, .. }) => self.node(kind, cursor),
            MergeAction::Update(MergeUpdateExpr {
                kind,
                update_predicate,
                delete_predicate,
                ..
            }) => {
                if let MergeUpdateKind::Set(assignments) = kind {
                    for assignment in assignments {
                        self.node(&assignment.value, cursor);
                    }
                }
                self.node(update_predicate, cursor);
                self.node(delete_predicate, cursor);
            }
            MergeAction::Delete { .. } | MergeAction::DoNothing { .. } => {}
        }
    }

    /// Records how each wildcard of a select list and each `NATURAL` join of
    /// its `FROM` clause would be spelled out. Nothing is recorded for a scope
    /// an opaque relation reaches, whose columns cannot all be named.
    fn spell_out(&mut self, projection: &[SelectItem], data: &FromScope<'_, '_, DB, DefinitionId>) {
        if data.has_opaque() {
            return;
        }
        for item in projection {
            let spelled = match item {
                SelectItem::Wildcard(options) if !wildcard_reshapes_output(options) => {
                    unqualified_wildcard_items(data)
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(qualifier),
                    options,
                ) if !wildcard_reshapes_output(options) => {
                    qualified_wildcard_items(data, qualifier)
                }
                _ => None,
            };
            if let Some(items) = spelled {
                self.references.wildcards.push((item_address(item), items));
            }
        }
        self.natural_joins(data);
    }

    /// Records how each `NATURAL` join of a scope would be spelled out.
    fn natural_joins(&mut self, data: &FromScope<'_, '_, DB, DefinitionId>) {
        for (join, names) in &data.natural_merges {
            if !names.is_empty() {
                self.references.natural_joins.push((
                    *join,
                    names.iter().map(|(name, quoted)| identifier(name, *quoted)).collect(),
                ));
            }
        }
    }
}

/// An identifier as a statement would write it.
fn identifier(name: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', name) } else { Ident::new(name) }
}

fn key_identifier(key: RelationKey<'_, '_>) -> Ident {
    identifier(key.value.get(), key.quoted)
}

/// A select-list item reading one column, under `qualifier` when given.
fn column_item(qualifier: Option<&[Ident]>, name: &str, quoted: bool) -> SelectItem {
    let column = identifier(name, quoted);
    SelectItem::UnnamedExpr(match qualifier {
        None => Expr::Identifier(column),
        Some(qualifier) => {
            let mut parts = qualifier.to_vec();
            parts.push(column);
            Expr::CompoundIdentifier(parts)
        }
    })
}

/// The items a bare `*` stands for, in PostgreSQL's join output order. A
/// column is written bare when a single `FROM` item reaches the scope, and
/// under the name its item is reached by otherwise. A column a join merged is
/// written under the name of the side it is read from, and bare after a `FULL`
/// join, which reads it from both. A `FROM` item reached by no name among
/// several leaves the wildcard as written, since its columns could not be
/// told apart.
fn unqualified_wildcard_items<DB: DatabaseLike>(
    data: &FromScope<'_, '_, DB, DefinitionId>,
) -> Option<Vec<SelectItem>> {
    let single = data.from_entry_count == 1;
    let mut items = Vec::new();
    for entry in data.wildcard_plans.iter().flatten() {
        match entry {
            WildcardEntry::Base(index) => {
                let base = &data.bases[*index];
                let qualifier = (!single).then(|| [key_identifier(base.key)]);
                push_exposed_columns(
                    &mut items,
                    data,
                    base.entry_index,
                    qualifier.as_ref().map(<[Ident; 1]>::as_slice),
                    base.output_columns.iter().map(|column| (&*column.name, column.quoted)),
                );
            }
            WildcardEntry::Derived(index) => {
                let relation = &data.derived[*index];
                let qualifier = match (single, relation.key) {
                    (true, _) => None,
                    (false, Some(key)) => Some([key_identifier(key)]),
                    (false, None) => return None,
                };
                push_exposed_columns(
                    &mut items,
                    data,
                    relation.entry_index,
                    qualifier.as_ref().map(<[Ident; 1]>::as_slice),
                    relation.shape.columns.iter().map(|column| (&*column.name, column.quoted)),
                );
            }
            WildcardEntry::Merged { name, quoted, provider } => {
                let qualifier =
                    provider.as_ref().map(|(provider, quoted)| [identifier(provider, *quoted)]);
                items.push(column_item(
                    qualifier.as_ref().map(<[Ident; 1]>::as_slice),
                    name,
                    *quoted,
                ));
            }
        }
    }
    (!items.is_empty()).then_some(items)
}

/// Pushes an item for each of `columns` that no join merged away from the
/// `FROM` item at `entry_index`, under `qualifier` when given.
fn push_exposed_columns<'c, DB: DatabaseLike>(
    items: &mut Vec<SelectItem>,
    data: &FromScope<'_, '_, DB, DefinitionId>,
    entry_index: usize,
    qualifier: Option<&[Ident]>,
    columns: impl Iterator<Item = (&'c str, bool)>,
) {
    for (name, quoted) in columns {
        if !subsumed_exposure(&data.merged, entry_index, name, quoted, data.case) {
            items.push(column_item(qualifier, name, quoted));
        }
    }
}

/// The items a qualified `t.*` stands for, each under the qualifier as
/// written.
fn qualified_wildcard_items<DB: DatabaseLike>(
    data: &FromScope<'_, '_, DB, DefinitionId>,
    qualifier: &ObjectName,
) -> Option<Vec<SelectItem>> {
    let parts = qualifier
        .0
        .iter()
        .map(|part| {
            match part {
                ObjectNamePart::Identifier(ident) => Some(ident.clone()),
                ObjectNamePart::Function(_) => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    let columns = expand_qualified_wildcard(&data.bases, &data.derived, qualifier, data.case)?;
    let items: Vec<SelectItem> = columns
        .iter()
        .map(|column| column_item(Some(&parts), &column.name, column.quoted))
        .collect();
    (!items.is_empty()).then_some(items)
}

/// Records every column reference a node holds outside nested queries, and
/// hands each nested query to the walk whole.
struct Sites<'walk, 'graph, 'sink, 'query, 'db, DB: DatabaseLike> {
    walk: &'walk mut Walk<'graph, 'sink, 'query, 'db, DB>,
    cursor: Option<ScopeCursor>,
    /// How many queries deep the visitor stands below the node it started
    /// from, whose references the nested walk records instead.
    depth: usize,
}

impl<DB: DatabaseLike> Visitor for Sites<'_, '_, '_, '_, '_, DB> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if self.depth == 0 {
            self.walk.query(query);
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if self.depth > 0 {
            return ControlFlow::Continue(());
        }
        match expression {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                self.walk.record(expression, self.cursor.map(SiteRule::Scope));
            }
            // A whole-row reference names a relation the resolver does not
            // follow.
            Expr::QualifiedWildcard(..) => self.walk.record(expression, None),
            _ => {}
        }
        ControlFlow::Continue(())
    }
}
