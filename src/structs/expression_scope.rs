//! What an expression written outside a query sees, which decides how
//! [`ExpressionContext::for_expression`](crate::structs::ExpressionContext::for_expression)
//! and [`ExpressionContext::for_statement`](crate::structs::ExpressionContext::for_statement)
//! bind its column references.

use sqlparser::ast::{CreateRuleEvent, DataType};

use crate::{structs::PlPgSqlScope, traits::DatabaseLike};

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
    /// The variables a PL/pgSQL body reaches where it evaluates the
    /// expression, by name, innermost block first, qualified by a block's
    /// label or the function's name, and `$n` for any argument, output ones
    /// included. A field of a row variable takes its column's type, and a
    /// field of a trigger function's `NEW` or `OLD` the type it has in every
    /// table a trigger executing the function is defined on.
    ///
    /// A name a relation of the expression's own queries holds binds that
    /// column, even where it also names a variable. PostgreSQL 18 refuses
    /// such a name as ambiguous under its default
    /// `plpgsql.variable_conflict = error`. Calls and operators resolve
    /// under the path the body runs the expression with. Relations in the
    /// expression's queries resolve under the database's own path.
    PlPgSql(&'s PlPgSqlScope<'s, 'db, DB>),
}
