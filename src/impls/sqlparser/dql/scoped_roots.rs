//! The scopes of an expression or a data-modifying statement written outside
//! a query, under the names its [`ExpressionScope`] reaches.

use alloc::{borrow::ToOwned, vec::Vec};
use core::slice;

use sqlparser::ast::{
    CreateRuleEvent, Delete, DoUpdate, Expr, FromTable, Insert, OnConflict, OnConflictAction,
    OnInsert, Statement, TableObject, Update, UpdateTableFromKind,
};

use super::{
    AstRef, DerivationProfile, DerivedColumn, DerivedRelationRef, DerivedShape, Deriving,
    NestedQueryWalker, RelationKey, collect_from_items,
    definition_graph::{DefinitionDerivation, DefinitionGraph, ScopeCursor, ScopeId},
    index_nested_query_scopes,
};
use crate::{
    errors::LookupError,
    structs::{ExpressionScope, IdentifierCase},
    traits::{DatabaseLike, FunctionLike},
};

/// An expression or a statement a context is built over.
#[derive(Clone, Copy)]
pub(crate) enum ScopedRoot<'query> {
    /// An expression of an object, as a check constraint's.
    Expression(&'query Expr),
    /// A query or a data-modifying statement, as a rule action or a
    /// statement of a `BEGIN ATOMIC` body.
    Statement(&'query Statement),
}

/// The scopes the clauses of a root resolve in.
#[derive(Clone, Copy)]
pub(crate) struct RootScopes {
    /// The names the [`ExpressionScope`] reaches.
    pub(crate) outer: ScopeCursor,
    /// A data-modifying statement's own relations: its target and the
    /// relations its `FROM` or `USING` lists.
    pub(crate) statement: Option<ScopeCursor>,
    /// An `INSERT ... ON CONFLICT DO UPDATE`'s target and `excluded` rows.
    pub(crate) conflict: Option<ScopeCursor>,
}

/// Builds the scopes of `root` under the names `scope` reaches.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving the root runs into.
pub(crate) fn scoped_graph<'query, 'db, DB: DatabaseLike>(
    root: ScopedRoot<'query>,
    scope: &ExpressionScope<'_, 'db, DB>,
    database: &'db DB,
    case: IdentifierCase,
) -> Result<(DefinitionGraph<'query, 'db, DB>, RootScopes), LookupError> {
    let deriving = Deriving::of(database, case);
    let mut profile = DefinitionDerivation::new(case);
    let outer = profile.begin_unowned_scope(None);
    reach(&mut profile, outer, scope, database);
    let outer_cursor = profile.cursor(&outer);
    let (statement, conflict) = match root {
        ScopedRoot::Expression(expression) => {
            nested_queries(&mut profile, deriving, outer_cursor, |walker| {
                walker.expression(expression)
            })?;
            (None, None)
        }
        ScopedRoot::Statement(statement) => {
            statement_scopes(&mut profile, deriving, outer_cursor, statement)?
        }
    };
    let (graph, outer) = profile.finish(outer);
    Ok((graph, RootScopes { outer, statement, conflict }))
}

/// Fills `outer` with the relations `scope` reaches.
fn reach<'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'_, 'db, DB>,
    outer: ScopeId,
    scope: &ExpressionScope<'_, 'db, DB>,
    database: &'db DB,
) {
    match scope {
        ExpressionScope::Empty | ExpressionScope::DomainValue(_) => {}
        ExpressionScope::Table(table) => profile.push_table_entry(outer, table, database, None),
        ExpressionScope::Trigger(table) => {
            profile.push_table_entry(outer, table, database, Some(("new", false)));
            profile.push_table_entry(outer, table, database, Some(("old", false)));
        }
        ExpressionScope::Rule { table, event } => {
            if matches!(event, CreateRuleEvent::Insert | CreateRuleEvent::Update) {
                profile.push_table_entry(outer, table, database, Some(("new", false)));
            }
            if !matches!(event, CreateRuleEvent::Insert) {
                profile.push_table_entry(outer, table, database, Some(("old", false)));
            }
        }
        ExpressionScope::FunctionBody(function) => {
            push_parameters(profile, outer, function, database);
        }
    }
}

/// Appends the named input parameters of `function` to `outer` as one
/// relation named after the function.
fn push_parameters<'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'_, 'db, DB>,
    outer: ScopeId,
    function: &'db DB::Function,
    database: &'db DB,
) {
    let mut columns = Vec::new();
    for (ordinal, name) in input_parameter_names(function, database).enumerate() {
        let Some(name) = name else { continue };
        let definition = profile.parameter_definition(function, ordinal);
        columns.push(DerivedColumn {
            name: name.name().to_owned(),
            quoted: name.name_is_quoted(),
            source: None,
            definition,
            marker: core::marker::PhantomData,
        });
    }
    let key =
        RelationKey { value: AstRef::Database(function.name()), quoted: function.name_is_quoted() };
    let mut outer = outer;
    let data = profile.scope_mut(&mut outer);
    data.derived.push(DerivedRelationRef {
        key: Some(key),
        identity: Some(key),
        shape: DerivedShape { columns, row_preserving: false },
        nullable: false,
        entry_index: data.from_entry_count,
        factor: 0,
    });
    data.from_entry_count += 1;
}

/// The name of each input parameter of `function`, in the order `$n` counts
/// them, `None` for an unnamed one.
pub(crate) fn input_parameter_names<'db, DB: DatabaseLike>(
    function: &'db DB::Function,
    database: &'db DB,
) -> impl Iterator<Item = Option<crate::structs::TargetName<'db>>> {
    function
        .argument_modes()
        .zip(function.argument_names(database))
        .filter(|(mode, _)| is_input(mode))
        .map(|(_, name)| name)
}

/// Whether a parameter of mode `mode` is an input a body reads.
pub(crate) const fn is_input(mode: &sqlparser::ast::ArgMode) -> bool {
    !matches!(mode, sqlparser::ast::ArgMode::Out)
}

/// Indexes the queries `walk` finds as nested under `parent`.
fn nested_queries<'query, 'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'query, 'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    parent: Option<ScopeCursor>,
    walk: impl FnOnce(
        &mut NestedQueryWalker<
            &mut dyn FnMut(&'query sqlparser::ast::Query) -> Result<(), LookupError>,
        >,
    ) -> Result<(), LookupError>,
) -> Result<(), LookupError> {
    let mut index = |query: &'query sqlparser::ast::Query| {
        index_nested_query_scopes(AstRef::Query(query), &[], deriving, parent, profile).map(|_| ())
    };
    let mut walker = NestedQueryWalker {
        query: &mut index
            as &mut dyn FnMut(&'query sqlparser::ast::Query) -> Result<(), LookupError>,
    };
    walk(&mut walker)
}

/// The scopes of a statement root, its own relations and an `ON CONFLICT`'s.
fn statement_scopes<'query, 'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'query, 'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    outer: Option<ScopeCursor>,
    statement: &'query Statement,
) -> Result<(Option<ScopeCursor>, Option<ScopeCursor>), LookupError> {
    match statement {
        Statement::Query(query) => {
            index_nested_query_scopes(AstRef::Query(query), &[], deriving, outer, profile)?;
            Ok((None, None))
        }
        Statement::Update(Update { table, from, assignments, selection, returning, .. }) => {
            let lists: [&'query [_]; 2] = [
                slice::from_ref(table),
                match from {
                    Some(
                        UpdateTableFromKind::BeforeSet(from) | UpdateTableFromKind::AfterSet(from),
                    ) => from,
                    None => &[],
                },
            ];
            let cursor = relations(profile, deriving, outer, &lists)?;
            nested_queries(profile, deriving, cursor, |walker| {
                for assignment in assignments {
                    walker.expression(&assignment.value)?;
                }
                walker.optional_expression(selection.as_ref())?;
                returning.iter().flatten().try_for_each(|item| walker.select_item(item))
            })?;
            Ok((cursor, None))
        }
        Statement::Delete(Delete { from, using, selection, returning, .. }) => {
            let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = from;
            let lists: [&'query [_]; 2] = [from, using.as_deref().unwrap_or_default()];
            let cursor = relations(profile, deriving, outer, &lists)?;
            nested_queries(profile, deriving, cursor, |walker| {
                walker.optional_expression(selection.as_ref())?;
                returning.iter().flatten().try_for_each(|item| walker.select_item(item))
            })?;
            Ok((cursor, None))
        }
        Statement::Insert(insert) => insert_scopes(profile, deriving, outer, insert),
        _ => Ok((None, None)),
    }
}

/// A scope under `outer` holding the relations `lists` name, in order.
fn relations<'query, 'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'query, 'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    outer: Option<ScopeCursor>,
    lists: &[&'query [sqlparser::ast::TableWithJoins]],
) -> Result<Option<ScopeCursor>, LookupError> {
    let mut scope = profile.begin_unowned_scope(outer);
    for list in lists {
        profile.record_from_items(list);
        collect_from_items(AstRef::Query(*list), &mut scope, &[], deriving, outer, profile)?;
    }
    Ok(profile.cursor(&scope))
}

/// The scopes of an `INSERT`: its source query under `outer`, its target for
/// `RETURNING`, and its target with the `excluded` row for `ON CONFLICT DO
/// UPDATE`.
fn insert_scopes<'query, 'db, DB: DatabaseLike>(
    profile: &mut DefinitionDerivation<'query, 'db, DB>,
    deriving: Deriving<'_, 'db, DB>,
    outer: Option<ScopeCursor>,
    insert: &'query Insert,
) -> Result<(Option<ScopeCursor>, Option<ScopeCursor>), LookupError> {
    let Insert { table, table_alias, source, on, returning, .. } = insert;
    if let Some(source) = source {
        index_nested_query_scopes(AstRef::Query(source), &[], deriving, outer, profile)?;
    }
    let TableObject::TableName(name) = table else {
        return Ok((None, None));
    };
    let Some(target) = deriving.table(name)? else {
        return Ok((None, None));
    };
    let alias = table_alias
        .as_ref()
        .map(|alias| (alias.alias.value.as_str(), alias.alias.quote_style.is_some()));
    let target_scope = |profile: &mut DefinitionDerivation<'query, 'db, DB>, excluded: bool| {
        let scope = profile.begin_unowned_scope(outer);
        profile.push_table_entry(scope, target, deriving.database, alias);
        if excluded {
            profile.push_table_entry(scope, target, deriving.database, Some(("excluded", false)));
        }
        profile.cursor(&scope)
    };
    let returned = target_scope(profile, false);
    nested_queries(profile, deriving, returned, |walker| {
        returning.iter().flatten().try_for_each(|item| walker.select_item(item))
    })?;
    let conflict = match on {
        Some(OnInsert::OnConflict(OnConflict {
            action: OnConflictAction::DoUpdate(DoUpdate { assignments, selection }),
            ..
        })) => {
            let cursor = target_scope(profile, true);
            nested_queries(profile, deriving, cursor, |walker| {
                for assignment in assignments {
                    walker.expression(&assignment.value)?;
                }
                walker.optional_expression(selection.as_ref())
            })?;
            cursor
        }
        _ => None,
    };
    Ok((returned, conflict))
}
