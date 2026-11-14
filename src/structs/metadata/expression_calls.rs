//! Submodule defining the functions each call of an expression may reach.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{Expr, Function, ObjectName, Visit};

use crate::{
    structs::{IdentifierCase, TargetName},
    traits::FunctionLike,
    utils::{
        call_sites::{Call, Site, call_reaches, for_each_call, for_each_site},
        object_name::object_name_last_part,
    },
};

/// Hands each function an engine provides under a called name to the
/// callback, one at a time.
pub(crate) type Provided<'a, F> = &'a dyn Fn(&str, &mut dyn FnMut(&Arc<F>));

/// The functions each call of an expression or a bound function body
/// reaches, resolved under the search path of the statement that wrote it.
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
}

/// One call of an expression and the functions it reaches.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BoundCall<F> {
    /// The functions the call reaches.
    reached: Vec<Arc<F>>,
    /// The overloads the call's argument types choose among, for a call
    /// reaching more than one function, one of them declared by the schema,
    /// or a declared one by one argument, which PostgreSQL may read as a
    /// cast instead.
    overloads: Option<Vec<Arc<F>>>,
    /// Whether `reached` no longer waits for the argument types to choose
    /// among `overloads`.
    settled: bool,
}

/// What the argument types of a waiting call choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallChoice {
    /// The function whose node sits at this address runs.
    Function(usize),
    /// PostgreSQL reads the call as a cast, which runs no function.
    Cast,
}

impl<F> BoundCall<F> {
    /// A call passing `arguments` arguments reaching `reached`, the argument
    /// types left to choose among them when `declared` says one of them is
    /// a function the schema declares and more than one is reached or the
    /// call passes one argument.
    fn new(reached: Vec<Arc<F>>, declared: bool, arguments: usize) -> Self {
        let overloads =
            (declared && (reached.len() > 1 || arguments == 1)).then(|| reached.clone());
        Self { settled: overloads.is_none(), reached, overloads }
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
                .map(|reached| BoundCall { reached, overloads: None, settled: true })
                .collect(),
        )
    }

    /// The record holding `calls`.
    fn of_calls(calls: Vec<BoundCall<F>>) -> Self {
        let mut record = Self { calls, functions: Vec::new() };
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

    /// Forgets the calls at the positions `calls`, counted in the order a
    /// pre-order walk meets them, which a part removed from the node held.
    pub(crate) fn remove_calls(&mut self, calls: core::ops::Range<usize>) {
        self.calls.drain(calls);
        self.collect_functions();
    }

    /// Settles every call of `node`, the node this record was built from,
    /// that waits for its argument types, each looked up by its called name:
    /// one `selected` answers a function for keeps that function alone, one
    /// read as a cast reaches none, and any other keeps every overload.
    pub(crate) fn settle<N: Visit + ?Sized>(
        &mut self,
        node: &N,
        selected: impl Fn(&ObjectName) -> Option<CallChoice>,
    ) {
        let mut calls = self.calls.iter_mut();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |call| {
            let Some(bound) = calls.next() else { return ControlFlow::Continue(()) };
            if bound.settled {
                return ControlFlow::Continue(());
            }
            match selected(call.name) {
                // A waiting call still reaches every overload.
                Some(CallChoice::Function(address)) => {
                    bound.reached.retain(|function| Arc::as_ptr(function).addr() == address);
                }
                Some(CallChoice::Cast) => bound.reached.clear(),
                None => {}
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
        for slot in slots.chain(self.functions.iter_mut()) {
            if Arc::ptr_eq(slot, stale) {
                *slot = Arc::clone(fresh);
            }
        }
    }
}

impl<F> Default for ExpressionCalls<F> {
    fn default() -> Self {
        Self { calls: Vec::new(), functions: Vec::new() }
    }
}

impl<F> Clone for ExpressionCalls<F> {
    fn clone(&self) -> Self {
        Self { calls: self.calls.clone(), functions: self.functions.clone() }
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
                BoundCall::new(site, declared, call.arguments)
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
        Self::of_calls(calls)
    }
}
