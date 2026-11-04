//! Submodule defining the functions each call of an expression may reach.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{Expr, Function, ObjectName, Visit};

use crate::{
    structs::IdentifierCase,
    traits::FunctionLike,
    utils::call_sites::{call_reaches, for_each_call},
};

/// The functions each call of an expression or a bound function body may
/// reach, resolved under the search path of the statement that wrote it.
///
/// A call reaches every overload accepting its argument count, since argument
/// types are not inferred.
#[derive(Debug, PartialEq, Eq)]
pub struct ExpressionCalls<F> {
    /// The functions each call may reach, in the order a pre-order walk of
    /// the expression meets the calls.
    sites: Vec<Vec<Arc<F>>>,
    /// Every function some call may reach, each once, first reached first.
    functions: Vec<Arc<F>>,
}

impl<F> ExpressionCalls<F> {
    /// Creates the record from the functions each call may reach, given in
    /// the order a pre-order walk of the expression meets the calls.
    #[must_use]
    pub fn new(sites: Vec<Vec<Arc<F>>>) -> Self {
        let mut seen = BTreeSet::new();
        let functions = sites
            .iter()
            .flatten()
            .filter(|function| seen.insert(Arc::as_ptr(function)))
            .map(Arc::clone)
            .collect();
        Self { sites, functions }
    }

    /// Returns every function some call may reach, each once.
    pub fn functions(&self) -> impl Iterator<Item = &F> {
        self.functions.iter().map(AsRef::as_ref)
    }

    /// Returns the functions each call may reach, in the order a pre-order
    /// walk of the expression meets the calls.
    pub(crate) fn sites(&self) -> &[Vec<Arc<F>>] {
        &self.sites
    }

    /// Returns the functions `call` may reach, or `None` when `call` is not a
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
        let found = for_each_call(expression, |site| {
            if site.node.is_some_and(|node| core::ptr::eq(node, call)) {
                return ControlFlow::Break(position);
            }
            position += 1;
            ControlFlow::Continue(())
        });
        let ControlFlow::Break(position) = found else {
            return None;
        };
        Some(self.sites.get(position)?.iter().map(AsRef::as_ref))
    }

    /// Re-points every function referencing the allocation `stale` at `fresh`.
    pub fn replace_function(&mut self, stale: &Arc<F>, fresh: &Arc<F>) {
        for slot in self.sites.iter_mut().flatten().chain(self.functions.iter_mut()) {
            if Arc::ptr_eq(slot, stale) {
                *slot = Arc::clone(fresh);
            }
        }
    }
}

impl<F> Default for ExpressionCalls<F> {
    fn default() -> Self {
        Self { sites: Vec::new(), functions: Vec::new() }
    }
}

impl<F> Clone for ExpressionCalls<F> {
    fn clone(&self) -> Self {
        Self { sites: self.sites.clone(), functions: self.functions.clone() }
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
    /// records the calls of its checks, policies and bound bodies this way.
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
        Self::resolve_carrying(node, functions, search_path, case, |_, _| None)
    }

    /// Resolves every call in `node` as [`resolve`](Self::resolve) does,
    /// except a call `carried` answers a binding for, which keeps it.
    pub(crate) fn resolve_carrying<N: Visit + ?Sized>(
        node: &N,
        functions: &[Arc<F>],
        search_path: &[(&str, bool)],
        case: IdentifierCase,
        mut carried: impl FnMut(&ObjectName, usize) -> Option<Vec<Arc<F>>>,
    ) -> Self {
        let path = || search_path.iter().copied();
        let mut sites = Vec::new();
        let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |call| {
            let site = carried(call.name, call.arguments).unwrap_or_else(|| {
                functions
                    .iter()
                    .filter(|function| {
                        call_reaches(function.as_ref(), call.name, call.arguments, path, case)
                    })
                    .cloned()
                    .collect()
            });
            sites.push(site);
            ControlFlow::Continue(())
        });
        Self::new(sites)
    }
}
