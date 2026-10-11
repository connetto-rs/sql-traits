//! Implementation of [`DQLLike`] for sqlparser's `Query` node.
//!
//! The `FROM` scope of a query's outer `SELECT` resolves each base-table
//! relation and derives the output columns of CTE references and derived
//! subqueries from their own definitions, since the projection that produces
//! them sits in the same statement. `projection_source_table` then resolves
//! each projected item to the base table it comes from. A derived relation
//! only feeds that row-identity answer when its body preserves row identity
//! (no grouping, deduplication, or window filtering), so a consumer
//! re-executing rows against the source table is never handed a computed or
//! collapsed row. It is deliberately strict: anything it cannot prove comes
//! from a single base table yields `Ok(None)`, and a table function, an
//! unresolvable name, or a body whose columns cannot be enumerated (an
//! unnamed computed projection item, a nested join) stays opaque.

use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    AccessExpr, CaseWhen, ConnectByKind, Cte, DictionaryField, Distinct, Expr, ExprWithAlias,
    Function, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArguments, GroupByExpr,
    Ident, Join, JoinConstraint, JoinOperator, JsonPathElem, LimitClause, MapEntry, Measure,
    NamedWindowExpr, ObjectName, OrderBy, OrderByExpr, OrderByKind, PipeOperator, PivotValueSource,
    Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator,
    SetQuantifier, Subscript, SymbolDefinition, TableAlias, TableAliasColumnDef, TableFactor,
    TableFunctionArgs, TableSample, TableSampleKind, TableVersion, TableWithJoins, TopQuantity,
    Values, Visit, Visitor, WildcardAdditionalOptions, WindowFrameBound, WindowSpec, WindowType,
    With, XmlNamespaceDefinition, XmlPassingClause, XmlTableColumn, XmlTableColumnOption,
};

use crate::{
    errors::LookupError,
    structs::{IdentifierCase, TargetName},
    traits::{ColumnLike, DQLLike, DatabaseLike, TableLike, ViewLike},
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{
            object_name_last_part, qualifier_of, render_table_candidate, resolve_bound_table,
            resolve_object_name, target_name_from_object_name,
        },
    },
};

pub(crate) mod definition_graph;

use definition_graph::{AstRef, DefinitionDerivation, ScopeCursor};

/// A view whose definition is being derived right now.
///
/// A `FROM` reference reaching one of these is a cycle, which PostgreSQL
/// accepts at creation and only refuses when the view is read, so the model
/// has to terminate on it rather than assume it cannot happen. The reference
/// stays opaque, exactly as a recursive CTE's self-reference does.
#[derive(Clone)]
enum DerivingView<'db, DB: DatabaseLike> {
    /// A plain view.
    Plain(&'db DB::View),
    /// A materialized view.
    Materialized(&'db DB::MaterializedView),
}

impl<DB: DatabaseLike> Copy for DerivingView<'_, DB> {}

impl<DB: DatabaseLike> DerivingView<'_, DB> {
    /// Whether both name the same recorded view.
    ///
    /// Compared by identity rather than by name, since a view's address in the
    /// database it was read from is what makes it that view.
    fn is(self, other: Self) -> bool {
        match (self, other) {
            (Self::Plain(left), Self::Plain(right)) => core::ptr::eq(left, right),
            (Self::Materialized(left), Self::Materialized(right)) => core::ptr::eq(left, right),
            _ => false,
        }
    }
}

/// One view on the chain a derivation is currently inside, linked to the one
/// that reached it.
///
/// A frame lives in the stack of the call that pushed it, which outlives every
/// call it makes, so extending the chain costs nothing. Holding the chain as a
/// slice instead meant copying it to a fresh allocation for every view
/// reference a query resolves.
struct DerivingFrame<'s, 'db, DB: DatabaseLike> {
    /// The view this frame stands for.
    view: DerivingView<'db, DB>,
    /// The frame that reached it, if any.
    parent: Option<&'s DerivingFrame<'s, 'db, DB>>,
}

/// The database a derivation reads, and the views whose definitions it is
/// already inside.
#[derive(Clone)]
struct Deriving<'s, 'db, DB: DatabaseLike> {
    /// The database every relation name resolves against.
    database: &'db DB,
    /// The innermost view being derived right now, if any.
    views: Option<&'s DerivingFrame<'s, 'db, DB>>,
}

impl<DB: DatabaseLike> Copy for Deriving<'_, '_, DB> {}

impl<'db, DB: DatabaseLike> Deriving<'_, 'db, DB> {
    /// A derivation of `database` that is inside no view.
    fn of(database: &'db DB) -> Self {
        Self { database, views: None }
    }

    /// Whether a reference to `view` would close a cycle.
    ///
    /// Walks the chain, whose length is the view nesting depth: a view appears
    /// on it at most once, since the second appearance is what this reports.
    fn is_deriving(&self, view: DerivingView<'db, DB>) -> bool {
        let mut frame = self.views;
        while let Some(entry) = frame {
            if entry.view.is(view) {
                return true;
            }
            frame = entry.parent;
        }
        false
    }

    /// The base table a written relation name denotes here.
    ///
    /// A query handed in by a caller resolves through the session's search
    /// path. A stored view's definition was bound when the view was created,
    /// so inside one the name is read as written instead, which keeps it on
    /// the relation it reached then.
    fn table(&self, name: &ObjectName) -> Result<Option<&'db DB::Table>, LookupError> {
        if self.views.is_some() {
            resolve_bound_table(name, self.database)
        } else {
            resolve_object_name(name, self.database)
        }
    }

    /// The plain view a written name denotes here, read the way
    /// [`Self::table`] reads a table.
    fn view(&self, target: TargetName<'_>) -> Result<Option<&'db DB::View>, LookupError> {
        if self.views.is_some() {
            self.database.view_by_target(target, IdentifierCase::AsWritten)
        } else {
            self.database.resolve_target_view(target, IdentifierCase::AsWritten)
        }
    }

    /// The materialized view a written name denotes here, read the way
    /// [`Self::table`] reads a table.
    fn materialized_view(
        &self,
        target: TargetName<'_>,
    ) -> Result<Option<&'db DB::MaterializedView>, LookupError> {
        if self.views.is_some() {
            self.database.materialized_view_by_target(target, IdentifierCase::AsWritten)
        } else {
            self.database.resolve_target_materialized_view(target, IdentifierCase::AsWritten)
        }
    }
}

/// One output column of a base relation.
struct BaseColumnRef<D: Copy> {
    name: String,
    quoted: bool,
    definition: D,
}

type BaseColumns<D> = Vec<BaseColumnRef<D>>;

#[derive(Clone, Copy)]
struct RelationKey<'query, 'db> {
    value: AstRef<'query, 'db, str>,
    quoted: bool,
}

struct FromTableRef<'query, 'db, DB: DatabaseLike, D: Copy> {
    key: RelationKey<'query, 'db>,
    schema_key: Option<RelationKey<'query, 'db>>,
    table: &'db DB::Table,
    nullable: bool,
    entry_index: usize,
    output_columns: BaseColumns<D>,
}

/// One output column of a derivable relation.
struct DerivedColumn<'query, 'db, DB: DatabaseLike, D: Copy> {
    name: String,
    quoted: bool,
    source: Option<&'db DB::Table>,
    definition: D,
    marker: core::marker::PhantomData<&'query ()>,
}

impl<DB: DatabaseLike, D: Copy> Clone for DerivedColumn<'_, '_, DB, D> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            quoted: self.quoted,
            source: self.source,
            definition: self.definition,
            marker: core::marker::PhantomData,
        }
    }
}

enum OutputNameSource<'names, 'query, 'db, DB: DatabaseLike, D: Copy> {
    Columns(&'names [DerivedColumn<'query, 'db, DB, D>]),
    AliasColumns(AstRef<'query, 'db, [TableAliasColumnDef]>),
    Declared(&'names [(String, bool)]),
}

impl<'names, DB: DatabaseLike, D: Copy> OutputNameSource<'names, '_, '_, DB, D> {
    fn from_alias<'query, 'db>(
        alias: AstRef<'query, 'db, TableAlias>,
    ) -> Option<OutputNameSource<'names, 'query, 'db, DB, D>>
    where
        'query: 'names,
        'db: 'names,
    {
        (!alias.get().columns.is_empty())
            .then_some(OutputNameSource::AliasColumns(alias.map(|alias| alias.columns.as_slice())))
    }

    fn get(&self, ordinal: usize) -> Option<(String, bool)> {
        match self {
            Self::Columns(columns) => {
                columns.get(ordinal).map(|column| (column.name.clone(), column.quoted))
            }
            Self::AliasColumns(columns) => {
                columns
                    .get()
                    .get(ordinal)
                    .map(|column| (column.name.value.clone(), column.name.quote_style.is_some()))
            }
            Self::Declared(names) => names.get(ordinal).cloned(),
        }
    }
}

/// The derivable output shape of a relation.
struct DerivedShape<'query, 'db, DB: DatabaseLike, D: Copy> {
    columns: Vec<DerivedColumn<'query, 'db, DB, D>>,
    row_preserving: bool,
}

impl<DB: DatabaseLike, D: Copy> Clone for DerivedShape<'_, '_, DB, D> {
    fn clone(&self) -> Self {
        Self { columns: self.columns.clone(), row_preserving: self.row_preserving }
    }
}

struct DerivedRelationRef<'query, 'db, DB: DatabaseLike, D: Copy> {
    key: Option<RelationKey<'query, 'db>>,
    shape: DerivedShape<'query, 'db, DB, D>,
    nullable: bool,
    entry_index: usize,
}

struct CteShape<'query, 'db, DB: DatabaseLike, D: Copy> {
    name: RelationKey<'query, 'db>,
    shape: Option<DerivedShape<'query, 'db, DB, D>>,
}

impl<DB: DatabaseLike, D: Copy> Clone for CteShape<'_, '_, DB, D> {
    fn clone(&self) -> Self {
        Self { name: self.name, shape: self.shape.clone() }
    }
}

#[derive(Clone, Copy)]
struct OpaqueQualifier<'query, 'db> {
    key: RelationKey<'query, 'db>,
    schema: Option<RelationKey<'query, 'db>>,
}

enum OpaqueIdentity<'query, 'db> {
    Known { key: RelationKey<'query, 'db>, schema: Option<RelationKey<'query, 'db>> },
    Multiple(Vec<OpaqueQualifier<'query, 'db>>),
    Anonymous,
}

struct OpaqueRelation<'query, 'db> {
    identity: OpaqueIdentity<'query, 'db>,
    entry_index: usize,
}

struct CteDependencyVisitor<'a> {
    candidates: &'a [Cte],
    shadowed: Vec<Vec<Ident>>,
    dependencies: Vec<bool>,
}

impl Visitor for CteDependencyVisitor<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.shadowed.push(query.with.as_ref().map_or_else(Vec::new, |with| {
            with.cte_tables.iter().map(|cte| cte.alias.name.clone()).collect()
        }));
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.shadowed.truncate(self.shadowed.len().saturating_sub(1));
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
        if relation.0.len() != 1 {
            return ControlFlow::Continue(());
        }
        let Some((value, quoted)) = object_name_last_part(relation) else {
            return ControlFlow::Continue(());
        };
        if self.shadowed.iter().rev().flatten().any(|ident| {
            identifiers_match(&ident.value, ident.quote_style.is_some(), value, quoted)
        }) {
            return ControlFlow::Continue(());
        }
        if let Some(position) = self.candidates.iter().position(|cte| {
            identifiers_match(
                &cte.alias.name.value,
                cte.alias.name.quote_style.is_some(),
                value,
                quoted,
            )
        }) {
            self.dependencies[position] = true;
        }
        ControlFlow::Continue(())
    }
}

fn cte_dependencies(cte: &Cte, candidates: &[Cte]) -> Vec<bool> {
    let mut visitor = CteDependencyVisitor {
        candidates,
        shadowed: Vec::new(),
        dependencies: vec![false; candidates.len()],
    };
    let _: ControlFlow<()> = cte.query.visit(&mut visitor);
    visitor.dependencies
}

fn cte_reaches(adjacency: &[Vec<bool>], from: usize, target: usize) -> bool {
    let mut visited = vec![false; adjacency.len()];
    let mut pending = vec![from];
    visited[from] = true;
    while let Some(current) = pending.pop() {
        for (next, follows) in adjacency[current].iter().copied().enumerate() {
            if !follows || visited[next] {
                continue;
            }
            if next == target {
                return true;
            }
            visited[next] = true;
            pending.push(next);
        }
    }
    false
}

fn mutually_recursive_ctes(with: &With) -> Vec<bool> {
    let adjacency: Vec<Vec<bool>> =
        with.cte_tables.iter().map(|cte| cte_dependencies(cte, &with.cte_tables)).collect();
    adjacency
        .iter()
        .enumerate()
        .map(|(left, _)| {
            adjacency.iter().enumerate().any(|(right, _)| {
                left != right
                    && cte_reaches(&adjacency, left, right)
                    && cte_reaches(&adjacency, right, left)
            })
        })
        .collect()
}
/// A column name merged by a `USING` or `NATURAL` join. `subsumed` is the
/// number of `FROM` entries the merge consumed: relations collected before
/// that boundary pass their exposure of the name into the merged column and
/// no longer count individually, while relations joined in afterwards collide
/// with it, as PostgreSQL reports for a bare reference.
struct MergedName {
    name: String,
    quoted: bool,
    subsumed: usize,
}

/// One output position of a `FROM` item's join chain as a `*` projection
/// sees it: a base relation (index into `FromScope::bases`), a derived
/// relation (index into `FromScope::derived`), or a column merged by a
/// `USING` or `NATURAL` join, whose coalesced value has no single source.
enum WildcardEntry {
    Base(usize),
    Derived(usize),
    Merged { name: String, quoted: bool },
}

struct FromScope<'query, 'db, DB: DatabaseLike, D: Copy> {
    bases: Vec<FromTableRef<'query, 'db, DB, D>>,
    derived: Vec<DerivedRelationRef<'query, 'db, DB, D>>,
    merged: Vec<MergedName>,
    wildcard_plans: Vec<Vec<WildcardEntry>>,
    opaque: Vec<OpaqueRelation<'query, 'db>>,
    from_entry_count: usize,
    unqualified_poison: bool,
}

impl<DB: DatabaseLike, D: Copy> FromScope<'_, '_, DB, D> {
    fn new() -> Self {
        Self {
            bases: Vec::new(),
            derived: Vec::new(),
            merged: Vec::new(),
            wildcard_plans: Vec::new(),
            opaque: Vec::new(),
            from_entry_count: 0,
            unqualified_poison: false,
        }
    }

    fn has_opaque(&self) -> bool {
        self.unqualified_poison || !self.opaque.is_empty()
    }
}

trait DerivationProfile<'query, 'db, DB: DatabaseLike> {
    type Definition: Copy;
    type Scope;
    type Cursor: Copy;
    type Checkpoint: Copy;

    const INDEX_NESTED_QUERIES: bool;

    fn no_parent(&self) -> Self::Cursor;
    fn begin_scope(
        &mut self,
        select: AstRef<'query, 'db, Select>,
        parent: Self::Cursor,
    ) -> Self::Scope;
    fn scope<'scope>(
        &'scope self,
        scope: &'scope Self::Scope,
    ) -> &'scope FromScope<'query, 'db, DB, Self::Definition>;
    fn scope_mut<'scope>(
        &'scope mut self,
        scope: &'scope mut Self::Scope,
    ) -> &'scope mut FromScope<'query, 'db, DB, Self::Definition>;
    fn cursor(&self, scope: &Self::Scope) -> Self::Cursor;
    fn cursor_for_select(&self, select: AstRef<'query, 'db, Select>) -> Option<Self::Cursor>;
    fn set_scope_owners_since(
        &mut self,
        checkpoint: Self::Checkpoint,
        inherited: Self::Cursor,
        owner: Self::Cursor,
    );
    fn opaque_definition(&self) -> Self::Definition;
    fn base_definition(
        &mut self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Self::Definition;
    fn expression_definition(
        &mut self,
        expression: AstRef<'query, 'db, Expr>,
        scope: Self::Cursor,
    ) -> Result<Self::Definition, LookupError>;
    fn set_definition(
        &mut self,
        operator: SetOperator,
        left: Self::Definition,
        right: Self::Definition,
    ) -> Self::Definition;
    fn recursive_definition(
        &mut self,
        anchor: Self::Definition,
        recursive: Self::Definition,
    ) -> Self::Definition;
    fn checkpoint(&self) -> Self::Checkpoint;
}

struct SourceDerivation;

impl<'query, 'db, DB> DerivationProfile<'query, 'db, DB> for SourceDerivation
where
    DB: DatabaseLike,
    DB::Table: 'db,
{
    type Definition = ();
    type Scope = FromScope<'query, 'db, DB, ()>;
    type Cursor = ();
    type Checkpoint = ();

    const INDEX_NESTED_QUERIES: bool = false;

    fn no_parent(&self) {}

    fn begin_scope(
        &mut self,
        _select: AstRef<'query, 'db, Select>,
        _parent: Self::Cursor,
    ) -> Self::Scope {
        FromScope::new()
    }

    fn scope<'scope>(
        &'scope self,
        scope: &'scope Self::Scope,
    ) -> &'scope FromScope<'query, 'db, DB, Self::Definition> {
        scope
    }

    fn scope_mut<'scope>(
        &'scope mut self,
        scope: &'scope mut Self::Scope,
    ) -> &'scope mut FromScope<'query, 'db, DB, Self::Definition> {
        scope
    }

    fn cursor(&self, _scope: &Self::Scope) {}

    fn cursor_for_select(&self, _select: AstRef<'query, 'db, Select>) -> Option<Self::Cursor> {
        None
    }

    fn set_scope_owners_since(
        &mut self,
        _checkpoint: Self::Checkpoint,
        _inherited: Self::Cursor,
        _owner: Self::Cursor,
    ) {
    }

    fn opaque_definition(&self) {}

    fn base_definition(&mut self, _table: &'db DB::Table, _column: &'db DB::Column) {}

    fn expression_definition(
        &mut self,
        _expression: AstRef<'query, 'db, Expr>,
        _scope: Self::Cursor,
    ) -> Result<Self::Definition, LookupError> {
        Ok(())
    }

    fn set_definition(
        &mut self,
        _operator: SetOperator,
        _left: Self::Definition,
        _right: Self::Definition,
    ) {
    }

    fn recursive_definition(&mut self, _anchor: Self::Definition, _recursive: Self::Definition) {}

    fn checkpoint(&self) {}
}

enum RelationContribution<'query, 'db, DB: DatabaseLike, D: Copy> {
    Base(FromTableRef<'query, 'db, DB, D>),
    Derived(DerivedRelationRef<'query, 'db, DB, D>),
    Opaque(OpaqueIdentity<'query, 'db>),
}

/// What one `FROM` factor contributes: the output names it exposes and the
/// wildcard plan entry naming what it pushed.
struct FactorContribution<'query, 'db, DB: DatabaseLike, D: Copy> {
    relation: RelationContribution<'query, 'db, DB, D>,
    names: Option<Vec<(String, bool)>>,
}

type FactorOutput = (Vec<(String, bool)>, Vec<WildcardEntry>);

fn append_factor<'query, 'db, DB, P>(
    profile: &mut P,
    scope: &mut P::Scope,
    contribution: FactorContribution<'query, 'db, DB, P::Definition>,
) -> Option<FactorOutput>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let data = profile.scope_mut(scope);
    let entry_index = data.from_entry_count;
    data.from_entry_count += 1;
    let entry = match contribution.relation {
        RelationContribution::Base(mut relation) => {
            relation.entry_index = entry_index;
            let entry = WildcardEntry::Base(data.bases.len());
            data.bases.push(relation);
            Some(entry)
        }
        RelationContribution::Derived(mut relation) => {
            relation.entry_index = entry_index;
            let entry = WildcardEntry::Derived(data.derived.len());
            data.derived.push(relation);
            Some(entry)
        }
        RelationContribution::Opaque(identity) => {
            data.opaque.push(OpaqueRelation { identity, entry_index });
            None
        }
    };
    contribution.names.map(|names| (names, entry.into_iter().collect()))
}

fn collect_source_from_clause<'query, 'db, DB: DatabaseLike>(
    query: &'query Query,
    database: &'db DB,
) -> Result<Option<FromScope<'query, 'db, DB, ()>>, LookupError> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    let deriving = Deriving::of(database);
    let mut profile = SourceDerivation;
    let parent = ();
    let cte_scope = match &query.with {
        Some(with) => derive_cte_shapes(AstRef::Query(with), &[], deriving, parent, &mut profile)?,
        None => Vec::new(),
    };
    collect_select_from(AstRef::Query(select), &cte_scope, deriving, parent, &mut profile).map(Some)
}

fn collect_select_from<'query, 'db, DB, P>(
    select: AstRef<'query, 'db, Select>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<P::Scope, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let mut scope = profile.begin_scope(select, parent);
    for table_with_joins in select.map(|select| select.from.as_slice()).iter() {
        let (entry_bases, entry_derived) = {
            let data = profile.scope(&scope);
            (data.bases.len(), data.derived.len())
        };
        let local_parent = profile.cursor(&scope);
        if P::INDEX_NESTED_QUERIES {
            index_table_factor_expression_queries(
                table_with_joins.map(|entry| &entry.relation),
                cte_scope,
                deriving,
                parent,
                local_parent,
                profile,
            )?;
        }
        let contribution = collect_factor(
            table_with_joins.map(|entry| &entry.relation),
            deriving,
            cte_scope,
            parent,
            local_parent,
            profile,
        )?;
        let (mut accumulated, mut plan) = match append_factor(profile, &mut scope, contribution) {
            Some((names, entries)) => (Some(names), Some(entries)),
            None => (None, None),
        };
        for join in table_with_joins.map(|entry| entry.joins.as_slice()).iter() {
            let (bases_before, derived_before) = {
                let data = profile.scope(&scope);
                (data.bases.len(), data.derived.len())
            };
            let local_parent = profile.cursor(&scope);
            if P::INDEX_NESTED_QUERIES {
                index_table_factor_expression_queries(
                    join.map(|join| &join.relation),
                    cte_scope,
                    deriving,
                    parent,
                    local_parent,
                    profile,
                )?;
            }
            let contribution = collect_factor(
                join.map(|join| &join.relation),
                deriving,
                cte_scope,
                parent,
                local_parent,
                profile,
            )?;
            let (right_names, right_entries) =
                match append_factor(profile, &mut scope, contribution) {
                    Some((names, entries)) => (Some(names), Some(entries)),
                    None => (None, None),
                };
            if P::INDEX_NESTED_QUERIES {
                let local_parent = profile.cursor(&scope);
                index_join_expression_queries(join, cte_scope, deriving, local_parent, profile)?;
            }
            let (left_nullable, right_nullable) = nullable_sides(&join.get().join_operator);
            let data = profile.scope_mut(&mut scope);
            if left_nullable {
                for base in &mut data.bases[entry_bases..bases_before] {
                    base.nullable = true;
                }
                for relation in &mut data.derived[entry_derived..derived_before] {
                    relation.nullable = true;
                }
            }
            if right_nullable {
                for base in &mut data.bases[bases_before..] {
                    base.nullable = true;
                }
                for relation in &mut data.derived[derived_before..] {
                    relation.nullable = true;
                }
            }
            let mut merged = Vec::new();
            if let Some(names) = merge_names(
                &join.get().join_operator,
                accumulated.as_deref(),
                right_names.as_deref(),
            ) {
                let boundary = data.from_entry_count;
                for (name, quoted) in &names {
                    merge_name(&mut data.merged, name.clone(), *quoted, boundary);
                }
                merged = names;
            } else {
                data.unqualified_poison = true;
            }
            plan = merge_plans(plan, right_entries, &merged);
            accumulated = merge_output_names(accumulated, right_names, &merged);
        }
        profile.scope_mut(&mut scope).wildcard_plans.push(plan.unwrap_or_default());
    }
    Ok(append_unmodeled_relations(select, profile, scope))
}

/// Makes lookups opaque where `LATERAL VIEW` relations or `CONNECT BY`
/// pseudo-columns could answer.
fn append_unmodeled_relations<'query, 'db, DB, P>(
    select: AstRef<'query, 'db, Select>,
    profile: &mut P,
    mut scope: P::Scope,
) -> P::Scope
where
    DB: DatabaseLike,
    DB::Table: 'db,
    P: DerivationProfile<'query, 'db, DB>,
{
    for view in select.map(|select| select.lateral_views.as_slice()).iter() {
        let identity = object_name_key(view.map(|view| &view.lateral_view_name))
            .map_or(OpaqueIdentity::Anonymous, |key| OpaqueIdentity::Known { key, schema: None });
        append_factor(profile, &mut scope, opaque_factor(identity));
    }
    if !select.get().connect_by.is_empty() {
        profile.scope_mut(&mut scope).unqualified_poison = true;
    }
    scope
}

/// Whether each side of a join can be null-extended: `(left, right)`. An
/// inner, cross, semi, or anti join extends nothing. `LEFT` null-extends the
/// joined relation, `RIGHT` the accumulated relation, `FULL` both, and
/// `OUTER APPLY` the applied relation.
fn nullable_sides(operator: &JoinOperator) -> (bool, bool) {
    match operator {
        JoinOperator::Left(_) | JoinOperator::LeftOuter(_) | JoinOperator::OuterApply => {
            (false, true)
        }
        JoinOperator::Right(_) | JoinOperator::RightOuter(_) => (true, false),
        JoinOperator::FullOuter(_) => (true, true),
        _ => (false, false),
    }
}

/// The column names a join merges: a `USING` list directly, a `NATURAL`
/// join as the intersection of the two sides' output names, each name once.
/// A `NATURAL` join whose sides' columns cannot be enumerated reports `None`
/// so the caller marks the scope opaque rather than guessing that a shared
/// name is unmerged.
fn merge_names(
    operator: &JoinOperator,
    left: Option<&[(String, bool)]>,
    right: Option<&[(String, bool)]>,
) -> Option<Vec<(String, bool)>> {
    let mut merged: Vec<(String, bool)> = Vec::new();
    match join_constraint(operator) {
        None | Some(JoinConstraint::On(_) | JoinConstraint::None) => {}
        Some(JoinConstraint::Using(names)) => {
            for name in names {
                if let Some((value, quoted)) = object_name_last_part(name) {
                    push_unseen(&mut merged, value, quoted);
                }
            }
        }
        Some(JoinConstraint::Natural) => {
            let (Some(left_names), Some(right_names)) = (left, right) else {
                return None;
            };
            for (name, quoted) in left_names {
                if right_names.iter().any(|(other, other_quoted)| {
                    identifiers_match(other, *other_quoted, name, *quoted)
                }) {
                    push_unseen(&mut merged, name, *quoted);
                }
            }
        }
    }
    Some(merged)
}

/// Appends an identifier unless the list already carries it.
fn push_unseen(names: &mut Vec<(String, bool)>, value: &str, quoted: bool) {
    if !names.iter().any(|(name, known)| identifiers_match(name, *known, value, quoted)) {
        names.push((value.to_string(), quoted));
    }
}

/// The wildcard plan after one join, in PostgreSQL's join output order: the
/// merged columns (in list order), then the accumulated plan's entries that
/// are not an earlier `Merged` entry of the same name, then the joined
/// relation's. A re-merge of a name therefore drops the older merged entry
/// and re-prepends it at the latest join's position. A poisoned side poisons
/// the item's whole plan.
fn merge_plans(
    left: Option<Vec<WildcardEntry>>,
    right: Option<Vec<WildcardEntry>>,
    merged: &[(String, bool)],
) -> Option<Vec<WildcardEntry>> {
    let (left, right) = (left?, right?);
    let mut next: Vec<WildcardEntry> = merged
        .iter()
        .map(|(name, quoted)| WildcardEntry::Merged { name: name.clone(), quoted: *quoted })
        .collect();
    let remerged = |entry: &WildcardEntry| {
        matches!(entry, WildcardEntry::Merged { name, quoted }
        if merged.iter().any(|(merged, merged_quoted)| {
            identifiers_match(merged, *merged_quoted, name, *quoted)
        }))
    };
    next.extend(left.into_iter().filter(|entry| !remerged(entry)));
    next.extend(right.into_iter().filter(|entry| !remerged(entry)));
    Some(next)
}

/// The accumulated output names after one join: the merged names first, then
/// each side's names that are not merged. Membership matches the names before
/// the join (a merged name still stands, as the join's own coalesced column),
/// so `NATURAL` intersections are unchanged. Only the order follows the
/// output. A poisoned side poisons the accumulated list.
fn merge_output_names(
    left: Option<Vec<(String, bool)>>,
    right: Option<Vec<(String, bool)>>,
    merged: &[(String, bool)],
) -> Option<Vec<(String, bool)>> {
    let (left, right) = (left?, right?);
    let unmerged = |entry: &&(String, bool)| {
        !merged.iter().any(|(merged, merged_quoted)| {
            identifiers_match(merged, *merged_quoted, &entry.0, entry.1)
        })
    };
    let mut names: Vec<(String, bool)> = merged.to_vec();
    names.extend(left.iter().filter(unmerged).cloned());
    names.extend(right.iter().filter(unmerged).cloned());
    Some(names)
}

/// The constraint carried by a join operator, if it carries one.
fn join_constraint(operator: &JoinOperator) -> Option<&JoinConstraint> {
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

/// Records a merged name, moving its subsumption boundary to the latest
/// merge of that name.
fn merge_name(merged: &mut Vec<MergedName>, name: String, quoted: bool, subsumed: usize) {
    match merged
        .iter_mut()
        .find(|merged| identifiers_match(&merged.name, merged.quoted, &name, quoted))
    {
        Some(existing) => existing.subsumed = subsumed,
        None => merged.push(MergedName { name, quoted, subsumed }),
    }
}

/// The subsumption boundary for this identifier if it is merged.
fn merged_boundary(merged: &[MergedName], name: &str, quoted: bool) -> Option<usize> {
    merged
        .iter()
        .find(|merged| identifiers_match(&merged.name, merged.quoted, name, quoted))
        .map(|merged| merged.subsumed)
}

/// Whether a relation's own copy of this column name is absorbed into a
/// merged join column: true when the relation was collected before that
/// name's merge.
fn subsumed_exposure(merged: &[MergedName], entry_index: usize, name: &str, quoted: bool) -> bool {
    merged_boundary(merged, name, quoted).is_some_and(|boundary| entry_index < boundary)
}

/// Derives the shapes of a `WITH` clause's CTEs in order. A recursive list
/// binds every name before any body is resolved. Multi-CTE cycles and forward
/// references stay opaque, while a self-recursive CTE is seeded from its
/// nonrecursive term. A non-recursive list registers names one by one.
fn derive_cte_shapes<'query, 'db, DB, P>(
    with: AstRef<'query, 'db, With>,
    outer: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<Vec<CteShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let mut shapes = outer.to_vec();
    #[cfg(test)]
    tests::record_cte_shape_derivation();
    let base = shapes.len();
    let mutually_recursive = if with.get().recursive {
        mutually_recursive_ctes(with.get())
    } else {
        vec![false; with.get().cte_tables.len()]
    };
    if with.get().recursive {
        for cte in with.map(|with| with.cte_tables.as_slice()).iter() {
            let alias = cte.map(|cte| &cte.alias);
            shapes.push(CteShape {
                name: RelationKey {
                    value: alias.map(|alias| alias.name.value.as_str()),
                    quoted: alias.get().name.quote_style.is_some(),
                },
                shape: None,
            });
        }
    }
    for (index, cte) in with.map(|with| with.cte_tables.as_slice()).iter().enumerate() {
        let position = if with.get().recursive { base + index } else { shapes.len() };
        if with.get().recursive {
            if mutually_recursive[index] {
                if P::INDEX_NESTED_QUERIES {
                    index_nested_query_scopes(
                        cte.map(|cte| cte.query.as_ref()),
                        &shapes,
                        deriving,
                        parent,
                        profile,
                    )?;
                }
                continue;
            }
            let body = derive_recursive_cte_query_shape(
                cte.map(|cte| cte.query.as_ref()),
                &mut shapes,
                position,
                cte.map(|cte| &cte.alias),
                deriving,
                parent,
                profile,
            )?;
            shapes[position].shape = apply_alias_columns(body, &cte.get().alias);
        } else {
            let alias = cte.map(|cte| &cte.alias);
            shapes.push(CteShape {
                name: RelationKey {
                    value: alias.map(|alias| alias.name.value.as_str()),
                    quoted: alias.get().name.quote_style.is_some(),
                },
                shape: None,
            });
            let body = derive_query_shape(
                cte.map(|cte| cte.query.as_ref()),
                &shapes,
                OutputNameSource::from_alias(alias),
                deriving,
                parent,
                profile,
            )?;
            shapes[position].shape = apply_alias_columns(body, alias.get());
        }
    }
    Ok(shapes)
}

enum SetExprRef<'query, 'db> {
    Select(AstRef<'query, 'db, Select>),
    Query(AstRef<'query, 'db, Query>),
    Values(AstRef<'query, 'db, Values>),
    SetOperation {
        operator: SetOperator,
        quantifier: SetQuantifier,
        left: AstRef<'query, 'db, SetExpr>,
        right: AstRef<'query, 'db, SetExpr>,
    },
    Other,
}

fn set_expr_ref<'query, 'db>(body: AstRef<'query, 'db, SetExpr>) -> SetExprRef<'query, 'db> {
    match body {
        AstRef::Query(SetExpr::Select(select)) => SetExprRef::Select(AstRef::Query(select)),
        AstRef::Database(SetExpr::Select(select)) => SetExprRef::Select(AstRef::Database(select)),
        AstRef::Query(SetExpr::Query(query)) => SetExprRef::Query(AstRef::Query(query)),
        AstRef::Database(SetExpr::Query(query)) => SetExprRef::Query(AstRef::Database(query)),
        AstRef::Query(SetExpr::SetOperation { op, set_quantifier, left, right }) => {
            SetExprRef::SetOperation {
                operator: *op,
                quantifier: *set_quantifier,
                left: AstRef::Query(left),
                right: AstRef::Query(right),
            }
        }
        AstRef::Database(SetExpr::SetOperation { op, set_quantifier, left, right }) => {
            SetExprRef::SetOperation {
                operator: *op,
                quantifier: *set_quantifier,
                left: AstRef::Database(left),
                right: AstRef::Database(right),
            }
        }
        AstRef::Query(SetExpr::Values(values)) => SetExprRef::Values(AstRef::Query(values)),
        AstRef::Database(SetExpr::Values(values)) => SetExprRef::Values(AstRef::Database(values)),
        AstRef::Query(
            SetExpr::Insert(_)
            | SetExpr::Update(_)
            | SetExpr::Delete(_)
            | SetExpr::Merge(_)
            | SetExpr::Table(_),
        )
        | AstRef::Database(
            SetExpr::Insert(_)
            | SetExpr::Update(_)
            | SetExpr::Delete(_)
            | SetExpr::Merge(_)
            | SetExpr::Table(_),
        ) => SetExprRef::Other,
    }
}

fn derived_body_cursor<'query, 'db, DB, P>(
    body: AstRef<'query, 'db, SetExpr>,
    parent: P::Cursor,
    profile: &P,
) -> P::Cursor
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match set_expr_ref(body) {
        SetExprRef::Select(select) => profile.cursor_for_select(select).unwrap_or(parent),
        SetExprRef::Query(query) => {
            derived_body_cursor(query.map(|query| query.body.as_ref()), parent, profile)
        }
        SetExprRef::Values(_) | SetExprRef::SetOperation { .. } | SetExprRef::Other => parent,
    }
}

fn derive_recursive_cte_query_shape<'query, 'db, DB, P>(
    query: AstRef<'query, 'db, Query>,
    cte_scope: &mut Vec<CteShape<'query, 'db, DB, P::Definition>>,
    position: usize,
    alias: AstRef<'query, 'db, TableAlias>,
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<Option<DerivedShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let mut scoped;
    let scope = if let Some(with) = query.try_map(|query| query.with.as_ref()) {
        scoped = derive_cte_shapes(with, cte_scope, deriving, parent, profile)?;
        &mut scoped
    } else {
        cte_scope
    };
    let shape = derive_recursive_cte_set_expr_shape(
        query.map(|query| query.body.as_ref()),
        scope,
        position,
        alias,
        deriving,
        parent,
        profile,
    )?;
    if P::INDEX_NESTED_QUERIES {
        let cursor = derived_body_cursor(query.map(|query| query.body.as_ref()), parent, profile);
        index_query_expression_queries(query, scope, deriving, cursor, profile)?;
    }
    Ok(shape)
}

fn derive_recursive_cte_set_expr_shape<'query, 'db, DB, P>(
    body: AstRef<'query, 'db, SetExpr>,
    cte_scope: &mut Vec<CteShape<'query, 'db, DB, P::Definition>>,
    position: usize,
    alias: AstRef<'query, 'db, TableAlias>,
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<Option<DerivedShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let output_names = OutputNameSource::from_alias(alias);
    match set_expr_ref(body) {
        SetExprRef::Query(query) => {
            derive_recursive_cte_query_shape(
                query, cte_scope, position, alias, deriving, parent, profile,
            )
        }
        SetExprRef::SetOperation { operator: SetOperator::Union, quantifier, left, right } => {
            let left_shape =
                derive_set_expr_shape(left, cte_scope, output_names, deriving, parent, profile)?;
            cte_scope[position].shape = apply_alias_columns(left_shape, alias.get());
            if cte_scope[position].shape.is_none() && !P::INDEX_NESTED_QUERIES {
                return Ok(None);
            }
            let right_names = cte_scope[position]
                .shape
                .as_ref()
                .map(|left_shape| OutputNameSource::Columns(&left_shape.columns));
            let right_shape =
                derive_set_expr_shape(right, cte_scope, right_names, deriving, parent, profile)?;
            let (Some(left_shape), Some(right_shape)) =
                (cte_scope[position].shape.take(), right_shape)
            else {
                return Ok(None);
            };
            Ok(merge_set_operation_shapes(
                left_shape,
                right_shape,
                SetOperator::Union,
                quantifier,
                true,
                deriving,
                profile,
            ))
        }
        _ => derive_set_expr_shape(body, cte_scope, output_names, deriving, parent, profile),
    }
}

/// Derives the output shape of a query used as a relation body. Returns
/// `Ok(None)` when the columns cannot be enumerated.
fn derive_query_shape<'query, 'db, DB, P>(
    query: AstRef<'query, 'db, Query>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    output_names: Option<OutputNameSource<'_, 'query, 'db, DB, P::Definition>>,
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<Option<DerivedShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let scoped;
    let scope = if let Some(with) = query.try_map(|query| query.with.as_ref()) {
        scoped = derive_cte_shapes(with, cte_scope, deriving, parent, profile)?;
        &scoped
    } else {
        cte_scope
    };
    let shape = derive_set_expr_shape(
        query.map(|query| query.body.as_ref()),
        scope,
        output_names,
        deriving,
        parent,
        profile,
    )?;
    if P::INDEX_NESTED_QUERIES {
        let cursor = derived_body_cursor(query.map(|query| query.body.as_ref()), parent, profile);
        index_query_expression_queries(query, scope, deriving, cursor, profile)?;
    }
    Ok(shape)
}

/// Derives the output shape of a body. A set operation merges its arms by
/// ordinal position: names come from the left arm (as in PostgreSQL) and a
/// column keeps a source only while the arms agree on one.
fn derive_set_expr_shape<'query, 'db, DB, P>(
    body: AstRef<'query, 'db, SetExpr>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    output_names: Option<OutputNameSource<'_, 'query, 'db, DB, P::Definition>>,
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<Option<DerivedShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match set_expr_ref(body) {
        SetExprRef::Select(select) => {
            derive_select_shape(select, cte_scope, output_names.as_ref(), deriving, parent, profile)
        }
        SetExprRef::Query(query) => {
            derive_query_shape(query, cte_scope, output_names, deriving, parent, profile)
        }
        SetExprRef::SetOperation { operator, quantifier, left, right } => {
            let left_shape =
                derive_set_expr_shape(left, cte_scope, output_names, deriving, parent, profile)?;
            if left_shape.is_none() && !P::INDEX_NESTED_QUERIES {
                return Ok(None);
            }
            let right_names = left_shape
                .as_ref()
                .map(|left_shape| OutputNameSource::Columns(&left_shape.columns));
            let right_shape =
                derive_set_expr_shape(right, cte_scope, right_names, deriving, parent, profile)?;
            let (Some(left_shape), Some(right_shape)) = (left_shape, right_shape) else {
                return Ok(None);
            };
            Ok(merge_set_operation_shapes(
                left_shape,
                right_shape,
                operator,
                quantifier,
                false,
                deriving,
                profile,
            ))
        }
        SetExprRef::Values(values) => {
            if P::INDEX_NESTED_QUERIES {
                index_values_expression_queries(values, cte_scope, deriving, parent, profile)?;
            }
            Ok(None)
        }
        SetExprRef::Other => Ok(None),
    }
}

fn merge_set_operation_shapes<'query, 'db, DB, P>(
    left: DerivedShape<'query, 'db, DB, P::Definition>,
    right: DerivedShape<'query, 'db, DB, P::Definition>,
    operator: SetOperator,
    set_quantifier: SetQuantifier,
    recursive: bool,
    deriving: Deriving<'_, 'db, DB>,
    profile: &mut P,
) -> Option<DerivedShape<'query, 'db, DB, P::Definition>>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    if left.columns.len() != right.columns.len() {
        return None;
    }
    let row_preserving =
        matches!(set_quantifier, SetQuantifier::All) && left.row_preserving && right.row_preserving;
    let columns = left
        .columns
        .into_iter()
        .zip(right.columns)
        .map(|(left, right)| {
            let definition = if recursive {
                profile.recursive_definition(left.definition, right.definition)
            } else {
                profile.set_definition(operator, left.definition, right.definition)
            };
            DerivedColumn {
                name: left.name,
                quoted: left.quoted,
                source: match (left.source, right.source) {
                    (Some(left_table), Some(right_table))
                        if deriving.database.table_id(left_table)
                            == deriving.database.table_id(right_table) =>
                    {
                        Some(left_table)
                    }
                    _ => None,
                },
                definition,
                marker: core::marker::PhantomData,
            }
        })
        .collect();
    Some(DerivedShape { columns, row_preserving })
}

/// Expands a `*` projection by materializing each `FROM` item's plan in
/// PostgreSQL's join output order. A relation contributes its own columns
/// except those a merge absorbed (relations collected before that name's
/// merge). A merged name stands once per join with no source, and a repeated
/// merge of the same name stands once at its latest position.
fn push_wildcard_columns<'query, 'db, DB: DatabaseLike, D: Copy>(
    scope: &FromScope<'query, 'db, DB, D>,
    columns: &mut Vec<DerivedColumn<'query, 'db, DB, D>>,
    opaque: D,
) {
    for plan in &scope.wildcard_plans {
        for entry in plan {
            match entry {
                WildcardEntry::Base(index) => {
                    let base = &scope.bases[*index];
                    columns.extend(base_columns(base).into_iter().filter(|column| {
                        !subsumed_exposure(
                            &scope.merged,
                            base.entry_index,
                            &column.name,
                            column.quoted,
                        )
                    }));
                }
                WildcardEntry::Derived(index) => {
                    let relation = &scope.derived[*index];
                    columns.extend(
                        relation
                            .shape
                            .columns
                            .iter()
                            .filter(|column| {
                                !subsumed_exposure(
                                    &scope.merged,
                                    relation.entry_index,
                                    &column.name,
                                    column.quoted,
                                )
                            })
                            .cloned(),
                    );
                }
                WildcardEntry::Merged { name, quoted } => {
                    columns.push(DerivedColumn {
                        name: name.clone(),
                        quoted: *quoted,
                        source: None,
                        definition: opaque,
                        marker: core::marker::PhantomData,
                    });
                }
            }
        }
    }
}

/// Resource guard for pathological `SELECT *` chains: a derivation whose
/// output exceeds this width becomes opaque rather than materializing more
/// columns. Without it, sibling `WITH` items that each reference the previous
/// one twice double the width every level. No ordinary query comes close:
/// PostgreSQL itself refuses target lists beyond roughly 1600 entries.
const MAX_DERIVED_COLUMNS: usize = 4096;

/// Whether a wildcard carries options that change what it outputs: `EXCLUDE`,
/// `EXCEPT` and `ILIKE` drop columns, `RENAME` relabels them, `REPLACE`
/// substitutes a computed value for one, and Redshift's trailing alias names
/// the expansion. None of those output sets are enumerated here, so a body
/// projecting such a wildcard stays opaque rather than claiming columns the
/// relation does not output.
fn wildcard_reshapes_output(options: &WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_some()
        || options.opt_exclude.is_some()
        || options.opt_except.is_some()
        || options.opt_replace.is_some()
        || options.opt_rename.is_some()
        || options.opt_alias.is_some()
}

/// Whether a wildcard substitutes a computed value for one of the columns it
/// expands (`REPLACE`). Such an output row is no longer a row of the source
/// table, so it carries no row identity, whereas dropping or relabelling
/// columns leaves the rows themselves intact.
fn wildcard_replaces_values(options: &WildcardAdditionalOptions) -> bool {
    options.opt_replace.is_some()
}

fn projection_output_name<DB: DatabaseLike, D: Copy>(
    expr: &Expr,
    bases: &[FromTableRef<'_, '_, DB, D>],
    output_names: Option<&OutputNameSource<'_, '_, '_, DB, D>>,
    ordinal: usize,
) -> Option<(String, bool)> {
    projected_column_name(expr)
        .or_else(|| three_part_output_name(expr, bases))
        .or_else(|| output_names.and_then(|names| names.get(ordinal)))
}

fn projection_definition<'query, 'db, DB, P>(
    expression: AstRef<'query, 'db, Expr>,
    scope: P::Cursor,
    profile: &mut P,
) -> Result<P::Definition, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    profile.expression_definition(expression, scope)
}

fn index_nested_query_scopes<'query, 'db, DB, P>(
    query: AstRef<'query, 'db, Query>,
    outer_ctes: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<P::Cursor, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let scoped_ctes;
    let cte_scope = if let Some(with) = query.try_map(|query| query.with.as_ref()) {
        scoped_ctes = derive_cte_shapes(with, outer_ctes, deriving, parent, profile)?;
        &scoped_ctes
    } else {
        outer_ctes
    };
    let cursor = index_nested_set_scopes(
        query.map(|query| query.body.as_ref()),
        cte_scope,
        deriving,
        parent,
        profile,
    )?;
    index_query_expression_queries(query, cte_scope, deriving, cursor, profile)?;
    Ok(cursor)
}

fn index_nested_set_scopes<'query, 'db, DB, P>(
    body: AstRef<'query, 'db, SetExpr>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<P::Cursor, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match set_expr_ref(body) {
        SetExprRef::Select(select) => {
            let scope = collect_select_from(select, cte_scope, deriving, parent, profile)?;
            let cursor = profile.cursor(&scope);
            index_select_expression_queries(select, cte_scope, deriving, cursor, profile)?;
            Ok(cursor)
        }
        SetExprRef::Query(query) => {
            index_nested_query_scopes(query, cte_scope, deriving, parent, profile)
        }
        SetExprRef::Values(values) => {
            index_values_expression_queries(values, cte_scope, deriving, parent, profile)?;
            Ok(parent)
        }
        SetExprRef::SetOperation { left, right, .. } => {
            index_nested_set_scopes(left, cte_scope, deriving, parent, profile)?;
            index_nested_set_scopes(right, cte_scope, deriving, parent, profile)?;
            Ok(parent)
        }
        SetExprRef::Other => Ok(parent),
    }
}

fn select_item_expression<'query, 'db>(
    item: AstRef<'query, 'db, SelectItem>,
) -> Option<AstRef<'query, 'db, Expr>> {
    item.try_map(|item| match item {
        SelectItem::UnnamedExpr(expression)
        | SelectItem::ExprWithAlias { expr: expression, .. }
        | SelectItem::ExprWithAliases { expr: expression, .. }
        | SelectItem::QualifiedWildcard(
            SelectItemQualifiedWildcardKind::Expr(expression),
            _,
        ) => Some(expression),
        SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(_), _)
        | SelectItem::Wildcard(_) => None,
    })
}

fn index_query_expression_queries<'query, 'db, DB, P>(
    query: AstRef<'query, 'db, Query>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<(), LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match query {
        AstRef::Query(query) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Query(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .query_expressions(query)
        }
        AstRef::Database(query) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Database(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .query_expressions(query)
        }
    }
}

fn index_select_expression_queries<'query, 'db, DB, P>(
    select: AstRef<'query, 'db, Select>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<(), LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match select {
        AstRef::Query(select) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Query(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .select(select)
        }
        AstRef::Database(select) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Database(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .select(select)
        }
    }
}

fn index_values_expression_queries<'query, 'db, DB, P>(
    values: AstRef<'query, 'db, Values>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<(), LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match values {
        AstRef::Query(values) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Query(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .values(values)
        }
        AstRef::Database(values) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Database(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .values(values)
        }
    }
}

fn index_table_factor_expression_queries<'query, 'db, DB, P>(
    factor: AstRef<'query, 'db, TableFactor>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    inherited_parent: P::Cursor,
    visible_parent: P::Cursor,
    profile: &mut P,
) -> Result<(), LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match factor {
        AstRef::Query(factor) => {
            NestedQueryWalker {
                query: |query| {
                    let checkpoint = profile.checkpoint();
                    index_nested_query_scopes(
                        AstRef::Query(query),
                        cte_scope,
                        deriving,
                        inherited_parent,
                        profile,
                    )?;
                    profile.set_scope_owners_since(checkpoint, inherited_parent, visible_parent);
                    Ok(())
                },
            }
            .factor_relation_inputs(factor)?;
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Query(query),
                        cte_scope,
                        deriving,
                        visible_parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .factor_visible_expressions(factor)
        }
        AstRef::Database(factor) => {
            NestedQueryWalker {
                query: |query| {
                    let checkpoint = profile.checkpoint();
                    index_nested_query_scopes(
                        AstRef::Database(query),
                        cte_scope,
                        deriving,
                        inherited_parent,
                        profile,
                    )?;
                    profile.set_scope_owners_since(checkpoint, inherited_parent, visible_parent);
                    Ok(())
                },
            }
            .factor_relation_inputs(factor)?;
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Database(query),
                        cte_scope,
                        deriving,
                        visible_parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .factor_visible_expressions(factor)
        }
    }
}

fn index_join_expression_queries<'query, 'db, DB, P>(
    join: AstRef<'query, 'db, Join>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<(), LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match join {
        AstRef::Query(join) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Query(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .join_operator(&join.join_operator)
        }
        AstRef::Database(join) => {
            NestedQueryWalker {
                query: |query| {
                    index_nested_query_scopes(
                        AstRef::Database(query),
                        cte_scope,
                        deriving,
                        parent,
                        profile,
                    )
                    .map(|_| ())
                },
            }
            .join_operator(&join.join_operator)
        }
    }
}

struct NestedQueryWalker<F> {
    query: F,
}

impl<'source, F> NestedQueryWalker<F>
where
    F: FnMut(&'source Query) -> Result<(), LookupError>,
{
    fn query(&mut self, query: &'source Query) -> Result<(), LookupError> {
        (self.query)(query)
    }

    fn query_expressions(&mut self, query: &'source Query) -> Result<(), LookupError> {
        let Query {
            with: _,
            body: _,
            order_by,
            limit_clause,
            fetch,
            locks: _,
            for_clause: _,
            settings,
            format_clause: _,
            pipe_operators,
        } = query;
        if let Some(order_by) = order_by {
            self.order_by_clause(order_by)?;
        }
        if let Some(limit_clause) = limit_clause {
            self.limit_clause(limit_clause)?;
        }
        if let Some(fetch) = fetch {
            self.optional_expression(fetch.quantity.as_ref())?;
        }
        if let Some(settings) = settings {
            for setting in settings {
                self.expression(&setting.value)?;
            }
        }
        for operator in pipe_operators {
            self.pipe_operator(operator)?;
        }
        Ok(())
    }

    fn select(&mut self, select: &'source Select) -> Result<(), LookupError> {
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
            from: _,
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
        if let Some(Distinct::On(expressions)) = distinct {
            self.expressions(expressions)?;
        }
        if let Some(top) = top
            && let Some(TopQuantity::Expr(expression)) = &top.quantity
        {
            self.expression(expression)?;
        }
        for item in projection {
            self.select_item(item)?;
        }
        for view in lateral_views {
            self.expression(&view.lateral_view)?;
        }
        for expression in [prewhere.as_ref(), selection.as_ref(), having.as_ref(), qualify.as_ref()]
            .into_iter()
            .flatten()
        {
            self.expression(expression)?;
        }
        for clause in connect_by {
            match clause {
                ConnectByKind::ConnectBy { relationships, .. } => {
                    self.expressions(relationships)?;
                }
                ConnectByKind::StartWith { condition, .. } => {
                    self.expression(condition)?;
                }
            }
        }
        if let GroupByExpr::Expressions(expressions, _) = group_by {
            self.expressions(expressions)?;
        }
        self.expressions(cluster_by)?;
        self.expressions(distribute_by)?;
        for order in sort_by {
            self.order_by(order)?;
        }
        for definition in named_window {
            if let NamedWindowExpr::WindowSpec(specification) = &definition.1 {
                self.window_spec(specification)?;
            }
        }
        Ok(())
    }

    fn select_item(&mut self, item: &'source SelectItem) -> Result<(), LookupError> {
        match item {
            SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias { expr: expression, .. }
            | SelectItem::ExprWithAliases { expr: expression, .. } => self.expression(expression),
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::Expr(expression),
                options,
            ) => {
                self.expression(expression)?;
                self.wildcard_options(options)
            }
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(_),
                options,
            )
            | SelectItem::Wildcard(options) => self.wildcard_options(options),
        }
    }

    fn values(&mut self, values: &'source Values) -> Result<(), LookupError> {
        for row in &values.rows {
            self.expressions(row)?;
        }
        Ok(())
    }

    fn order_by_clause(&mut self, order_by: &'source OrderBy) -> Result<(), LookupError> {
        match &order_by.kind {
            OrderByKind::Expressions(expressions) => {
                for expression in expressions {
                    self.order_by(expression)?;
                }
            }
            OrderByKind::All(_) => {}
        }
        if let Some(interpolate) = &order_by.interpolate
            && let Some(expressions) = &interpolate.exprs
        {
            for expression in expressions {
                self.optional_expression(expression.expr.as_ref())?;
            }
        }
        Ok(())
    }

    fn limit_clause(&mut self, limit: &'source LimitClause) -> Result<(), LookupError> {
        match limit {
            LimitClause::LimitOffset { limit, offset, limit_by } => {
                self.optional_expression(limit.as_ref())?;
                if let Some(offset) = offset {
                    self.expression(&offset.value)?;
                }
                self.expressions(limit_by)
            }
            LimitClause::OffsetCommaLimit { offset, limit } => self.expression_pair(offset, limit),
        }
    }

    fn pipe_operator(&mut self, operator: &'source PipeOperator) -> Result<(), LookupError> {
        match operator {
            PipeOperator::Limit { expr, offset } => {
                self.expression(expr)?;
                self.optional_expression(offset.as_ref())
            }
            PipeOperator::Where { expr } => self.expression(expr),
            PipeOperator::OrderBy { exprs } => {
                for order in exprs {
                    self.order_by(order)?;
                }
                Ok(())
            }
            PipeOperator::Select { exprs } | PipeOperator::Extend { exprs } => {
                for item in exprs {
                    self.select_item(item)?;
                }
                Ok(())
            }
            PipeOperator::Set { assignments } => {
                for assignment in assignments {
                    self.expression(&assignment.value)?;
                }
                Ok(())
            }
            PipeOperator::Aggregate { full_table_exprs, group_by_expr } => {
                for expression in full_table_exprs.iter().chain(group_by_expr) {
                    self.expression(&expression.expr.expr)?;
                }
                Ok(())
            }
            PipeOperator::Join(join) => {
                self.factor_relation_inputs(&join.relation)?;
                self.factor_visible_expressions(&join.relation)?;
                self.join_operator(&join.join_operator)
            }
            PipeOperator::TableSample { sample } => self.table_sample(sample),
            PipeOperator::Call { function, alias: _ } => self.function(function),
            PipeOperator::Pivot {
                aggregate_functions,
                value_column: _,
                value_source,
                alias: _,
            } => {
                for aggregate in aggregate_functions {
                    self.expression(&aggregate.expr)?;
                }
                self.pivot_value_source(value_source)
            }
            PipeOperator::Union { set_quantifier: _, queries }
            | PipeOperator::Intersect { set_quantifier: _, queries }
            | PipeOperator::Except { set_quantifier: _, queries } => {
                for query in queries {
                    self.query(query)?;
                }
                Ok(())
            }
            PipeOperator::Unpivot {
                value_column: _,
                name_column: _,
                unpivot_columns: _,
                alias: _,
            }
            | PipeOperator::Drop { columns: _ }
            | PipeOperator::As { alias: _ }
            | PipeOperator::Rename { mappings: _ } => Ok(()),
        }
    }

    fn optional_expression(
        &mut self,
        expression: Option<&'source Expr>,
    ) -> Result<(), LookupError> {
        if let Some(expression) = expression {
            self.expression(expression)?;
        }
        Ok(())
    }

    fn expressions(&mut self, expressions: &'source [Expr]) -> Result<(), LookupError> {
        for expression in expressions {
            self.expression(expression)?;
        }
        Ok(())
    }

    fn expression_pair(
        &mut self,
        left: &'source Expr,
        right: &'source Expr,
    ) -> Result<(), LookupError> {
        self.expression(left)?;
        self.expression(right)
    }

    fn factor_relation_inputs(&mut self, factor: &'source TableFactor) -> Result<(), LookupError> {
        match factor {
            TableFactor::Table {
                name: _,
                alias: _,
                args: _,
                with_hints,
                version,
                with_ordinality: _,
                partitions: _,
                json_path,
                sample,
                index_hints: _,
            } => {
                self.expressions(with_hints)?;
                if let Some(version) = version {
                    self.table_version(version)?;
                }
                if let Some(path) = json_path {
                    for element in &path.path {
                        self.json_path_element(element)?;
                    }
                }
                if let Some(sample) = sample {
                    self.table_sample_kind(sample)?;
                }
                Ok(())
            }
            TableFactor::Derived { lateral: _, subquery: _, alias: _, sample } => {
                if let Some(sample) = sample {
                    self.table_sample_kind(sample)?;
                }
                Ok(())
            }
            TableFactor::TableFunction { expr: _, alias: _ }
            | TableFactor::Function {
                lateral: _,
                name: _,
                args: _,
                alias: _,
                with_ordinality: _,
            }
            | TableFactor::UNNEST {
                alias: _,
                array_exprs: _,
                with_offset: _,
                with_offset_alias: _,
                with_ordinality: _,
            }
            | TableFactor::JsonTable { json_expr: _, json_path: _, columns: _, alias: _ }
            | TableFactor::OpenJsonTable { json_expr: _, json_path: _, columns: _, alias: _ }
            | TableFactor::XmlTable {
                namespaces: _,
                row_expression: _,
                passing: _,
                columns: _,
                alias: _,
            }
            | TableFactor::SemanticView {
                name: _,
                dimensions: _,
                metrics: _,
                facts: _,
                where_clause: _,
                alias: _,
            }
            | TableFactor::UnpivotExpr { expression: _, value_alias: _, attribute_alias: _ } => {
                Ok(())
            }
            TableFactor::NestedJoin { table_with_joins, alias: _ } => {
                self.table_with_joins_relation_inputs(table_with_joins)
            }
            TableFactor::Pivot { table, .. }
            | TableFactor::Unpivot { table, .. }
            | TableFactor::MatchRecognize { table, .. } => self.factor_relation_inputs(table),
        }
    }

    fn table_with_joins_relation_inputs(
        &mut self,
        table: &'source sqlparser::ast::TableWithJoins,
    ) -> Result<(), LookupError> {
        self.factor_relation_inputs(&table.relation)?;
        for join in &table.joins {
            self.factor_relation_inputs(&join.relation)?;
        }
        Ok(())
    }

    fn factor_visible_expressions(
        &mut self,
        factor: &'source TableFactor,
    ) -> Result<(), LookupError> {
        match factor {
            TableFactor::Table {
                name: _,
                alias: _,
                args,
                with_hints: _,
                version: _,
                with_ordinality: _,
                partitions: _,
                json_path: _,
                sample: _,
                index_hints: _,
            } => {
                if let Some(arguments) = args {
                    self.table_function_args(arguments)?;
                }
                Ok(())
            }
            TableFactor::Derived { lateral: _, subquery: _, alias: _, sample: _ } => Ok(()),
            TableFactor::TableFunction { expr, alias: _ } => self.expression(expr),
            TableFactor::Function { lateral: _, name: _, args, alias: _, with_ordinality: _ } => {
                self.function_argument_list(args)
            }
            TableFactor::UNNEST {
                alias: _,
                array_exprs,
                with_offset: _,
                with_offset_alias: _,
                with_ordinality: _,
            } => self.expressions(array_exprs),
            TableFactor::JsonTable { json_expr, json_path: _, columns: _, alias: _ }
            | TableFactor::OpenJsonTable { json_expr, json_path: _, columns: _, alias: _ } => {
                self.expression(json_expr)
            }
            TableFactor::NestedJoin { table_with_joins, alias: _ } => {
                self.nested_join_visible_expressions(table_with_joins)
            }
            TableFactor::Pivot {
                table,
                aggregate_functions,
                value_column,
                value_source,
                default_on_null,
                alias: _,
            } => {
                self.pivot_expressions(
                    table,
                    aggregate_functions,
                    value_column,
                    value_source,
                    default_on_null.as_ref(),
                )
            }
            TableFactor::Unpivot {
                table,
                value,
                name: _,
                columns,
                alias: _,
                null_inclusion: _,
            } => {
                self.factor_visible_expressions(table)?;
                self.expression(value)?;
                for column in columns {
                    self.expression(&column.expr)?;
                }
                Ok(())
            }
            TableFactor::UnpivotExpr { expression, value_alias: _, attribute_alias: _ } => {
                self.expression(expression)
            }
            TableFactor::MatchRecognize {
                table,
                partition_by,
                order_by,
                measures,
                rows_per_match: _,
                after_match_skip: _,
                pattern: _,
                symbols,
                alias: _,
            } => self.match_recognize_expressions(table, partition_by, order_by, measures, symbols),
            TableFactor::XmlTable { namespaces, row_expression, passing, columns, alias: _ } => {
                self.xml_table_expressions(namespaces, row_expression, passing, columns)
            }
            TableFactor::SemanticView {
                name: _,
                dimensions,
                metrics,
                facts,
                where_clause,
                alias: _,
            } => {
                self.expressions(dimensions)?;
                self.expressions(metrics)?;
                self.expressions(facts)?;
                self.optional_expression(where_clause.as_ref())
            }
        }
    }

    fn nested_join_visible_expressions(
        &mut self,
        table: &'source TableWithJoins,
    ) -> Result<(), LookupError> {
        self.factor_visible_expressions(&table.relation)?;
        for join in &table.joins {
            self.factor_visible_expressions(&join.relation)?;
            self.join_operator(&join.join_operator)?;
        }
        Ok(())
    }

    fn pivot_expressions(
        &mut self,
        table: &'source TableFactor,
        aggregates: &'source [ExprWithAlias],
        values: &'source [Expr],
        source: &'source PivotValueSource,
        default: Option<&'source Expr>,
    ) -> Result<(), LookupError> {
        self.factor_visible_expressions(table)?;
        for aggregate in aggregates {
            self.expression(&aggregate.expr)?;
        }
        self.expressions(values)?;
        self.pivot_value_source(source)?;
        self.optional_expression(default)
    }

    fn match_recognize_expressions(
        &mut self,
        table: &'source TableFactor,
        partition_by: &'source [Expr],
        order_by: &'source [OrderByExpr],
        measures: &'source [Measure],
        symbols: &'source [SymbolDefinition],
    ) -> Result<(), LookupError> {
        self.factor_visible_expressions(table)?;
        self.expressions(partition_by)?;
        for order in order_by {
            self.order_by(order)?;
        }
        for measure in measures {
            self.expression(&measure.expr)?;
        }
        for symbol in symbols {
            self.expression(&symbol.definition)?;
        }
        Ok(())
    }

    fn xml_table_expressions(
        &mut self,
        namespaces: &'source [XmlNamespaceDefinition],
        row_expression: &'source Expr,
        passing: &'source XmlPassingClause,
        columns: &'source [XmlTableColumn],
    ) -> Result<(), LookupError> {
        for namespace in namespaces {
            self.expression(&namespace.uri)?;
        }
        self.expression(row_expression)?;
        for argument in &passing.arguments {
            self.expression(&argument.expr)?;
        }
        for column in columns {
            self.xml_table_column(&column.option)?;
        }
        Ok(())
    }

    fn function_argument_list(
        &mut self,
        arguments: &'source [FunctionArg],
    ) -> Result<(), LookupError> {
        for argument in arguments {
            self.function_argument(argument)?;
        }
        Ok(())
    }

    fn table_function_args(
        &mut self,
        arguments: &'source TableFunctionArgs,
    ) -> Result<(), LookupError> {
        for argument in &arguments.args {
            self.function_argument(argument)?;
        }
        if let Some(settings) = &arguments.settings {
            for setting in settings {
                self.expression(&setting.value)?;
            }
        }
        Ok(())
    }

    fn table_sample_kind(&mut self, sample: &'source TableSampleKind) -> Result<(), LookupError> {
        match sample {
            TableSampleKind::BeforeTableAlias(sample)
            | TableSampleKind::AfterTableAlias(sample) => self.table_sample(sample),
        }
    }

    fn table_sample(&mut self, sample: &'source TableSample) -> Result<(), LookupError> {
        if let Some(quantity) = &sample.quantity {
            self.expression(&quantity.value)?;
        }
        if let Some(bucket) = &sample.bucket {
            self.optional_expression(bucket.on.as_ref())?;
        }
        self.optional_expression(sample.offset.as_ref())
    }

    fn table_version(&mut self, version: &'source TableVersion) -> Result<(), LookupError> {
        match version {
            TableVersion::ForSystemTimeAsOf(expression)
            | TableVersion::TimestampAsOf(expression)
            | TableVersion::VersionAsOf(expression)
            | TableVersion::Function(expression) => self.expression(expression),
            TableVersion::Changes { changes, at, end } => {
                self.expression(changes)?;
                self.expression(at)?;
                self.optional_expression(end.as_ref())
            }
        }
    }

    fn pivot_value_source(&mut self, source: &'source PivotValueSource) -> Result<(), LookupError> {
        match source {
            PivotValueSource::List(values) => {
                for value in values {
                    self.expression(&value.expr)?;
                }
                Ok(())
            }
            PivotValueSource::Any(orders) => {
                for order in orders {
                    self.order_by(order)?;
                }
                Ok(())
            }
            PivotValueSource::Subquery(query) => self.query(query),
        }
    }

    fn join_operator(&mut self, operator: &'source JoinOperator) -> Result<(), LookupError> {
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
            | JoinOperator::RightAnti(constraint)
            | JoinOperator::StraightJoin(constraint) => self.join_constraint(constraint),
            JoinOperator::AsOf { match_condition, constraint } => {
                self.expression(match_condition)?;
                self.join_constraint(constraint)
            }
            JoinOperator::CrossApply
            | JoinOperator::OuterApply
            | JoinOperator::ArrayJoin
            | JoinOperator::LeftArrayJoin
            | JoinOperator::InnerArrayJoin => Ok(()),
        }
    }

    fn join_constraint(&mut self, constraint: &'source JoinConstraint) -> Result<(), LookupError> {
        match constraint {
            JoinConstraint::On(expression) => self.expression(expression),
            JoinConstraint::Using(_) | JoinConstraint::Natural | JoinConstraint::None => Ok(()),
        }
    }

    fn xml_table_column(
        &mut self,
        option: &'source XmlTableColumnOption,
    ) -> Result<(), LookupError> {
        match option {
            XmlTableColumnOption::NamedInfo { r#type: _, path, default, nullable: _ } => {
                self.optional_expression(path.as_ref())?;
                self.optional_expression(default.as_ref())
            }
            XmlTableColumnOption::ForOrdinality => Ok(()),
        }
    }

    fn access(&mut self, access: &'source AccessExpr) -> Result<(), LookupError> {
        match access {
            AccessExpr::Dot(expression) => self.expression(expression),
            AccessExpr::Subscript(subscript) => self.subscript(subscript),
        }
    }

    fn subscript(&mut self, subscript: &'source Subscript) -> Result<(), LookupError> {
        match subscript {
            Subscript::Index { index } => self.expression(index),
            Subscript::Slice { lower_bound, upper_bound, stride } => {
                self.optional_expression(lower_bound.as_ref())?;
                self.optional_expression(upper_bound.as_ref())?;
                self.optional_expression(stride.as_ref())
            }
        }
    }

    fn json_path_element(&mut self, element: &'source JsonPathElem) -> Result<(), LookupError> {
        match element {
            JsonPathElem::Bracket { key } | JsonPathElem::ColonBracket { key } => {
                self.expression(key)
            }
            JsonPathElem::Dot { .. } => Ok(()),
        }
    }

    fn function_argument_expression(
        &mut self,
        argument: &'source FunctionArgExpr,
    ) -> Result<(), LookupError> {
        match argument {
            FunctionArgExpr::Expr(expression) => self.expression(expression),
            FunctionArgExpr::WildcardWithOptions(options) => self.wildcard_options(options),
            FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => Ok(()),
        }
    }

    fn function_argument(&mut self, argument: &'source FunctionArg) -> Result<(), LookupError> {
        match argument {
            FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
                self.function_argument_expression(arg)
            }
            FunctionArg::ExprNamed { name, arg, .. } => {
                self.expression(name)?;
                self.function_argument_expression(arg)
            }
        }
    }

    fn order_by(&mut self, order: &'source OrderByExpr) -> Result<(), LookupError> {
        self.expression(&order.expr)?;
        if let Some(fill) = &order.with_fill {
            self.optional_expression(fill.from.as_ref())?;
            self.optional_expression(fill.to.as_ref())?;
            self.optional_expression(fill.step.as_ref())?;
        }
        Ok(())
    }

    fn window_bound(&mut self, bound: &'source WindowFrameBound) -> Result<(), LookupError> {
        match bound {
            WindowFrameBound::Preceding(value) | WindowFrameBound::Following(value) => {
                self.optional_expression(value.as_deref())
            }
            WindowFrameBound::CurrentRow => Ok(()),
        }
    }

    fn window(&mut self, window: &'source WindowType) -> Result<(), LookupError> {
        match window {
            WindowType::NamedWindow(_) => Ok(()),
            WindowType::WindowSpec(specification) => self.window_spec(specification),
        }
    }

    fn window_spec(&mut self, specification: &'source WindowSpec) -> Result<(), LookupError> {
        self.expressions(&specification.partition_by)?;
        for order in &specification.order_by {
            self.order_by(order)?;
        }
        if let Some(frame) = &specification.window_frame {
            self.window_bound(&frame.start_bound)?;
            if let Some(bound) = &frame.end_bound {
                self.window_bound(bound)?;
            }
        }
        Ok(())
    }

    fn function_clause(
        &mut self,
        clause: &'source FunctionArgumentClause,
    ) -> Result<(), LookupError> {
        match clause {
            FunctionArgumentClause::Where(expression)
            | FunctionArgumentClause::Limit(expression) => self.expression(expression),
            FunctionArgumentClause::OrderBy(orders) => {
                for order in orders {
                    self.order_by(order)?;
                }
                Ok(())
            }
            FunctionArgumentClause::OnOverflow(sqlparser::ast::ListAggOnOverflow::Truncate {
                filler,
                ..
            }) => self.optional_expression(filler.as_deref()),
            FunctionArgumentClause::Having(bound) => self.expression(&bound.1),
            FunctionArgumentClause::OnOverflow(_)
            | FunctionArgumentClause::IgnoreOrRespectNulls(_)
            | FunctionArgumentClause::Separator(_)
            | FunctionArgumentClause::JsonNullClause(_)
            | FunctionArgumentClause::JsonReturningClause(_) => Ok(()),
        }
    }

    fn function_arguments(
        &mut self,
        arguments: &'source FunctionArguments,
    ) -> Result<(), LookupError> {
        match arguments {
            FunctionArguments::None => Ok(()),
            FunctionArguments::Subquery(query) => self.query(query),
            FunctionArguments::List(list) => {
                for argument in &list.args {
                    self.function_argument(argument)?;
                }
                for clause in &list.clauses {
                    self.function_clause(clause)?;
                }
                Ok(())
            }
        }
    }

    fn function(&mut self, function: &'source Function) -> Result<(), LookupError> {
        self.function_arguments(&function.parameters)?;
        self.function_arguments(&function.args)?;
        self.optional_expression(function.filter.as_deref())?;
        if let Some(window) = &function.over {
            self.window(window)?;
        }
        for order in &function.within_group {
            self.order_by(order)?;
        }
        Ok(())
    }

    fn wildcard_options(
        &mut self,
        options: &'source WildcardAdditionalOptions,
    ) -> Result<(), LookupError> {
        if let Some(replace) = &options.opt_replace {
            for item in &replace.items {
                self.expression(&item.expr)?;
            }
        }
        Ok(())
    }

    fn case_when(&mut self, case: &'source CaseWhen) -> Result<(), LookupError> {
        self.expression(&case.condition)?;
        self.expression(&case.result)
    }

    fn dictionary_field(&mut self, field: &'source DictionaryField) -> Result<(), LookupError> {
        self.expression(&field.value)
    }

    fn map_entry(&mut self, entry: &'source MapEntry) -> Result<(), LookupError> {
        self.expression(&entry.key)?;
        self.expression(&entry.value)
    }

    fn expression_with_many(
        &mut self,
        expression: &'source Expr,
        expressions: &'source [Expr],
    ) -> Result<(), LookupError> {
        self.expression(expression)?;
        self.expressions(expressions)
    }

    fn expression_triple(
        &mut self,
        first: &'source Expr,
        second: &'source Expr,
        third: &'source Expr,
    ) -> Result<(), LookupError> {
        self.expression(first)?;
        self.expression(second)?;
        self.expression(third)
    }

    fn trim_expression(
        &mut self,
        expression: &'source Expr,
        trim_what: Option<&'source Expr>,
        trim_characters: Option<&'source [Expr]>,
    ) -> Result<(), LookupError> {
        self.expression(expression)?;
        self.optional_expression(trim_what)?;
        if let Some(characters) = trim_characters {
            self.expressions(characters)?;
        }
        Ok(())
    }

    fn case_expression(
        &mut self,
        operand: Option<&'source Expr>,
        conditions: &'source [CaseWhen],
        else_result: Option<&'source Expr>,
    ) -> Result<(), LookupError> {
        self.optional_expression(operand)?;
        for case in conditions {
            self.case_when(case)?;
        }
        self.optional_expression(else_result)
    }

    fn expression_groups(&mut self, groups: &'source [Vec<Expr>]) -> Result<(), LookupError> {
        for group in groups {
            self.expressions(group)?;
        }
        Ok(())
    }

    fn dictionary_fields(&mut self, fields: &'source [DictionaryField]) -> Result<(), LookupError> {
        for field in fields {
            self.dictionary_field(field)?;
        }
        Ok(())
    }

    fn map_entries(&mut self, entries: &'source [MapEntry]) -> Result<(), LookupError> {
        for entry in entries {
            self.map_entry(entry)?;
        }
        Ok(())
    }

    fn expression(&mut self, expression: &'source Expr) -> Result<(), LookupError> {
        match expression {
            Expr::InSubquery { expr, subquery, .. } => {
                self.expression(expr)?;
                self.query(subquery)
            }
            Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => self.query(subquery),
            Expr::CompoundFieldAccess { root, access_chain, .. } => {
                self.expression(root)?;
                for access in access_chain {
                    self.access(access)?;
                }
                Ok(())
            }
            Expr::JsonAccess { value, path } => {
                self.expression(value)?;
                for element in &path.path {
                    self.json_path_element(element)?;
                }
                Ok(())
            }
            Expr::IsFalse(expression)
            | Expr::IsNotFalse(expression)
            | Expr::IsTrue(expression)
            | Expr::IsNotTrue(expression)
            | Expr::IsNull(expression)
            | Expr::IsNotNull(expression)
            | Expr::IsUnknown(expression)
            | Expr::IsNotUnknown(expression)
            | Expr::Nested(expression)
            | Expr::OuterJoin(expression)
            | Expr::Prior(expression)
            | Expr::IsJson { expr: expression, .. }
            | Expr::IsNormalized { expr: expression, .. }
            | Expr::UnaryOp { expr: expression, .. }
            | Expr::Cast { expr: expression, .. }
            | Expr::Extract { expr: expression, .. }
            | Expr::Ceil { expr: expression, .. }
            | Expr::Floor { expr: expression, .. }
            | Expr::Collate { expr: expression, .. }
            | Expr::Named { expr: expression, .. }
            | Expr::Prefixed { value: expression, .. } => self.expression(expression),
            Expr::Interval(interval) => self.expression(&interval.value),
            Expr::Lambda(lambda) => self.expression(&lambda.body),
            Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right)
            | Expr::BinaryOp { left, right, .. }
            | Expr::AnyOp { left, right, .. }
            | Expr::AllOp { left, right, .. } => self.expression_pair(left, right),
            Expr::InUnnest { expr, array_expr, .. } => self.expression_pair(expr, array_expr),
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. }
            | Expr::RLike { expr, pattern, .. } => self.expression_pair(expr, pattern),
            Expr::AtTimeZone { timestamp, time_zone } => self.expression_pair(timestamp, time_zone),
            Expr::Position { expr, r#in } => self.expression_pair(expr, r#in),
            Expr::MemberOf(member) => self.expression_pair(&member.value, &member.array),
            Expr::InList { expr, list, .. } => self.expression_with_many(expr, list),
            Expr::Between { expr, low, high, .. } => self.expression_triple(expr, low, high),
            Expr::Convert { expr, styles, .. } => self.expression_with_many(expr, styles),
            Expr::Substring { expr, substring_from, substring_for, .. } => {
                self.expression(expr)?;
                self.optional_expression(substring_from.as_deref())?;
                self.optional_expression(substring_for.as_deref())
            }
            Expr::Trim { expr, trim_what, trim_characters, .. } => {
                self.trim_expression(expr, trim_what.as_deref(), trim_characters.as_deref())
            }
            Expr::Overlay { expr, overlay_what, overlay_from, overlay_for } => {
                self.expression(expr)?;
                self.expression(overlay_what)?;
                self.expression(overlay_from)?;
                self.optional_expression(overlay_for.as_deref())
            }
            Expr::Function(function) => self.function(function),
            Expr::Case { operand, conditions, else_result, .. } => {
                self.case_expression(operand.as_deref(), conditions, else_result.as_deref())
            }
            Expr::GroupingSets(groups) | Expr::Cube(groups) | Expr::Rollup(groups) => {
                self.expression_groups(groups)
            }
            Expr::Tuple(expressions) => self.expressions(expressions),
            Expr::Struct { values, .. } => self.expressions(values),
            Expr::Array(array) => self.expressions(&array.elem),
            Expr::Dictionary(fields) => self.dictionary_fields(fields),
            Expr::Map(map) => self.map_entries(&map.entries),
            Expr::Identifier(_)
            | Expr::CompoundIdentifier(_)
            | Expr::Value(_)
            | Expr::TypedString(_)
            | Expr::MatchAgainst { .. }
            | Expr::Wildcard(_)
            | Expr::QualifiedWildcard(_, _) => Ok(()),
        }
    }
}

fn derived_projection_source<'db, DB: DatabaseLike, D: Copy>(
    expression: &Expr,
    scope: &FromScope<'_, 'db, DB, D>,
    opaque: D,
) -> Result<Option<&'db DB::Table>, LookupError> {
    match column_source(expression, scope, scope.from_entry_count, opaque, false) {
        Ok(source) => Ok(source),
        Err(LookupError::AmbiguousTableLookup { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn append_select_item<'query, 'db, DB, P>(
    item: AstRef<'query, 'db, SelectItem>,
    scope: &P::Scope,
    scope_cursor: P::Cursor,
    output_names: Option<&OutputNameSource<'_, 'query, 'db, DB, P::Definition>>,
    profile: &mut P,
    columns: &mut Vec<DerivedColumn<'query, 'db, DB, P::Definition>>,
) -> Result<bool, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match item.get() {
        SelectItem::UnnamedExpr(_) => {
            let Some(expression) = select_item_expression(item) else {
                return Ok(false);
            };
            let opaque = profile.opaque_definition();
            let (named, source) = {
                let data = profile.scope(scope);
                (
                    projection_output_name(
                        expression.get(),
                        &data.bases,
                        output_names,
                        columns.len(),
                    ),
                    derived_projection_source(expression.get(), data, opaque)?,
                )
            };
            let Some((name, quoted)) = named else {
                return Ok(false);
            };
            let definition = projection_definition(expression, scope_cursor, profile)?;
            columns.push(DerivedColumn {
                name,
                quoted,
                source,
                definition,
                marker: core::marker::PhantomData,
            });
            Ok(true)
        }
        SelectItem::ExprWithAlias { alias, .. } => {
            let Some(expression) = select_item_expression(item) else {
                return Ok(false);
            };
            let opaque = profile.opaque_definition();
            let source = derived_projection_source(expression.get(), profile.scope(scope), opaque)?;
            let definition = projection_definition(expression, scope_cursor, profile)?;
            columns.push(DerivedColumn {
                name: alias.value.clone(),
                quoted: alias.quote_style.is_some(),
                source,
                definition,
                marker: core::marker::PhantomData,
            });
            Ok(true)
        }
        SelectItem::ExprWithAliases { .. }
        | SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(_), _) => Ok(false),
        SelectItem::Wildcard(options) => {
            let data = profile.scope(scope);
            if data.has_opaque() || wildcard_reshapes_output(options) {
                return Ok(false);
            }
            let opaque = profile.opaque_definition();
            push_wildcard_columns(data, columns, opaque);
            Ok(true)
        }
        SelectItem::QualifiedWildcard(
            SelectItemQualifiedWildcardKind::ObjectName(object_name),
            options,
        ) => {
            if wildcard_reshapes_output(options) {
                return Ok(false);
            }
            let Some(expansion) = ({
                let data = profile.scope(scope);
                expand_qualified_wildcard(&data.bases, &data.derived, object_name)
            }) else {
                return Ok(false);
            };
            columns.extend(expansion);
            Ok(true)
        }
    }
}

/// Derives the output shape of a plain `SELECT`: each projected column's name
/// and pass-through source, enumerated from the projection (wildcards expand
/// over the `FROM` relations they stand for).
fn derive_select_shape<'query, 'db, DB, P>(
    select: AstRef<'query, 'db, Select>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    output_names: Option<&OutputNameSource<'_, 'query, 'db, DB, P::Definition>>,
    deriving: Deriving<'_, 'db, DB>,
    parent: P::Cursor,
    profile: &mut P,
) -> Result<Option<DerivedShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let unsupported = !select.get().lateral_views.is_empty()
        || select.get().exclude.is_some()
        || select.get().value_table_mode.is_some()
        || !select.get().connect_by.is_empty();
    if unsupported && !P::INDEX_NESTED_QUERIES {
        return Ok(None);
    }
    let scope = collect_select_from(select, cte_scope, deriving, parent, profile)?;
    let scope_cursor = profile.cursor(&scope);
    if P::INDEX_NESTED_QUERIES {
        index_select_expression_queries(select, cte_scope, deriving, scope_cursor, profile)?;
    }
    if unsupported {
        return Ok(None);
    }
    let mut columns = Vec::new();
    for item in select.map(|select| select.projection.as_slice()).iter() {
        if !append_select_item(item, &scope, scope_cursor, output_names, profile, &mut columns)? {
            return Ok(None);
        }
    }
    let grouped = match &select.get().group_by {
        GroupByExpr::All(_) => true,
        GroupByExpr::Expressions(expressions, _) => !expressions.is_empty(),
    };
    let outer_join = select.get().from.iter().flat_map(|entry| &entry.joins).any(|join| {
        let (left_nullable, right_nullable) = nullable_sides(&join.join_operator);
        left_nullable || right_nullable
    });
    let reads_non_preserving =
        profile.scope(&scope).derived.iter().any(|relation| !relation.shape.row_preserving);
    let row_preserving = select.get().distinct.is_none()
        && select.get().having.is_none()
        && select.get().qualify.is_none()
        && !grouped
        && !outer_join
        && !reads_non_preserving;
    if columns.len() > MAX_DERIVED_COLUMNS {
        return Ok(None);
    }
    Ok(Some(DerivedShape { columns, row_preserving }))
}

/// The output name and quoting a pass-through column projection writes, or
/// `None` for an expression that is not a column reference.
fn projected_column_name(expr: &Expr) -> Option<(String, bool)> {
    match expr {
        Expr::Identifier(ident) => Some((ident.value.clone(), ident.quote_style.is_some())),
        // Only the two-part `qualifier.column` shape names a pass-through
        // column outright. A three-part `schema.table.column` reference is
        // named separately, once its schema is checked against a base
        // table, and anything longer has no name derived here.
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let last = &parts[1];
            Some((last.value.clone(), last.quote_style.is_some()))
        }
        _ => None,
    }
}

/// The output name a `schema.table.column` projection writes: PostgreSQL
/// labels such a column with the trailing name, but only when the reference
/// names a base table in that schema. An unresolvable reference belongs to a
/// statement PostgreSQL would reject, so it yields no name and the body
/// stays opaque rather than inventing an output column.
fn three_part_output_name<DB: DatabaseLike, D: Copy>(
    expr: &Expr,
    bases: &[FromTableRef<'_, '_, DB, D>],
) -> Option<(String, bool)> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    if parts.len() != 3 {
        return None;
    }
    let base = base_for_qualified_name(
        bases,
        parts[0].value.as_str(),
        parts[0].quote_style.is_some(),
        parts[1].value.as_str(),
        parts[1].quote_style.is_some(),
        false,
        usize::MAX,
    )?;
    base_exposes_column(base, &parts[2])
        .then(|| (parts[2].value.clone(), parts[2].quote_style.is_some()))
}

/// Every column a base relation outputs as a derived column sourced by that
/// table: the relation's `output_names` in declaration order, so an alias
/// column list renames the wildcard expansion too.
fn base_columns<'query, 'db, DB: DatabaseLike, D: Copy>(
    base: &FromTableRef<'query, 'db, DB, D>,
) -> Vec<DerivedColumn<'query, 'db, DB, D>> {
    base.output_columns
        .iter()
        .map(|column| {
            DerivedColumn {
                name: column.name.clone(),
                quoted: column.quoted,
                source: Some(base.table),
                definition: column.definition,
                marker: core::marker::PhantomData,
            }
        })
        .collect()
}

/// The relation a qualified wildcard prefix names.
enum WildcardTarget<'scope, 'query, 'db, DB: DatabaseLike, D: Copy> {
    Base(&'scope FromTableRef<'query, 'db, DB, D>),
    Derived(&'scope DerivedRelationRef<'query, 'db, DB, D>),
}

fn resolve_wildcard_target<'scope, 'query, 'db, DB: DatabaseLike, D: Copy>(
    bases: &'scope [FromTableRef<'query, 'db, DB, D>],
    derived: &'scope [DerivedRelationRef<'query, 'db, DB, D>],
    qualifier: &ObjectName,
    require_row_identity: bool,
) -> Option<WildcardTarget<'scope, 'query, 'db, DB, D>> {
    let (value, quoted) = object_name_last_part(qualifier)?;
    match qualifier.0.len() {
        1 => {
            base_for_qualifier(bases, value, quoted, require_row_identity, usize::MAX)
                .map(WildcardTarget::Base)
                .or_else(|| {
                    derived_for_qualifier(derived, value, quoted, require_row_identity, usize::MAX)
                        .map(WildcardTarget::Derived)
                })
        }
        2 => {
            let (schema, schema_quoted) = qualifier_of(qualifier).named()?;
            base_for_qualified_name(
                bases,
                schema,
                schema_quoted,
                value,
                quoted,
                require_row_identity,
                usize::MAX,
            )
            .map(WildcardTarget::Base)
        }
        _ => None,
    }
}

fn expand_qualified_wildcard<'query, 'db, DB: DatabaseLike, D: Copy>(
    bases: &[FromTableRef<'query, 'db, DB, D>],
    derived: &[DerivedRelationRef<'query, 'db, DB, D>],
    qualifier: &ObjectName,
) -> Option<Vec<DerivedColumn<'query, 'db, DB, D>>> {
    match resolve_wildcard_target(bases, derived, qualifier, false)? {
        WildcardTarget::Base(base) => Some(base_columns(base)),
        WildcardTarget::Derived(relation) => Some(relation.shape.columns.clone()),
    }
}

fn apply_alias_columns<'query, 'db, DB: DatabaseLike, D: Copy>(
    shape: Option<DerivedShape<'query, 'db, DB, D>>,
    alias: &TableAlias,
) -> Option<DerivedShape<'query, 'db, DB, D>> {
    let mut shape = shape?;
    if alias.columns.is_empty() {
        return Some(shape);
    }
    if alias.columns.len() > shape.columns.len() {
        return None;
    }
    for (column, alias_column) in shape.columns.iter_mut().zip(&alias.columns) {
        alias_column.name.value.clone_into(&mut column.name);
        column.quoted = alias_column.name.quote_style.is_some();
    }
    Some(shape)
}

fn find_cte<'query, 'db, 'scope, DB: DatabaseLike, D: Copy>(
    name: &ObjectName,
    cte_scope: &'scope [CteShape<'query, 'db, DB, D>],
) -> Option<&'scope CteShape<'query, 'db, DB, D>> {
    if name.0.len() != 1 {
        return None;
    }
    let (value, quoted) = object_name_last_part(name)?;
    cte_scope
        .iter()
        .rev()
        .find(|cte| identifiers_match(cte.name.value.get(), cte.name.quoted, value, quoted))
}

fn base_exposes_column<DB: DatabaseLike, D: Copy>(
    base: &FromTableRef<'_, '_, DB, D>,
    column: &Ident,
) -> bool {
    base.output_columns.iter().any(|candidate| {
        identifiers_match(
            &candidate.name,
            candidate.quoted,
            column.value.as_str(),
            column.quote_style.is_some(),
        )
    })
}

fn find_base_column<'scope, DB: DatabaseLike, D: Copy>(
    base: &'scope FromTableRef<'_, '_, DB, D>,
    column: &Ident,
) -> Option<&'scope BaseColumnRef<D>> {
    base.output_columns.iter().find(|candidate| {
        identifiers_match(
            &candidate.name,
            candidate.quoted,
            column.value.as_str(),
            column.quote_style.is_some(),
        )
    })
}

fn key_matches(key: RelationKey<'_, '_>, value: &str, quoted: bool) -> bool {
    identifiers_match(key.value.get(), key.quoted, value, quoted)
}

fn base_for_qualifier<'scope, 'query, 'db, DB: DatabaseLike, D: Copy>(
    bases: &'scope [FromTableRef<'query, 'db, DB, D>],
    value: &str,
    quoted: bool,
    require_row_identity: bool,
    visible_entries: usize,
) -> Option<&'scope FromTableRef<'query, 'db, DB, D>> {
    bases
        .iter()
        .filter(|base| base.entry_index < visible_entries)
        .filter(|base| !require_row_identity || !base.nullable)
        .find(|base| key_matches(base.key, value, quoted))
}

fn base_for_qualified_name<'scope, 'query, 'db, DB: DatabaseLike, D: Copy>(
    bases: &'scope [FromTableRef<'query, 'db, DB, D>],
    schema: &str,
    schema_quoted: bool,
    table_part: &str,
    table_quoted: bool,
    require_row_identity: bool,
    visible_entries: usize,
) -> Option<&'scope FromTableRef<'query, 'db, DB, D>> {
    bases
        .iter()
        .filter(|base| base.entry_index < visible_entries)
        .filter(|base| !require_row_identity || !base.nullable)
        .find(|base| {
            base.schema_key.is_some_and(|stored| {
                key_matches(stored, schema, schema_quoted)
                    && key_matches(base.key, table_part, table_quoted)
            })
        })
}

fn derived_for_qualifier<'scope, 'query, 'db, DB: DatabaseLike, D: Copy>(
    derived: &'scope [DerivedRelationRef<'query, 'db, DB, D>],
    value: &str,
    quoted: bool,
    require_row_identity: bool,
    visible_entries: usize,
) -> Option<&'scope DerivedRelationRef<'query, 'db, DB, D>> {
    derived
        .iter()
        .filter(|relation| relation.entry_index < visible_entries)
        .find(|relation| relation.key.is_some_and(|key| key_matches(key, value, quoted)))
        .filter(|relation| {
            !require_row_identity || (relation.shape.row_preserving && !relation.nullable)
        })
}

enum DerivedMatch<'scope, 'query, 'db, DB: DatabaseLike, D: Copy> {
    Column(&'scope DerivedColumn<'query, 'db, DB, D>),
    Ambiguous,
}

fn find_derived_column<'scope, 'query, 'db, DB: DatabaseLike, D: Copy>(
    columns: &'scope [DerivedColumn<'query, 'db, DB, D>],
    column: &Ident,
) -> Option<DerivedMatch<'scope, 'query, 'db, DB, D>> {
    let mut matches = columns.iter().filter(|candidate| {
        identifiers_match(
            &candidate.name,
            candidate.quoted,
            column.value.as_str(),
            column.quote_style.is_some(),
        )
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        return Some(DerivedMatch::Ambiguous);
    }
    Some(DerivedMatch::Column(first))
}

fn single_source_relation<'db, DB: DatabaseLike, D: Copy>(
    relation: &DerivedRelationRef<'_, 'db, DB, D>,
    database: &DB,
) -> Option<&'db DB::Table> {
    if !relation.shape.row_preserving {
        return None;
    }
    let first = relation.shape.columns.first()?.source?;
    relation
        .shape
        .columns
        .iter()
        .all(|column| {
            column.source.is_some_and(|table| database.table_id(table) == database.table_id(first))
        })
        .then_some(first)
}

fn relation_key_display<DB: DatabaseLike, D: Copy>(
    relation: &DerivedRelationRef<'_, '_, DB, D>,
) -> String {
    match relation.key {
        Some(key) if key.quoted => format!("\"{}\"", key.value.get()),
        Some(key) => key.value.get().to_string(),
        None => "(subquery)".to_string(),
    }
}

struct ResolvedColumn<'db, DB: DatabaseLike, D: Copy> {
    source: Option<&'db DB::Table>,
    definition: D,
}

enum LookupOutcome<T> {
    Found(T),
    SearchParent,
    Stop,
}

fn visible_merged_boundary(
    merged: &[MergedName],
    column: &Ident,
    visible_entries: usize,
) -> Option<usize> {
    merged
        .iter()
        .filter(|merged| merged.subsumed <= visible_entries)
        .rev()
        .find(|merged| {
            identifiers_match(
                &merged.name,
                merged.quoted,
                column.value.as_str(),
                column.quote_style.is_some(),
            )
        })
        .map(|merged| merged.subsumed)
}

fn opaque_qualifier_matches_key(
    key: RelationKey<'_, '_>,
    schema: Option<RelationKey<'_, '_>>,
    qualifier: &[Ident],
) -> bool {
    match qualifier {
        [table] => key_matches(key, table.value.as_str(), table.quote_style.is_some()),
        [schema_part, table] => {
            schema.is_some_and(|schema| {
                key_matches(schema, schema_part.value.as_str(), schema_part.quote_style.is_some())
            }) && key_matches(key, table.value.as_str(), table.quote_style.is_some())
        }
        _ => false,
    }
}

fn opaque_qualifier_matches(identity: &OpaqueIdentity<'_, '_>, qualifier: &[Ident]) -> bool {
    match identity {
        OpaqueIdentity::Known { key, schema } => {
            opaque_qualifier_matches_key(*key, *schema, qualifier)
        }
        OpaqueIdentity::Multiple(qualifiers) => {
            qualifiers.iter().any(|candidate| {
                opaque_qualifier_matches_key(candidate.key, candidate.schema, qualifier)
            })
        }
        OpaqueIdentity::Anonymous => false,
    }
}

fn unqualified_definition_local<'db, DB: DatabaseLike, D: Copy>(
    scope: &FromScope<'_, 'db, DB, D>,
    visible_entries: usize,
    opaque: D,
    column: &Ident,
    require_row_identity: bool,
) -> Result<LookupOutcome<ResolvedColumn<'db, DB, D>>, LookupError> {
    if scope.unqualified_poison
        || scope.opaque.iter().any(|entry| entry.entry_index < visible_entries)
    {
        return Ok(LookupOutcome::Found(ResolvedColumn { source: None, definition: opaque }));
    }
    let mut definitions = Vec::new();
    let mut candidates = Vec::new();
    let boundary = visible_merged_boundary(&scope.merged, column, visible_entries);
    if boundary.is_some() {
        definitions.push(ResolvedColumn { source: None, definition: opaque });
    }
    let start = boundary.unwrap_or(0);
    for base in scope
        .bases
        .iter()
        .filter(|base| base.entry_index >= start && base.entry_index < visible_entries)
    {
        if let Some(base_column) = find_base_column(base, column) {
            definitions.push(ResolvedColumn {
                source: (!require_row_identity || !base.nullable).then_some(base.table),
                definition: base_column.definition,
            });
            candidates.push(render_table_candidate(base.table));
        }
    }
    for relation in scope
        .derived
        .iter()
        .filter(|relation| relation.entry_index >= start && relation.entry_index < visible_entries)
    {
        match find_derived_column(&relation.shape.columns, column) {
            Some(DerivedMatch::Column(found)) => {
                definitions.push(ResolvedColumn {
                    source: (!require_row_identity || !relation.nullable)
                        .then_some(found.source)
                        .flatten(),
                    definition: found.definition,
                });
                candidates.push(relation_key_display(relation));
            }
            Some(DerivedMatch::Ambiguous) => {
                definitions.push(ResolvedColumn { source: None, definition: opaque });
                definitions.push(ResolvedColumn { source: None, definition: opaque });
                candidates.push(relation_key_display(relation));
            }
            None => {}
        }
    }
    match definitions.len() {
        0 => Ok(LookupOutcome::SearchParent),
        1 => Ok(LookupOutcome::Found(definitions.swap_remove(0))),
        _ => {
            candidates.sort_unstable();
            candidates.dedup();
            Err(LookupError::AmbiguousTableLookup { object_name: column.value.clone(), candidates })
        }
    }
}

fn resolve_definition_local<'db, DB: DatabaseLike, D: Copy>(
    scope: &FromScope<'_, 'db, DB, D>,
    visible_entries: usize,
    opaque: D,
    expression: &Expr,
    require_row_identity: bool,
) -> Result<LookupOutcome<ResolvedColumn<'db, DB, D>>, LookupError> {
    match expression {
        Expr::Identifier(column) => {
            unqualified_definition_local(
                scope,
                visible_entries,
                opaque,
                column,
                require_row_identity,
            )
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let column = &parts[1];
            let qualifier = &parts[0];
            let value = qualifier.value.as_str();
            let quoted = qualifier.quote_style.is_some();
            if let Some(base) = base_for_qualifier(
                &scope.bases,
                value,
                quoted,
                require_row_identity,
                visible_entries,
            ) {
                return Ok(match find_base_column(base, column) {
                    Some(column) => {
                        LookupOutcome::Found(ResolvedColumn {
                            source: Some(base.table),
                            definition: column.definition,
                        })
                    }
                    None => LookupOutcome::Stop,
                });
            }
            if let Some(relation) = derived_for_qualifier(
                &scope.derived,
                value,
                quoted,
                require_row_identity,
                visible_entries,
            ) {
                return match find_derived_column(&relation.shape.columns, column) {
                    Some(DerivedMatch::Column(column)) => {
                        Ok(LookupOutcome::Found(ResolvedColumn {
                            source: column.source,
                            definition: column.definition,
                        }))
                    }
                    Some(DerivedMatch::Ambiguous) => {
                        Err(LookupError::AmbiguousTableLookup {
                            object_name: column.value.clone(),
                            candidates: vec![relation_key_display(relation)],
                        })
                    }
                    None => Ok(LookupOutcome::Stop),
                };
            }
            if scope.opaque.iter().any(|entry| {
                entry.entry_index < visible_entries
                    && opaque_qualifier_matches(&entry.identity, &parts[..1])
            }) {
                Ok(LookupOutcome::Found(ResolvedColumn { source: None, definition: opaque }))
            } else {
                Ok(LookupOutcome::SearchParent)
            }
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 3 => {
            if let Some(base) = base_for_qualified_name(
                &scope.bases,
                parts[0].value.as_str(),
                parts[0].quote_style.is_some(),
                parts[1].value.as_str(),
                parts[1].quote_style.is_some(),
                require_row_identity,
                visible_entries,
            ) {
                return Ok(match find_base_column(base, &parts[2]) {
                    Some(column) => {
                        LookupOutcome::Found(ResolvedColumn {
                            source: Some(base.table),
                            definition: column.definition,
                        })
                    }
                    None => LookupOutcome::Stop,
                });
            }
            if scope.opaque.iter().any(|entry| {
                entry.entry_index < visible_entries
                    && opaque_qualifier_matches(&entry.identity, &parts[..2])
            }) {
                Ok(LookupOutcome::Found(ResolvedColumn { source: None, definition: opaque }))
            } else {
                Ok(LookupOutcome::SearchParent)
            }
        }
        _ => Ok(LookupOutcome::Stop),
    }
}

fn column_source<'db, DB: DatabaseLike, D: Copy>(
    expression: &Expr,
    scope: &FromScope<'_, 'db, DB, D>,
    visible_entries: usize,
    opaque: D,
    require_row_identity: bool,
) -> Result<Option<&'db DB::Table>, LookupError> {
    if require_row_identity
        && scope
            .derived
            .iter()
            .filter(|relation| relation.entry_index < visible_entries)
            .any(|relation| !relation.shape.row_preserving)
    {
        return Ok(None);
    }
    match resolve_definition_local(
        scope,
        visible_entries,
        opaque,
        expression,
        require_row_identity,
    )? {
        LookupOutcome::Found(column) => Ok(column.source),
        LookupOutcome::SearchParent | LookupOutcome::Stop => Ok(None),
    }
}

/// A base table's output names with the alias column list applied
/// positionally. PostgreSQL replaces the originals with the aliases (a
/// partial list keeps the tail's own names). More aliases than columns is a
/// mismatch PostgreSQL rejects, reported here as `None` so the relation
/// stays opaque.
fn aliased_output_columns<'query, 'db, DB, P>(
    table: &'db DB::Table,
    alias: Option<AstRef<'query, 'db, TableAlias>>,
    database: &'db DB,
    profile: &mut P,
) -> Result<Option<BaseColumns<P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let mut output_columns = table
        .columns(database)?
        .map(|column| {
            BaseColumnRef {
                name: column.column_name().to_string(),
                quoted: column.column_name_is_quoted(),
                definition: profile.base_definition(table, column),
            }
        })
        .collect::<Vec<_>>();
    if let Some(table_alias) = alias
        && !table_alias.get().columns.is_empty()
    {
        if table_alias.get().columns.len() > output_columns.len() {
            return Ok(None);
        }
        for (column, alias_column) in output_columns.iter_mut().zip(&table_alias.get().columns) {
            alias_column.name.value.clone_into(&mut column.name);
            column.quoted = alias_column.name.quote_style.is_some();
        }
    }
    Ok(Some(output_columns))
}

/// Records a `FROM` relation that names a view, deriving its output columns
/// from the definition the database holds.
///
/// A view's definition is resolved in its own context, so the enclosing
/// statement's `WITH` items are not in scope inside it. Its own column list
/// renames what the definition produces, positionally, and the `FROM` alias's
/// column list then renames again on top, both as PostgreSQL applies them.
///
/// A materialized view answers column references the same way, because a
/// column's declared type is inherited from what it reads and cannot go stale.
/// Its rows, though, are a snapshot taken when it was last populated rather
/// than the current rows of anything, so its shape never preserves row
/// identity and the row-identity question stops there.
///
/// A reference reaching a view already being derived closes a cycle, which
/// PostgreSQL accepts at creation and refuses only on read, so the reference
/// stays opaque rather than recursing.
fn collect_view_factor<'query, 'db, DB, P>(
    name: AstRef<'query, 'db, ObjectName>,
    alias: Option<AstRef<'query, 'db, TableAlias>>,
    key: RelationKey<'query, 'db>,
    schema: Option<RelationKey<'query, 'db>>,
    deriving: Deriving<'_, 'db, DB>,
    profile: &mut P,
) -> Result<FactorContribution<'query, 'db, DB, P::Definition>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let Some(target) = target_name_from_object_name(name.get()) else {
        return Ok(opaque_factor(OpaqueIdentity::Known { key, schema }));
    };
    let (view, row_preserving) = if let Some(view) = deriving.view(target.clone())? {
        (DerivingView::Plain(view), true)
    } else if let Some(view) = deriving.materialized_view(target)? {
        (DerivingView::Materialized(view), false)
    } else {
        return Ok(opaque_factor(OpaqueIdentity::Known { key, schema }));
    };
    let shape = derive_view_shape(view, row_preserving, deriving, profile)?;
    let shape = match alias {
        Some(table_alias) => apply_alias_columns(shape, table_alias.get()),
        None => shape,
    };
    let Some(shape) = shape else {
        return Ok(opaque_factor(OpaqueIdentity::Known { key, schema }));
    };
    let names = derived_column_names(&shape.columns);
    Ok(FactorContribution {
        relation: RelationContribution::Derived(DerivedRelationRef {
            key: Some(key),
            shape,
            nullable: false,
            entry_index: 0,
        }),
        names: Some(names),
    })
}

/// The output shape of a view's definition, with the view's own column list
/// applied, or [`None`] when the definition's columns cannot be enumerated or
/// the reference closes a cycle.
fn derive_view_shape<'query, 'db, DB, P>(
    view: DerivingView<'db, DB>,
    row_preserving: bool,
    deriving: Deriving<'_, 'db, DB>,
    profile: &mut P,
) -> Result<Option<DerivedShape<'query, 'db, DB, P::Definition>>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    if deriving.is_deriving(view) {
        return Ok(None);
    }
    let (definition, declared) = match view {
        DerivingView::Plain(view) => (view.definition(), view.declared_column_names()),
        DerivingView::Materialized(view) => (view.definition(), view.declared_column_names()),
    };
    let frame = DerivingFrame { view, parent: deriving.views };
    let inner = Deriving { database: deriving.database, views: Some(&frame) };
    let output_names = (!declared.is_empty()).then_some(OutputNameSource::Declared(declared));
    let parent = profile.no_parent();
    let Some(mut shape) = derive_query_shape(
        AstRef::Database(definition),
        &[],
        output_names,
        inner,
        parent,
        profile,
    )?
    else {
        return Ok(None);
    };
    if declared.len() > shape.columns.len() {
        return Ok(None);
    }
    for (column, (declared_name, declared_quoted)) in shape.columns.iter_mut().zip(declared.iter())
    {
        declared_name.clone_into(&mut column.name);
        column.quoted = *declared_quoted;
    }
    shape.row_preserving &= row_preserving;
    Ok(Some(shape))
}
fn derived_column_names<DB: DatabaseLike, D: Copy>(
    columns: &[DerivedColumn<'_, '_, DB, D>],
) -> Vec<(String, bool)> {
    columns.iter().map(|column| (column.name.clone(), column.quoted)).collect()
}

fn alias_key<'query, 'db>(alias: AstRef<'query, 'db, TableAlias>) -> RelationKey<'query, 'db> {
    RelationKey {
        value: alias.map(|alias| alias.name.value.as_str()),
        quoted: alias.get().name.quote_style.is_some(),
    }
}

fn object_name_key<'query, 'db>(
    name: AstRef<'query, 'db, ObjectName>,
) -> Option<RelationKey<'query, 'db>> {
    match name {
        AstRef::Query(name) => {
            let part = name.0.last()?.as_ident()?;
            Some(RelationKey {
                value: AstRef::Query(part.value.as_str()),
                quoted: part.quote_style.is_some(),
            })
        }
        AstRef::Database(name) => {
            let part = name.0.last()?.as_ident()?;
            Some(RelationKey {
                value: AstRef::Database(part.value.as_str()),
                quoted: part.quote_style.is_some(),
            })
        }
    }
}

fn object_name_schema<'query, 'db>(
    name: AstRef<'query, 'db, ObjectName>,
) -> Option<RelationKey<'query, 'db>> {
    match name {
        AstRef::Query(name) if name.0.len() == 2 => {
            let part = name.0.first()?.as_ident()?;
            Some(RelationKey {
                value: AstRef::Query(part.value.as_str()),
                quoted: part.quote_style.is_some(),
            })
        }
        AstRef::Database(name) if name.0.len() == 2 => {
            let part = name.0.first()?.as_ident()?;
            Some(RelationKey {
                value: AstRef::Database(part.value.as_str()),
                quoted: part.quote_style.is_some(),
            })
        }
        AstRef::Query(_) | AstRef::Database(_) => None,
    }
}

fn stored_schema_key<'query, DB: DatabaseLike>(table: &DB::Table) -> RelationKey<'query, '_> {
    RelationKey {
        value: AstRef::Database(table.table_schema().unwrap_or("public")),
        quoted: table.table_schema().is_some() && table.table_schema_is_quoted(),
    }
}

fn opaque_factor<'query, 'db, DB: DatabaseLike, D: Copy>(
    identity: OpaqueIdentity<'query, 'db>,
) -> FactorContribution<'query, 'db, DB, D> {
    FactorContribution { relation: RelationContribution::Opaque(identity), names: None }
}

fn factor_alias<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
) -> Option<AstRef<'query, 'db, TableAlias>> {
    factor.try_map(|factor| {
        match factor {
            TableFactor::Table { alias, .. }
            | TableFactor::Derived { alias, .. }
            | TableFactor::TableFunction { alias, .. }
            | TableFactor::Function { alias, .. }
            | TableFactor::UNNEST { alias, .. }
            | TableFactor::JsonTable { alias, .. }
            | TableFactor::OpenJsonTable { alias, .. }
            | TableFactor::NestedJoin { alias, .. }
            | TableFactor::Pivot { alias, .. }
            | TableFactor::Unpivot { alias, .. }
            | TableFactor::MatchRecognize { alias, .. }
            | TableFactor::XmlTable { alias, .. }
            | TableFactor::SemanticView { alias, .. } => alias.as_ref(),
            TableFactor::UnpivotExpr { .. } => None,
        }
    })
}

fn factor_name<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
) -> Option<AstRef<'query, 'db, ObjectName>> {
    factor.try_map(|factor| {
        match factor {
            TableFactor::Table { name, .. } | TableFactor::Function { name, .. } => Some(name),
            _ => None,
        }
    })
}

fn derived_factor_query<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
) -> Option<AstRef<'query, 'db, Query>> {
    factor.try_map(|factor| {
        match factor {
            TableFactor::Derived { subquery, .. } => Some(subquery.as_ref()),
            _ => None,
        }
    })
}

fn table_function_expression<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
) -> Option<AstRef<'query, 'db, Expr>> {
    factor.try_map(|factor| {
        match factor {
            TableFactor::TableFunction { expr, .. } => Some(expr),
            _ => None,
        }
    })
}

fn table_function_name<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
) -> Option<AstRef<'query, 'db, ObjectName>> {
    match table_function_expression(factor)? {
        AstRef::Query(Expr::Function(function)) => Some(AstRef::Query(&function.name)),
        AstRef::Database(Expr::Function(function)) => Some(AstRef::Database(&function.name)),
        AstRef::Query(_) | AstRef::Database(_) => None,
    }
}

fn opaque_identity_from_alias<'query, 'db>(
    alias: Option<AstRef<'query, 'db, TableAlias>>,
) -> OpaqueIdentity<'query, 'db> {
    alias.map_or(OpaqueIdentity::Anonymous, |alias| {
        OpaqueIdentity::Known { key: alias_key(alias), schema: None }
    })
}

fn push_opaque_qualifier<'query, 'db>(
    first: &mut Option<OpaqueQualifier<'query, 'db>>,
    additional: &mut Vec<OpaqueQualifier<'query, 'db>>,
    qualifier: OpaqueQualifier<'query, 'db>,
) {
    if first.is_none() {
        *first = Some(qualifier);
    } else {
        additional.push(qualifier);
    }
}

fn collect_nested_factor_qualifiers<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
    first: &mut Option<OpaqueQualifier<'query, 'db>>,
    additional: &mut Vec<OpaqueQualifier<'query, 'db>>,
) {
    if let Some(alias) = factor_alias(factor) {
        push_opaque_qualifier(
            first,
            additional,
            OpaqueQualifier { key: alias_key(alias), schema: None },
        );
        return;
    }
    match factor.get() {
        TableFactor::Table { .. } => {
            let Some(name) = factor_name(factor) else { return };
            let Some(key) = object_name_key(name) else { return };
            push_opaque_qualifier(
                first,
                additional,
                OpaqueQualifier { key, schema: object_name_schema(name) },
            );
        }
        TableFactor::TableFunction { .. } => {
            let Some(key) = table_function_name(factor).and_then(object_name_key) else {
                return;
            };
            push_opaque_qualifier(first, additional, OpaqueQualifier { key, schema: None });
        }
        TableFactor::Function { .. } => {
            let Some(key) = factor_name(factor).and_then(object_name_key) else { return };
            push_opaque_qualifier(first, additional, OpaqueQualifier { key, schema: None });
        }
        TableFactor::NestedJoin { .. } => {
            let Some(table) = factor.try_map(|factor| {
                let TableFactor::NestedJoin { table_with_joins, .. } = factor else { return None };
                Some(table_with_joins.as_ref())
            }) else {
                return;
            };
            collect_nested_factor_qualifiers(table.map(|table| &table.relation), first, additional);
            for join in table.map(|table| table.joins.as_slice()).iter() {
                collect_nested_factor_qualifiers(
                    join.map(|join| &join.relation),
                    first,
                    additional,
                );
            }
        }
        TableFactor::Derived { .. }
        | TableFactor::UNNEST { .. }
        | TableFactor::JsonTable { .. }
        | TableFactor::OpenJsonTable { .. }
        | TableFactor::Pivot { .. }
        | TableFactor::Unpivot { .. }
        | TableFactor::UnpivotExpr { .. }
        | TableFactor::MatchRecognize { .. }
        | TableFactor::XmlTable { .. }
        | TableFactor::SemanticView { .. } => {}
    }
}

fn nested_join_identity<'query, 'db>(
    factor: AstRef<'query, 'db, TableFactor>,
) -> OpaqueIdentity<'query, 'db> {
    if let Some(alias) = factor_alias(factor) {
        return opaque_identity_from_alias(Some(alias));
    }
    let mut first = None;
    let mut additional = Vec::new();
    collect_nested_factor_qualifiers(factor, &mut first, &mut additional);
    let Some(first) = first else { return OpaqueIdentity::Anonymous };
    if additional.is_empty() {
        return OpaqueIdentity::Known { key: first.key, schema: first.schema };
    }
    additional.insert(0, first);
    OpaqueIdentity::Multiple(additional)
}

fn collect_table_factor<'query, 'db, DB, P>(
    factor: AstRef<'query, 'db, TableFactor>,
    has_arguments: bool,
    deriving: Deriving<'_, 'db, DB>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    profile: &mut P,
) -> Result<FactorContribution<'query, 'db, DB, P::Definition>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    let Some(name) = factor_name(factor) else {
        return Ok(opaque_factor(OpaqueIdentity::Anonymous));
    };
    let alias = factor_alias(factor);
    let Some(key) = alias.map(alias_key).or_else(|| object_name_key(name)) else {
        return Ok(opaque_factor(OpaqueIdentity::Anonymous));
    };
    let schema = alias.is_none().then(|| object_name_schema(name)).flatten();
    if has_arguments {
        return Ok(opaque_factor(OpaqueIdentity::Known { key, schema: None }));
    }
    if let Some(cte) = find_cte(name.get(), cte_scope) {
        let shape = match (cte.shape.clone(), alias) {
            (Some(shape), Some(table_alias)) => apply_alias_columns(Some(shape), table_alias.get()),
            (shape, _) => shape,
        };
        let Some(shape) = shape else {
            return Ok(opaque_factor(OpaqueIdentity::Known { key, schema: None }));
        };
        let names = derived_column_names(&shape.columns);
        return Ok(FactorContribution {
            relation: RelationContribution::Derived(DerivedRelationRef {
                key: Some(key),
                shape,
                nullable: false,
                entry_index: 0,
            }),
            names: Some(names),
        });
    }
    let database = deriving.database;
    let Some(table) = deriving.table(name.get())? else {
        return collect_view_factor(name, alias, key, schema, deriving, profile);
    };
    let Some(output_columns) = aliased_output_columns(table, alias, database, profile)? else {
        return Ok(opaque_factor(OpaqueIdentity::Known { key, schema }));
    };
    let names = output_columns.iter().map(|column| (column.name.clone(), column.quoted)).collect();
    Ok(FactorContribution {
        relation: RelationContribution::Base(FromTableRef {
            key,
            schema_key: alias.is_none().then(|| stored_schema_key::<DB>(table)),
            table,
            nullable: false,
            entry_index: 0,
            output_columns,
        }),
        names: Some(names),
    })
}

/// Records a single `FROM` table factor into the scope, returning the output
/// column names it contributes and the wildcard plan entry naming what it
/// pushed, or `None` when the factor is opaque.
fn collect_factor<'query, 'db, DB, P>(
    factor: AstRef<'query, 'db, TableFactor>,
    deriving: Deriving<'_, 'db, DB>,
    cte_scope: &[CteShape<'query, 'db, DB, P::Definition>],
    inherited_parent: P::Cursor,
    local_parent: P::Cursor,
    profile: &mut P,
) -> Result<FactorContribution<'query, 'db, DB, P::Definition>, LookupError>
where
    DB: DatabaseLike,
    P: DerivationProfile<'query, 'db, DB>,
{
    match factor.get() {
        TableFactor::Table { args, .. } => {
            collect_table_factor(factor, args.is_some(), deriving, cte_scope, profile)
        }
        TableFactor::Derived { lateral, .. } => {
            let Some(subquery) = derived_factor_query(factor) else {
                return Ok(opaque_factor(OpaqueIdentity::Anonymous));
            };
            let alias = factor_alias(factor);
            let output_names = alias.and_then(OutputNameSource::from_alias);
            let parent = if *lateral { local_parent } else { inherited_parent };
            let body =
                derive_query_shape(subquery, cte_scope, output_names, deriving, parent, profile)?;
            let shape = match alias {
                Some(table_alias) => apply_alias_columns(body, table_alias.get()),
                None => body,
            };
            let Some(shape) = shape else {
                return Ok(opaque_factor(opaque_identity_from_alias(alias)));
            };
            let names = derived_column_names(&shape.columns);
            Ok(FactorContribution {
                relation: RelationContribution::Derived(DerivedRelationRef {
                    key: alias.map(alias_key),
                    shape,
                    nullable: false,
                    entry_index: 0,
                }),
                names: Some(names),
            })
        }
        TableFactor::TableFunction { .. } => {
            let alias = factor_alias(factor);
            let key = alias
                .map(alias_key)
                .or_else(|| table_function_name(factor).and_then(object_name_key));
            Ok(opaque_factor(key.map_or(OpaqueIdentity::Anonymous, |key| {
                OpaqueIdentity::Known { key, schema: None }
            })))
        }
        TableFactor::Function { .. } => {
            let Some(name) = factor_name(factor) else {
                return Ok(opaque_factor(OpaqueIdentity::Anonymous));
            };
            let alias = factor_alias(factor);
            let key = alias.map(alias_key).or_else(|| object_name_key(name));
            Ok(opaque_factor(key.map_or(OpaqueIdentity::Anonymous, |key| {
                OpaqueIdentity::Known { key, schema: None }
            })))
        }
        TableFactor::NestedJoin { .. } => Ok(opaque_factor(nested_join_identity(factor))),
        _ => Ok(opaque_factor(opaque_identity_from_alias(factor_alias(factor)))),
    }
}

pub(crate) fn build_definition_graph<'query, 'db, DB: DatabaseLike>(
    query: &'query Query,
    database: &'db DB,
) -> Result<(definition_graph::DefinitionGraph<'query, 'db, DB>, ScopeCursor), LookupError> {
    let deriving = Deriving::of(database);
    let mut profile = DefinitionDerivation::new();
    let parent = profile.no_parent();
    let root =
        index_nested_query_scopes(AstRef::Query(query), &[], deriving, parent, &mut profile)?;
    profile.record_outputs(query)?;
    Ok(match root {
        Some(cursor) if matches!(query.body.as_ref(), SetExpr::Select(_)) => {
            profile.finish(cursor.scope)
        }
        _ => profile.empty_scope(),
    })
}

impl<DB: DatabaseLike> DQLLike<DB> for Query {
    fn projection_source_table<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Option<&'db DB::Table>, LookupError> {
        // Only a plain SELECT body has a single projection to analyze.
        let SetExpr::Select(select) = self.body.as_ref() else {
            return Ok(None);
        };

        // DISTINCT and GROUP BY collapse rows, so the output is not keyed by a
        // base table's primary key.
        if select.distinct.is_some() {
            return Ok(None);
        }
        match &select.group_by {
            GroupByExpr::All(_) => return Ok(None),
            GroupByExpr::Expressions(expressions, _) if !expressions.is_empty() => {
                return Ok(None);
            }
            GroupByExpr::Expressions(_, _) => {}
        }

        let Some(scope) = collect_source_from_clause(self, database)? else {
            return Ok(None);
        };

        let mut source: Option<&'db DB::Table> = None;
        for item in &select.projection {
            let item_source = match item {
                // `*` is a single base-table row only when the FROM is exactly
                // that one base table, or exactly one row-preserving derived
                // relation whose every column passes through the same table.
                // `REPLACE` substitutes a computed value for one of the
                // columns, so the output is no longer a source row, while
                // `EXCLUDE`, `EXCEPT`, `ILIKE` and `RENAME` only drop or
                // relabel columns and leave the rows themselves intact.
                SelectItem::Wildcard(options) => {
                    if wildcard_replaces_values(options) {
                        None
                    } else if scope.from_entry_count == 1 && scope.bases.len() == 1 {
                        Some(scope.bases[0].table)
                    } else if scope.from_entry_count == 1 && scope.derived.len() == 1 {
                        single_source_relation(&scope.derived[0], database)
                    } else {
                        None
                    }
                }
                SelectItem::QualifiedWildcard(_, options) if wildcard_replaces_values(options) => {
                    None
                }
                SelectItem::QualifiedWildcard(kind, _) => {
                    match kind {
                        SelectItemQualifiedWildcardKind::ObjectName(object_name) => {
                            match resolve_wildcard_target(
                                &scope.bases,
                                &scope.derived,
                                object_name,
                                true,
                            ) {
                                Some(WildcardTarget::Base(base)) => Some(base.table),
                                Some(WildcardTarget::Derived(relation)) => {
                                    single_source_relation(relation, database)
                                }
                                None => None,
                            }
                        }
                        SelectItemQualifiedWildcardKind::Expr(_) => None,
                    }
                }
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    column_source(expr, &scope, scope.from_entry_count, (), true)?
                }
                // Spark's `expr AS (a, b)` names several outputs for one
                // expression, which is not a single pass-through column.
                SelectItem::ExprWithAliases { .. } => None,
            };

            match item_source {
                None => return Ok(None),
                Some(table) => {
                    match source {
                        None => source = Some(table),
                        Some(existing) => {
                            if database.table_id(existing) != database.table_id(table) {
                                return Ok(None);
                            }
                        }
                    }
                }
            }
        }

        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{Ident, SelectItem, SetExpr, Statement, Table},
        dialect::{BigQueryDialect, Dialect, GenericDialect},
        parser::Parser,
    };

    use super::{
        AstRef, DerivationProfile, OpaqueIdentity, RelationKey, SourceDerivation, cte_dependencies,
        opaque_qualifier_matches, opaque_qualifier_matches_key, set_expr_ref,
    };
    use crate::{
        errors::LookupError,
        prelude::ParserDB,
        structs::{ColumnDefinition, ColumnScope},
        traits::{DQLLike, TableLike},
    };

    std::thread_local! {
        static CTE_SHAPE_DERIVATIONS: core::cell::Cell<usize> =
            const { core::cell::Cell::new(0) };
    }

    pub(super) fn record_cte_shape_derivation() {
        CTE_SHAPE_DERIVATIONS.with(|count| count.set(count.get() + 1));
    }

    fn reset_cte_shape_derivations() {
        CTE_SHAPE_DERIVATIONS.with(|count| count.set(0));
    }

    fn cte_shape_derivations() -> usize {
        CTE_SHAPE_DERIVATIONS.with(core::cell::Cell::get)
    }

    const SCHEMA: &str = "
        CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
        CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total INT);
    ";

    fn schema_db() -> ParserDB {
        ParserDB::parse::<GenericDialect>(SCHEMA).expect("schema parses")
    }

    fn query_of_with<D: Dialect>(dialect: &D, sql: &str) -> sqlparser::ast::Query {
        let mut statements = Parser::parse_sql(dialect, sql).expect("query parses");
        match statements.pop().expect("one statement") {
            Statement::Query(query) => *query,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    fn query_of(sql: &str) -> sqlparser::ast::Query {
        query_of_with(&GenericDialect {}, sql)
    }

    fn source_name(sql: &str, db: &ParserDB) -> Option<String> {
        query_of(sql)
            .projection_source_table(db)
            .expect("projection_source_table succeeds")
            .map(|table| table.table_name().to_string())
    }

    fn nested_recursive_cte_body(depth: usize) -> String {
        if depth == 0 {
            return "SELECT id FROM users UNION ALL SELECT t0.id FROM t0 WHERE false".to_string();
        }
        let inner = depth - 1;
        format!(
            "WITH RECURSIVE t{inner} AS ({}) SELECT id FROM t{inner} \
             UNION ALL SELECT t{depth}.id FROM t{depth} WHERE false",
            nested_recursive_cte_body(inner)
        )
    }

    #[test]
    fn source_profile_and_non_select_bodies_take_their_explicit_fallbacks() {
        fn exercise_source_profile<'query, 'db>(
            select: &'query sqlparser::ast::Select,
            _database: &'db ParserDB,
        ) {
            let mut profile = SourceDerivation;
            assert!(
                <SourceDerivation as DerivationProfile<'query, 'db, ParserDB>>::cursor_for_select(
                    &profile,
                    AstRef::Query(select),
                )
                .is_none()
            );
            <SourceDerivation as DerivationProfile<'query, 'db, ParserDB>>::set_scope_owners_since(
                &mut profile,
                (),
                (),
                (),
            );
        }

        let database = schema_db();
        let select_query = query_of("SELECT id FROM users");
        let SetExpr::Select(select) = select_query.body.as_ref() else {
            panic!("expected a select body")
        };
        exercise_source_profile(select, &database);

        let mut table_query = query_of("SELECT id FROM users");
        *table_query.body = SetExpr::Table(Box::new(Table {
            table_name: Some(Ident::new("users")),
            schema_name: None,
        }));
        assert!(matches!(
            set_expr_ref(AstRef::Query(table_query.body.as_ref())),
            super::SetExprRef::Other
        ));
        assert!(matches!(
            set_expr_ref(AstRef::Database(table_query.body.as_ref())),
            super::SetExprRef::Other
        ));
        assert!(
            super::collect_source_from_clause(&table_query, &database)
                .expect("source collection succeeds")
                .is_none()
        );
    }

    #[test]
    fn qualified_expression_wildcards_are_walked_before_the_shape_stays_opaque() {
        let database = schema_db();
        let query = query_of_with(
            &BigQueryDialect {},
            "SELECT d.x FROM (\
                 SELECT f((SELECT id FROM users)).field.*, id AS x FROM users\
             ) AS d",
        );
        let scope = ColumnScope::from_query(&query, &database).expect("scope builds");
        let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a select body") };
        let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
            panic!("expected an expression projection")
        };
        assert!(matches!(
            scope.resolve_column_definition(reference).expect("definition resolves"),
            Some(ColumnDefinition::Opaque)
        ));
    }

    #[test]
    fn private_fallback_helpers_reject_unrepresentable_names() {
        let key = RelationKey { value: AstRef::Query("table"), quoted: false };
        let qualifier = [Ident::new("catalog"), Ident::new("schema"), Ident::new("table")];
        assert!(!opaque_qualifier_matches_key(key, None, &qualifier));
        assert!(!opaque_qualifier_matches(&OpaqueIdentity::Anonymous, &qualifier));

        let ast = AstRef::Query("value");
        assert_eq!(ast.clone().get(), "value");

        let cte = &query_of("WITH c AS (SELECT 1 FROM schema.c) SELECT * FROM c")
            .with
            .expect("with clause")
            .cte_tables[0];
        assert_eq!(cte_dependencies(cte, &[]), Vec::<bool>::new());
        let empty = sqlparser::ast::ObjectName(Vec::new());
        let mut visitor = super::CteDependencyVisitor {
            candidates: &[],
            shadowed: Vec::new(),
            dependencies: Vec::new(),
        };
        assert!(matches!(
            sqlparser::ast::Visitor::pre_visit_relation(&mut visitor, &empty),
            core::ops::ControlFlow::Continue(())
        ));
    }

    #[test]
    fn nested_recursive_ctes_derive_each_with_once() {
        let depth = 6;
        let db = schema_db();
        let sql = format!(
            "WITH RECURSIVE t{depth} AS ({}) SELECT t{depth}.id FROM t{depth}",
            nested_recursive_cte_body(depth)
        );
        reset_cte_shape_derivations();
        assert_eq!(source_name(&sql, &db), Some("users".to_string()));
        assert_eq!(cte_shape_derivations(), depth + 1);
    }

    #[test]
    fn single_table_columns() {
        let db = schema_db();
        assert_eq!(source_name("SELECT id, name FROM users", &db), Some("users".to_string()));
    }

    #[test]
    fn single_table_wildcard() {
        let db = schema_db();
        assert_eq!(source_name("SELECT * FROM users", &db), Some("users".to_string()));
    }

    #[test]
    fn join_qualified_single_table_columns() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT o.id, o.total FROM orders o JOIN users u ON o.user_id = u.id", &db),
            Some("orders".to_string())
        );
    }

    #[test]
    fn join_qualified_wildcard() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT o.* FROM orders o JOIN users u ON o.user_id = u.id", &db),
            Some("orders".to_string())
        );
    }

    #[test]
    fn join_multi_table_projection_is_none() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT o.id, u.name FROM orders o JOIN users u ON o.user_id = u.id", &db),
            None
        );
    }

    #[test]
    fn wildcard_over_join_is_none() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT * FROM orders o JOIN users u ON o.user_id = u.id", &db),
            None
        );
    }

    #[test]
    fn computed_projection_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT o.id + 1 FROM orders o", &db), None);
    }

    #[test]
    fn aggregate_projection_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT COUNT(*) FROM users", &db), None);
    }

    #[test]
    fn group_by_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT user_id FROM orders GROUP BY user_id", &db), None);
    }

    #[test]
    fn distinct_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT DISTINCT user_id FROM orders", &db), None);
    }

    #[test]
    fn subquery_in_where_keeps_single_table() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT id, name FROM users WHERE id IN (SELECT user_id FROM orders)", &db),
            Some("users".to_string())
        );
    }

    #[test]
    fn set_operation_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT id FROM users UNION SELECT id FROM orders", &db), None);
    }

    #[test]
    fn cte_reference_resolves_through_a_row_preserving_body() {
        let db = schema_db();
        // `users` here is the CTE, not the base table. The CTE renames one
        // table's rows, so the projection is still rows of `orders`.
        assert_eq!(
            source_name("WITH users AS (SELECT id FROM orders) SELECT id FROM users", &db),
            Some("orders".to_string())
        );
    }

    #[test]
    fn derived_table_projection_resolves_through_the_body() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT total FROM (SELECT total FROM orders) d", &db),
            Some("orders".to_string())
        );
    }

    #[test]
    fn grouped_cte_body_refuses_the_row_identity_answer() {
        let db = schema_db();
        // The CTE column passes through `orders`, but grouped rows are not
        // orders rows, so the row-identity answer must stay nothing.
        assert_eq!(
            source_name(
                "WITH s AS (SELECT user_id, count(*) AS n FROM orders GROUP BY user_id) \
                 SELECT user_id FROM s",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH s AS (SELECT user_id, count(*) AS n FROM orders GROUP BY user_id) \
                 SELECT s.user_id FROM s",
                &db
            ),
            None
        );
    }

    #[test]
    fn distinct_cte_body_refuses_the_row_identity_answer() {
        let db = schema_db();
        assert_eq!(
            source_name("WITH v AS (SELECT DISTINCT name FROM users) SELECT name FROM v", &db),
            None
        );
    }

    #[test]
    fn wildcard_over_row_preserving_cte_resolves() {
        let db = schema_db();
        assert_eq!(
            source_name("WITH v AS (SELECT id, total FROM orders) SELECT * FROM v", &db),
            Some("orders".to_string())
        );
        assert_eq!(
            source_name("WITH v AS (SELECT id, total FROM orders) SELECT v.* FROM v", &db),
            Some("orders".to_string())
        );
    }

    // A wildcard over a relation whose body collapses rows is not rows of the
    // base table, even though every output column passes through it, so the
    // row-identity question answers nothing. The qualified path refuses such
    // a relation earlier, so only a wildcard reaches this rule.
    #[test]
    fn wildcard_over_collapsing_body_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT * FROM (SELECT DISTINCT total FROM orders) s", &db), None);
        assert_eq!(
            source_name(
                "WITH v AS (SELECT total FROM orders UNION SELECT total FROM orders) \
                 SELECT * FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name("SELECT * FROM (SELECT total FROM orders) s", &db),
            Some("orders".to_string())
        );
    }

    #[test]
    fn wildcard_over_cte_with_a_computed_column_is_none() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH s AS (SELECT user_id, count(*) AS n FROM orders GROUP BY user_id) \
                 SELECT * FROM s",
                &db
            ),
            None
        );
    }

    #[test]
    fn ambiguous_unqualified_column_errors() {
        let db = schema_db();
        let result = query_of("SELECT id FROM users JOIN orders ON users.id = orders.user_id")
            .projection_source_table(&db);
        assert!(matches!(result, Err(LookupError::AmbiguousTableLookup { .. })), "got {result:?}");
    }

    #[test]
    fn self_join_unqualified_column_is_ambiguous() {
        let db = schema_db();
        let result = query_of("SELECT name FROM users a JOIN users b ON a.id = b.id")
            .projection_source_table(&db);
        assert!(matches!(result, Err(LookupError::AmbiguousTableLookup { .. })), "got {result:?}");
    }

    #[test]
    fn self_join_qualified_wildcard_picks_one_alias() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT a.* FROM users a JOIN users b ON a.id = b.id", &db),
            Some("users".to_string())
        );
    }

    #[test]
    fn aliased_projection_columns_keep_single_table() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT o.id AS oid, o.total AS amount FROM orders o", &db),
            Some("orders".to_string()),
        );
    }

    #[test]
    fn qualified_column_with_unknown_qualifier_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT x.id FROM orders o", &db), None);
    }

    #[test]
    fn qualified_column_absent_from_table_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT o.nope FROM orders o", &db), None);
    }

    #[test]
    fn unknown_from_table_is_opaque_and_not_eligible() {
        let db = schema_db();
        assert_eq!(source_name("SELECT anything FROM does_not_exist", &db), None);
    }

    #[test]
    fn two_part_public_name_matches_schemaless_table() {
        let db = schema_db();
        // A schema-less table resides in `public`, so both spellings reach it.
        assert_eq!(source_name("SELECT id FROM public.users", &db), Some("users".to_string()));
        // Any other schema stays a miss, and the relation is treated as opaque.
        assert_eq!(source_name("SELECT id FROM app.users", &db), None);
    }

    #[test]
    fn group_by_all_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT id FROM users GROUP BY ALL", &db), None);
    }

    #[test]
    fn overqualified_from_name_errors() {
        let db = schema_db();
        let result = query_of("SELECT * FROM a.b.c").projection_source_table(&db);
        assert!(matches!(result, Err(LookupError::InvalidObjectName { .. })), "got {result:?}");
    }

    // Measured on PostgreSQL 18.4: `UNION ALL` keeps every input row, while a
    // deduplicating `UNION` does not, so only the `ALL` form carries rows
    // through for the row-identity question.
    #[test]
    fn union_without_all_is_not_row_preserving() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users UNION SELECT id FROM users) \
                 SELECT v.id FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users UNION ALL SELECT id FROM users) \
                 SELECT v.id FROM v",
                &db
            ),
            Some("users".to_string())
        );
    }

    #[test]
    fn outer_join_body_is_not_row_preserving() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH v AS (SELECT u.id FROM users u LEFT JOIN orders o ON o.user_id = u.id) \
                 SELECT v.id FROM v",
                &db
            ),
            None
        );
    }

    #[test]
    fn null_extended_side_refuses_row_identity() {
        let db = schema_db();
        assert_eq!(
            source_name("SELECT u.id FROM users u LEFT JOIN orders o ON o.user_id = u.id", &db),
            Some("users".to_string())
        );
        assert_eq!(
            source_name("SELECT o.total FROM users u LEFT JOIN orders o ON o.user_id = u.id", &db),
            None
        );
        // The null-extended side's name exposure still collides with the
        // other side's under a bare reference, matching PostgreSQL.
        assert!(matches!(
            query_of("SELECT id FROM users u LEFT JOIN orders o ON o.user_id = u.id")
                .projection_source_table(&db),
            Err(LookupError::AmbiguousTableLookup { .. })
        ));
    }

    #[test]
    fn multi_name_alias_projection_is_none() {
        let db = schema_db();
        assert_eq!(source_name("SELECT id AS (x, y) FROM users", &db), None);
    }

    // Measured on PostgreSQL 18.4: the merged `USING` column stands once in
    // the join output and its coalesced value belongs to neither table.
    #[test]
    fn using_merged_column_refuses_row_identity() {
        let db = schema_db();
        assert_eq!(source_name("SELECT id FROM users JOIN orders USING (id)", &db), None);
        assert_eq!(
            source_name("SELECT total FROM users JOIN orders USING (id)", &db),
            Some("orders".to_string())
        );
    }

    // Measured on PostgreSQL 18.4: a three-part reference resolves when the
    // leading part is the base table's own schema, and never for a CTE or a
    // schema mismatch.
    #[test]
    fn three_part_reference_resolves_for_matching_schema() {
        let db = schema_db();
        // A bare table lives in `public`, so `public.` names it.
        assert_eq!(
            source_name("SELECT public.users.id FROM users", &db),
            Some("users".to_string())
        );
        // An alias shadows the qualified name, matching PostgreSQL's
        // `invalid reference to FROM-clause entry`.
        assert_eq!(source_name("SELECT public.users.id FROM users AS u", &db), None);
        // Quoted schema identifiers stay case-sensitive.
        assert_eq!(source_name("SELECT \"Public\".users.id FROM users", &db), None);
        // The leading part must be the table's own schema.
        assert_eq!(source_name("SELECT other.users.id FROM users", &db), None);
        assert_eq!(source_name("SELECT public.orders.id FROM users", &db), None);
        // A column the table does not have answers nothing.
        assert_eq!(source_name("SELECT public.users.nope FROM users", &db), None);
        // An alias on the projection keeps the source.
        assert_eq!(
            source_name("SELECT public.users.id AS x FROM users", &db),
            Some("users".to_string())
        );
        // The database part is not modeled.
        assert_eq!(source_name("SELECT postgres.public.users.id FROM users", &db), None);
        // A null-extended side never answers the row-identity question.
        assert_eq!(
            source_name("SELECT public.orders.id FROM users LEFT JOIN orders ON true", &db),
            None
        );
    }

    #[test]
    fn unqualified_reference_under_outer_join_still_errors_when_ambiguous() {
        let db = schema_db();
        // The nullable side still counts toward ambiguity: PostgreSQL
        // rejects the bare name, the scope must not silently pick a side.
        let error = query_of("SELECT id FROM users LEFT JOIN orders ON true")
            .projection_source_table(&db)
            .expect_err("ambiguous");
        assert!(matches!(error, LookupError::AmbiguousTableLookup { .. }));
    }

    // A body that reads a relation whose rows are not source rows (a grouped
    // or deduplicated body) is itself not row preserving, one nesting level
    // deep or many.
    #[test]
    fn row_preserving_is_transitive_through_relations() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH g AS (SELECT user_id, count(*) AS n FROM orders \
                 GROUP BY user_id), v AS (SELECT g.user_id FROM g) \
                 SELECT v.user_id FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH d AS (SELECT DISTINCT id FROM users), \
                 v AS (SELECT d.id FROM d) SELECT v.id FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH f AS (SELECT id FROM users), v AS (SELECT f.id FROM f) \
                 SELECT v.id FROM v",
                &db
            ),
            Some("users".to_string())
        );
    }

    #[test]
    fn right_and_full_outer_join_nullability() {
        let db = schema_db();
        assert_eq!(source_name("SELECT users.id FROM users RIGHT JOIN orders ON true", &db), None);
        assert_eq!(
            source_name("SELECT orders.id FROM users RIGHT JOIN orders ON true", &db),
            Some("orders".to_string())
        );
        assert_eq!(source_name("SELECT users.id FROM users FULL JOIN orders ON true", &db), None);
        assert_eq!(source_name("SELECT orders.id FROM users FULL JOIN orders ON true", &db), None);
    }

    // A comma is a cross join: an outer join inside one `FROM` item
    // null-extends only that item's own relations.
    #[test]
    fn comma_from_item_keeps_identity_across_outer_join() {
        let db = ParserDB::parse::<GenericDialect>(
            "CREATE TABLE a(id INT); CREATE TABLE b(id INT); CREATE TABLE c(id INT);",
        )
        .expect("schema parses");
        assert_eq!(
            source_name("SELECT a.id FROM a, b RIGHT JOIN c ON true", &db),
            Some("a".to_string())
        );
        assert_eq!(source_name("SELECT b.id FROM a, b RIGHT JOIN c ON true", &db), None);
        assert_eq!(
            source_name("SELECT c.id FROM a, b RIGHT JOIN c ON true", &db),
            Some("c".to_string())
        );
    }

    #[test]
    fn having_and_qualify_poison_the_body() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users HAVING count(*) > 1) \
                 SELECT v.id FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users QUALIFY count(*) OVER () > 1) \
                 SELECT v.id FROM v",
                &db
            ),
            None
        );
    }

    #[test]
    fn intersect_and_except_poison_row_identity_without_all() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users INTERSECT SELECT id FROM users) \
                 SELECT v.id FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users INTERSECT ALL SELECT id FROM users) \
                 SELECT v.id FROM v",
                &db
            ),
            Some("users".to_string())
        );
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users EXCEPT SELECT id FROM orders) \
                 SELECT v.id FROM v",
                &db
            ),
            None
        );
    }

    // Sibling `WITH` items that each reference the previous one twice double
    // the width every level. The width cap turns the over-wide relation
    // opaque rather than letting materialization explode.
    // The lint's suggestion (`push_fmt`) is nightly-only, and CI gates on
    // stable.
    #[allow(clippy::format_push_string)]
    #[test]
    fn doubling_wildcard_chain_becomes_opaque_at_the_cap() {
        let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t(x INT, y INT);")
            .expect("schema parses");
        let mut sql = String::from("WITH a1 AS (SELECT * FROM t t1, t t2)");
        for level in 2..=30 {
            sql.push_str(&format!(
                ", a{level} AS (SELECT * FROM a{} x, a{} y)",
                level - 1,
                level - 1
            ));
        }
        sql.push_str(" SELECT z.x FROM a30 z");
        assert_eq!(source_name(&sql, &db), None);
    }

    // The second `USING` on the same name moves the subsumption boundary, so
    // the bare name is still one merged column (source-less), not ambiguous
    // against the third relation.
    #[test]
    fn second_using_on_the_same_name_stays_sourceless() {
        let db = ParserDB::parse::<GenericDialect>(
            "CREATE TABLE a(id INT); CREATE TABLE b(id INT); CREATE TABLE c(id INT);",
        )
        .expect("schema parses");
        assert_eq!(source_name("SELECT id FROM a JOIN b USING (id) JOIN c USING (id)", &db), None);
    }

    // A body-internal ambiguity degrades to a source-less output column: the
    // name is still exposed, and asking about it answers nothing rather than
    // failing the whole derivation.
    #[test]
    fn body_internal_ambiguity_degrades_to_sourceless_column() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH v AS (SELECT id FROM users JOIN orders \
                 ON users.id = orders.user_id) SELECT v.id FROM v",
                &db
            ),
            None
        );
    }

    #[test]
    fn qualified_wildcard_in_body_passes_through() {
        let db = schema_db();
        assert_eq!(
            source_name("WITH v AS (SELECT u.* FROM users u) SELECT v.name FROM v", &db),
            Some("users".to_string())
        );
    }

    // Set-operation arms pair by ordinal, naming the output from the left
    // arm: a right arm written in a different column order pairs `name`
    // against a foreign source and answers nothing.
    #[test]
    fn set_operation_projection_pairs_arms_by_ordinal() {
        let db = schema_db();
        assert_eq!(
            source_name(
                "WITH v AS (SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id \
                 UNION ALL SELECT o.total FROM users u JOIN orders o ON u.id = o.user_id) \
                 SELECT v.name FROM v",
                &db
            ),
            None
        );
        assert_eq!(
            source_name(
                "WITH v AS (SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id \
                 UNION ALL SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id) \
                 SELECT v.name FROM v",
                &db
            ),
            Some("users".to_string())
        );
    }

    // A base-table alias column list renames the exposed columns: the new
    // name answers, the replaced one stops answering, and the tail keeps
    // its own name. More aliases than columns is a mismatch PostgreSQL
    // rejects, so the relation stays opaque.
    #[test]
    fn base_alias_list_renames_under_projection_identity() {
        let db = schema_db();
        assert_eq!(source_name("SELECT u.n FROM users u(n)", &db), Some("users".to_string()));
        assert_eq!(source_name("SELECT u.id FROM users u(n)", &db), None);
        assert_eq!(source_name("SELECT u.name FROM users u(n)", &db), Some("users".to_string()));
        assert_eq!(source_name("SELECT id FROM users u(a,b,c)", &db), None);
    }

    // A qualified wildcard's prefix resolves by the column reference rules
    // (see the type docs): a one-part prefix matches aliases and keys, a
    // two-part prefix must match a base relation's own schema, and three
    // parts match nothing because the database name is not modeled.
    #[test]
    fn qualified_wildcard_prefix_follows_column_rules_under_identity() {
        let db = schema_db();
        assert_eq!(source_name("SELECT public.users.* FROM users", &db), Some("users".to_string()));
        assert_eq!(source_name("SELECT nosch.users.* FROM users", &db), None);
        assert_eq!(
            source_name("WITH cv AS (SELECT name FROM users) SELECT sch.cv.* FROM cv", &db),
            None
        );
        assert_eq!(source_name("SELECT probe.public.users.* FROM users", &db), None);
    }

    // A wildcard carrying `EXCLUDE`, `EXCEPT`, `ILIKE`, `RENAME`, `REPLACE`
    // or a trailing alias does not output the columns a bare `*` would, so a
    // body projecting one is not enumerated and a reference through it
    // answers nothing rather than claiming a column the relation dropped,
    // renamed or replaced.
    #[test]
    fn reshaped_wildcard_leaves_the_body_opaque() {
        let db = schema_db();
        for body in [
            "SELECT * EXCLUDE (name) FROM users",
            "SELECT * EXCEPT (name) FROM users",
            "SELECT * ILIKE 'id%' FROM users",
            "SELECT * RENAME (name AS handle) FROM users",
            "SELECT * REPLACE ('x' AS name) FROM users",
            "SELECT users.* EXCLUDE (name) FROM users",
        ] {
            let sql = format!("WITH v AS ({body}) SELECT v.name FROM v");
            assert_eq!(source_name(&sql, &db), None, "{body}");
        }
        // A plain wildcard body still resolves.
        assert_eq!(
            source_name("WITH v AS (SELECT * FROM users) SELECT v.name FROM v", &db),
            Some("users".to_string())
        );
    }

    // Every join spelling the resolver models routes through the same
    // nullability rules: the accumulated side keeps row identity unless the
    // operator null-extends it, and `APPLY` carries no join constraint at all.
    #[test]
    fn join_spellings_share_the_nullability_rules() {
        let db = schema_db();
        for (sql, expected) in [
            ("SELECT u.id FROM users u INNER JOIN orders o ON u.id = o.user_id", Some("users")),
            (
                "SELECT u.id FROM users u LEFT OUTER JOIN orders o ON u.id = o.user_id",
                Some("users"),
            ),
            ("SELECT u.id FROM users u RIGHT OUTER JOIN orders o ON u.id = o.user_id", None),
            ("SELECT u.id FROM users u CROSS JOIN orders o", Some("users")),
            ("SELECT u.id FROM users u CROSS APPLY orders o", Some("users")),
            ("SELECT u.id FROM users u OUTER APPLY orders o", Some("users")),
            ("SELECT u.id FROM users u SEMI JOIN orders o ON u.id = o.user_id", Some("users")),
            ("SELECT u.id FROM users u LEFT ANTI JOIN orders o ON u.id = o.user_id", Some("users")),
            ("SELECT u.id FROM users u LEFT SEMI JOIN orders o ON u.id = o.user_id", Some("users")),
            (
                "SELECT u.id FROM users u RIGHT SEMI JOIN orders o ON u.id = o.user_id",
                Some("users"),
            ),
            ("SELECT u.id FROM users u ANTI JOIN orders o ON u.id = o.user_id", Some("users")),
        ] {
            assert_eq!(source_name(sql, &db), expected.map(str::to_string), "{sql}");
        }
    }

    // A `NATURAL` join needs both sides' columns to know what it merges, so an
    // unresolvable side leaves the scope opaque instead of guessing that a
    // shared name is unmerged.
    #[test]
    fn natural_join_with_an_unenumerable_side_is_opaque() {
        let db = schema_db();
        assert_eq!(source_name("SELECT id FROM users NATURAL JOIN absent_table", &db), None);
    }

    // Bodies whose output columns follow rules this resolver does not model
    // stay opaque: a `VALUES` list names columns positionally, a nested join
    // factor is not a relation with a key, and `GROUP BY ALL` collapses rows.
    #[test]
    fn unmodeled_from_and_group_forms_stay_opaque() {
        let db = schema_db();
        assert_eq!(source_name("WITH v AS (VALUES (1)) SELECT * FROM v", &db), None);
        assert_eq!(
            source_name("SELECT * FROM (users JOIN orders ON users.id = orders.user_id)", &db),
            None
        );
        assert_eq!(
            source_name(
                "WITH v AS (SELECT name FROM users GROUP BY ALL) SELECT v.name FROM v",
                &db
            ),
            None
        );
        // A hierarchical query names output columns by rules this resolver
        // does not model.
        assert_eq!(
            source_name(
                "WITH v AS (SELECT name FROM users CONNECT BY id = 1) SELECT v.name FROM v",
                &db
            ),
            None
        );
    }

    // `REPLACE` substitutes a computed value for one of the expanded columns,
    // so the output row is no longer a row of the source table and carries no
    // row identity. Dropping columns (`EXCLUDE`, `EXCEPT`, `ILIKE`) or
    // relabelling them (`RENAME`) leaves the rows intact, so those still
    // answer.
    #[test]
    fn replacing_wildcard_carries_no_row_identity() {
        let db = schema_db();
        assert_eq!(source_name("SELECT * REPLACE ('x' AS name) FROM users", &db), None);
        assert_eq!(source_name("SELECT users.* REPLACE ('x' AS name) FROM users", &db), None);
        for sql in [
            "SELECT * EXCLUDE (name) FROM users",
            "SELECT * EXCEPT (name) FROM users",
            "SELECT * ILIKE 'id%' FROM users",
            "SELECT * RENAME (name AS handle) FROM users",
            "SELECT users.* EXCLUDE (name) FROM users",
        ] {
            assert_eq!(source_name(sql, &db), Some("users".to_string()), "{sql}");
        }
    }
}
