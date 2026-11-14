//! Keeps, for each call a statement stores, the overload its argument types
//! select, the function PostgreSQL records the call's dependency on or none
//! for a call PostgreSQL reads as a cast, and for each operator application
//! the operator its operand types select, which is what PostgreSQL records
//! the dependencies on.
//!
//! A call is resolved by name and argument count when its statement runs,
//! and one reaching several overloads, one of them declared by the schema,
//! or a declared one by one argument waits for its argument types, as every
//! new operator node waits for its operand types. Once the statement has
//! run, each waiting node is typed in the scope its expression sees, over
//! the catalog as the statement left it. A call keeps the function
//! selected, none when read as a cast, or, when the types select none,
//! every overload, and an operator node the operators a statement added
//! that its applications select.
//!
//! An expression `ALTER COLUMN ... TYPE` has PostgreSQL parse again from the
//! text it deparsed is typed twice: once over the column's previous type and
//! the operators selected then, to read what the deparse pins, and once more
//! honouring those pins.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{
    ColumnOption, CreateFunction, DataType, Expr, Function, Ident, ObjectName, Query, Statement,
    TableFactor, Visit, Visitor,
};

use super::{ParserDB, ParserDBBuilder, PostgresCatalogOperator, constraint_names, partition_keys};
use crate::{
    errors::Error,
    structs::{
        ArraySource, CallApplication, CallRefusal, CallResolution, ExpressionContext,
        ExpressionScope, IdentifierCase, Operand, OperandEvidence, OperandType, OperatorResolution,
        OperatorRow, ReparsePins, Site, TableAttribute, TargetName, applies_operators,
        metadata::{
            CallChoice, ExpressionCalls, NodeState, OperatorNode, WrittenPins,
            for_each_operator_node,
        },
    },
    traits::{DatabaseLike, OperatorLike, PolicyLike, Relation, RuleLike, TriggerLike},
    utils::{
        call_sites::for_each_call,
        call_typing::{reached_by_name, written_argument_names},
        identifier_resolution::stored_identifier,
        object_name::qualifier_of,
        walk::walk,
    },
};

/// The schema a function stored without one lives in.
const DEFAULT_SCHEMA: &str = "public";

/// What each typed call and operator application selects.
#[derive(Default)]
struct Selections {
    /// What each call chooses, by the address of the name it calls, which a
    /// function expression and a function in `FROM` both hold.
    calls: BTreeMap<usize, CallChoice>,
    /// The operators a statement added that each node's applications select,
    /// by the address of the node, with each application's position.
    operators: BTreeMap<usize, Vec<(usize, OperatorRow)>>,
    /// The refusal of the first application of each node PostgreSQL
    /// refuses, by the address of the node.
    refused_operators: BTreeMap<usize, Error>,
    /// The refusal of each call PostgreSQL refuses, by the address of the
    /// name it calls, which a function expression and a function in `FROM`
    /// both hold.
    refused_calls: BTreeMap<usize, Error>,
    /// What the text each expression a type change parsed again was
    /// deparsed into writes, by the address of the expression.
    written: BTreeMap<usize, WrittenPins>,
}

impl Selections {
    /// The refusal of the first node or call of `node`, whose record is
    /// `calls`, the statement writes or parses again and PostgreSQL refuses.
    fn refusal<N: Visit + ?Sized>(
        &mut self,
        calls: &ExpressionCalls<CreateFunction>,
        node: &N,
    ) -> Option<Error> {
        let mut refused = None;
        let mut states = calls.operator_nodes().iter().map(OperatorNode::state);
        for_each_operator_node(node, |expression| {
            let typed = states.next().is_some_and(|state| state != NodeState::Settled);
            if typed && refused.is_none() {
                refused = self.refused_operators.remove(&address(expression));
            }
        });
        let mut states = calls.call_states();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |call| {
            let typed = states.next().is_some_and(|state| state != NodeState::Settled);
            if typed && refused.is_none() {
                refused = self.refused_calls.remove(&address(call.name));
            }
            ControlFlow::Continue(())
        });
        refused
    }
}

/// Whether `calls` holds a call or an operator node still to type.
fn is_waiting(calls: &ExpressionCalls<CreateFunction>) -> bool {
    calls.is_unsettled() || calls.has_pending_operators() || calls.has_untyped_calls()
}

/// Settles every waiting call and operator node of `calls`, the record of
/// `node`, as `selections` has them, refusing first, when `refuse`, a node
/// or a call the statement writes or parses again that PostgreSQL refuses.
fn settle<N: Visit + ?Sized>(
    calls: &mut ExpressionCalls<CreateFunction>,
    node: &N,
    selections: &mut Selections,
    refuse: bool,
) -> Result<(), Error> {
    if let Some(refused) = refuse.then(|| selections.refusal(calls, node)).flatten() {
        return Err(refused);
    }
    if calls.is_unsettled() {
        calls.settle(node, |name: &ObjectName| selections.calls.get(&address(name)).copied());
    }
    if calls.has_pending_operators() {
        calls.settle_operators(node, |node| selections.operators.remove(&node).unwrap_or_default());
    } else if calls.has_untyped_calls() {
        calls.settle_remaining_operators();
    }
    if let Some(written) = selections.written.remove(&root_address(node)) {
        calls.write(written);
    }
    Ok(())
}

/// Where `node`, a whole expression or statement, sits in memory, which
/// identifies it between typing it and settling its record.
fn root_address<N: ?Sized>(node: &N) -> usize {
    core::ptr::from_ref(node).cast::<()>().addr()
}

impl ParserDBBuilder {
    /// Every record of the calls and operator applications a stored
    /// expression holds.
    fn records(&self) -> impl Iterator<Item = &ExpressionCalls<CreateFunction>> {
        let checks = self.check_constraints().iter().map(|(_, metadata)| metadata.calls());
        let columns = self.columns().iter().map(|(_, metadata, _)| metadata.calls());
        let indices = self.indices().iter().map(|(_, metadata)| metadata.calls());
        let unique_keys = self.unique_indices().iter().map(|(_, metadata)| metadata.calls());
        let views = self.views().iter().map(|(_, metadata, _)| metadata.calls());
        let materialized =
            self.materialized_views().iter().map(|(_, metadata, _)| metadata.calls());
        let policies = self
            .policies()
            .iter()
            .flat_map(|(_, metadata)| [metadata.using_calls(), metadata.check_calls()]);
        let rules = self.rules().iter().map(|(_, calls)| calls);
        let triggers = self.triggers().iter().map(|(_, calls)| calls);
        let functions = self.functions().iter().map(|(_, metadata)| metadata.body_calls());
        let domains = self.domains().iter().map(|(_, metadata)| metadata.calls());
        let tables = self.tables().iter().flat_map(|(_, metadata, _)| {
            core::iter::once(metadata.partition_key_calls()).chain(metadata.exclusion_calls())
        });
        checks
            .chain(columns)
            .chain(indices)
            .chain(unique_keys)
            .chain(views)
            .chain(materialized)
            .chain(policies)
            .chain(rules)
            .chain(triggers)
            .chain(functions)
            .chain(domains)
            .chain(tables)
    }

    /// Whether some stored call still waits for its argument types to choose
    /// among its overloads, or some operator node for its operand types.
    pub(super) fn has_unsettled_calls(&self) -> bool {
        self.records().any(is_waiting)
    }

    /// Settles every waiting operator node as selecting no operator a
    /// statement added, for a catalog holding none, but a record whose calls
    /// are parsed again, which the deparse pins still have to type.
    pub(super) fn settle_operators_on_built_ins(&mut self) {
        for (_, metadata) in self.check_constraints_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata, _) in self.columns_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata) in self.indices_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata) in self.unique_indices_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata, _) in self.views_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata, _) in self.materialized_views_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata) in self.policies_mut() {
            for calls in metadata.calls_mut() {
                calls.settle_on_built_ins();
            }
        }
        for (_, calls) in self.rules_mut() {
            calls.settle_on_built_ins();
        }
        for (_, calls) in self.triggers_mut() {
            calls.settle_on_built_ins();
        }
        for (_, metadata) in self.functions_mut() {
            metadata.body_calls_mut().settle_on_built_ins();
        }
        for (_, metadata) in self.domains_mut() {
            metadata.calls_mut().settle_on_built_ins();
        }
        for (_, metadata, _) in self.tables_mut() {
            metadata.partition_key_calls_mut().settle_on_built_ins();
            for calls in metadata.exclusion_calls_mut() {
                calls.settle_on_built_ins();
            }
        }
    }

    /// Settles every waiting call and operator node against `database`, the
    /// catalog this builder holds, refusing, when `refuse`, an expression a
    /// statement writes or parses again whose operator or call PostgreSQL
    /// refuses. A function body is not refused, since PostgreSQL validates
    /// one only under `check_function_bodies`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnresolvedOperator`] or [`Error::UnresolvedCall`]
    /// for the first expression refused.
    pub(super) fn settle_calls(&mut self, database: &ParserDB, refuse: bool) -> Result<(), Error> {
        let mut selections = self.selections(database);
        let selections = &mut selections;
        for (check, metadata) in self.check_constraints_mut() {
            settle(metadata.calls_mut(), &*check.attribute().expr, selections, refuse)?;
        }
        for (column, metadata, _) in self.columns_mut() {
            settle(metadata.calls_mut(), column.attribute(), selections, refuse)?;
        }
        for (index, metadata) in self.indices_mut() {
            settle(metadata.calls_mut(), index.attribute(), selections, refuse)?;
        }
        for (unique_key, metadata) in self.unique_indices_mut() {
            settle(metadata.calls_mut(), unique_key.attribute(), selections, refuse)?;
        }
        for (view, metadata, _) in self.views_mut() {
            settle(metadata.calls_mut(), view.declaration().query(), selections, refuse)?;
        }
        for (view, metadata, _) in self.materialized_views_mut() {
            settle(metadata.calls_mut(), view.declaration().query(), selections, refuse)?;
        }
        for (policy, metadata) in self.policies_mut() {
            let [using, check] = metadata.calls_mut();
            settle(using, &policy.using, selections, refuse)?;
            settle(check, &policy.with_check, selections, refuse)?;
        }
        for (rule, calls) in self.rules_mut() {
            settle(calls, &**rule, selections, refuse)?;
        }
        for (trigger, calls) in self.triggers_mut() {
            settle(calls, &trigger.condition, selections, refuse)?;
        }
        for (function, metadata) in self.functions_mut() {
            settle(metadata.body_calls_mut(), &function.function_body, selections, false)?;
        }
        for (domain, metadata) in self.domains_mut() {
            settle(metadata.calls_mut(), &**domain, selections, refuse)?;
        }
        for (table, metadata, _) in self.tables_mut() {
            if let Some(entries) = partition_keys::key_entries(table) {
                settle(metadata.partition_key_calls_mut(), entries, selections, refuse)?;
            }
            let exclusions = constraint_names::exclusions(table);
            for (exclusion, calls) in exclusions.zip(metadata.exclusion_calls_mut()) {
                settle(calls, exclusion, selections, refuse)?;
            }
        }
        Ok(())
    }

    /// The function each waiting call and the operators each waiting
    /// operator node selects, typed in the scope its expression sees over
    /// `database`.
    fn selections(&self, database: &ParserDB) -> Selections {
        let mut selections = Selections::default();
        let case = self.identifier_case();
        let mut select =
            Select { database, case, selections: &mut selections, pins: None, deparsed: None };
        for (check, metadata) in self.check_constraints() {
            let expression = &check.attribute().expr;
            let scope = ExpressionScope::Table(TableAttribute::table(check));
            select.typed(&**expression, metadata.calls(), |select| {
                select.expression(expression, &scope);
            });
        }
        for (column, metadata, _) in self.columns() {
            if !is_waiting(metadata.calls()) {
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
            let scope = ExpressionScope::Table(TableAttribute::table(index));
            select.typed(index.attribute(), metadata.calls(), |select| {
                select.roots(index.attribute(), &scope);
            });
        }
        for (unique_key, metadata) in self.unique_indices() {
            let scope = ExpressionScope::Table(TableAttribute::table(unique_key));
            select.typed(unique_key.attribute(), metadata.calls(), |select| {
                select.roots(unique_key.attribute(), &scope);
            });
        }
        for (view, metadata, _) in self.views() {
            if is_waiting(metadata.calls()) {
                select.query(view.declaration().query());
            }
        }
        for (view, metadata, _) in self.materialized_views() {
            if is_waiting(metadata.calls()) {
                select.query(view.declaration().query());
            }
        }
        for (policy, metadata) in self.policies() {
            let waiting = is_waiting(metadata.using_calls()) || is_waiting(metadata.check_calls());
            if let (true, Ok(table)) = (waiting, policy.table(database)) {
                let scope = ExpressionScope::Table(table);
                select.roots(&policy.using, &scope);
                select.roots(&policy.with_check, &scope);
            }
        }
        for (rule, calls) in self.rules() {
            // A rule on a view sees rows the typing scopes do not describe,
            // so its calls keep every overload and its operators record
            // nothing.
            if let (true, Ok(Relation::Table(table))) = (is_waiting(calls), rule.relation(database))
            {
                select.roots(&**rule, &ExpressionScope::Rule { table, event: rule.event });
            }
        }
        for (trigger, calls) in self.triggers() {
            if let (true, Ok(table)) = (is_waiting(calls), trigger.table(database)) {
                select.roots(&trigger.condition, &ExpressionScope::Trigger(table));
            }
        }
        for (function, metadata) in self.functions() {
            if is_waiting(metadata.body_calls()) {
                let scope = ExpressionScope::FunctionBody(function.as_ref());
                select.roots(&function.function_body, &scope);
            }
        }
        for (domain, metadata) in self.domains() {
            if is_waiting(metadata.calls()) {
                select.roots(&domain.default, &ExpressionScope::Empty);
                select.roots(&domain.constraints, &ExpressionScope::DomainValue(&domain.data_type));
            }
        }
        for (table, metadata, _) in self.tables() {
            let scope = ExpressionScope::Table(table.as_ref());
            if let (true, Some(entries)) =
                (is_waiting(metadata.partition_key_calls()), partition_keys::key_entries(table))
            {
                select.roots(entries, &scope);
            }
            for (exclusion, calls) in
                constraint_names::exclusions(table).zip(metadata.exclusion_calls())
            {
                select.typed(exclusion, calls, |select| select.roots(exclusion, &scope));
            }
        }
        selections
    }
}

/// Where a node sits in memory, which identifies a call, a function or an
/// operator node among the nodes the builder and its snapshot share.
fn address<T>(node: &T) -> usize {
    core::ptr::from_ref(node).addr()
}

/// Types the calls and operator applications of the expressions and
/// statements handed to it, recording what each selects. One the model
/// cannot scope selects nothing.
struct Select<'s> {
    database: &'s ParserDB,
    case: IdentifierCase,
    selections: &'s mut Selections,
    /// What the deparsed text pins, for an expression parsed again.
    pins: Option<&'s (dyn ReparsePins<ParserDB> + 's)>,
    /// Where the pins the typing reads are written, instead of recording
    /// selections.
    deparsed: Option<&'s mut Deparsed>,
}

impl<'s> Select<'s> {
    /// Types `node`, whose record is `calls`, through `roots` when something
    /// of it waits, twice with what the deparse pins when it is parsed again.
    fn typed<N: Visit + ?Sized>(
        &mut self,
        node: &N,
        calls: &ExpressionCalls<CreateFunction>,
        roots: impl Fn(&mut Select<'_>),
    ) {
        if calls.is_rewritten() {
            let recorded = Recorded::of(node, calls, self.database, self.case);
            let mut deparsed = Deparsed::of(calls.written(), &recorded.positions);
            roots(&mut Select {
                database: self.database,
                case: self.case,
                selections: &mut *self.selections,
                pins: Some(&recorded),
                deparsed: Some(&mut deparsed),
            });
            roots(&mut Select {
                database: self.database,
                case: self.case,
                selections: &mut *self.selections,
                pins: Some(&deparsed),
                deparsed: None,
            });
            let written = deparsed.into_written(&recorded.positions);
            self.selections.written.insert(root_address(node), written);
        } else if is_waiting(calls) {
            roots(self);
        }
    }

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
        self.record(context.ok(), expression);
    }

    /// Types the calls of `statement`, written outside a query, in `scope`.
    fn statement(&mut self, statement: &Statement, scope: &ExpressionScope<'_, '_, ParserDB>) {
        let context = ExpressionContext::for_statement(statement, scope, self.database, self.case);
        self.record(context.ok(), statement);
    }

    /// Types the calls of `query`.
    fn query(&mut self, query: &Query) {
        let context = ExpressionContext::new(query, self.database, self.case);
        self.record(context.ok(), query);
    }

    /// Records what each call and operator node of `node`, typed by
    /// `context` as the pins have it, selects, and nothing without a
    /// context.
    fn record<'q, N: Visit + ?Sized>(
        &mut self,
        context: Option<ExpressionContext<'q, '_, ParserDB>>,
        node: &N,
    ) where
        's: 'q,
    {
        let Some(context) = context else { return };
        let context = match self.pins {
            Some(pins) => context.pinned(pins),
            None => context,
        };
        let mut nodes = Nodes {
            context: &context,
            database: self.database,
            case: self.case,
            selections: self.selections,
            deparsed: self.deparsed.as_deref_mut(),
        };
        let ControlFlow::Continue(()) = walk(node, &mut nodes);
    }
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

/// Records the function each call a context types selects and the operators
/// a statement added each operator node selects, or, reading what a deparse
/// pins, the pins.
struct Nodes<'c, 'q, 'db> {
    context: &'c ExpressionContext<'q, 'db, ParserDB>,
    database: &'db ParserDB,
    case: IdentifierCase,
    selections: &'c mut Selections,
    deparsed: Option<&'c mut Deparsed>,
}

impl Nodes<'_, '_, '_> {
    /// Records what the call of `name`, applied as `application`, chooses,
    /// or the refusal PostgreSQL gives it.
    fn select_call(
        &mut self,
        name: &ObjectName,
        application: Option<CallApplication<'_, '_, ParserDB>>,
    ) {
        let choice = match application.as_ref().map(CallApplication::resolution) {
            Some(CallResolution::Selected(selected)) => {
                Some(CallChoice::Function(address(selected.function())))
            }
            Some(CallResolution::Cast(_)) => Some(CallChoice::Cast),
            _ => None,
        };
        if let Some(choice) = choice {
            self.selections.calls.insert(address(name), choice);
        }
        let Some((called, refusal)) = application.and_then(CallApplication::into_refusal) else {
            return;
        };
        let refused = match refusal {
            CallRefusal::Unresolved((reason, argument_types)) => {
                Error::UnresolvedCall { function_name: called.to_string(), argument_types, reason }
            }
            CallRefusal::Misapplied(rule) => {
                Error::MisappliedCall { function_name: stored_name(&called), rule }
            }
        };
        self.selections.refused_calls.insert(address(name), refused);
    }

    /// Records the operators a statement added that the applications of
    /// `expression` select, and the refusal of the first one PostgreSQL
    /// refuses.
    fn select_operators(&mut self, expression: &Expr) {
        let Ok(Some(applications)) = self.context.operators(expression) else { return };
        let mut selected = Vec::new();
        for (position, application) in applications.into_iter().enumerate() {
            if let OperatorResolution::Selected(chosen) = application.resolution() {
                let row = chosen.operator().row;
                if matches!(row, OperatorRow::Added(_)) {
                    selected.push((position, row));
                }
            } else if let Some((name, (reason, operand_types))) = application.into_refusal() {
                self.selections.refused_operators.entry(address(expression)).or_insert_with(|| {
                    Error::UnresolvedOperator {
                        operator_name: name.name().into(),
                        operand_types,
                        reason,
                    }
                });
            }
        }
        if !selected.is_empty() {
            self.selections.operators.insert(address(expression), selected);
        }
    }
}

impl Visitor for Nodes<'_, '_, '_> {
    type Break = core::convert::Infallible;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if let Some(deparsed) = self.deparsed.as_deref_mut() {
            if applies_operators(expression) {
                deparsed.read(self.context, self.database, self.case, expression);
            }
            if let Expr::Function(function) = expression {
                deparsed.read_call(self.context, self.database, self.case, function, expression);
            }
            return ControlFlow::Continue(());
        }
        if let Expr::Function(function) = expression
            && let Ok(application) = self.context.call(expression)
        {
            self.select_call(&function.name, application);
        }
        if applies_operators(expression) {
            self.select_operators(expression);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        // No expression parsed again holds a `FROM` item.
        if self.deparsed.is_none()
            && let TableFactor::Table { name, .. } | TableFactor::Function { name, .. } = factor
            && let Ok(application) = self.context.relation_call(factor)
        {
            self.select_call(name, application);
        }
        ControlFlow::Continue(())
    }
}

/// Where each operator node and each call of an expression sits, both by
/// its position in a pre-order walk and by its address.
struct Positions {
    /// The address of each operator node, by its position.
    nodes: Vec<usize>,
    /// The position of each operator node, by its address.
    node_at: BTreeMap<usize, usize>,
    /// The address of each call's node, by the call's position, `None` for
    /// a call written without one.
    calls: Vec<Option<usize>>,
    /// The position of each call, by the address of its node.
    call_at: BTreeMap<usize, usize>,
}

/// What an expression parsed again selected before: the previous type of
/// each retyped column, the operators a statement added each rewritten
/// node's applications selected, the others selecting built-in ones, the
/// functions each call reached, and what the text an earlier type change
/// deparsed it into writes.
struct Recorded<'r> {
    retyped: &'r [(Ident, DataType)],
    /// The selections of each rewritten node, by its address.
    rewritten: BTreeMap<usize, &'r [(usize, OperatorRow)]>,
    /// The functions each call reached, by the address of its node.
    calls: BTreeMap<usize, &'r [Arc<CreateFunction>]>,
    /// What the expression's text writes, by position.
    written: &'r WrittenPins,
    /// Where the expression's operator nodes and calls sit.
    positions: Positions,
    /// The catalog the selected operators are looked up in.
    database: &'r ParserDB,
    case: IdentifierCase,
}

impl<'r> Recorded<'r> {
    /// What `calls`, the record of `node`, selected before, over `database`.
    fn of<N: Visit + ?Sized>(
        node: &N,
        calls: &'r ExpressionCalls<CreateFunction>,
        database: &'r ParserDB,
        case: IdentifierCase,
    ) -> Self {
        let mut rewritten = BTreeMap::new();
        let mut nodes = Vec::new();
        let mut held = calls.operator_nodes().iter();
        for_each_operator_node(node, |expression| {
            if let Some(held) = held.next()
                && held.state() == NodeState::Rewritten
            {
                rewritten.insert(address(expression), held.selected());
            }
            nodes.push(address(expression));
        });
        let mut reached = BTreeMap::new();
        let mut written_calls = Vec::new();
        let mut previous = calls.previous_calls().iter();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |call| {
            if let (Some(previous), Some(written)) = (previous.next(), call.node) {
                reached.insert(address(written), previous.as_slice());
            }
            written_calls.push(call.node.map(address));
            ControlFlow::Continue(())
        });
        let node_at = nodes.iter().enumerate().map(|(at, node)| (*node, at)).collect();
        let call_at = written_calls
            .iter()
            .enumerate()
            .filter_map(|(at, call)| call.map(|call| (call, at)))
            .collect();
        Self {
            retyped: calls.retyped(),
            rewritten,
            calls: reached,
            written: calls.written(),
            positions: Positions { nodes, node_at, calls: written_calls, call_at },
            database,
            case,
        }
    }

    /// The operator a statement added that the application at `site`
    /// selected before, if it selected one.
    fn recorded(&self, (node, position): Site) -> Option<OperatorRow> {
        let selected = self.rewritten.get(&node)?;
        selected.iter().find(|(held, _)| *held == position).map(|(_, row)| *row)
    }
}

impl ReparsePins<ParserDB> for Recorded<'_> {
    fn column_type(&self, column: &str, quoted: bool) -> Option<&DataType> {
        self.retyped
            .iter()
            .find(|(retyped, _)| {
                self.case.identifiers_match(
                    &retyped.value,
                    retyped.quote_style.is_some(),
                    column,
                    quoted,
                )
            })
            .map(|(_, previous)| previous)
    }

    fn admits(&self, site: Site, operator: &PostgresCatalogOperator) -> bool {
        !self.rewritten.contains_key(&site.0)
            || operator.row == self.recorded(site).unwrap_or(OperatorRow::BuiltIn)
    }

    fn operand(&self, (node, at): Site, left: bool) -> Option<&OperandType<'static>> {
        let node = self.positions.node_at.get(&node)?;
        self.written.operands.get(&(*node, at, left))
    }

    /// The schema the text writes, or else the schema the operator selected
    /// before lives in now, wherever the path in force leaves it, since the
    /// old selection is read in full.
    fn schema(&self, site: Site) -> Option<(&str, bool)> {
        let written = self.positions.node_at.get(&site.0).and_then(|node| {
            self.written
                .schemas
                .get(&(*node, site.1))
                .map(|(schema, quoted)| (schema.as_str(), *quoted))
        });
        if written.is_some() {
            return written;
        }
        let row = self.recorded(site)?;
        let operator = self.database.operators()?.find(|operator| operator.row == row)?;
        Some((operator.target.schema()?, operator.target.schema_is_quoted()))
    }

    fn admits_call(&self, call: usize, function: &CreateFunction) -> bool {
        self.calls
            .get(&call)
            .is_none_or(|reached| reached.iter().any(|held| core::ptr::eq(held.as_ref(), function)))
    }

    fn argument(&self, call: usize, index: usize) -> Option<&OperandType<'static>> {
        let call = self.positions.call_at.get(&call)?;
        self.written.arguments.get(&(*call, index))
    }

    /// The schema the text writes, or else the schema of the one function
    /// the call reached before, wherever the path in force leaves it, a
    /// function stored bare living in `public`.
    fn call_schema(&self, call: usize) -> Option<(&str, bool)> {
        let written = self.positions.call_at.get(&call).and_then(|at| {
            self.written.call_schemas.get(at).map(|(schema, quoted)| (schema.as_str(), *quoted))
        });
        if written.is_some() {
            return written;
        }
        let [function] = self.calls.get(&call)? else { return None };
        Some(qualifier_of(&function.name).named().unwrap_or((DEFAULT_SCHEMA, false)))
    }

    fn cast(&self, call: usize) -> Option<&TargetName<'static>> {
        let call = self.positions.call_at.get(&call)?;
        self.written.casts.get(call)
    }
}

/// What the text PostgreSQL deparses an expression into pins: the cast
/// around each operand and argument a selection converted, the cast written
/// for a call read as one, and the schema of an operator or a function its
/// name alone does not reach.
#[derive(Default)]
struct Deparsed {
    /// The type each converted operand is cast to, by its application and
    /// whether it is the left operand.
    operands: BTreeMap<(Site, bool), OperandType<'static>>,
    /// The schema each qualified operator name is written in.
    schemas: BTreeMap<Site, (String, bool)>,
    /// The type each converted argument is cast to, by the address of its
    /// call's node and its position.
    arguments: BTreeMap<(usize, usize), OperandType<'static>>,
    /// The schema each qualified called name is written in, by the address
    /// of the call's node.
    call_schemas: BTreeMap<usize, (String, bool)>,
    /// The type each call read as a cast is deparsed as a cast to, by the
    /// address of the call's node.
    casts: BTreeMap<usize, TargetName<'static>>,
}

impl Deparsed {
    /// The pins of the text an expression whose operator nodes and calls sit
    /// at `positions` was deparsed into before, which `written` holds, so a
    /// deparse of that text writes them again.
    fn of(written: &WrittenPins, positions: &Positions) -> Self {
        let node = |at: &usize| positions.nodes.get(*at).copied();
        let call = |at: &usize| positions.calls.get(*at).copied().flatten();
        Self {
            operands: written
                .operands
                .iter()
                .filter_map(|((at, application, left), pin)| {
                    Some((((node(at)?, *application), *left), pin.clone()))
                })
                .collect(),
            schemas: written
                .schemas
                .iter()
                .filter_map(|((at, application), pin)| {
                    Some(((node(at)?, *application), pin.clone()))
                })
                .collect(),
            arguments: written
                .arguments
                .iter()
                .filter_map(|((at, index), pin)| Some(((call(at)?, *index), pin.clone())))
                .collect(),
            call_schemas: written
                .call_schemas
                .iter()
                .filter_map(|(at, pin)| Some((call(at)?, pin.clone())))
                .collect(),
            casts: written
                .casts
                .iter()
                .filter_map(|(at, pin)| Some((call(at)?, pin.clone())))
                .collect(),
        }
    }

    /// What this deparse writes, by the positions `positions` gives the
    /// operator nodes and calls it was read from.
    fn into_written(self, positions: &Positions) -> WrittenPins {
        let node = |address: usize| positions.node_at.get(&address).copied();
        let call = |address: usize| positions.call_at.get(&address).copied();
        WrittenPins {
            operands: self
                .operands
                .into_iter()
                .filter_map(|(((at, application), left), pin)| {
                    Some(((node(at)?, application, left), pin))
                })
                .collect(),
            schemas: self
                .schemas
                .into_iter()
                .filter_map(|((at, application), pin)| Some(((node(at)?, application), pin)))
                .collect(),
            arguments: self
                .arguments
                .into_iter()
                .filter_map(|((at, index), pin)| Some(((call(at)?, index), pin)))
                .collect(),
            call_schemas: self
                .call_schemas
                .into_iter()
                .filter_map(|(at, pin)| Some((call(at)?, pin)))
                .collect(),
            casts: self.casts.into_iter().filter_map(|(at, pin)| Some((call(at)?, pin))).collect(),
        }
    }

    /// Reads what the deparse of the applications of `expression`, typed by
    /// `context` over its previous selections, pins.
    fn read(
        &mut self,
        context: &ExpressionContext<'_, '_, ParserDB>,
        database: &ParserDB,
        case: IdentifierCase,
        expression: &Expr,
    ) {
        let Ok(Some(applications)) = context.operators(expression) else { return };
        for (position, application) in applications.iter().enumerate() {
            let OperatorResolution::Selected(selected) = application.resolution() else {
                continue;
            };
            let site = (address(expression), position);
            let declared = |evidence: &OperandEvidence<'_, ParserDB>| {
                OperandType::Known {
                    type_name: evidence.target().clone().into_owned(),
                    typmod: None,
                }
            };
            // A built array is deparsed as `ARRAY[...]` of the type its
            // elements took, converted or not.
            let built = matches!(
                application.right(),
                Operand::Any(ArraySource::Built(_)) | Operand::All(ArraySource::Built(_))
            );
            for (left, evidence) in [(true, selected.left()), (false, Some(selected.right()))] {
                if let Some(evidence) = evidence
                    && (!evidence.conversion().is_empty() || (built && !left))
                {
                    self.operands.insert((site, left), declared(evidence));
                }
            }
            // `generate_operator_name` qualifies an operator its name and the
            // operand types it declares do not find on the path.
            let operator = selected.operator();
            let name = application.name();
            let found = database.resolve_operator(
                TargetName::new(name.name(), name.name_is_quoted()),
                selected.left().map(declared),
                declared(selected.right()),
                case,
            );
            let reached = matches!(
                found,
                OperatorResolution::Selected(found) if core::ptr::eq(found.operator(), operator)
            );
            let target = operator.target_name();
            if let (false, Some(schema)) = (reached, target.schema()) {
                self.schemas.insert(site, (String::from(schema), target.schema_is_quoted()));
            }
        }
    }

    /// Reads what the deparse of the call `function`, the node of
    /// `expression`, typed by `context` over its previous selection, pins.
    fn read_call(
        &mut self,
        context: &ExpressionContext<'_, '_, ParserDB>,
        database: &ParserDB,
        case: IdentifierCase,
        function: &Function,
        expression: &Expr,
    ) {
        let Ok(Some(call)) = context.call(expression) else { return };
        let node = address(function);
        if let CallResolution::Cast(target) = call.resolution() {
            self.casts.insert(node, target.clone().into_owned());
        }
        let CallResolution::Selected(selected) = call.resolution() else { return };
        for (position, argument) in selected.arguments().iter().enumerate() {
            let evidence = argument.evidence();
            if !evidence.conversion().is_empty() {
                let target = evidence.target().clone().into_owned();
                let pinned = OperandType::Known { type_name: target, typmod: None };
                self.arguments.insert((node, position), pinned);
            }
        }
        // `generate_function_name` qualifies a function its name does not
        // reach on the path.
        let Some(written) = written_argument_names(function) else { return };
        let bare = TargetName::new(call.name().name(), call.name().name_is_quoted());
        let selected = selected.function();
        let reached = reached_by_name(database, &bare, &written, case)
            .iter()
            .any(|(held, _)| core::ptr::eq(*held, selected));
        if !reached {
            let (schema, quoted) =
                qualifier_of(&selected.name).named().unwrap_or((DEFAULT_SCHEMA, false));
            self.call_schemas.insert(node, (String::from(schema), quoted));
        }
    }
}

impl ReparsePins<ParserDB> for Deparsed {
    fn column_type(&self, _column: &str, _quoted: bool) -> Option<&DataType> {
        None
    }

    fn admits(&self, _site: Site, _operator: &PostgresCatalogOperator) -> bool {
        true
    }

    fn operand(&self, site: Site, left: bool) -> Option<&OperandType<'static>> {
        self.operands.get(&(site, left))
    }

    fn schema(&self, site: Site) -> Option<(&str, bool)> {
        self.schemas.get(&site).map(|(schema, quoted)| (schema.as_str(), *quoted))
    }

    fn admits_call(&self, _call: usize, _function: &CreateFunction) -> bool {
        true
    }

    fn argument(&self, call: usize, index: usize) -> Option<&OperandType<'static>> {
        self.arguments.get(&(call, index))
    }

    fn call_schema(&self, call: usize) -> Option<(&str, bool)> {
        self.call_schemas.get(&call).map(|(schema, quoted)| (schema.as_str(), *quoted))
    }

    fn cast(&self, call: usize) -> Option<&TargetName<'static>> {
        self.casts.get(&call)
    }
}

/// The name PostgreSQL gives `name` in a message, each part as it stores
/// it.
fn stored_name(name: &TargetName<'_>) -> String {
    let function = stored_identifier(name.name(), name.name_is_quoted());
    match name.schema() {
        Some(schema) => {
            format!("{}.{function}", stored_identifier(schema, name.schema_is_quoted()))
        }
        None => function.into_owned(),
    }
}
