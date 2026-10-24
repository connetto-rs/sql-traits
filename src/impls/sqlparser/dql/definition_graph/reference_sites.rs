//! The column references of a query, each with what it reaches, and how its
//! wildcards and `NATURAL` joins would be spelled out.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::{
    Distinct, Expr, GroupByExpr, Ident, ObjectName, ObjectNamePart, OrderByKind, Query, Select,
    SelectItem, SelectItemQualifiedWildcardKind, SetExpr, TableFactor, TableWithJoins, Visit,
    Visitor,
};

use super::{
    super::{
        ColumnOrigin, FromScope, RelationKey, WildcardEntry, build_definition_graph,
        expand_qualified_wildcard, factor_address, join_constraint, subsumed_exposure,
        wildcard_reshapes_output,
    },
    Bound, DefinitionGraph, DefinitionId, ScopeCursor, ScopeId,
    output_columns::unwrap_nested,
};
use crate::{errors::LookupError, structs::IdentifierCase, traits::DatabaseLike};

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
}

impl QueryReferences {
    /// Whether every reference reached something certain.
    pub(crate) fn certain(&self) -> bool {
        self.sites.iter().all(|site| site.outcome != SiteOutcome::Unknown)
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
    let mut walk =
        Walk { graph: &graph, references: QueryReferences::default(), origins: Vec::new() };
    walk.query(query);
    let Walk { mut references, origins, .. } = walk;
    references.sites = origins
        .into_iter()
        .map(|(expression, origin)| {
            let outcome = match origin {
                Reached::Origin(ColumnOrigin::Entry { factor, ordinal }) => {
                    references
                        .factors
                        .iter()
                        .position(|address| *address == factor)
                        .map_or(SiteOutcome::Unknown, |factor| {
                            SiteOutcome::Entry { factor, ordinal }
                        })
                }
                Reached::Origin(ColumnOrigin::Merged) => SiteOutcome::Merged,
                Reached::Origin(ColumnOrigin::Output) => SiteOutcome::Output,
                Reached::Origin(ColumnOrigin::Unknown) | Reached::Unknown => SiteOutcome::Unknown,
                Reached::Absent => SiteOutcome::Absent,
            };
            ReferenceSite { expression, outcome }
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

struct Walk<'graph, 'query, 'db, DB: DatabaseLike> {
    graph: &'graph DefinitionGraph<'query, 'db, DB>,
    references: QueryReferences,
    origins: Vec<(usize, Reached)>,
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

impl<DB: DatabaseLike> Walk<'_, '_, '_, DB> {
    fn record(&mut self, expression: &Expr, reached: Reached) {
        self.origins.push((expression_address(expression), reached));
    }

    fn cursor(&self, scope: Option<ScopeId>) -> Option<ScopeCursor> {
        scope.map(|scope| self.graph.select_cursor(scope))
    }

    /// Walks the references of any node with a select's scope, entering the
    /// queries nested in it on their own terms.
    fn node<N: Visit + ?Sized>(&mut self, node: &N, cursor: Option<ScopeCursor>) {
        let mut sites = Sites { walk: self, cursor, depth: 0 };
        let walked = node.visit(&mut sites);
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
                            let reached = Reached::of(&self.graph.resolve_order_by(query_id, bare));
                            self.record(bare, reached);
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

        let mut entries = 0;
        for table in from {
            self.factor(&table.relation, scope, entries);
            entries += 1;
            for join in &table.joins {
                self.factor(&join.relation, scope, entries);
                entries += 1;
                if let Some(constraint) = join_constraint(&join.join_operator) {
                    let joined = scope.map(|scope| ScopeCursor { scope, visible_entries: entries });
                    self.node(constraint, joined);
                }
            }
        }

        if let Some(Distinct::On(keys)) = distinct {
            for key in keys {
                let bare = unwrap_nested(key);
                match cursor {
                    Some(cursor) if is_column_name(bare) => {
                        let reached = Reached::of(&self.graph.resolve_output_first(cursor, bare));
                        self.record(bare, reached);
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
                        let reached = Reached::of(&self.graph.resolve_input_first(cursor, bare));
                        self.record(bare, reached);
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

    /// Records a `FROM` item and walks what it holds. `entries` counts the
    /// items before it, which is what a function argument may read.
    fn factor(&mut self, factor: &TableFactor, scope: Option<ScopeId>, entries: usize) {
        self.references.factors.push(factor_address(factor));
        match factor {
            TableFactor::Derived { subquery, .. } => self.query(subquery),
            // The resolver reads a parenthesized join as one opaque relation.
            TableFactor::NestedJoin { table_with_joins, .. } => self.nested_join(table_with_joins),
            other => {
                let preceding = scope.map(|scope| ScopeCursor { scope, visible_entries: entries });
                self.node(other, preceding);
            }
        }
    }

    fn nested_join(&mut self, table: &TableWithJoins) {
        self.factor(&table.relation, None, 0);
        for join in &table.joins {
            self.factor(&join.relation, None, 0);
            if let Some(constraint) = join_constraint(&join.join_operator) {
                self.node(constraint, None);
            }
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
struct Sites<'walk, 'graph, 'query, 'db, DB: DatabaseLike> {
    walk: &'walk mut Walk<'graph, 'query, 'db, DB>,
    cursor: Option<ScopeCursor>,
    /// How many queries deep the visitor stands below the node it started
    /// from, whose references the nested walk records instead.
    depth: usize,
}

impl<DB: DatabaseLike> Visitor for Sites<'_, '_, '_, '_, DB> {
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
                let reached = match self.cursor {
                    Some(cursor) => {
                        Reached::of(&self.walk.graph.resolve_binding(cursor, expression))
                    }
                    None => Reached::Unknown,
                };
                self.walk.record(expression, reached);
            }
            // A whole-row reference names a relation the resolver does not
            // follow.
            Expr::QualifiedWildcard(..) => self.walk.record(expression, Reached::Unknown),
            _ => {}
        }
        ControlFlow::Continue(())
    }
}
