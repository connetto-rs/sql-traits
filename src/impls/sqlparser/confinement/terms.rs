//! Expressions and SQL function bodies used by confinement proofs.

use alloc::{borrow::Cow, string::ToString, vec, vec::Vec};

use sqlparser::{
    ast::{BinaryOperator, Expr, FunctionArguments, Query, Statement, Value},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use crate::{
    errors::LookupError,
    structs::{ExpressionTruth, IdentifierCase, Outcomes, Raising, TargetName},
    traits::{DatabaseLike, FunctionLike, TableLike, UserDefinedTypeLike},
};

/// Removes casts and parentheses from an expression.
pub(super) fn unwrapped(mut expression: &Expr) -> &Expr {
    loop {
        match expression {
            Expr::Nested(inner) | Expr::Cast { expr: inner, .. } => expression = inner,
            _ => return expression,
        }
    }
}

/// The query of a body of statements, when it is one query.
fn single_query(mut statements: Vec<Statement>) -> Option<Query> {
    match (statements.pop(), statements.is_empty()) {
        (Some(Statement::Query(query)), true) => Some(*query),
        _ => None,
    }
}

/// Whether `function` sets no configuration parameter besides
/// `search_path`.
pub(super) fn sets_only_search_path<F: FunctionLike>(function: &F) -> bool {
    function
        .configuration_parameters()
        .iter()
        .all(|parameter| parameter.name.to_string().eq_ignore_ascii_case("search_path"))
}

/// The query a `LANGUAGE sql` function runs, when its body is one query
/// written as a string or as a `BEGIN ATOMIC` block.
pub(super) fn sql_query_body<'f, F: FunctionLike>(
    database: &F::DB,
    function: &'f F,
) -> Result<Option<Cow<'f, Query>>, LookupError> {
    if function.stored_language().is_some_and(|language| language != "sql") {
        return Ok(None);
    }
    if let Some(body) = function.body() {
        return Ok(Parser::parse_sql(&PostgreSqlDialect {}, body)
            .ok()
            .and_then(single_query)
            .map(Cow::Owned));
    }
    Ok(match function.atomic_statements(database)? {
        Some(Cow::Borrowed([Statement::Query(query)])) => Some(Cow::Borrowed(query.as_ref())),
        Some(Cow::Owned(statements)) => single_query(statements).map(Cow::Owned),
        _ => None,
    })
}

pub(super) use crate::traits::check_constraint::unnested;

/// The operands of the `AND` chain of an expression.
pub(super) fn conjuncts(expression: &Expr) -> Vec<&Expr> {
    chain(expression, &BinaryOperator::And)
}

/// The operands of the `OR` chain of an expression.
pub(super) fn disjuncts(expression: &Expr) -> Vec<&Expr> {
    chain(expression, &BinaryOperator::Or)
}

/// The operands of the `operator` chain of an expression, through
/// parentheses.
fn chain<'e>(expression: &'e Expr, operator: &BinaryOperator) -> Vec<&'e Expr> {
    match expression {
        Expr::Nested(inner) => chain(inner, operator),
        Expr::BinaryOp { left, op, right } if op == operator => {
            let mut operands = chain(left, operator);
            operands.extend(chain(right, operator));
            operands
        }
        _ => vec![expression],
    }
}

/// Whether the expression names a column, bare or qualified.
pub(super) fn is_column_reference(expression: &Expr) -> bool {
    matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

/// Whether the expression reads the current or session role, `user` being
/// PostgreSQL's spelling of `current_user`.
pub(super) fn is_role_read(expression: &Expr) -> bool {
    is_call_to(expression, &["current_user", "current_role", "session_user", "user"], true)
}

/// Whether the expression calls one of `names`, written without parentheses
/// when `bare`.
fn is_call_to(expression: &Expr, names: &[&str], bare: bool) -> bool {
    let Expr::Function(function) = expression else {
        return false;
    };
    matches!(function.args, FunctionArguments::None) == bare
        && function
            .name
            .0
            .last()
            .and_then(|part| part.as_ident())
            .is_some_and(|ident| names.iter().any(|name| ident.value.eq_ignore_ascii_case(name)))
}

/// Returns whether PostgreSQL can prove the expression true for every row.
pub(super) fn is_tautology<DB: DatabaseLike>(
    database: &DB,
    table: &DB::Table,
    expression: &Expr,
) -> Result<bool, LookupError> {
    Ok(admits_every_row(table.expression_truth(database, expression, IdentifierCase::AsWritten)?))
}

/// Whether a policy clause of this truth admits every row, being true
/// without raising.
fn admits_every_row(truth: ExpressionTruth) -> bool {
    truth.outcomes == Outcomes::TRUE && truth.raising == Raising::Never
}

/// The text of a string literal, in any quoting PostgreSQL accepts.
pub(super) fn string_value(value: &Value) -> Option<&str> {
    match value {
        Value::SingleQuotedString(text)
        | Value::EscapedStringLiteral(text)
        | Value::UnicodeStringLiteral(text)
        | Value::NationalStringLiteral(text) => Some(text),
        Value::DollarQuotedString(quoted) => Some(&quoted.value),
        _ => None,
    }
}

/// The literal `expression` is under parentheses and casts, `NULL`
/// included.
pub(super) fn literal(mut expression: &Expr) -> Option<&Value> {
    loop {
        match expression {
            Expr::Nested(inner) | Expr::Cast { expr: inner, .. } => expression = inner,
            Expr::Value(literal) => return Some(&literal.value),
            Expr::TypedString(typed) => return Some(&typed.value.value),
            _ => return None,
        }
    }
}

/// The text of the string literal `expression` is under parentheses and
/// casts.
pub(super) fn text_literal(expression: &Expr) -> Option<&str> {
    literal(expression).and_then(string_value)
}

/// The user-defined type `type_name` names, as a typed operand names it,
/// with the schema it omits taken as `public`.
pub(super) fn user_type_named<'db, DB: DatabaseLike>(
    database: &'db DB,
    type_name: &TargetName<'_>,
) -> Option<&'db DB::UserDefinedType> {
    database.user_defined_types().find(|user_type| {
        user_type.stored_name() == type_name.name()
            && type_name.schema().is_none_or(|schema| {
                user_type.stored_schema().as_deref().unwrap_or("public") == schema
            })
    })
}

/// Whether a function body cannot be read as resolvable SQL.
pub(super) fn unreadable_body(error: &LookupError) -> bool {
    matches!(
        error,
        LookupError::FunctionBodyNotSql { .. }
            | LookupError::FunctionBodyUnavailable { .. }
            | LookupError::FunctionBodyUnresolvable { .. }
            | LookupError::FunctionBodyUnparsable { .. }
    )
}
