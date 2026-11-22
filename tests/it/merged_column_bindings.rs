//! A reference reaching a column a `USING` or `NATURAL` join merged names the
//! join that merged it, so a subquery's reference to the outer merge stays
//! apart from a merge of the subquery's own `FROM`.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::unreachable,
    reason = "Fixtures must hold the nodes they navigate to."
)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Query, Select, SelectItem, SetExpr, Statement, TableFactor},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE a (k INT, m INT); CREATE TABLE b (k INT, m INT);
         CREATE TABLE c (k INT); CREATE TABLE d (x INT);",
    )
    .unwrap()
}

fn query(sql: &str) -> Query {
    match Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap().remove(0) {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}

fn select(query: &Query) -> &Select {
    let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    select
}

/// The `SELECT` of the `EXISTS` subquery the `WHERE` clause of `outer` holds.
fn exists_select(outer: &Select) -> &Select {
    let Some(Expr::Exists { subquery, .. }) = &outer.selection else { unreachable!() };
    select(subquery)
}

/// The expressions `select` projects.
fn projected(select: &Select) -> Vec<&Expr> {
    select
        .projection
        .iter()
        .map(|item| {
            let SelectItem::UnnamedExpr(expression) = item else { unreachable!() };
            expression
        })
        .collect()
}

/// The merge each reference `select` projects reached, in `context`.
fn merges<'context>(
    context: &'context ExpressionContext<'_, '_, ParserDB>,
    select: &Select,
) -> Vec<Option<MergedColumn<'context>>> {
    projected(select)
        .into_iter()
        .map(|reference| context.reference(reference).unwrap().unwrap().1.merge())
        .collect()
}

/// A bare reference in a subquery reaching the merge of the outer `FROM`
/// names the outer join, and one reaching the subquery's own merge names
/// that one, while a reference to a plain column names no merge.
#[test]
fn subquery_references_name_the_merge_they_reached() {
    let db = database();
    let outer_merge = query("SELECT 1 FROM a JOIN b USING (k) WHERE EXISTS (SELECT k, d.x FROM d)");
    let context = ExpressionContext::new(&outer_merge, &db, IdentifierCase::Folded).unwrap();
    let outer = select(&outer_merge);
    let [Some(merge), None] = merges(&context, exists_select(outer))[..] else { unreachable!() };
    assert!(core::ptr::eq(merge.from(), core::ptr::from_ref(&outer.from[0])));
    assert_eq!((merge.join(), merge.ordinal()), (0, 0));

    let own_merge = query("SELECT 1 FROM d WHERE EXISTS (SELECT k FROM b JOIN c USING (k))");
    let context = ExpressionContext::new(&own_merge, &db, IdentifierCase::Folded).unwrap();
    let inner = exists_select(select(&own_merge));
    let [Some(merge)] = merges(&context, inner)[..] else { unreachable!() };
    assert!(core::ptr::eq(merge.from(), core::ptr::from_ref(&inner.from[0])));
    assert_eq!((merge.join(), merge.ordinal()), (0, 0));
}

/// Each column a join merges is its own, numbered in the join's output
/// order, and the alias `USING (…) AS` gives the join reaches the same
/// column a bare reference does.
#[test]
fn merged_columns_are_numbered_and_reached_through_the_join_alias() {
    let db = database();
    let listed = query("SELECT k, m, j.k, j.m FROM a JOIN b USING (m, k) AS j");
    let context = ExpressionContext::new(&listed, &db, IdentifierCase::Folded).unwrap();
    let [Some(k), Some(m), Some(aliased_k), Some(aliased_m)] =
        merges(&context, select(&listed))[..]
    else {
        unreachable!()
    };
    assert_eq!((k.join(), k.ordinal(), m.ordinal()), (0, 1, 0));
    assert_ne!(k, m);
    assert_eq!((aliased_k, aliased_m), (k, m));

    let natural = query("SELECT k, m FROM a NATURAL JOIN b");
    let context = ExpressionContext::new(&natural, &db, IdentifierCase::Folded).unwrap();
    let [Some(k), Some(m)] = merges(&context, select(&natural))[..] else { unreachable!() };
    assert_eq!((k.join(), k.ordinal(), m.join(), m.ordinal()), (0, 0, 0, 1));
}

/// A later join merging a name again takes it over, the outer join of a
/// parenthesized one included, while the alias of the earlier join still
/// names that join's own column.
#[test]
fn a_later_merge_of_the_same_name_takes_it_over() {
    let db = database();
    let chained = query("SELECT k, j.k FROM a JOIN b USING (k) AS j JOIN c USING (k)");
    let context = ExpressionContext::new(&chained, &db, IdentifierCase::Folded).unwrap();
    let from = &select(&chained).from[0];
    let [Some(bare), Some(aliased)] = merges(&context, select(&chained))[..] else {
        unreachable!()
    };
    assert!(core::ptr::eq(bare.from(), from) && core::ptr::eq(aliased.from(), from));
    assert_eq!((bare.join(), aliased.join()), (1, 0));

    let parenthesized = query("SELECT k, m FROM (a JOIN b USING (k, m)) JOIN c USING (k)");
    let context = ExpressionContext::new(&parenthesized, &db, IdentifierCase::Folded).unwrap();
    let from = &select(&parenthesized).from[0];
    let TableFactor::NestedJoin { table_with_joins: nested, .. } = &from.relation else {
        unreachable!()
    };
    let [Some(k), Some(m)] = merges(&context, select(&parenthesized))[..] else { unreachable!() };
    assert!(core::ptr::eq(k.from(), from));
    assert!(core::ptr::eq(m.from(), nested.as_ref()));
    assert_eq!((k.join(), k.ordinal(), m.join(), m.ordinal()), (0, 0, 0, 1));
}
