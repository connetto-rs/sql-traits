//! Submodule defining a generic `PolicyMetadata` struct.

use alloc::{string::String, sync::Arc};

use sql_values::{DateTimeError, PgType};

use super::ExpressionCalls;
use crate::{
    errors::LookupError,
    traits::{DatabaseLike, PolicyLike},
};

#[derive(Debug, Clone)]
/// Struct collecting metadata about a policy.
pub struct PolicyMetadata<U: PolicyLike> {
    /// The calls of the using expression.
    using_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    /// The calls of the check expression.
    check_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    /// The first literal of the using expression its creation read a value
    /// for that is not known here.
    using_unreadable: Option<UnreadableLiteral>,
    /// The first literal of the check expression its creation read a value
    /// for that is not known here.
    check_unreadable: Option<UnreadableLiteral>,
}

/// A literal of a policy expression that PostgreSQL read as a date or time
/// value when it created the policy, and whose value is not known here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnreadableLiteral {
    /// The literal's text, as written.
    pub(crate) literal: String,
    /// The type PostgreSQL read the literal as.
    pub(crate) data_type: PgType,
    /// What reading the literal needed or refused.
    pub(crate) reason: DateTimeError,
}

impl UnreadableLiteral {
    /// The error answering for this literal of the policy named `policy`.
    pub(crate) fn error(&self, policy: &str) -> LookupError {
        LookupError::PolicyLiteralUnreadable {
            policy: policy.into(),
            literal: self.literal.clone(),
            data_type: self.data_type,
            reason: self.reason,
        }
    }
}

impl<U: PolicyLike> PolicyMetadata<U> {
    /// Creates a new `PolicyMetadata` instance.
    #[inline]
    #[must_use]
    pub fn new(
        using_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
        check_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    ) -> Self {
        Self { using_calls, check_calls, using_unreadable: None, check_unreadable: None }
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

    /// The literal of the using expression whose value is not known here.
    pub(crate) fn using_unreadable(&self) -> Option<&UnreadableLiteral> {
        self.using_unreadable.as_ref()
    }

    /// Records the literal of the current using expression whose value is
    /// not known here, if any.
    pub(crate) fn set_using_unreadable(&mut self, literal: Option<UnreadableLiteral>) {
        self.using_unreadable = literal;
    }

    /// The literal of the check expression whose value is not known here.
    pub(crate) fn check_unreadable(&self) -> Option<&UnreadableLiteral> {
        self.check_unreadable.as_ref()
    }

    /// Records the literal of the current check expression whose value is
    /// not known here, if any.
    pub(crate) fn set_check_unreadable(&mut self, literal: Option<UnreadableLiteral>) {
        self.check_unreadable = literal;
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
