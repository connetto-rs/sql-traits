//! Whether a query fills its output columns from a wildcard.

use sqlparser::ast::{Query, SelectItem, SetExpr};

/// Whether `query` takes its output columns from a wildcard: `*` or `t.*` in
/// the select list of its outermost `SELECT` or of any arm of a set operation
/// at the top, or a `TABLE t` query, which PostgreSQL reads as `SELECT * FROM
/// t`.
///
/// A wildcard nested deeper, in a `FROM` subquery, a common table expression
/// or a predicate such as `EXISTS (SELECT * ...)`, only reaches the output
/// through the outer select list, so it does not count.
pub(crate) fn output_from_wildcard(query: &Query) -> bool {
    body_from_wildcard(&query.body)
}

fn body_from_wildcard(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => {
            select.projection.iter().any(|item| {
                matches!(item, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..))
            })
        }
        SetExpr::Query(query) => output_from_wildcard(query),
        SetExpr::SetOperation { left, right, .. } => {
            body_from_wildcard(left) || body_from_wildcard(right)
        }
        SetExpr::Table(_) => true,
        _ => false,
    }
}
