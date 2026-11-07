//! A clause of a policy, as far as its literals could be read.

use sqlparser::ast::Expr;

use crate::errors::LookupError;

/// A clause of a policy, `USING` or `WITH CHECK`, as far as its literals
/// could be read.
///
/// PostgreSQL stores each literal of a clause as the value it read when it
/// created the clause. A clause whose every literal could be read here is
/// [`Self::Stored`]. One holding a literal that needed session state the input
/// never settles, such as a `timestamptz` literal before any `TimeZone`, is
/// [`Self::Written`]. Either form answers the relations, subqueries and calls
/// the clause holds through [`Self::expression`], and only [`Self::stored`]
/// answers the values it compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyExpression<'db> {
    /// The clause with every literal holding the value PostgreSQL stored.
    Stored(&'db Expr),
    /// The clause as the input spells it, every literal unbound, since one of
    /// them holds a value not known here.
    Written {
        /// The clause as written.
        expression: &'db Expr,
        /// The [`LookupError::PolicyLiteralUnreadable`] naming the first
        /// literal whose value is not known.
        unreadable: LookupError,
    },
}

impl<'db> PolicyExpression<'db> {
    /// The clause for a walk of the relations, subqueries and calls it holds,
    /// stored or written alike.
    #[must_use]
    pub fn expression(&self) -> &'db Expr {
        match self {
            Self::Stored(expression) | Self::Written { expression, .. } => expression,
        }
    }

    /// The clause with every literal holding the value PostgreSQL stored.
    ///
    /// # Errors
    ///
    /// Returns the [`LookupError::PolicyLiteralUnreadable`] of a
    /// [`Self::Written`] clause.
    pub fn stored(self) -> Result<&'db Expr, LookupError> {
        match self {
            Self::Stored(expression) => Ok(expression),
            Self::Written { unreadable, .. } => Err(unreadable),
        }
    }
}
