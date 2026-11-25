//! Predicates and expressions that cannot be true, or turn null, while a
//! bound column is null.

use sqlparser::ast::{BinaryOperator, CastKind, Expr, InTarget, UnaryOperator};

use crate::{
    errors::LookupError,
    structs::{CallResolution, ExpressionContext, OperatorResolution},
    traits::{DatabaseLike, FunctionLike, OperatorLike},
};

/// Whether a predicate cannot be true while the column `bound` accepts is
/// null, reading operators as `types` types them.
pub(super) fn rejects_null<DB: DatabaseLike>(
    filter: &Expr,
    bound: &impl Fn(&Expr) -> Result<bool, LookupError>,
    database: &DB,
    types: &ExpressionContext<'_, '_, DB>,
) -> Result<bool, LookupError> {
    Ok(match filter {
        Expr::IsNotNull(inner) => null_when(inner, bound, database, types)?,
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => {
            let mut inner = expr.as_ref();
            while let Expr::Nested(nested) = inner {
                inner = nested;
            }
            match inner {
                Expr::IsNull(inner) => null_when(inner, bound, database, types)?,
                _ => null_when(inner, bound, database, types)?,
            }
        }
        Expr::Nested(inner) => rejects_null(inner, bound, database, types)?,
        // A `NULL` left operand makes the comparison `NULL`, or `false` over
        // no element, and neither is true.
        Expr::AnyOp { left, compare_op, .. } => {
            (strictness(database, types, filter, compare_op)?.0
                && null_when(left, bound, database, types)?)
                || null_when(filter, bound, database, types)?
        }
        // A `NULL` left operand makes `IN` over a query `NULL`, or `false`
        // over no row, and neither is true.
        Expr::In { expr, target: InTarget::Subquery(_), negated: false, .. } => {
            null_when(expr, bound, database, types)?
        }
        _ => null_when(filter, bound, database, types)?,
    })
}

/// Whether a strict expression becomes null when the column `bound` accepts
/// is null, reading operators as `types` types them.
fn null_when<DB: DatabaseLike>(
    expression: &Expr,
    bound: &impl Fn(&Expr) -> Result<bool, LookupError>,
    database: &DB,
    types: &ExpressionContext<'_, '_, DB>,
) -> Result<bool, LookupError> {
    let null = |inner: &Expr| null_when(inner, bound, database, types);
    Ok(match expression {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => bound(expression)?,
        Expr::Nested(inner)
        | Expr::Cast { kind: CastKind::Cast | CastKind::DoubleColon, expr: inner, .. }
        | Expr::UnaryOp {
            op: UnaryOperator::Plus | UnaryOperator::Minus | UnaryOperator::Not,
            expr: inner,
        } => null(inner)?,
        Expr::BinaryOp { left, op: operator, right } => {
            let (left_strict, right_strict) = strictness(database, types, expression, operator)?;
            (left_strict && null(left)?) || (right_strict && null(right)?)
        }
        Expr::AnyOp { left, compare_op: operator, right, .. } => {
            let (left_strict, right_strict) = strictness(database, types, expression, operator)?;
            let has_elements =
                matches!(right.as_ref(), Expr::Array(array) if !array.elem.is_empty());
            (left_strict && has_elements && null(left)?) || (right_strict && null(right)?)
        }
        Expr::Like { expr, pattern, .. }
        | Expr::ILike { expr, pattern, .. }
        | Expr::SimilarTo { expr, pattern, .. } => null(expr)? || null(pattern)?,
        Expr::Between { expr, .. } | Expr::In { expr, target: InTarget::List(_), .. } => {
            null(expr)?
        }
        Expr::Function(_) => {
            find_null_passing_argument(database, types, expression, |argument| {
                Ok(null(argument)?.then_some(()))
            })?
            .is_some()
        }
        _ => false,
    })
}

/// The first of `judge`'s answers for the arguments of the call `expression`
/// that pass a parameter whose `NULL` makes the function PostgreSQL selects
/// return `NULL`, none where `types` cannot tell the selection.
fn find_null_passing_argument<'node, DB: DatabaseLike, T>(
    database: &DB,
    types: &ExpressionContext<'_, '_, DB>,
    expression: &'node Expr,
    mut judge: impl FnMut(&'node Expr) -> Result<Option<T>, LookupError>,
) -> Result<Option<T>, LookupError> {
    let Some(application) = types.call(expression)? else {
        return Ok(None);
    };
    let CallResolution::Selected(selected) = application.resolution() else {
        return Ok(None);
    };
    for (&argument, passed) in application.arguments().iter().zip(selected.arguments()) {
        if selected.function().returns_null_on_null_argument(database, passed.parameter())
            && let Some(answer) = judge(argument)?
        {
            return Ok(Some(answer));
        }
    }
    Ok(None)
}

/// Whether `expression`, applying `operator`, is `NULL` for a `NULL` left
/// operand and for a `NULL` right one, as the function implementing the
/// operator PostgreSQL selects declares. Where `types` cannot tell the
/// selection, each overload of a comparison or an arithmetic operator is
/// strict, unlike those of `||`, which appends to an array whatever the
/// other operand holds.
fn strictness<DB: DatabaseLike>(
    database: &DB,
    types: &ExpressionContext<'_, '_, DB>,
    expression: &Expr,
    operator: &BinaryOperator,
) -> Result<(bool, bool), LookupError> {
    if let Some(applications) = types.operators(expression)?
        && let [application] = applications.as_slice()
        && let OperatorResolution::Selected(selected) = application.resolution()
        && let Some(function) = selected.operator().implementation(database)
    {
        return Ok((
            function.returns_null_on_null_argument(database, 0),
            function.returns_null_on_null_argument(database, 1),
        ));
    }
    let strict = is_strict_operator(operator);
    Ok((strict, strict))
}

/// Operators whose every overload yields `NULL` when an operand is `NULL`.
fn is_strict_operator(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::PGRegexMatch
            | BinaryOperator::PGRegexIMatch
            | BinaryOperator::PGRegexNotMatch
            | BinaryOperator::PGRegexNotIMatch
            | BinaryOperator::Arrow
            | BinaryOperator::LongArrow
    )
}
