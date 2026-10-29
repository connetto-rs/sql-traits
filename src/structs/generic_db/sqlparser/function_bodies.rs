//! Binding the relations a function's `RETURN` body reads, as PostgreSQL does
//! at `CREATE FUNCTION` time.
//!
//! A body written `RETURN <expression>` parses the body up front, so its
//! relation names resolve under the session's `search_path` in effect at
//! creation, exactly as a view's definition does. A name the body cannot
//! resolve refuses the statement. A body written as a string is never bound,
//! and neither is a name a visible `WITH` item answers or a table function.

use alloc::{boxed::Box, string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CreateFunction, CreateFunctionBody, Expr, GroupByExpr, ObjectName, Query, Select, SelectFlavor,
    SelectItem, SetExpr, TableFactor, VisitMut, VisitorMut, helpers::attached_token::AttachedToken,
};

use super::{ParserDBBuilder, bind_reference, views::relations_read_in};
use crate::{
    errors::Error,
    utils::{
        last_str,
        object_name::{RelationKey, target_name_from_object_name},
        with_scopes::WithScopes,
    },
};

/// Binds every stored relation the body of `function` reads, refusing the
/// statement when one of them is not held.
///
/// # Errors
///
/// Returns [`Error::RelationNotFoundForFunctionBody`] naming the first
/// relation the body names that the builder does not hold, the way PostgreSQL
/// refuses the `CREATE FUNCTION`.
pub(super) fn bind(builder: &ParserDBBuilder, function: &mut CreateFunction) -> Result<(), Error> {
    let Some(body) = function.function_body.as_mut() else {
        return Ok(());
    };
    let Some(mut query) = definition(body) else {
        return Ok(());
    };
    let mut binder = BodyBinder { builder, scopes: WithScopes::default(), missing: None };
    let walked = query.visit(&mut binder);
    debug_assert!(walked.is_continue(), "the binder never breaks");
    if let Some(missing) = binder.missing {
        return Err(Error::RelationNotFoundForFunctionBody {
            relation_name: missing.to_string(),
            function_name: last_str(&function.name).to_string(),
        });
    }
    write_definition(&mut function.function_body, query);
    Ok(())
}

/// The query shape of a body that holds a parsed expression, or [`None`] for
/// a body written as a string or as statements.
///
/// The wrapper is the shape the rename machinery rewrites and checks, so a
/// body gains a relation to follow only by holding one of the variants here.
/// When the parser grows the `BEGIN ATOMIC` body, its expression joins the
/// match here.
pub(super) fn definition(body: &CreateFunctionBody) -> Option<Query> {
    match body {
        CreateFunctionBody::Return(expr) => {
            Some(Query {
                with: None,
                body: Box::new(SetExpr::Select(Box::new(body_select(expr)))),
                order_by: None,
                limit_clause: None,
                fetch: None,
                locks: Vec::new(),
                for_clause: None,
                settings: None,
                format_clause: None,
                pipe_operators: Vec::new(),
            })
        }
        _ => None,
    }
}

/// The select a wrapper query holds around a body expression: one projection
/// item, no `FROM`, nothing else to resolve.
fn body_select(expr: &Expr) -> Select {
    Select {
        select_token: AttachedToken::empty(),
        optimizer_hints: Vec::new(),
        distinct: None,
        select_modifiers: None,
        top: None,
        top_before_distinct: false,
        projection: alloc::vec![SelectItem::UnnamedExpr(expr.clone())],
        exclude: None,
        into: None,
        from: Vec::new(),
        lateral_views: Vec::new(),
        prewhere: None,
        selection: None,
        connect_by: Vec::new(),
        group_by: GroupByExpr::Expressions(Vec::new(), Vec::new()),
        cluster_by: Vec::new(),
        distribute_by: Vec::new(),
        sort_by: Vec::new(),
        having: None,
        named_window: Vec::new(),
        qualify: None,
        window_before_qualify: false,
        value_table_mode: None,
        flavor: SelectFlavor::Standard,
    }
}

/// Writes a rewritten body back into `body`, in the variant it was read from.
pub(super) fn write_definition(body: &mut Option<CreateFunctionBody>, query: Query) {
    // `definition` built `query`, so the shape below always matches.
    if let Some(CreateFunctionBody::Return(expr)) = body
        && let SetExpr::Select(select) = *query.body
        && let Some(SelectItem::UnnamedExpr(rewritten)) = select.projection.into_iter().next()
    {
        *expr = rewritten;
    }
}

/// The normalized identity of every relation a function body reads, or
/// [`None`] for a body that holds no parsed expression.
pub(super) fn relations_read(body: &CreateFunctionBody) -> Option<Vec<RelationKey>> {
    definition(body).map(|definition| relations_read_in(&definition))
}

/// Binds every stored relation a function body reads, remembering the first
/// one the builder does not hold.
struct BodyBinder<'builder> {
    builder: &'builder ParserDBBuilder,
    scopes: WithScopes,
    missing: Option<ObjectName>,
}

impl VisitorMut for BodyBinder<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        self.scopes.enter(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        self.scopes.leave(query);
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, args: None, .. } = factor
            && !self.scopes.reads_item(name)
        {
            match self.builder.bound_relation(name) {
                Some(bound) => bind_reference(name, bound.qualifier),
                // A name no readable target can read stays as written, as in
                // a view definition; a plain name the builder does not hold
                // is the one PostgreSQL refuses.
                None if target_name_from_object_name(name).is_some() && self.missing.is_none() => {
                    self.missing = Some(name.clone());
                }
                None => {}
            }
        }
        ControlFlow::Continue(())
    }
}
