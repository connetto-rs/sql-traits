//! What an expression written outside a query sees, which decides how
//! [`ExpressionContext::for_expression`](crate::structs::ExpressionContext::for_expression)
//! and [`ExpressionContext::for_statement`](crate::structs::ExpressionContext::for_statement)
//! bind its column references.

use sqlparser::ast::{CreateRuleEvent, DataType};

use crate::traits::DatabaseLike;

/// The names an expression outside a query reaches besides the relations of
/// the queries and statements it holds, as PostgreSQL 18 resolves them for
/// the object the expression belongs to.
///
/// A reference no scope reaches, or one reaching an unreachable `NEW` or
/// `OLD`, takes no type.
pub enum ExpressionScope<'s, 'db, DB: DatabaseLike> {
    /// No relation, as a column default sees.
    Empty,
    /// One table, its columns bare or qualified by its name or by its schema
    /// and name, as a check constraint, a generated column, an index
    /// expression or predicate, a policy and a partition key see.
    Table(&'db DB::Table),
    /// The `NEW` and `OLD` rows of a trigger's table, as its `WHEN`
    /// condition sees them, a bare column ambiguous between the two.
    /// PostgreSQL refuses `OLD` in the condition of an `INSERT` trigger and
    /// `NEW` in that of a `DELETE` one when the trigger is created, so the
    /// events do not change the scope.
    Trigger(&'db DB::Table),
    /// The `new` and `old` rows of a rule's table, as its condition and its
    /// actions see them: `new` alone on `INSERT`, `old` alone on `DELETE` and
    /// `SELECT`, and both on `UPDATE`, where a bare column is ambiguous. An
    /// action's own relations, its target included, come first.
    Rule {
        /// The table the rule is defined on.
        table: &'db DB::Table,
        /// The event the rule fires on.
        event: CreateRuleEvent,
    },
    /// The `VALUE` a domain constraint checks, of the domain's base type.
    DomainValue(&'s DataType),
    /// The input parameters of a function, by name, by `$n` and qualified by
    /// the function's name, as its `RETURN` or `BEGIN ATOMIC` body sees them.
    /// Every relation of the body comes first, so a column of the same name
    /// wins over a parameter.
    FunctionBody(&'db DB::Function),
}
