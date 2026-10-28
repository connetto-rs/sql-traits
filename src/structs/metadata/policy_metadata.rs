//! Submodule defining a generic `PolicyMetadata` struct.

use alloc::sync::Arc;

use super::ExpressionCalls;
use crate::traits::{DatabaseLike, PolicyLike};

#[derive(Debug, Clone)]
/// Struct collecting metadata about a policy.
pub struct PolicyMetadata<U: PolicyLike> {
    /// The calls of the using expression.
    using_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    /// The calls of the check expression.
    check_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
}

impl<U: PolicyLike> PolicyMetadata<U> {
    /// Creates a new `PolicyMetadata` instance.
    #[inline]
    #[must_use]
    pub fn new(
        using_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
        check_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    ) -> Self {
        Self { using_calls, check_calls }
    }

    /// Returns the calls of the using expression.
    #[must_use]
    #[inline]
    pub fn using_calls(&self) -> &ExpressionCalls<<U::DB as DatabaseLike>::Function> {
        &self.using_calls
    }

    /// Replaces the calls of the using expression.
    ///
    /// # Arguments
    ///
    /// * `calls` - The calls of the current using expression.
    #[inline]
    pub fn set_using_calls(&mut self, calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>) {
        self.using_calls = calls;
    }

    /// Returns the calls of the check expression.
    #[must_use]
    #[inline]
    pub fn check_calls(&self) -> &ExpressionCalls<<U::DB as DatabaseLike>::Function> {
        &self.check_calls
    }

    /// Replaces the calls of the check expression.
    ///
    /// # Arguments
    ///
    /// * `calls` - The calls of the current check expression.
    #[inline]
    pub fn set_check_calls(&mut self, calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>) {
        self.check_calls = calls;
    }

    /// Re-points every cached function still referencing the allocation
    /// `stale` names at `fresh` instead.
    ///
    /// `ALTER FUNCTION` and `CREATE OR REPLACE` both rewrite the
    /// canonical store, so a cache resolved at policy creation would
    /// otherwise keep answering with the superseded node.
    #[inline]
    pub fn replace_function(
        &mut self,
        stale: &Arc<<U::DB as DatabaseLike>::Function>,
        fresh: &Arc<<U::DB as DatabaseLike>::Function>,
    ) {
        self.using_calls.replace_function(stale, fresh);
        self.check_calls.replace_function(stale, fresh);
    }
}
