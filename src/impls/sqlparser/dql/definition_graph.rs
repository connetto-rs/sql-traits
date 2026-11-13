use alloc::{borrow::ToOwned, vec::Vec};
use core::{marker::PhantomData, slice};

use sqlparser::ast::{
    Expr, Ident, ObjectName, Select, SetOperator, Statement, TableFactor, TableWithJoins,
};

use super::{
    BaseColumnRef, ColumnOrigin, CteShape, DerivationProfile, DerivedShape, Deriving, FromScope,
    LookupOutcome, OutputNameSource, RelationKey, ResolvedColumn, WildcardEntry,
    derive_query_shape, factor_address,
    modifying::{self, ModifyingScopes},
    resolve_definition_local, resolve_relation_local,
};
use crate::{
    errors::LookupError,
    structs::{
        ColumnBinding, ColumnDefinition, ColumnDefinitionRef, ColumnDefinitionScope,
        IdentifierCase, RangeTableEntry, RelationBinding,
    },
    traits::{ColumnLike, CreatedFromQuery, DatabaseLike, TableLike},
};

/// A resolved column's definition and the names it matched.
pub(crate) type Bound<'scope, 'query, 'db, DB> =
    (ColumnDefinition<'scope, 'query, 'db, DB>, ColumnBinding<'scope>);

/// A resolved column with the scope it binds in.
type FoundIn<'scope, 'db, DB> = (ResolvedColumn<'scope, 'db, DB, DefinitionId>, ScopeId);

mod output_columns;
pub(crate) mod reference_sites;

pub(crate) use output_columns::QueryId;
use output_columns::{OutputEntry, QueryNode};

pub(super) enum AstRef<'query, 'db, T: ?Sized> {
    Query(&'query T),
    Database(&'db T),
}

impl<T: ?Sized> Copy for AstRef<'_, '_, T> {}

impl<T: ?Sized> Clone for AstRef<'_, '_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'query, 'db, T: ?Sized> AstRef<'query, 'db, T> {
    #[expect(clippy::match_same_arms, reason = "the variants carry independent lifetimes")]
    pub(super) fn get(&self) -> &T {
        match self {
            Self::Query(value) => value,
            Self::Database(value) => value,
        }
    }

    pub(super) fn map<U: ?Sized>(
        self,
        map: impl for<'source> FnOnce(&'source T) -> &'source U,
    ) -> AstRef<'query, 'db, U> {
        match self {
            Self::Query(value) => AstRef::Query(map(value)),
            Self::Database(value) => AstRef::Database(map(value)),
        }
    }

    pub(super) fn try_map<U: ?Sized>(
        self,
        map: impl for<'source> FnOnce(&'source T) -> Option<&'source U>,
    ) -> Option<AstRef<'query, 'db, U>> {
        match self {
            Self::Query(value) => map(value).map(AstRef::Query),
            Self::Database(value) => map(value).map(AstRef::Database),
        }
    }
}

pub(super) enum AstRefIter<'query, 'db, T> {
    Query(slice::Iter<'query, T>),
    Database(slice::Iter<'db, T>),
}

impl<'query, 'db, T> Iterator for AstRefIter<'query, 'db, T> {
    type Item = AstRef<'query, 'db, T>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Query(iter) => iter.next().map(AstRef::Query),
            Self::Database(iter) => iter.next().map(AstRef::Database),
        }
    }
}

impl<'query, 'db, T> AstRef<'query, 'db, [T]> {
    pub(super) fn iter(self) -> AstRefIter<'query, 'db, T> {
        match self {
            Self::Query(values) => AstRefIter::Query(values.iter()),
            Self::Database(values) => AstRefIter::Database(values.iter()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DefinitionId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScopeId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScopeCursor {
    pub(super) scope: ScopeId,
    pub(super) visible_entries: usize,
}

#[derive(Clone, Copy)]
pub(super) struct GraphCheckpoint {
    scopes: usize,
}

enum DefinitionNode<'query, 'db, DB: DatabaseLike> {
    Base {
        table: &'db DB::Table,
        column: &'db DB::Column,
        database: &'db DB,
    },
    Expression {
        expression: AstRef<'query, 'db, Expr>,
        scope: ScopeCursor,
    },
    SetOperation {
        operator: SetOperator,
        left: DefinitionId,
        right: DefinitionId,
    },
    /// Input parameter `ordinal` of the function whose body is resolved.
    Parameter {
        function: &'db DB::Function,
        ordinal: usize,
    },
    RecursiveUnion {
        anchor: DefinitionId,
        recursive: DefinitionId,
    },
    Opaque,
}

struct ScopeNode<'query, 'db, DB: DatabaseLike> {
    select: Option<AstRef<'query, 'db, Select>>,
    parent: Option<ScopeCursor>,
    owner: Option<ScopeId>,
    data: FromScope<'query, 'db, DB, DefinitionId>,
    outputs: Vec<OutputEntry<'query, 'db>>,
}

struct SelectIndex {
    address: usize,
    scope: ScopeId,
}

fn select_address(select: &Select) -> usize {
    core::ptr::from_ref(select).addr()
}

/// Calls `found` with every `FROM` item of `tables`, those inside
/// parenthesized joins included.
fn each_from_item<'a>(tables: &'a [TableWithJoins], found: &mut impl FnMut(&'a TableFactor)) {
    for table in tables {
        let factors =
            core::iter::once(&table.relation).chain(table.joins.iter().map(|join| &join.relation));
        for factor in factors {
            found(factor);
            if let TableFactor::NestedJoin { table_with_joins, .. } = factor {
                each_from_item(slice::from_ref(table_with_joins.as_ref()), found);
            }
        }
    }
}

pub(crate) struct DefinitionGraph<'query, 'db, DB: DatabaseLike> {
    definitions: Vec<DefinitionNode<'query, 'db, DB>>,
    scopes: Vec<ScopeNode<'query, 'db, DB>>,
    select_index: Vec<SelectIndex>,
    /// Every `FROM` item of every scope, which a binding's origin names by
    /// address.
    from_items: Vec<AstRef<'query, 'db, TableFactor>>,
    queries: Vec<QueryNode<'query, 'db>>,
    /// The scopes the clauses of every data-modifying statement resolve in,
    /// by the statement's address.
    modifying: Vec<(usize, ModifyingScopes)>,
    case: IdentifierCase,
}

impl<'query, 'db, DB: DatabaseLike> DefinitionGraph<'query, 'db, DB> {
    /// The scopes the clauses of the data-modifying statement `statement`
    /// resolve in, `None` for a statement the graph was not built over.
    pub(crate) fn modifying_scopes(&self, statement: &Statement) -> Option<ModifyingScopes> {
        let address = core::ptr::from_ref(statement).addr();
        self.modifying.iter().find(|(held, _)| *held == address).map(|(_, scopes)| *scopes)
    }

    pub(crate) fn definition<'scope>(
        &'scope self,
        id: DefinitionId,
    ) -> ColumnDefinition<'scope, 'query, 'db, DB> {
        match &self.definitions[id.0] {
            DefinitionNode::Base { table, column, database } => {
                ColumnDefinition::Base { table, column, database }
            }
            DefinitionNode::Expression { expression, scope } => {
                ColumnDefinition::Expression {
                    expression: expression.get(),
                    scope: ColumnDefinitionScope::new(self, *scope),
                }
            }
            DefinitionNode::SetOperation { operator, left, right } => {
                ColumnDefinition::SetOperation {
                    operator: *operator,
                    left: ColumnDefinitionRef::new(self, *left),
                    right: ColumnDefinitionRef::new(self, *right),
                }
            }
            DefinitionNode::RecursiveUnion { anchor, recursive } => {
                ColumnDefinition::RecursiveUnion {
                    anchor: ColumnDefinitionRef::new(self, *anchor),
                    recursive: ColumnDefinitionRef::new(self, *recursive),
                }
            }
            DefinitionNode::Parameter { function, ordinal } => {
                ColumnDefinition::Parameter { function, ordinal: *ordinal }
            }
            DefinitionNode::Opaque => ColumnDefinition::Opaque,
        }
    }

    fn resolve_definition_id<'scope>(
        &'scope self,
        cursor: ScopeCursor,
        reference: &Expr,
    ) -> Result<Option<ResolvedColumn<'scope, 'db, DB, DefinitionId>>, LookupError> {
        Ok(self.resolve_definition_in(cursor, reference)?.map(|(resolved, _)| resolved))
    }

    /// Resolves `reference` from the scope `cursor` names outwards, with the
    /// scope it binds in.
    fn resolve_definition_in<'scope>(
        &'scope self,
        mut cursor: ScopeCursor,
        reference: &Expr,
    ) -> Result<Option<FoundIn<'scope, 'db, DB>>, LookupError> {
        loop {
            match resolve_definition_local(
                &self.scopes[cursor.scope.0].data,
                cursor.visible_entries,
                DefinitionId(0),
                reference,
                false,
                self.scopes[cursor.scope.0].data.case,
            )? {
                LookupOutcome::Found(resolved) => return Ok(Some((resolved, cursor.scope))),
                LookupOutcome::Stop => return Ok(None),
                LookupOutcome::SearchParent => {
                    let Some(parent) = self.scopes[cursor.scope.0].parent else {
                        return Ok(None);
                    };
                    cursor = parent;
                }
            }
        }
    }

    /// Whether `reference`, sitting in the scope `cursor` names, binds in the
    /// query `levels_up` query levels out from that scope, [`None`] when it
    /// binds nothing.
    pub(crate) fn binds_levels_up(
        &self,
        cursor: ScopeCursor,
        reference: &Expr,
        levels_up: usize,
    ) -> Result<Option<bool>, LookupError> {
        let level = (0..levels_up).try_fold(cursor.scope, |scope, _| self.scopes[scope.0].owner);
        Ok(self
            .resolve_definition_in(cursor, reference)?
            .zip(level)
            .map(|((_, found), level)| found == level))
    }

    pub(crate) fn resolve_definition<'scope>(
        &'scope self,
        cursor: ScopeCursor,
        reference: &Expr,
    ) -> Result<Option<ColumnDefinition<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(self
            .resolve_definition_id(cursor, reference)?
            .map(|resolved| self.definition(resolved.definition)))
    }

    fn bound<'scope>(
        &'scope self,
        resolved: &ResolvedColumn<'scope, 'db, DB, DefinitionId>,
    ) -> Bound<'scope, 'query, 'db, DB> {
        let entry = match resolved.origin {
            ColumnOrigin::Entry { factor, ordinal } => {
                self.from_items
                    .iter()
                    .find(|item| factor_address(item.get()) == factor)
                    .map(|item| RangeTableEntry::new(item.get(), ordinal))
            }
            ColumnOrigin::Merged | ColumnOrigin::Output | ColumnOrigin::Unknown => None,
        };
        (
            self.definition(resolved.definition),
            ColumnBinding::new(resolved.relation, resolved.exposed, resolved.origin, entry),
        )
    }

    pub(crate) fn resolve_binding<'scope>(
        &'scope self,
        cursor: ScopeCursor,
        reference: &Expr,
    ) -> Result<Option<Bound<'scope, 'query, 'db, DB>>, LookupError> {
        Ok(self.resolve_definition_id(cursor, reference)?.map(|resolved| self.bound(&resolved)))
    }

    pub(crate) fn resolve_relation<'scope>(
        &'scope self,
        mut cursor: ScopeCursor,
        qualifier: &ObjectName,
    ) -> Option<RelationBinding<'scope>> {
        loop {
            let scope = &self.scopes[cursor.scope.0];
            match resolve_relation_local(&scope.data, cursor.visible_entries, qualifier) {
                LookupOutcome::Found(relation) => return Some(RelationBinding::new(relation)),
                LookupOutcome::Stop => return None,
                LookupOutcome::SearchParent => cursor = scope.parent?,
            }
        }
    }

    pub(crate) fn resolve_source(
        &self,
        cursor: ScopeCursor,
        reference: &Expr,
    ) -> Result<Option<&'db DB::Table>, LookupError> {
        match resolve_definition_local(
            &self.scopes[cursor.scope.0].data,
            cursor.visible_entries,
            DefinitionId(0),
            reference,
            false,
            self.scopes[cursor.scope.0].data.case,
        )? {
            LookupOutcome::Found(column) => Ok(column.source),
            LookupOutcome::SearchParent | LookupOutcome::Stop => Ok(None),
        }
    }

    pub(crate) fn scope_for_select<'scope>(
        &'scope self,
        origin: Option<ScopeCursor>,
        select: &Select,
    ) -> Option<ColumnDefinitionScope<'scope, 'query, 'db, DB>> {
        let address = select_address(select);
        let start = self.select_index.partition_point(|entry| entry.address < address);
        let end = self.select_index.partition_point(|entry| entry.address <= address);
        self.select_index[start..end]
            .iter()
            .find(|entry| {
                self.scopes[entry.scope.0]
                    .select
                    .is_some_and(|recorded| core::ptr::eq(recorded.get(), select))
                    && origin.is_none_or(|origin| self.descends_from(entry.scope, origin.scope))
            })
            .map(|entry| {
                let scope = &self.scopes[entry.scope.0];
                ColumnDefinitionScope::new(
                    self,
                    ScopeCursor {
                        scope: entry.scope,
                        visible_entries: scope.data.from_entry_count,
                    },
                )
            })
    }

    fn descends_from(&self, mut candidate: ScopeId, ancestor: ScopeId) -> bool {
        loop {
            let Some(owner) = self.scopes[candidate.0].owner else {
                return false;
            };
            if owner == ancestor {
                return true;
            }
            candidate = owner;
        }
    }
}

pub(super) struct DefinitionDerivation<'query, 'db, DB: DatabaseLike> {
    graph: DefinitionGraph<'query, 'db, DB>,
    /// The output definitions of every `CREATE TABLE … AS` query derived so
    /// far, by the query's address, so a table read twice derives it once.
    created: Vec<(usize, Vec<DefinitionId>)>,
    marker: PhantomData<(&'query (), &'db ())>,
}

impl<'query, 'db, DB: DatabaseLike> DefinitionDerivation<'query, 'db, DB> {
    pub(super) fn new(case: IdentifierCase) -> Self {
        Self {
            graph: DefinitionGraph {
                definitions: alloc::vec![DefinitionNode::Opaque],
                scopes: Vec::new(),
                select_index: Vec::new(),
                from_items: Vec::new(),
                queries: Vec::new(),
                modifying: Vec::new(),
                case,
            },
            created: Vec::new(),
            marker: PhantomData,
        }
    }

    fn push_definition(&mut self, node: DefinitionNode<'query, 'db, DB>) -> DefinitionId {
        let id = DefinitionId(self.graph.definitions.len());
        self.graph.definitions.push(node);
        id
    }

    /// The definition of the output a column was created from, derived
    /// against the catalog its query read, or opaque when it cannot be.
    fn created_definition(&mut self, created: &CreatedFromQuery<'db, DB>) -> DefinitionId {
        let address = core::ptr::from_ref(created.query()).addr();
        if !self.created.iter().any(|(derived, _)| *derived == address) {
            let deriving = Deriving::of(created.catalog(), IdentifierCase::AsWritten);
            let parent = self.no_parent();
            // Reading the same query against the same catalog succeeded when
            // the table was created, so a lookup error cannot arise here.
            let definitions = derive_query_shape(
                AstRef::Database(created.query()),
                &[],
                Some(OutputNameSource::Declared(created.output_names())),
                deriving,
                parent,
                self,
            )
            .ok()
            .flatten()
            .map(|shape| shape.columns.iter().map(|column| column.definition).collect())
            .unwrap_or_default();
            self.created.push((address, definitions));
        }
        self.created
            .iter()
            .find(|(derived, _)| *derived == address)
            .and_then(|(_, definitions)| definitions.get(created.ordinal()).copied())
            .unwrap_or(DefinitionId(0))
    }

    pub(super) fn finish(
        mut self,
        scope: ScopeId,
    ) -> (DefinitionGraph<'query, 'db, DB>, ScopeCursor) {
        self.graph.select_index.sort_unstable_by_key(|entry| entry.address);
        let visible_entries = self.graph.scopes[scope.0].data.from_entry_count;
        (self.graph, ScopeCursor { scope, visible_entries })
    }

    pub(super) fn empty_scope(mut self) -> (DefinitionGraph<'query, 'db, DB>, ScopeCursor) {
        let scope = self.begin_unowned_scope(None);
        self.finish(scope)
    }

    /// Opens a scope no `SELECT` owns, under `parent`: the names an
    /// expression outside a query reaches, or a data-modifying statement's
    /// relations.
    pub(super) fn begin_unowned_scope(&mut self, parent: Option<ScopeCursor>) -> ScopeId {
        let scope = ScopeId(self.graph.scopes.len());
        self.graph.scopes.push(ScopeNode {
            select: None,
            parent,
            owner: parent.map(|parent| parent.scope),
            data: FromScope::new(self.graph.case),
            outputs: Vec::new(),
        });
        scope
    }

    /// Records the `FROM` items of a data-modifying statement, which a
    /// binding's origin names.
    pub(super) fn record_from_items(&mut self, tables: &'query [TableWithJoins]) {
        each_from_item(tables, &mut |item| self.graph.from_items.push(AstRef::Query(item)));
    }

    /// Records a `MERGE` target or source, which a binding's origin names. A
    /// parenthesized join source stands as one opaque relation, as in a
    /// `FROM` clause, so no origin names an item inside it.
    pub(super) fn record_factor(&mut self, factor: &'query TableFactor) {
        self.graph.from_items.push(AstRef::Query(factor));
    }

    /// Records the scopes the clauses of `statement` resolve in.
    pub(super) fn record_modifying(&mut self, statement: &Statement, scopes: ModifyingScopes) {
        self.graph.modifying.push((core::ptr::from_ref(statement).addr(), scopes));
    }

    /// Whether a relation named `name`, unquoted, is in reach from `cursor`.
    pub(super) fn reaches_relation(&self, cursor: Option<ScopeCursor>, name: &str) -> bool {
        let qualifier = ObjectName::from(alloc::vec![Ident::new(name)]);
        cursor.is_some_and(|cursor| self.graph.resolve_relation(cursor, &qualifier).is_some())
    }

    /// The definition of input parameter `ordinal` of `function`.
    pub(super) fn parameter_definition(
        &mut self,
        function: &'db DB::Function,
        ordinal: usize,
    ) -> DefinitionId {
        self.push_definition(DefinitionNode::Parameter { function, ordinal })
    }

    /// Appends `table` to `scope` as its next `FROM` entry, exposed under
    /// the alias `alias` names with its quoting when one is given, and
    /// otherwise under its own schema and name.
    pub(super) fn push_table_entry(
        &mut self,
        scope: ScopeId,
        table: &'db DB::Table,
        database: &'db DB,
        alias: Option<(&'query str, bool)>,
    ) {
        let output_columns: Vec<_> = table
            .columns(database)
            .into_iter()
            .flatten()
            .map(|column| {
                BaseColumnRef {
                    name: column.column_name().to_owned(),
                    quoted: column.column_name_is_quoted(),
                    definition: self.column_definition(table, column, database),
                }
            })
            .collect();
        let (key, schema_key) = if let Some((alias, quoted)) = alias {
            (RelationKey { value: AstRef::Query(alias), quoted }, None)
        } else {
            let schema = table.table_schema().map_or(
                RelationKey { value: AstRef::Database("public"), quoted: false },
                |schema| {
                    RelationKey {
                        value: AstRef::Database(schema),
                        quoted: table.table_schema_is_quoted(),
                    }
                },
            );
            let name = RelationKey {
                value: AstRef::Database(table.table_name()),
                quoted: table.table_name_is_quoted(),
            };
            (name, Some(schema))
        };
        let data = &mut self.graph.scopes[scope.0].data;
        data.wildcard_plans.push(alloc::vec![WildcardEntry::Base(data.bases.len())]);
        data.bases.push(super::FromTableRef {
            key,
            schema_key,
            table,
            has_alias: alias.is_some(),
            nullable: false,
            reads_inheritors: false,
            entry_index: data.from_entry_count,
            output_columns,
            factor: 0,
        });
        data.from_entry_count += 1;
    }
}

impl<'query, 'db, DB: DatabaseLike> DerivationProfile<'query, 'db, DB>
    for DefinitionDerivation<'query, 'db, DB>
{
    type Definition = DefinitionId;
    type Scope = ScopeId;
    type Cursor = Option<ScopeCursor>;
    type Checkpoint = GraphCheckpoint;

    const INDEX_NESTED_QUERIES: bool = true;

    fn no_parent(&self) -> Self::Cursor {
        None
    }

    fn begin_scope(
        &mut self,
        select: AstRef<'query, 'db, Select>,
        parent: Self::Cursor,
    ) -> Self::Scope {
        let scope = ScopeId(self.graph.scopes.len());
        let owner = parent.map(|parent| parent.scope);
        self.graph.scopes.push(ScopeNode {
            select: Some(select),
            parent,
            owner,
            data: FromScope::new(match select {
                AstRef::Query(_) => self.graph.case,
                AstRef::Database(_) => IdentifierCase::AsWritten,
            }),
            outputs: Vec::new(),
        });
        self.graph.select_index.push(SelectIndex { address: select_address(select.get()), scope });
        match select {
            AstRef::Query(select) => {
                each_from_item(&select.from, &mut |item| {
                    self.graph.from_items.push(AstRef::Query(item));
                });
            }
            AstRef::Database(select) => {
                each_from_item(&select.from, &mut |item| {
                    self.graph.from_items.push(AstRef::Database(item));
                });
            }
        }
        scope
    }

    fn scope<'scope>(
        &'scope self,
        scope: &'scope Self::Scope,
    ) -> &'scope FromScope<'query, 'db, DB, Self::Definition> {
        &self.graph.scopes[scope.0].data
    }

    fn scope_mut<'scope>(
        &'scope mut self,
        scope: &'scope mut Self::Scope,
    ) -> &'scope mut FromScope<'query, 'db, DB, Self::Definition> {
        &mut self.graph.scopes[scope.0].data
    }

    fn cursor(&self, scope: &Self::Scope) -> Self::Cursor {
        Some(ScopeCursor {
            scope: *scope,
            visible_entries: self.graph.scopes[scope.0].data.from_entry_count,
        })
    }

    fn cursor_for_select(&self, select: AstRef<'query, 'db, Select>) -> Option<Self::Cursor> {
        let address = select_address(select.get());
        self.graph.select_index.iter().rev().find(|entry| entry.address == address).map(|entry| {
            Some(ScopeCursor {
                scope: entry.scope,
                visible_entries: self.graph.scopes[entry.scope.0].data.from_entry_count,
            })
        })
    }

    fn set_scope_owners_since(
        &mut self,
        checkpoint: Self::Checkpoint,
        inherited: Self::Cursor,
        owner: Self::Cursor,
    ) {
        let inherited = inherited.map(|cursor| cursor.scope);
        let owner = owner.map(|cursor| cursor.scope);
        for scope in &mut self.graph.scopes[checkpoint.scopes..] {
            if scope.owner == inherited {
                scope.owner = owner;
            }
        }
    }

    fn opaque_definition(&self) -> Self::Definition {
        DefinitionId(0)
    }

    fn column_definition(
        &mut self,
        table: &'db DB::Table,
        column: &'db DB::Column,
        database: &'db DB,
    ) -> Self::Definition {
        match column.created_from_query(database) {
            Some(created) => self.created_definition(&created),
            None => self.push_definition(DefinitionNode::Base { table, column, database }),
        }
    }

    fn expression_definition(
        &mut self,
        expression: AstRef<'query, 'db, Expr>,
        scope: Self::Cursor,
    ) -> Result<Self::Definition, LookupError> {
        let Some(scope) = scope else {
            return Ok(DefinitionId(0));
        };
        match self.graph.resolve_definition_id(scope, expression.get()) {
            Ok(Some(resolved)) => Ok(resolved.definition),
            Ok(None) => Ok(self.push_definition(DefinitionNode::Expression { expression, scope })),
            Err(LookupError::AmbiguousTableLookup { .. }) => Ok(DefinitionId(0)),
            Err(error) => Err(error),
        }
    }

    fn set_definition(
        &mut self,
        operator: SetOperator,
        left: Self::Definition,
        right: Self::Definition,
    ) -> Self::Definition {
        self.push_definition(DefinitionNode::SetOperation { operator, left, right })
    }

    fn recursive_definition(
        &mut self,
        anchor: Self::Definition,
        recursive: Self::Definition,
    ) -> Self::Definition {
        self.push_definition(DefinitionNode::RecursiveUnion { anchor, recursive })
    }

    fn checkpoint(&self) -> Self::Checkpoint {
        GraphCheckpoint { scopes: self.graph.scopes.len() }
    }

    fn modifying_shape(
        &mut self,
        statement: &'query Statement,
        cte_scope: &[CteShape<'query, 'db, DB, Self::Definition>],
        deriving: Deriving<'_, 'db, DB>,
        parent: Self::Cursor,
    ) -> Result<Option<DerivedShape<'query, 'db, DB, Self::Definition>>, LookupError> {
        modifying::derive(self, statement, cte_scope, deriving, parent)
    }
}

pub(crate) fn table_graph<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    database: &'db DB,
    case: IdentifierCase,
) -> (DefinitionGraph<'db, 'db, DB>, ScopeCursor) {
    let mut profile = DefinitionDerivation::new(case);
    let scope = profile.begin_unowned_scope(None);
    profile.push_table_entry(scope, table, database, None);
    profile.finish(scope)
}
