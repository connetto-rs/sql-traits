use alloc::{borrow::ToOwned, vec::Vec};
use core::{marker::PhantomData, slice};

use sqlparser::ast::{Expr, ObjectName, Select, SetOperator};

use super::{
    BaseColumnRef, DerivationProfile, FromScope, LookupOutcome, RelationKey, ResolvedColumn,
    resolve_definition_local, resolve_relation_local,
};
use crate::{
    errors::LookupError,
    structs::{
        ColumnBinding, ColumnDefinition, ColumnDefinitionRef, ColumnDefinitionScope,
        IdentifierCase, RelationBinding,
    },
    traits::{ColumnLike, DatabaseLike, TableLike},
};

/// A resolved column's definition and the names it matched.
pub(crate) type Bound<'scope, 'query, 'db, DB> =
    (ColumnDefinition<'scope, 'query, 'db, DB>, ColumnBinding<'scope>);

mod output_columns;

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
    Base { table: &'db DB::Table, column: &'db DB::Column },
    Expression { expression: AstRef<'query, 'db, Expr>, scope: ScopeCursor },
    SetOperation { operator: SetOperator, left: DefinitionId, right: DefinitionId },
    RecursiveUnion { anchor: DefinitionId, recursive: DefinitionId },
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

pub(crate) struct DefinitionGraph<'query, 'db, DB: DatabaseLike> {
    definitions: Vec<DefinitionNode<'query, 'db, DB>>,
    scopes: Vec<ScopeNode<'query, 'db, DB>>,
    select_index: Vec<SelectIndex>,
    queries: Vec<QueryNode<'query, 'db>>,
    case: IdentifierCase,
}

impl<'query, 'db, DB: DatabaseLike> DefinitionGraph<'query, 'db, DB> {
    pub(crate) fn definition<'scope>(
        &'scope self,
        id: DefinitionId,
    ) -> ColumnDefinition<'scope, 'query, 'db, DB> {
        match &self.definitions[id.0] {
            DefinitionNode::Base { table, column } => ColumnDefinition::Base { table, column },
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
            DefinitionNode::Opaque => ColumnDefinition::Opaque,
        }
    }

    fn resolve_definition_id<'scope>(
        &'scope self,
        mut cursor: ScopeCursor,
        reference: &Expr,
    ) -> Result<Option<ResolvedColumn<'scope, 'db, DB, DefinitionId>>, LookupError> {
        loop {
            match resolve_definition_local(
                &self.scopes[cursor.scope.0].data,
                cursor.visible_entries,
                DefinitionId(0),
                reference,
                false,
                self.scopes[cursor.scope.0].data.case,
            )? {
                LookupOutcome::Found(resolved) => return Ok(Some(resolved)),
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
        (
            self.definition(resolved.definition),
            ColumnBinding::new(resolved.relation, resolved.exposed),
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
    marker: PhantomData<(&'query (), &'db ())>,
}

impl<'query, 'db, DB: DatabaseLike> DefinitionDerivation<'query, 'db, DB> {
    pub(super) fn new(case: IdentifierCase) -> Self {
        Self {
            graph: DefinitionGraph {
                definitions: alloc::vec![DefinitionNode::Opaque],
                scopes: Vec::new(),
                select_index: Vec::new(),
                queries: Vec::new(),
                case,
            },
            marker: PhantomData,
        }
    }

    fn push_definition(&mut self, node: DefinitionNode<'query, 'db, DB>) -> DefinitionId {
        let id = DefinitionId(self.graph.definitions.len());
        self.graph.definitions.push(node);
        id
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
        let scope = ScopeId(self.graph.scopes.len());
        self.graph.scopes.push(ScopeNode {
            select: None,
            parent: None,
            owner: None,
            data: FromScope::new(self.graph.case),
            outputs: Vec::new(),
        });
        self.finish(scope)
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

    fn base_definition(
        &mut self,
        table: &'db DB::Table,
        column: &'db DB::Column,
    ) -> Self::Definition {
        self.push_definition(DefinitionNode::Base { table, column })
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
}

pub(crate) fn table_graph<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    database: &'db DB,
    case: IdentifierCase,
) -> (DefinitionGraph<'db, 'db, DB>, ScopeCursor) {
    let mut profile = DefinitionDerivation::new(case);
    let scope = ScopeId(0);
    profile.graph.scopes.push(ScopeNode {
        select: None,
        parent: None,
        owner: None,
        data: FromScope::new(case),
        outputs: Vec::new(),
    });
    let mut output_columns = Vec::new();
    if let Ok(columns) = table.columns(database) {
        for column in columns {
            let definition = profile.base_definition(table, column);
            output_columns.push(BaseColumnRef {
                name: column.column_name().to_owned(),
                quoted: column.column_name_is_quoted(),
                definition,
            });
        }
    }
    let schema_key = Some(match table.table_schema() {
        Some(schema) => {
            RelationKey { value: AstRef::Database(schema), quoted: table.table_schema_is_quoted() }
        }
        None => RelationKey { value: AstRef::Database("public"), quoted: false },
    });
    profile.graph.scopes[scope.0].data.bases.push(super::FromTableRef {
        key: RelationKey {
            value: AstRef::Database(table.table_name()),
            quoted: table.table_name_is_quoted(),
        },
        schema_key,
        table,
        has_alias: false,
        nullable: false,
        entry_index: 0,
        output_columns,
    });
    profile.graph.scopes[scope.0].data.from_entry_count = 1;
    profile.finish(scope)
}
