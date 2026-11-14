//! Keeps, for each call a statement stores, the overload its argument types
//! select, the function PostgreSQL records the call's dependency on, or
//! none for a call PostgreSQL reads as a cast.
//!
//! A call is resolved by name and argument count when its statement runs,
//! and one reaching several overloads, one of them declared by the schema,
//! or a declared one by one argument waits for its argument types. Once the
//! statement has run, each waiting call is typed in the scope its
//! expression sees, over the catalog as the statement left it, and keeps
//! the function selected, none when read as a cast, or, when the types
//! select none, every overload.

use alloc::collections::BTreeMap;
use core::ops::ControlFlow;

use sqlparser::ast::{ColumnOption, Expr, Function, Query, Statement, Visit, Visitor};

use super::{ParserDB, ParserDBBuilder, constraint_names, partition_keys};
use crate::{
    structs::{
        CallResolution, ExpressionContext, ExpressionScope, IdentifierCase, TableAttribute,
        metadata::{CallChoice, ExpressionCalls},
    },
    traits::{PolicyLike, Relation, RuleLike, TriggerLike},
    utils::walk::walk,
};

/// What each typed call chooses, by the address of the call's node.
type Selections = BTreeMap<usize, CallChoice>;

impl ParserDBBuilder {
    /// Whether some stored call still waits for its argument types to choose
    /// among its overloads.
    pub(super) fn has_unsettled_calls(&self) -> bool {
        self.check_constraints().iter().any(|(_, metadata)| metadata.calls().is_unsettled())
            || self.columns().iter().any(|(_, metadata, _)| metadata.calls().is_unsettled())
            || self.indices().iter().any(|(_, metadata)| metadata.calls().is_unsettled())
            || self.views().iter().any(|(_, metadata, _)| metadata.calls().is_unsettled())
            || self
                .materialized_views()
                .iter()
                .any(|(_, metadata, _)| metadata.calls().is_unsettled())
            || self.policies().iter().any(|(_, metadata)| {
                metadata.using_calls().is_unsettled() || metadata.check_calls().is_unsettled()
            })
            || self.rules().iter().any(|(_, calls)| calls.is_unsettled())
            || self.triggers().iter().any(|(_, calls)| calls.is_unsettled())
            || self.functions().iter().any(|(_, metadata)| metadata.body_calls().is_unsettled())
            || self.domains().iter().any(|(_, metadata)| metadata.calls().is_unsettled())
            || self.tables().iter().any(|(_, metadata, _)| {
                metadata.partition_key_calls().is_unsettled()
                    || metadata.exclusion_calls().iter().any(ExpressionCalls::is_unsettled)
            })
    }

    /// Settles every waiting call against `database`, the catalog this
    /// builder holds.
    pub(super) fn settle_calls(&mut self, database: &ParserDB) {
        let selections = self.selections(database);
        let selected = |call: &Function| selections.get(&address(call)).copied();
        for (check, metadata) in self.check_constraints_mut() {
            if metadata.calls().is_unsettled() {
                metadata.calls_mut().settle(&*check.attribute().expr, selected);
            }
        }
        for (column, metadata, _) in self.columns_mut() {
            if metadata.calls().is_unsettled() {
                metadata.calls_mut().settle(column.attribute(), selected);
            }
        }
        for (index, metadata) in self.indices_mut() {
            if metadata.calls().is_unsettled() {
                metadata.calls_mut().settle(index.attribute(), selected);
            }
        }
        for (view, metadata, _) in self.views_mut() {
            if metadata.calls().is_unsettled() {
                metadata.calls_mut().settle(view.declaration().query(), selected);
            }
        }
        for (view, metadata, _) in self.materialized_views_mut() {
            if metadata.calls().is_unsettled() {
                metadata.calls_mut().settle(view.declaration().query(), selected);
            }
        }
        for (policy, metadata) in self.policies_mut() {
            let [using, check] = metadata.calls_mut();
            using.settle(&policy.using, selected);
            check.settle(&policy.with_check, selected);
        }
        for (rule, calls) in self.rules_mut() {
            if calls.is_unsettled() {
                calls.settle(&**rule, selected);
            }
        }
        for (trigger, calls) in self.triggers_mut() {
            if calls.is_unsettled() {
                calls.settle(&trigger.condition, selected);
            }
        }
        for (function, metadata) in self.functions_mut() {
            if metadata.body_calls().is_unsettled() {
                metadata.body_calls_mut().settle(&function.function_body, selected);
            }
        }
        for (domain, metadata) in self.domains_mut() {
            if metadata.calls().is_unsettled() {
                metadata.calls_mut().settle(&**domain, selected);
            }
        }
        for (table, metadata, _) in self.tables_mut() {
            if let (true, Some(entries)) =
                (metadata.partition_key_calls().is_unsettled(), partition_keys::key_entries(table))
            {
                metadata.partition_key_calls_mut().settle(entries, selected);
            }
            let exclusions = constraint_names::exclusions(table);
            for (exclusion, calls) in exclusions.zip(metadata.exclusion_calls_mut()) {
                if calls.is_unsettled() {
                    calls.settle(exclusion, selected);
                }
            }
        }
    }

    /// The function each waiting call selects, typed in the scope its
    /// expression sees over `database`.
    fn selections(&self, database: &ParserDB) -> Selections {
        let mut selections = Selections::new();
        let case = self.identifier_case();
        let mut select = Select { database, case, selections: &mut selections };
        for (check, metadata) in self.check_constraints() {
            if metadata.calls().is_unsettled() {
                let scope = ExpressionScope::Table(TableAttribute::table(check));
                select.expression(&check.attribute().expr, &scope);
            }
        }
        for (column, metadata, _) in self.columns() {
            if !metadata.calls().is_unsettled() {
                continue;
            }
            let table = ExpressionScope::Table(TableAttribute::table(column));
            for option in &column.attribute().options {
                // A default sees no column, every other expression the table.
                let scope = match option.option {
                    ColumnOption::Default(_) => &ExpressionScope::Empty,
                    _ => &table,
                };
                select.roots(&option.option, scope);
            }
        }
        for (index, metadata) in self.indices() {
            if metadata.calls().is_unsettled() {
                let scope = ExpressionScope::Table(TableAttribute::table(index));
                select.roots(index.attribute(), &scope);
            }
        }
        for (view, metadata, _) in self.views() {
            if metadata.calls().is_unsettled() {
                select.query(view.declaration().query());
            }
        }
        for (view, metadata, _) in self.materialized_views() {
            if metadata.calls().is_unsettled() {
                select.query(view.declaration().query());
            }
        }
        for (policy, metadata) in self.policies() {
            let unsettled =
                metadata.using_calls().is_unsettled() || metadata.check_calls().is_unsettled();
            if let (true, Ok(table)) = (unsettled, policy.table(database)) {
                let scope = ExpressionScope::Table(table);
                select.roots(&policy.using, &scope);
                select.roots(&policy.with_check, &scope);
            }
        }
        for (rule, calls) in self.rules() {
            // A rule on a view sees rows the typing scopes do not describe,
            // so its calls keep every overload.
            if let (true, Ok(Relation::Table(table))) =
                (calls.is_unsettled(), rule.relation(database))
            {
                select.roots(&**rule, &ExpressionScope::Rule { table, event: rule.event });
            }
        }
        for (trigger, calls) in self.triggers() {
            if let (true, Ok(table)) = (calls.is_unsettled(), trigger.table(database)) {
                select.roots(&trigger.condition, &ExpressionScope::Trigger(table));
            }
        }
        for (function, metadata) in self.functions() {
            if metadata.body_calls().is_unsettled() {
                let scope = ExpressionScope::FunctionBody(function.as_ref());
                select.roots(&function.function_body, &scope);
            }
        }
        for (domain, metadata) in self.domains() {
            if metadata.calls().is_unsettled() {
                select.roots(&domain.default, &ExpressionScope::Empty);
                select.roots(&domain.constraints, &ExpressionScope::DomainValue(&domain.data_type));
            }
        }
        for (table, metadata, _) in self.tables() {
            let scope = ExpressionScope::Table(table.as_ref());
            if let (true, Some(entries)) =
                (metadata.partition_key_calls().is_unsettled(), partition_keys::key_entries(table))
            {
                select.roots(entries, &scope);
            }
            for (exclusion, calls) in
                constraint_names::exclusions(table).zip(metadata.exclusion_calls())
            {
                if calls.is_unsettled() {
                    select.roots(exclusion, &scope);
                }
            }
        }
        selections
    }
}

/// Where a node sits in memory, which identifies a call or a function among
/// the nodes the builder and its snapshot share.
fn address<T>(node: &T) -> usize {
    core::ptr::from_ref(node).addr()
}

/// Types the calls of the expressions and statements handed to it, recording
/// the function each selects. One the model cannot scope selects nothing.
struct Select<'s> {
    database: &'s ParserDB,
    case: IdentifierCase,
    selections: &'s mut Selections,
}

impl Select<'_> {
    /// Types every statement and expression `node` holds that no other
    /// statement or expression holds, each in `scope`.
    fn roots<N: Visit + ?Sized>(&mut self, node: &N, scope: &ExpressionScope<'_, '_, ParserDB>) {
        let mut roots = Roots { depth: 0, select: self, scope };
        let ControlFlow::Continue(()) = walk(node, &mut roots);
    }

    /// Types the calls of `expression`, written outside a query, in `scope`.
    fn expression(&mut self, expression: &Expr, scope: &ExpressionScope<'_, '_, ParserDB>) {
        let context =
            ExpressionContext::for_expression(expression, scope, self.database, self.case);
        record(context.as_ref().ok(), expression, self.selections);
    }

    /// Types the calls of `statement`, written outside a query, in `scope`.
    fn statement(&mut self, statement: &Statement, scope: &ExpressionScope<'_, '_, ParserDB>) {
        let context = ExpressionContext::for_statement(statement, scope, self.database, self.case);
        record(context.as_ref().ok(), statement, self.selections);
    }

    /// Types the calls of `query`.
    fn query(&mut self, query: &Query) {
        let context = ExpressionContext::new(query, self.database, self.case);
        record(context.as_ref().ok(), query, self.selections);
    }
}

/// Records the function each call of `node`, typed by `context`, selects,
/// and none without a context.
fn record<N: Visit + ?Sized>(
    context: Option<&ExpressionContext<'_, '_, ParserDB>>,
    node: &N,
    selections: &mut Selections,
) {
    let mut calls = Calls { context, selections };
    let ControlFlow::Continue(()) = walk(node, &mut calls);
}

/// Types every statement and expression a node holds that no other statement
/// or expression holds, in one scope.
struct Roots<'r, 's, 'v, 'db> {
    depth: usize,
    select: &'r mut Select<'s>,
    scope: &'r ExpressionScope<'v, 'db, ParserDB>,
}

impl Visitor for Roots<'_, '_, '_, '_> {
    type Break = core::convert::Infallible;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        if self.depth == 0 {
            self.select.statement(statement, self.scope);
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_statement(&mut self, _statement: &Statement) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if self.depth == 0 {
            self.select.expression(expression, self.scope);
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, _expression: &Expr) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
}

/// Records the function each call a context types selects.
struct Calls<'c, 'q, 'db> {
    context: Option<&'c ExpressionContext<'q, 'db, ParserDB>>,
    selections: &'c mut Selections,
}

impl Visitor for Calls<'_, '_, '_> {
    type Break = core::convert::Infallible;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if let Expr::Function(function) = expression
            && let Some(context) = self.context
            && let Ok(Some(call)) = context.call(expression)
        {
            let choice = match call.resolution() {
                CallResolution::Selected(selected) => {
                    Some(CallChoice::Function(address(selected.function())))
                }
                CallResolution::Cast(_) => Some(CallChoice::Cast),
                _ => None,
            };
            if let Some(choice) = choice {
                self.selections.insert(address(function), choice);
            }
        }
        ControlFlow::Continue(())
    }
}
