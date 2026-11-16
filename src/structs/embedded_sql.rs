//! One piece of SQL a function body runs.

use core::ops::ControlFlow;

use sqlparser::ast::{Query, Statement, Visit, Visitor};

use crate::utils::walk::walk;

/// One piece of SQL a function body runs, a statement or the `SELECT` a
/// PL/pgSQL body evaluates an expression with.
#[derive(Clone, Copy, Debug)]
pub enum EmbeddedSql<'s> {
    /// A statement.
    Statement(&'s Statement),
    /// The `SELECT` evaluating an expression, as PostgreSQL 18 compiles a
    /// PL/pgSQL condition, assigned value, loop bound or default.
    Expression(&'s Query),
}

impl Visit for EmbeddedSql<'_> {
    fn visit<V: Visitor>(&self, visitor: &mut V) -> ControlFlow<V::Break> {
        match self {
            Self::Statement(statement) => walk(*statement, visitor),
            Self::Expression(query) => walk(*query, visitor),
        }
    }
}

impl core::fmt::Display for EmbeddedSql<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Statement(statement) => statement.fmt(formatter),
            Self::Expression(query) => query.fmt(formatter),
        }
    }
}
