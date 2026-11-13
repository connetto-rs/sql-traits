//! The scopes of an expression or a statement written outside a query, under
//! the names its [`ExpressionScope`] reaches.

use alloc::{borrow::ToOwned, vec::Vec};

use sqlparser::ast::{CreateRuleEvent, Expr, Statement};

use super::{
    AstRef, DerivationProfile, DerivedColumn, DerivedRelationRef, DerivedShape, Deriving,
    RelationKey,
    definition_graph::{DefinitionDerivation, DefinitionGraph, ScopeCursor, ScopeId},
    index_nested_query_scopes,
    modifying::index_nested,
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

/// Builds the scopes of `root` under the names `scope` reaches, and answers
/// the scope holding those names.
///
/// # Errors
///
/// Returns the relation-name lookup errors resolving the root runs into.
pub(crate) fn scoped_graph<'query, 'db, DB: DatabaseLike>(
    root: ScopedRoot<'query>,
    scope: &ExpressionScope<'_, 'db, DB>,
    database: &'db DB,
    case: IdentifierCase,
) -> Result<(DefinitionGraph<'query, 'db, DB>, ScopeCursor), LookupError> {
    let deriving = Deriving::of(database, case);
    let mut profile = DefinitionDerivation::new(case);
    let outer = profile.begin_unowned_scope(None);
    reach(&mut profile, outer, scope, database);
    let outer_cursor = profile.cursor(&outer);
    match root {
        ScopedRoot::Expression(expression) => {
            index_nested(&mut profile, &[], deriving, outer_cursor, |walker| {
                walker.expression(expression)
            })?;
        }
        ScopedRoot::Statement(Statement::Query(query)) => {
            index_nested_query_scopes(
                AstRef::Query(query),
                &[],
                deriving,
                outer_cursor,
                &mut profile,
            )?;
        }
        ScopedRoot::Statement(statement) => {
            profile.modifying_shape(statement, &[], deriving, outer_cursor)?;
        }
    }
    Ok(profile.finish(outer))
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
