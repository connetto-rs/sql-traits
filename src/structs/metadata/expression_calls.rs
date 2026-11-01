//! Submodule defining the functions each call of an expression may reach.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{Expr, Function};

use crate::utils::call_sites::for_each_call;

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
