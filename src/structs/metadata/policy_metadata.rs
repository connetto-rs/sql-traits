//! Submodule defining a generic `PolicyMetadata` struct.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::ops::ControlFlow;

use sql_values::{DateTimeError, PgType};
use sqlparser::ast::{CastKind, DataType, Expr, Value, VisitorMut};

use super::ExpressionCalls;
use crate::{
    errors::LookupError,
    traits::{DatabaseLike, PolicyLike},
    utils::walk::walk_mut,
};

#[derive(Debug, Clone)]
/// Struct collecting metadata about a policy.
pub struct PolicyMetadata<U: PolicyLike> {
    /// The calls of the using expression.
    using_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    /// The calls of the check expression.
    check_calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    /// How the literals of the using expression were read.
    using_literals: ClauseLiterals,
    /// How the literals of the check expression were read.
    check_literals: ClauseLiterals,
}

/// The calls of one clause of the policy `U`.
type PolicyCalls<U> = ExpressionCalls<<<U as PolicyLike>::DB as DatabaseLike>::Function>;

/// How the literals of one clause of a policy were read when PostgreSQL
/// created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClauseLiterals {
    /// No literal holds a value other than the one written, so the clause
    /// reads as written.
    AsWritten,
    /// Literals hold the value PostgreSQL stored, and this is the clause as
    /// the input spells it.
    Stored(Box<Expr>),
    /// A literal holds a value not known here, so the clause stays as written.
    Unreadable(UnreadableLiteral),
}

impl ClauseLiterals {
    /// The clause as the input spells it, where its stored literals differ.
    fn written_mut(&mut self) -> Option<&mut Expr> {
        match self {
            Self::Stored(written) => Some(written),
            Self::AsWritten | Self::Unreadable(_) => None,
        }
    }
}

/// A literal of a check or a policy expression that PostgreSQL read as a date
/// or time value when it created the expression, and whose value is not
/// known here.
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

    /// The error answering for this literal of the check named `check` on the
    /// table named `table`.
    pub(crate) fn check_error(&self, table: &str, check: Option<String>) -> LookupError {
        LookupError::CheckLiteralUnreadable {
            table: table.into(),
            check,
            literal: self.literal.clone(),
            data_type: self.data_type,
            reason: self.reason,
        }
    }
}

/// A literal PostgreSQL read as a date or time value when it created the
/// expression holding it, with the value it stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundLiteral {
    /// The literal's place among the nodes of its expression, counted in the
    /// order a pre-order walk meets them.
    pub(crate) place: usize,
    /// The type an untyped literal is cast to, or `None` for a literal a cast
    /// or a typed string types, which takes the stored text in place.
    pub(crate) cast: Option<DataType>,
    /// The text PostgreSQL stored.
    pub(crate) stored: String,
}

/// Writes the value of each of `literals` into its place in `expression`.
pub(crate) fn respell(expression: &mut Expr, literals: &[BoundLiteral]) {
    let walked = walk_mut(expression, &mut Respell { literals, met: 0, places: Vec::new() });
    debug_assert!(walked.is_continue(), "the respelling never breaks");
}

/// Writes stored literals into the places it counts while it walks.
struct Respell<'a> {
    literals: &'a [BoundLiteral],
    /// How many nodes the walk met.
    met: usize,
    /// The place of each node the walk stands in, innermost last.
    places: Vec<usize>,
}

impl VisitorMut for Respell<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, _expr: &mut Expr) -> ControlFlow<Self::Break> {
        self.places.push(self.met);
        self.met += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        let place = self.places.pop();
        let Some(literal) = self.literals.iter().find(|literal| Some(literal.place) == place)
        else {
            return ControlFlow::Continue(());
        };
        let stored = Value::SingleQuotedString(literal.stored.clone());
        match (&literal.cast, expr) {
            (Some(data_type), expr) => {
                *expr = Expr::Cast {
                    kind: CastKind::DoubleColon,
                    expr: Box::new(Expr::Value(stored.with_empty_span())),
                    data_type: data_type.clone(),
                    format: None,
                };
            }
            (None, Expr::TypedString(typed)) => typed.value.value = stored,
            // The literal a cast holds.
            (None, expr) => *expr = Expr::Value(stored.with_empty_span()),
        }
        ControlFlow::Continue(())
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
        Self {
            using_calls,
            check_calls,
            using_literals: ClauseLiterals::AsWritten,
            check_literals: ClauseLiterals::AsWritten,
        }
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

    /// Returns the calls of the using and the check expressions, to settle
    /// them.
    pub(crate) fn calls_mut(
        &mut self,
    ) -> [&mut ExpressionCalls<<U::DB as DatabaseLike>::Function>; 2] {
        [&mut self.using_calls, &mut self.check_calls]
    }

    /// How the literals of the using expression were read.
    pub(crate) fn using_literals(&self) -> &ClauseLiterals {
        &self.using_literals
    }

    /// Records how the literals of the current using expression were read.
    pub(crate) fn set_using_literals(&mut self, literals: ClauseLiterals) {
        self.using_literals = literals;
    }

    /// How the literals of the check expression were read.
    pub(crate) fn check_literals(&self) -> &ClauseLiterals {
        &self.check_literals
    }

    /// Records how the literals of the current check expression were read.
    pub(crate) fn set_check_literals(&mut self, literals: ClauseLiterals) {
        self.check_literals = literals;
    }

    /// The using and check expressions as the input spells them, where their
    /// stored literals differ, each with the calls it holds.
    pub(crate) fn written_clauses_mut(&mut self) -> [(Option<&mut Expr>, &PolicyCalls<U>); 2] {
        [
            (self.using_literals.written_mut(), &self.using_calls),
            (self.check_literals.written_mut(), &self.check_calls),
        ]
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
