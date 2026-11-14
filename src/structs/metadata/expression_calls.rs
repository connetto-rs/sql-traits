//! Submodule defining the functions each call of an expression may reach,
//! and the operators its applications select.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{DataType, Expr, Function, Ident, ObjectName, Visit};

use crate::{
    structs::{IdentifierCase, OperatorRow, TargetName, applies_operators},
    traits::FunctionLike,
    utils::{
        call_sites::{Call, Site, call_reaches, for_each_call, for_each_site},
        object_name::object_name_last_part,
        walk::walk_expressions,
    },
};

/// Hands each function an engine provides under a called name to the
/// callback, one at a time.
pub(crate) type Provided<'a, F> = &'a dyn Fn(&str, &mut dyn FnMut(&Arc<F>));

/// The functions each call of an expression or a bound function body
/// reaches, resolved under the search path of the statement that wrote it,
/// and the operators each of its operator applications selects.
///
/// A call reaches the overloads accepting its argument count. Where those
/// include a function the schema declares, a parsed script records the one
/// the argument types select, as PostgreSQL binds it, and keeps every
/// overload when the types do not decide one.
#[derive(Debug, PartialEq, Eq)]
pub struct ExpressionCalls<F> {
    /// Each call, in the order a pre-order walk of the expression meets the
    /// calls.
    calls: Vec<BoundCall<F>>,
    /// Every function some call reaches, each once, first reached first.
    functions: Vec<Arc<F>>,
    /// Each node of the expression applying operators, in the order a
    /// pre-order walk meets them.
    operators: Vec<OperatorNode>,
    /// Each column `ALTER COLUMN ... TYPE` retyped under a rewritten node,
    /// with the type it had.
    retyped: Vec<(Ident, DataType)>,
    /// The functions each call reached before a rebuild wrote the expression
    /// again, by the call's position in a pre-order walk, which a rewritten
    /// node's previous selections were typed with.
    previous_calls: Vec<Vec<Arc<F>>>,
}

/// What a rebuild carries of the operator nodes of a record to the
/// expression it writes again.
pub(crate) struct CarriedNodes<F> {
    /// The operator nodes, in pre-order.
    pub(crate) nodes: Vec<OperatorNode>,
    /// Each column retyped under the rewritten nodes, with the type it had.
    pub(crate) retyped: Vec<(Ident, DataType)>,
    /// The functions each call reached when the nodes were selected.
    pub(crate) calls: Vec<Vec<Arc<F>>>,
}

/// What one node applying operators selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OperatorNode {
    /// The operators a statement added that the node's applications select,
    /// by the application's position among the node's.
    selected: Vec<(usize, OperatorRow)>,
    /// Whether `selected` is settled.
    state: NodeState,
}

/// Whether a node applying operators, or a call, is typed as it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeState {
    /// The node holds what its applications selected, the call is typed.
    Settled,
    /// The node or the call is new and waits for its operand types.
    Pending,
    /// The node or the call is parsed again from the text PostgreSQL
    /// deparsed it to, a node's `selected` holding what it selected before.
    Rewritten,
}

impl OperatorNode {
    /// A node waiting for its operand types.
    pub(crate) const PENDING: Self = Self { selected: Vec::new(), state: NodeState::Pending };

    /// The operators a statement added that the node's applications select,
    /// by the application's position among the node's.
    pub(crate) fn selected(&self) -> &[(usize, OperatorRow)] {
        &self.selected
    }

    /// Whether the selections of the node are settled.
    pub(crate) const fn state(&self) -> NodeState {
        self.state
    }
}

/// Hands each node of `node` applying operators to `each`, in pre-order.
pub(crate) fn for_each_operator_node<N: Visit + ?Sized>(node: &N, mut each: impl FnMut(&Expr)) {
    let ControlFlow::<Infallible>::Continue(()) = walk_expressions(node, |expression| {
        if applies_operators(expression) {
            each(expression);
        }
        ControlFlow::Continue(())
    });
}

/// How many nodes of `node` apply operators, which is how many operator
/// nodes of the record of the node holding it `node` takes.
pub(crate) fn operator_node_count<N: Visit + ?Sized>(node: &N) -> usize {
    let mut count = 0;
    for_each_operator_node(node, |_| count += 1);
    count
}

/// One call of an expression and the functions it reaches.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BoundCall<F> {
    /// The functions the call reaches.
    reached: Vec<Arc<F>>,
    /// The overloads the call's argument types choose among, for a call
    /// reaching more than one function, one of them declared by the schema.
    overloads: Option<Vec<Arc<F>>>,
    /// Whether `reached` no longer waits for the argument types to choose
    /// among `overloads`.
    settled: bool,
    /// Whether the call is typed as it stands, or new, or parsed again.
    state: NodeState,
}

impl<F> BoundCall<F> {
    /// A new call reaching `reached`, the argument types left to choose
    /// among them when more than one is reached and `declared` says one of
    /// them is a function the schema declares.
    fn new(reached: Vec<Arc<F>>, declared: bool) -> Self {
        let overloads = (declared && reached.len() > 1).then(|| reached.clone());
        Self { settled: overloads.is_none(), reached, overloads, state: NodeState::Pending }
    }

    /// Returns the functions the call reaches.
    pub(crate) fn reached(&self) -> &[Arc<F>] {
        &self.reached
    }

    /// The same call written again, to be resolved anew in the schemas of the
    /// functions it could reach, the way PostgreSQL parses again the text
    /// it deparsed the call to.
    pub(crate) fn rewritten(self) -> Carried<F> {
        Carried::Rewritten(self.overloads.unwrap_or(self.reached))
    }
}

/// What a call carried over a rebuild keeps.
pub(crate) enum Carried<F> {
    /// The binding it held.
    Bound(BoundCall<F>),
    /// Nothing but the functions it could reach, the call being resolved anew
    /// among the functions now declared in their schemas.
    Rewritten(Vec<Arc<F>>),
}

impl<F> Clone for BoundCall<F> {
    fn clone(&self) -> Self {
        Self {
            reached: self.reached.clone(),
            overloads: self.overloads.clone(),
            settled: self.settled,
            state: self.state,
        }
    }
}

impl<F> ExpressionCalls<F> {
    /// Creates the record from the functions each call reaches, given in the
    /// order a pre-order walk of the expression meets the calls.
    #[must_use]
    pub fn new(sites: Vec<Vec<Arc<F>>>) -> Self {
        Self::of_calls(
            sites
                .into_iter()
                .map(|reached| {
                    BoundCall { reached, overloads: None, settled: true, state: NodeState::Settled }
                })
                .collect(),
        )
    }

    /// The record holding `calls`.
    fn of_calls(calls: Vec<BoundCall<F>>) -> Self {
        let mut record = Self {
            calls,
            functions: Vec::new(),
            operators: Vec::new(),
            retyped: Vec::new(),
            previous_calls: Vec::new(),
        };
        record.collect_functions();
        record
    }

    /// Lists every function some call reaches, each once.
    fn collect_functions(&mut self) {
        let mut seen = BTreeSet::new();
        self.functions = self
            .calls
            .iter()
            .flat_map(BoundCall::reached)
            .filter(|function| seen.insert(Arc::as_ptr(function)))
            .map(Arc::clone)
            .collect();
    }

    /// Returns every function some call reaches, each once.
    pub fn functions(&self) -> impl Iterator<Item = &F> {
        self.functions.iter().map(AsRef::as_ref)
    }

    /// Returns the functions each call reaches, in the order a pre-order walk
    /// of the expression meets the calls.
    pub(crate) fn sites(&self) -> impl Iterator<Item = &[Arc<F>]> {
        self.calls.iter().map(BoundCall::reached)
    }

    /// Returns each call, in the order a pre-order walk of the expression
    /// meets the calls.
    pub(crate) fn bound_calls(&self) -> &[BoundCall<F>] {
        &self.calls
    }

    /// Whether some call still waits for its argument types to choose among
    /// its overloads.
    pub(crate) fn is_unsettled(&self) -> bool {
        self.calls.iter().any(|call| !call.settled)
    }

    /// Forgets the calls at the positions `calls` and the operator nodes at
    /// the positions `operators`, counted in the order a pre-order walk meets
    /// them, which a part removed from the node held.
    pub(crate) fn remove_part(
        &mut self,
        calls: core::ops::Range<usize>,
        operators: core::ops::Range<usize>,
    ) {
        self.calls.drain(calls);
        self.collect_functions();
        let end = operators.end.min(self.operators.len());
        self.operators.drain(operators.start.min(end)..end);
    }

    /// Returns each node applying operators, in the order a pre-order walk
    /// meets them.
    pub(crate) fn operator_nodes(&self) -> &[OperatorNode] {
        &self.operators
    }

    /// Returns each column a rewritten node reads retyped, with the type it
    /// had.
    pub(crate) fn retyped(&self) -> &[(Ident, DataType)] {
        &self.retyped
    }

    /// Returns the functions each call reached when the rewritten nodes
    /// selected, by the call's position in a pre-order walk.
    pub(crate) fn previous_calls(&self) -> &[Vec<Arc<F>>] {
        &self.previous_calls
    }

    /// Whether some node applying operators waits for its operand types.
    pub(crate) fn has_pending_operators(&self) -> bool {
        self.operators.iter().any(|node| node.state != NodeState::Settled)
    }

    /// Whether some call is new or parsed again, and waits to be typed.
    pub(crate) fn has_untyped_calls(&self) -> bool {
        self.calls.iter().any(|call| call.state != NodeState::Settled)
    }

    /// Each call's state, in the order a pre-order walk meets the calls.
    pub(crate) fn call_states(&self) -> impl Iterator<Item = NodeState> {
        self.calls.iter().map(|call| call.state)
    }

    /// Gives the calls at the positions `calls`, counted in the order a
    /// pre-order walk meets them, the state `state`.
    pub(crate) fn set_call_states(&mut self, calls: core::ops::Range<usize>, state: NodeState) {
        let end = calls.end.min(self.calls.len());
        for call in &mut self.calls[calls.start.min(end)..end] {
            call.state = state;
        }
    }

    /// Whether some node applying operators or some call is parsed again.
    pub(crate) fn is_rewritten(&self) -> bool {
        let nodes = self.operators.iter().map(|node| node.state);
        nodes.chain(self.call_states()).any(|state| state == NodeState::Rewritten)
    }

    /// Whether an application of the operator nodes at the positions `nodes`
    /// selects one of `rows`.
    pub(crate) fn applies_in(&self, nodes: core::ops::Range<usize>, rows: &[OperatorRow]) -> bool {
        let end = nodes.end.min(self.operators.len());
        self.operators[nodes.start.min(end)..end]
            .iter()
            .flat_map(OperatorNode::selected)
            .any(|(_, row)| rows.contains(row))
    }

    /// Whether an application of the expression selects one of `rows`.
    pub(crate) fn applies(&self, rows: &[OperatorRow]) -> bool {
        self.applies_in(0..self.operators.len(), rows)
    }

    /// Settles every node of `node`, the node this record was built from,
    /// that waits for its operand types, each keeping what `selected` takes
    /// for the node's address.
    pub(crate) fn settle_operators<N: Visit + ?Sized>(
        &mut self,
        node: &N,
        mut selected: impl FnMut(usize) -> Vec<(usize, OperatorRow)>,
    ) {
        let mut held = self.operators.iter_mut();
        for_each_operator_node(node, |expression| {
            if let Some(held) = held.next()
                && held.state != NodeState::Settled
            {
                held.selected = selected(core::ptr::from_ref(expression).addr());
                held.state = NodeState::Settled;
            }
        });
        self.settle_remaining_operators();
    }

    /// Settles every waiting node as selecting no operator a statement
    /// added, the catalog holding none, and every call as typed.
    pub(crate) fn settle_remaining_operators(&mut self) {
        for node in &mut self.operators {
            if node.state != NodeState::Settled {
                node.state = NodeState::Settled;
                node.selected.clear();
            }
        }
        for call in &mut self.calls {
            call.state = NodeState::Settled;
        }
        self.retyped.clear();
        self.previous_calls.clear();
    }

    /// Settles the record as [`settle_remaining_operators`] does, the catalog
    /// holding no operator a statement added, unless a call is parsed again
    /// and waits to be typed with what the deparse pins.
    ///
    /// [`settle_remaining_operators`]: Self::settle_remaining_operators
    pub(crate) fn settle_on_built_ins(&mut self) {
        if self.call_states().all(|state| state != NodeState::Rewritten) {
            self.settle_remaining_operators();
        }
    }

    /// Hands the operator nodes over, as a rebuild carries them to the
    /// expression it writes again, leaving this record without any, with
    /// the functions each call reached when they were selected.
    pub(crate) fn take_operator_nodes(&mut self) -> CarriedNodes<F> {
        let calls = if self.previous_calls.is_empty() {
            self.sites().map(<[Arc<F>]>::to_vec).collect()
        } else {
            core::mem::take(&mut self.previous_calls)
        };
        CarriedNodes {
            nodes: core::mem::take(&mut self.operators),
            retyped: core::mem::take(&mut self.retyped),
            calls,
        }
    }

    /// Takes `carried` as the operator nodes of the expression, rebuilt from
    /// the one they were recorded for, which holds as many nodes.
    pub(crate) fn carry_operator_nodes(&mut self, carried: CarriedNodes<F>) {
        self.operators = carried.nodes;
        self.retyped = carried.retyped;
        self.previous_calls = carried.calls;
    }

    /// Forgets the functions the calls reached before the rebuild unless a
    /// node or a call is parsed again, the only reader of them.
    pub(crate) fn keep_previous_calls_for_rewritten(&mut self) {
        if !self.is_rewritten() {
            self.previous_calls.clear();
        }
    }

    /// Has every settled node and call parsed again from its deparsed text,
    /// the column `column` having been of type `previous`. The earliest type
    /// a column had is the one read, so a later retype only adds an entry.
    pub(crate) fn rewrite_operators(&mut self, column: &Ident, previous: &DataType) {
        let nodes = self.operators.iter_mut().map(|node| &mut node.state);
        for state in nodes.chain(self.calls.iter_mut().map(|call| &mut call.state)) {
            if *state == NodeState::Settled {
                *state = NodeState::Rewritten;
            }
        }
        self.retyped.push((column.clone(), previous.clone()));
    }

    /// Settles every call of `node`, the node this record was built from,
    /// that waits for its argument types: one `selected` answers the address
    /// of the function it runs for keeps that function alone, and any other
    /// keeps every overload.
    pub(crate) fn settle<N: Visit + ?Sized>(
        &mut self,
        node: &N,
        selected: impl Fn(&Function) -> Option<usize>,
    ) {
        let mut calls = self.calls.iter_mut();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |call| {
            let Some(bound) = calls.next() else { return ControlFlow::Continue(()) };
            if bound.settled {
                return ControlFlow::Continue(());
            }
            let chosen = call.node.and_then(&selected).and_then(|address| {
                bound
                    .overloads
                    .iter()
                    .flatten()
                    .find(|function| Arc::as_ptr(function).addr() == address)
            });
            if let Some(function) = chosen {
                bound.reached = alloc::vec![Arc::clone(function)];
            }
            ControlFlow::Continue(())
        });
        for bound in &mut self.calls {
            bound.settled = true;
        }
        self.collect_functions();
    }

    /// Returns the functions `call` reaches, or `None` when `call` is not a
    /// node of `expression`, the expression this record was built from.
    ///
    /// `call` is matched by address, so it has to be borrowed from
    /// `expression` itself rather than from a copy.
    pub fn reached_by<'s>(
        &'s self,
        expression: &Expr,
        call: &Function,
    ) -> Option<impl Iterator<Item = &'s F> + use<'s, F>> {
        let mut position = 0;
        let found = for_each_site(expression, |site| {
            match site {
                Site::Call(Call { node: Some(node), .. }) if core::ptr::eq(node, call) => {
                    ControlFlow::Break(Some(position))
                }
                Site::ExpressionForm(node) if core::ptr::eq(node, call) => ControlFlow::Break(None),
                Site::Call(_) => {
                    position += 1;
                    ControlFlow::Continue(())
                }
                Site::ExpressionForm(_) => ControlFlow::Continue(()),
            }
        });
        let ControlFlow::Break(position) = found else {
            return None;
        };
        let reached: &[Arc<F>] = match position {
            Some(position) => self.calls.get(position)?.reached(),
            None => &[],
        };
        Some(reached.iter().map(AsRef::as_ref))
    }

    /// Re-points every function referencing the allocation `stale` at `fresh`.
    pub fn replace_function(&mut self, stale: &Arc<F>, fresh: &Arc<F>) {
        let slots = self
            .calls
            .iter_mut()
            .flat_map(|call| call.reached.iter_mut().chain(call.overloads.iter_mut().flatten()));
        for slot in
            slots.chain(self.functions.iter_mut()).chain(self.previous_calls.iter_mut().flatten())
        {
            if Arc::ptr_eq(slot, stale) {
                *slot = Arc::clone(fresh);
            }
        }
    }
}

impl<F> Default for ExpressionCalls<F> {
    fn default() -> Self {
        Self::of_calls(Vec::new())
    }
}

impl<F> Clone for ExpressionCalls<F> {
    fn clone(&self) -> Self {
        Self {
            calls: self.calls.clone(),
            functions: self.functions.clone(),
            operators: self.operators.clone(),
            retyped: self.retyped.clone(),
            previous_calls: self.previous_calls.clone(),
        }
    }
}

impl<F: FunctionLike> ExpressionCalls<F> {
    /// Resolves every call in `node`, subqueries included, to the functions
    /// among `functions` it may reach, in the order a pre-order walk meets the
    /// calls.
    ///
    /// An unqualified call walks `search_path`, the path in force where the
    /// call is written, as
    /// [`PolicyLike::functions_reached_by`](crate::traits::PolicyLike::functions_reached_by)
    /// describes, and every overload accepting the call's argument count
    /// counts. `case` decides how schema names compare. A parsed script
    /// records the calls of its checks, policies and bound bodies this way,
    /// and then keeps the overload the argument types select.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use std::sync::Arc;
    ///
    /// use sql_traits::{prelude::*, structs::metadata::ExpressionCalls};
    /// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE FUNCTION app.allowed(id INT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';",
    /// )?;
    /// let functions: Vec<_> = db.functions().cloned().map(Arc::new).collect();
    /// let expression = Parser::new(&PostgreSqlDialect {})
    ///     .try_with_sql("app.allowed(id) AND allowed(id)")
    ///     .and_then(|mut parser| parser.parse_expr())
    ///     .expect("the expression parses");
    /// let calls = ExpressionCalls::resolve(
    ///     &expression,
    ///     &functions,
    ///     &[("public", false)],
    ///     IdentifierCase::AsWritten,
    /// );
    /// let reached: Vec<_> =
    ///     calls.functions().map(|function| function.target_name().to_string()).collect();
    /// // `app` is not on the path the bare call is written under.
    /// assert_eq!(reached, ["app.allowed"]);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn resolve<N: Visit + ?Sized>(
        node: &N,
        functions: &[Arc<F>],
        search_path: &[(&str, bool)],
        case: IdentifierCase,
    ) -> Self {
        Self::resolve_carrying(node, functions, &|_, _| {}, search_path, case, |_, _| None)
    }

    /// Resolves every call in `node` as [`resolve`](Self::resolve) does,
    /// among `functions` and the functions `provided` hands over for a called
    /// name, those the engine provides whose name folds as it does, except a
    /// call `carried` answers a binding for, which keeps it.
    ///
    /// `provided` is a trait object, since this walk is instantiated for every
    /// node type a visitor meets and each generic parameter would lengthen
    /// every one of those symbols.
    pub(crate) fn resolve_carrying<N: Visit + ?Sized>(
        node: &N,
        functions: &[Arc<F>],
        provided: Provided<'_, F>,
        search_path: &[(&str, bool)],
        case: IdentifierCase,
        mut carried: impl FnMut(&ObjectName, usize) -> Option<Carried<F>>,
    ) -> Self {
        let mut calls = Vec::new();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |call| {
            let resolve = |path: &[(&str, bool)]| {
                let path = || path.iter().copied();
                let reaches = |function: &Arc<F>| {
                    call_reaches(function.as_ref(), call.name, call.arguments, path, case)
                };
                let mut site: Vec<Arc<F>> =
                    functions.iter().filter(|function| reaches(function)).cloned().collect();
                let declared = !site.is_empty();
                let named = object_name_last_part(call.name).map_or("", |(name, _)| name);
                provided(named, &mut |function| {
                    if reaches(function) {
                        site.push(Arc::clone(function));
                    }
                });
                BoundCall::new(site, declared)
            };
            let bound = match carried(call.name, call.arguments) {
                Some(Carried::Bound(bound)) => bound,
                Some(Carried::Rewritten(previous)) => {
                    let schemas: Vec<TargetName<'_>> =
                        previous.iter().map(|function| function.target_name()).collect();
                    let path: Vec<(&str, bool)> = schemas
                        .iter()
                        .map(|name| {
                            name.schema().map_or(("public", false), |schema| {
                                (schema, name.schema_is_quoted())
                            })
                        })
                        .collect();
                    resolve(&path)
                }
                None => resolve(search_path),
            };
            calls.push(bound);
            ControlFlow::Continue(())
        });
        let mut record = Self::of_calls(calls);
        record.operators = alloc::vec![OperatorNode::PENDING; operator_node_count(node)];
        record
    }
}
