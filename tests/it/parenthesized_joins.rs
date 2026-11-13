//! References into a parenthesized join, typed and bound as PostgreSQL 18
//! binds them over `p (a int4, x int4)` and `q (a int8, y numeric)`.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::unreachable,
    reason = "Fixtures must hold the nodes they navigate to."
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{Expr, JoinConstraint, JoinOperator, Query, SelectItem, SetExpr, TableFactor},
    dialect::PostgreSqlDialect,
};

use crate::support::query_parsing::query_with;

const SCHEMA: &str = "CREATE TABLE p (a INT4, x INT4); CREATE TABLE q (a INT8, y NUMERIC);";

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).unwrap()
}

/// The `pg_catalog` type name each projected expression of `sql` takes, or
/// an empty name for an expression the context leaves untyped.
fn types(sql: &str) -> Vec<String> {
    let db = database();
    let query = query_with(&PostgreSqlDialect {}, sql);
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    projected(&query)
        .into_iter()
        .map(|node| {
            match context.type_of(node).unwrap() {
                OperandType::Known { type_name, typmod: None }
                    if type_name.schema() == Some("pg_catalog") =>
                {
                    type_name.name().to_owned()
                }
                OperandType::Unknown => String::new(),
                other => panic!("expected a catalog type, got {other:?}"),
            }
        })
        .collect()
}

/// The expressions the outermost `SELECT` of `query` projects.
fn projected(query: &Query) -> Vec<&Expr> {
    let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    select
        .projection
        .iter()
        .map(|item| {
            match item {
                SelectItem::UnnamedExpr(expression)
                | SelectItem::ExprWithAlias { expr: expression, .. } => expression,
                other => panic!("expected an expression, got {other:?}"),
            }
        })
        .collect()
}

/// The first `FROM` item of the outermost `SELECT` of `query`.
fn first_item(query: &Query) -> &TableFactor {
    let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    &select.from[0].relation
}

/// The item `index` of the parenthesized join `factor`, counted from its
/// first.
fn joined_item(factor: &TableFactor, index: usize) -> &TableFactor {
    let TableFactor::NestedJoin { table_with_joins, .. } = factor else {
        panic!("expected a parenthesized join, got {factor:?}")
    };
    match index {
        0 => &table_with_joins.relation,
        _ => &table_with_joins.joins[index - 1].relation,
    }
}

/// Whether the first projected expression of `sql` is refused as ambiguous.
fn is_ambiguous(sql: &str) -> bool {
    let db = database();
    let query = query_with(&PostgreSqlDialect {}, sql);
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    matches!(context.type_of(projected(&query)[0]), Err(LookupError::AmbiguousTableLookup { .. }))
}

/// An unaliased parenthesized join exposes its items' names, qualified and
/// bare, the way the same join written without parentheses does.
#[test]
fn an_unaliased_join_keeps_its_items_visible() {
    assert_eq!(
        types("SELECT u.y, s.x, y FROM (p AS s JOIN q AS u ON s.a = u.a)"),
        ["numeric", "int4", "numeric"]
    );
    assert_eq!(
        types("SELECT a, s.a, u.a FROM (p AS s JOIN q AS u USING (a))"),
        ["", "int4", "int8"],
        "a merged column is typed as the flat join types it"
    );
    assert_eq!(
        types("SELECT w.y, s.x FROM ((p AS s JOIN q AS u ON s.a = u.a) JOIN q AS w ON w.a = u.a)"),
        ["numeric", "int4"]
    );
    assert_eq!(
        types("SELECT l.z FROM (p AS s JOIN q AS u ON s.a = u.a), LATERAL (SELECT u.y AS z) AS l"),
        ["numeric"]
    );
}

/// A merge inside a parenthesized join absorbs only that join's columns, so
/// an item outside it keeps its own.
#[test]
fn a_merge_inside_parentheses_absorbs_only_its_own_items() {
    assert_eq!(
        types("SELECT o.a, s.a FROM p AS o JOIN (p AS s JOIN q AS u USING (a)) ON true"),
        ["int4", "int4"]
    );
    assert!(is_ambiguous("SELECT a FROM p AS o JOIN (p AS s JOIN q AS u USING (a)) ON true"));
    assert_eq!(
        types("SELECT a, o.a, x FROM p AS o JOIN (p AS s(a, sx) JOIN q AS u USING (a)) USING (a)"),
        ["", "int4", "int4"]
    );
}

/// A reference into an unaliased parenthesized join reaches the item inside
/// it, and a merged column no single item.
#[test]
fn an_unaliased_join_binds_to_the_item_inside_it() {
    let db = database();
    let query =
        query_with(&PostgreSqlDialect {}, "SELECT u.y, a FROM (p AS s JOIN q AS u USING (a))");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let [y, merged] = projected(&query)[..] else { unreachable!() };
    let entry = context.reference(y).unwrap().unwrap().1.entry().unwrap();
    assert!(core::ptr::eq(entry.factor(), joined_item(first_item(&query), 1)));
    assert_eq!(entry.ordinal(), 1);
    assert_eq!(context.reference(merged).unwrap().unwrap().1.entry(), None);
}

/// An aliased parenthesized join exposes its output columns under the alias
/// alone, renamed by the alias column list from the first, and hides its
/// items' names.
#[test]
fn an_aliased_join_exposes_its_columns_under_the_alias() {
    assert_eq!(types("SELECT j.y FROM (p AS s JOIN q AS u ON s.a = u.a) AS j"), ["numeric"]);
    assert_eq!(types("SELECT u.y, s.x FROM (p AS s JOIN q AS u ON s.a = u.a) AS j"), ["", ""]);
    assert!(
        is_ambiguous("SELECT j.a FROM (p AS s JOIN q AS u ON s.a = u.a) AS j"),
        "the join outputs two columns named a"
    );
    assert_eq!(types("SELECT j.a FROM (p AS s JOIN q AS u USING (a)) AS j"), [""]);
    assert_eq!(
        types("SELECT j.c, j.c2, j.y, j.x FROM (p AS s JOIN q AS u ON s.a = u.a) AS j(c1, c2, c)"),
        ["int8", "int4", "numeric", ""]
    );
}

/// A reference into an aliased join reaches the join's own output column.
#[test]
fn an_aliased_join_binds_to_its_own_column() {
    let db = database();
    let query =
        query_with(&PostgreSqlDialect {}, "SELECT j.y FROM (p AS s JOIN q AS u ON s.a = u.a) AS j");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let entry = context.reference(projected(&query)[0]).unwrap().unwrap().1.entry().unwrap();
    assert!(core::ptr::eq(entry.factor(), first_item(&query)));
    assert_eq!(entry.ordinal(), 3);
}

/// The `ON` condition of a join inside an aliased parenthesized join reads
/// the join's own items.
#[test]
fn an_aliased_join_condition_reads_its_items() {
    let db = database();
    let query =
        query_with(&PostgreSqlDialect {}, "SELECT j.y FROM (p AS s JOIN q AS u ON s.x = u.y) AS j");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let TableFactor::NestedJoin { table_with_joins, .. } = first_item(&query) else {
        unreachable!()
    };
    let JoinOperator::Join(JoinConstraint::On(Expr::BinaryOp { left, right, .. })) =
        &table_with_joins.joins[0].join_operator
    else {
        unreachable!()
    };
    let typed = |node: &Expr| {
        match context.type_of(node).unwrap() {
            OperandType::Known { type_name, .. } => type_name.name().to_owned(),
            other => panic!("expected a known type, got {other:?}"),
        }
    };
    assert_eq!([typed(left), typed(right)], ["int4", "numeric"]);
}

/// A `LATERAL` item inside an aliased join sees the comma items before the
/// join as well as the join's own earlier items.
#[test]
fn a_lateral_item_inside_an_aliased_join_sees_earlier_items() {
    assert_eq!(
        types(
            "SELECT j.z, j.a FROM p AS o,
                 (q AS u CROSS JOIN LATERAL (SELECT o.a AS z) AS l) AS j(a, y, z)"
        ),
        ["int4", "int8"]
    );
    assert_eq!(
        types("SELECT j.z FROM (q AS u CROSS JOIN LATERAL (SELECT u.y AS z) AS l) AS j"),
        ["numeric"]
    );
}

fn columns(sql: &str) -> Vec<(String, String)> {
    let db = ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} CREATE TABLE ct AS {sql};"))
        .unwrap();
    db.table_by_target(TargetName::new("ct", false), IdentifierCase::AsWritten)
        .unwrap()
        .unwrap()
        .columns(&db)
        .unwrap()
        .map(|column| (column.column_name().to_owned(), column.data_type(&db).to_string()))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected.iter().map(|(name, ty)| ((*name).to_owned(), (*ty).to_owned())).collect()
}

/// A wildcard over a parenthesized join expands to the join's columns in
/// PostgreSQL's order, the columns of `CREATE TABLE ... AS` reading it.
#[test]
fn a_wildcard_expands_a_parenthesized_join() {
    let merged = pairs(&[("a", ""), ("x", "INT4"), ("y", "NUMERIC")]);
    assert_eq!(columns("SELECT * FROM (p AS s JOIN q AS u USING (a))"), merged);
    assert_eq!(columns("SELECT * FROM (p AS s JOIN q AS u USING (a)) AS j"), merged);
    assert_eq!(columns("SELECT j.* FROM (p AS s NATURAL JOIN q AS u) AS j"), merged);
    assert_eq!(
        columns("SELECT * FROM ((p AS s JOIN q AS u USING (a)) JOIN p AS w USING (a, x))"),
        pairs(&[("a", ""), ("x", ""), ("y", "NUMERIC")]),
        "a merged column is typed as the flat join types it"
    );
    assert_eq!(
        columns("SELECT * FROM (p AS s JOIN q AS u ON s.a = u.a) AS j(c1, c2, c)"),
        pairs(&[("c1", "INT4"), ("c2", "INT4"), ("c", "INT8"), ("y", "NUMERIC")])
    );
    assert_eq!(
        columns(
            "SELECT j.* FROM p AS o,
                 (q AS u CROSS JOIN LATERAL (SELECT o.a AS z) AS l) AS j(a, y, z)"
        ),
        pairs(&[("a", "INT8"), ("y", "NUMERIC"), ("z", "INT4")])
    );
    assert_eq!(
        columns("SELECT u.*, s.x FROM (p AS s JOIN q AS u ON s.a = u.a)"),
        pairs(&[("a", "INT8"), ("y", "NUMERIC"), ("x", "INT4")])
    );
    assert_eq!(
        columns("SELECT * FROM (p AS s LEFT JOIN q AS u USING (a)) AS j(c1)"),
        pairs(&[("c1", ""), ("x", "INT4"), ("y", "NUMERIC")])
    );
    assert_eq!(
        columns("SELECT * FROM (p AS s JOIN q AS u USING (a)) AS j JOIN p AS w USING (a, x)"),
        pairs(&[("a", ""), ("x", ""), ("y", "NUMERIC")]),
        "a merge outside the alias absorbs the join's own columns"
    );
}

/// An aliased join whose columns cannot all be enumerated, because an item
/// inside it hides its columns or the alias lists more columns than the join
/// outputs, stands as an opaque relation under its alias.
#[test]
fn an_unenumerable_aliased_join_is_opaque() {
    let db = database();
    for sql in [
        "SELECT j.x FROM (p AS s CROSS JOIN generate_series(1, 2) AS g) AS j",
        "SELECT j.x FROM (p AS s JOIN q AS u ON true) AS j(a, x, b, y, z)",
    ] {
        let query = query_with(&PostgreSqlDialect {}, sql);
        let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
        let reached = context.reference(projected(&query)[0]).unwrap();
        assert!(matches!(reached, Some((ColumnDefinition::Opaque, _))), "{sql}");
    }
}

/// A view reading an aliased join exposes the join's columns, typed.
#[test]
fn a_view_over_an_aliased_join_exposes_its_columns() {
    let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE VIEW v AS SELECT j.y, j.x FROM (p AS s JOIN q AS u ON s.a = u.a) AS j;"
    ))
    .unwrap();
    let query = query_with(&PostgreSqlDialect {}, "SELECT y, x FROM v");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let typed: Vec<_> = projected(&query)
        .into_iter()
        .map(|node| {
            match context.type_of(node).unwrap() {
                OperandType::Known { type_name, .. } => type_name.name().to_owned(),
                other => panic!("expected a known type, got {other:?}"),
            }
        })
        .collect();
    assert_eq!(typed, ["numeric", "int4"]);
}

/// A lookup error inside the parentheses refuses the whole query.
#[test]
fn a_lookup_error_inside_the_parentheses_is_reported() {
    let db = database();
    for sql in [
        "SELECT 1 FROM (p AS s JOIN a.b.c AS u ON true)",
        "SELECT 1 FROM (p AS s JOIN a.b.c AS u ON true) AS j",
    ] {
        let query = query_with(&PostgreSqlDialect {}, sql);
        let built = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten);
        assert!(matches!(built, Err(LookupError::InvalidObjectName { .. })), "{sql}");
    }
}
