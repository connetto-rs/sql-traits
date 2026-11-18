//! The scopes the clauses of a data-modifying statement resolve in, wherever
//! the statement stands: by itself, as the body of a `WITH`-prefixed query,
//! or as a data-modifying CTE, whose output is its `RETURNING` list.

use alloc::vec::Vec;
use core::slice;

use sqlparser::ast::{
    Delete, DoUpdate, FromTable, Insert, Merge, MergeAction, MergeClause, MergeClauseKind,
    MergeInsertExpr, MergeInsertKind, MergeUpdateExpr, MergeUpdateKind, OnConflict,
    OnConflictAction, OnInsert, OutputClause, Query, SelectItem, Statement, TableFactor,
    TableObject, TableWithJoins, Update, UpdateTableFromKind,
};

use super::{
    AstRef, CteShape, DerivationProfile, DerivedShape, Deriving, NestedQueryWalker,
    append_select_item, collect_from_items, collect_lone_factor,
    definition_graph::{DefinitionDerivation, DefinitionId, ScopeCursor, ScopeId},
    index_nested_query_scopes,
};
use crate::{errors::LookupError, traits::DatabaseLike};

/// The scopes the clauses of one data-modifying statement resolve in.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ModifyingScopes {
    /// What `SET`, `WHERE`, `ON` and `WHEN MATCHED` read: the target and the
    /// relations `FROM`, `USING` or a `MERGE` source add.
    pub(crate) relations: Option<ScopeCursor>,
    /// What `RETURNING` reads: the target and those relations, with `old`
    /// and `new` as rows of the target that only a qualified reference
    /// reaches.
    pub(crate) returning: Option<ScopeCursor>,
    /// The target and `excluded` row an `INSERT ... ON CONFLICT DO UPDATE`
    /// reads.
    pub(crate) conflict: Option<ScopeCursor>,
    /// The `MERGE` source alone, which `WHEN NOT MATCHED [BY TARGET]` reads.
    pub(crate) source: Option<ScopeCursor>,
    /// The `MERGE` target alone, which `WHEN NOT MATCHED BY SOURCE` reads.
    pub(crate) target: Option<ScopeCursor>,
}

/// The CTEs in reach of a statement.
type Ctes<'c, 'query, 'db, DB> = &'c [CteShape<'query, 'db, DB, DefinitionId>];

/// The columns a `RETURNING` list outputs, `None` when it outputs none or
/// some cannot be named.
type Returned<'query, 'db, DB> = Option<DerivedShape<'query, 'db, DB, DefinitionId>>;

/// The walker indexing the queries nested in a clause.
type Nested<'f, 'query> =
    NestedQueryWalker<&'f mut dyn FnMut(&'query Query) -> Result<(), LookupError>>;

/// Builds and records the scopes of `statement`'s clauses under `outer`,
/// with `ctes` in reach, and answers the columns its `RETURNING` list
/// outputs. A statement that modifies no data builds nothing.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving the statement runs into.
pub(super) fn derive<'query, 'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'query, 'db, DB>,
    statement: &'query Statement,
    ctes: Ctes<'_, 'query, 'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    outer: Option<ScopeCursor>,
) -> Result<Returned<'query, 'db, DB>, LookupError> {
    let mut builder = Builder { profile, ctes, deriving, outer };
    let (scopes, returned) = match statement {
        Statement::Update(update) => builder.update(update)?,
        Statement::Delete(delete) => builder.delete(delete)?,
        Statement::Insert(insert) => builder.insert(insert)?,
        Statement::Merge(merge) => builder.merge(merge)?,
        _ => return Ok(None),
    };
    builder.profile.record_modifying(statement, scopes);
    Ok(returned)
}

/// Indexes the queries `walk` finds as nested under `parent`, with `ctes` in
/// reach.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving a nested query runs
/// into.
pub(super) fn index_nested<'query, 'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'query, 'db, DB>,
    ctes: Ctes<'_, 'query, 'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    parent: Option<ScopeCursor>,
    walk: impl FnOnce(&mut Nested<'_, 'query>) -> Result<(), LookupError>,
) -> Result<(), LookupError> {
    let mut index = |query: &'query Query| {
        index_nested_query_scopes(AstRef::Query(query), ctes, deriving, parent, profile).map(|_| ())
    };
    let mut walker = NestedQueryWalker {
        query: &mut index as &mut dyn FnMut(&'query Query) -> Result<(), LookupError>,
    };
    walk(&mut walker)
}

/// What every clause of one statement is built against.
struct Builder<'p, 'c, 'd, 'query, 'db, DB: DatabaseLike> {
    profile: &'p mut DefinitionDerivation<'query, 'db, DB>,
    ctes: Ctes<'c, 'query, 'db, DB>,
    deriving: Deriving<'d, 'db, DB>,
    /// The scope the statement stands in.
    outer: Option<ScopeCursor>,
}

impl<'query, 'db, DB: DatabaseLike> Builder<'_, '_, '_, 'query, 'db, DB> {
    fn update(
        &mut self,
        update: &'query Update,
    ) -> Result<(ModifyingScopes, Returned<'query, 'db, DB>), LookupError> {
        let Update { table, from, assignments, selection, returning, .. } = update;
        let from: &'query [TableWithJoins] = match from {
            Some(UpdateTableFromKind::BeforeSet(from) | UpdateTableFromKind::AfterSet(from)) => {
                from
            }
            None => &[],
        };
        let target = self.target(&table.relation)?;
        let scope = self.relations(&[slice::from_ref(table), from])?;
        let cursor = self.profile.cursor(&scope);
        self.nested(cursor, |walker| {
            for assignment in assignments {
                walker.expression(&assignment.value)?;
            }
            walker.optional_expression(selection.as_ref())
        })?;
        let rows = self.returning_rows(target, scope);
        let returned = self.returning(scope, rows, returning.as_deref())?;
        let scopes =
            ModifyingScopes { relations: cursor, returning: rows, ..ModifyingScopes::default() };
        Ok((scopes, returned))
    }

    fn delete(
        &mut self,
        delete: &'query Delete,
    ) -> Result<(ModifyingScopes, Returned<'query, 'db, DB>), LookupError> {
        let Delete { from, using, selection, returning, .. } = delete;
        let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = from;
        // The first table a `DELETE` names is its target.
        let target = from.first().map(|table| self.target(&table.relation)).transpose()?.flatten();
        let scope = self.relations(&[from, using.as_deref().unwrap_or_default()])?;
        let cursor = self.profile.cursor(&scope);
        self.nested(cursor, |walker| walker.optional_expression(selection.as_ref()))?;
        let rows = self.returning_rows(target, scope);
        let returned = self.returning(scope, rows, returning.as_deref())?;
        let scopes =
            ModifyingScopes { relations: cursor, returning: rows, ..ModifyingScopes::default() };
        Ok((scopes, returned))
    }

    /// An `INSERT`: its source query reads the scope it stands in,
    /// `RETURNING` its target, and `ON CONFLICT DO UPDATE` its target with
    /// the `excluded` row.
    fn insert(
        &mut self,
        insert: &'query Insert,
    ) -> Result<(ModifyingScopes, Returned<'query, 'db, DB>), LookupError> {
        let Insert { table, table_alias, source, on, returning, .. } = insert;
        if let Some(source) = source {
            index_nested_query_scopes(
                AstRef::Query(source),
                self.ctes,
                self.deriving,
                self.outer,
                self.profile,
            )?;
        }
        let target = match table {
            TableObject::TableName(name) => self.deriving.table(name)?,
            TableObject::TableFunction(_) | TableObject::TableQuery(_) => None,
        };
        let alias = table_alias
            .as_ref()
            .map(|alias| (alias.alias.value.as_str(), alias.alias.quote_style.is_some()));
        let returned_scope = self.profile.begin_unowned_scope(self.outer);
        let database = self.deriving.database;
        if let Some(target) = target {
            self.profile.push_table_entry(returned_scope, target, database, alias);
        }
        let conflict = match on {
            Some(OnInsert::OnConflict(OnConflict {
                action: OnConflictAction::DoUpdate(DoUpdate { assignments, selection }),
                ..
            })) => {
                let scope = self.profile.begin_unowned_scope(self.outer);
                if let Some(target) = target {
                    self.profile.push_table_entry(scope, target, database, alias);
                    self.profile.push_table_entry(
                        scope,
                        target,
                        database,
                        Some(("excluded", false)),
                    );
                }
                let cursor = self.profile.cursor(&scope);
                self.nested(cursor, |walker| {
                    for assignment in assignments {
                        walker.expression(&assignment.value)?;
                    }
                    walker.optional_expression(selection.as_ref())
                })?;
                cursor
            }
            _ => None,
        };
        let rows = self.returning_rows(target, returned_scope);
        let returned = self.returning(returned_scope, rows, returning.as_deref())?;
        let scopes = ModifyingScopes { returning: rows, conflict, ..ModifyingScopes::default() };
        Ok((scopes, returned))
    }

    /// A `MERGE`: the source and then the target, the order `RETURNING *`
    /// lists their columns in, with the source alone and the target alone
    /// for the clauses that read one side.
    fn merge(
        &mut self,
        merge: &'query Merge,
    ) -> Result<(ModifyingScopes, Returned<'query, 'db, DB>), LookupError> {
        let Merge { table, source, on, clauses, output, .. } = merge;
        let target = self.target(table)?;
        let mut both = self.profile.begin_unowned_scope(self.outer);
        self.profile.record_factor(source);
        collect_lone_factor(
            AstRef::Query(source),
            &mut both,
            self.ctes,
            self.deriving,
            self.outer,
            self.profile,
        )?;
        let source_alone = self.profile.cursor(&both);
        self.profile.record_factor(table);
        collect_lone_factor(
            AstRef::Query(table),
            &mut both,
            self.ctes,
            self.deriving,
            self.outer,
            self.profile,
        )?;
        let mut target_alone = self.profile.begin_unowned_scope(self.outer);
        collect_lone_factor(
            AstRef::Query(table),
            &mut target_alone,
            self.ctes,
            self.deriving,
            self.outer,
            self.profile,
        )?;
        let scopes = ModifyingScopes {
            relations: self.profile.cursor(&both),
            returning: self.returning_rows(target, both),
            conflict: None,
            source: source_alone,
            target: self.profile.cursor(&target_alone),
        };
        self.nested(scopes.relations, |walker| walker.expression(on))?;
        for MergeClause { clause_kind, predicate, action, .. } in clauses {
            let cursor = scopes.clause(*clause_kind);
            self.nested(cursor, |walker| {
                walker.optional_expression(predicate.as_ref())?;
                match action {
                    MergeAction::Insert(MergeInsertExpr {
                        kind: MergeInsertKind::Values(values),
                        ..
                    }) => walker.values(values),
                    MergeAction::Update(MergeUpdateExpr {
                        kind,
                        update_predicate,
                        delete_predicate,
                        ..
                    }) => {
                        if let MergeUpdateKind::Set(assignments) = kind {
                            for assignment in assignments {
                                walker.expression(&assignment.value)?;
                            }
                        }
                        walker.optional_expression(update_predicate.as_ref())?;
                        walker.optional_expression(delete_predicate.as_ref())
                    }
                    _ => Ok(()),
                }
            })?;
        }
        let returning = match output {
            Some(OutputClause::Returning { select_items, .. }) => Some(select_items.as_slice()),
            _ => None,
        };
        let returned = self.returning(both, scopes.returning, returning)?;
        Ok((scopes, returned))
    }

    /// The table `factor` names as a statement's target.
    fn target(&self, factor: &TableFactor) -> Result<Option<&'db DB::Table>, LookupError> {
        match factor {
            TableFactor::Table { name, .. } => self.deriving.table(name),
            _ => Ok(None),
        }
    }

    /// The scope `RETURNING` reads: the statement's `relations`, behind `old`
    /// and `new` as rows of `target` that only a qualified reference reaches,
    /// each only when no relation of that name is in reach, as a `FROM` item
    /// or a rule action's row may take it.
    fn returning_rows(
        &mut self,
        target: Option<&'db DB::Table>,
        relations: ScopeId,
    ) -> Option<ScopeCursor> {
        let relations = self.profile.cursor(&relations);
        let (Some(target), Some(parent)) = (target, relations) else {
            return relations;
        };
        let mut rows = None;
        for name in ["old", "new"] {
            if self.profile.reaches_relation(relations, name) {
                continue;
            }
            let scope = *rows.get_or_insert_with(|| self.profile.begin_qualified_scope(parent));
            self.profile.push_table_entry(
                scope,
                target,
                self.deriving.database,
                Some((name, false)),
            );
        }
        rows.map_or(relations, |rows| self.profile.cursor(&rows))
    }

    /// A scope under the one the statement stands in, holding the relations
    /// `lists` name, in order.
    fn relations(&mut self, lists: &[&'query [TableWithJoins]]) -> Result<ScopeId, LookupError> {
        let mut scope = self.profile.begin_unowned_scope(self.outer);
        for list in lists {
            self.profile.record_from_items(list);
            collect_from_items(
                AstRef::Query(*list),
                &mut scope,
                self.ctes,
                self.deriving,
                self.outer,
                self.profile,
            )?;
        }
        Ok(scope)
    }

    /// Indexes the queries `walk` finds as nested under `parent`.
    fn nested(
        &mut self,
        parent: Option<ScopeCursor>,
        walk: impl FnOnce(&mut Nested<'_, 'query>) -> Result<(), LookupError>,
    ) -> Result<(), LookupError> {
        index_nested(self.profile, self.ctes, self.deriving, parent, walk)
    }

    /// Indexes the queries `items`, read through `cursor`, hold, and answers
    /// the columns they output, a wildcard listing those of the relations
    /// `scope` holds.
    fn returning(
        &mut self,
        scope: ScopeId,
        cursor: Option<ScopeCursor>,
        items: Option<&'query [SelectItem]>,
    ) -> Result<Returned<'query, 'db, DB>, LookupError> {
        let Some(items) = items else {
            return Ok(None);
        };
        self.nested(cursor, |walker| items.iter().try_for_each(|item| walker.select_item(item)))?;
        let mut columns = Vec::with_capacity(items.len());
        for item in items {
            if !append_select_item(
                AstRef::Query(item),
                &scope,
                cursor,
                None,
                self.profile,
                &mut columns,
            )? {
                return Ok(None);
            }
        }
        Ok(Some(DerivedShape { columns, row_preserving: false }))
    }
}

impl ModifyingScopes {
    /// The scope a `MERGE` clause of `kind` reads.
    pub(crate) const fn clause(self, kind: MergeClauseKind) -> Option<ScopeCursor> {
        match kind {
            MergeClauseKind::Matched => self.relations,
            MergeClauseKind::NotMatched | MergeClauseKind::NotMatchedByTarget => self.source,
            MergeClauseKind::NotMatchedBySource => self.target,
        }
    }
}
