//! What a PL/pgSQL function's output parameters hold when it returns.

use alloc::{boxed::Box, string::String};

use sqlparser::ast::Expr;

/// One output parameter of a function and what it holds when the function
/// returns, from
/// [`FunctionLike::returned_values`](crate::traits::FunctionLike::returned_values).
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnedValue {
    name: String,
    output: ReturnedOutput,
}

impl ReturnedValue {
    pub(crate) const fn new(name: String, output: ReturnedOutput) -> Self {
        Self { name, output }
    }

    /// Returns the parameter's name as PostgreSQL stores it, or `$n` for the
    /// unnamed parameter at position `n`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns what the parameter holds when the function returns.
    #[must_use]
    pub const fn output(&self) -> &ReturnedOutput {
        &self.output
    }

    /// Returns the expression assigned to the parameter on every path to a
    /// return, `None` unless [`Self::output`] is
    /// [`ReturnedOutput::Expression`].
    #[must_use]
    pub fn expression(&self) -> Option<&Expr> {
        match &self.output {
            ReturnedOutput::Expression(expression) => Some(expression),
            ReturnedOutput::Null | ReturnedOutput::Several | ReturnedOutput::Unread => None,
        }
    }
}

/// What an output parameter holds when its function returns.
#[derive(Debug, Clone, PartialEq)]
pub enum ReturnedOutput {
    /// The expression of the one assignment reaching every return, which
    /// reads no variable of the body and may hold a scalar subquery.
    ///
    /// PL/pgSQL converts the value to the parameter's declared type as it
    /// assigns it, so `a := 1.7` to an `integer` parameter returns `2`. The
    /// expression is evaluated where it is assigned, under the function's
    /// own `search_path`, role and `SET` clause, so `current_user` in a
    /// `SECURITY DEFINER` function reads its owner. An assignment reading a
    /// setting the body itself may set is [`Self::Unread`] instead.
    Expression(Box<Expr>),
    /// The `NULL` the parameter starts with, which no assignment replaces on
    /// any path to a return.
    Null,
    /// What one of several assignments, or the `NULL` the parameter starts
    /// with, gives it, depending on the path taken.
    Several,
    /// What one write gives it other than an assignment of an expression
    /// reading no variable, such as an assignment reading a variable of the
    /// body or writing a part of the parameter, a `SELECT INTO` or
    /// `EXECUTE INTO` target, or the value an `INOUT` parameter is passed.
    Unread,
}
