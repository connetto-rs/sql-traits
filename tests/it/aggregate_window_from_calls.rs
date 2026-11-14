//! Tests that aggregate and window calls, and functions called in `FROM`,
//! are resolved as PostgreSQL 18 resolves them.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{Query, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

/// The table the queries read.
const SCHEMA: &str = "CREATE TABLE t (a INT4, s TEXT);";

fn query(sql: &str) -> Query {
    let Statement::Query(query) =
        Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("the query parses").remove(0)
    else {
        panic!("a query")
    };
    *query
}

/// What a call resolves to, the selected function's result type for one
/// PostgreSQL runs.
fn describe(call: &CallApplication<'_, '_, ParserDB>) -> String {
    match call.resolution() {
        CallResolution::Selected(selected) => {
            format!("returns {}", selected.result_type().map_or("?", TargetName::name))
        }
        CallResolution::Cast(target) => format!("cast to {}", target.name()),
        CallResolution::NotFound => "not found".to_owned(),
        CallResolution::Ambiguous(_) => "ambiguous".to_owned(),
        CallResolution::UnboundPolymorphic(_) => "unbound".to_owned(),
        CallResolution::Undetermined => "undetermined".to_owned(),
    }
}

/// What each projected call of `sql` over [`SCHEMA`] resolves to.
fn projected_calls(sql: &str) -> Vec<String> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema parses");
    let query = query(sql);
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded).expect("typed");
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a select") };
    select
        .projection
        .iter()
        .map(|item| {
            let (SelectItem::UnnamedExpr(node) | SelectItem::ExprWithAlias { expr: node, .. }) =
                item
            else {
                panic!("an expression")
            };
            describe(&context.call(node).expect("read").expect("a call"))
        })
        .collect()
}

/// What the first `FROM` item of `sql` over [`SCHEMA`] calls.
fn relation_call(sql: &str) -> Option<String> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema parses");
    let query = query(sql);
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded).expect("typed");
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a select") };
    let from = select.from.last().expect("a FROM item");
    let factor = from.joins.last().map_or(&from.relation, |join| &join.relation);
    context.relation_call(factor).expect("read").as_ref().map(describe)
}

/// Aggregate and window calls are resolved on the arguments they pass,
/// `DISTINCT`, `ORDER BY`, `FILTER` and `OVER` aside, with the ordering
/// arguments of `WITHIN GROUP` after them and none for `count(*)`.
#[test]
fn aggregate_and_window_calls_are_resolved() {
    assert_eq!(
        projected_calls(
            "SELECT sum(a), count(*), string_agg(DISTINCT s, ',' ORDER BY s),
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY a),
                    count(a) FILTER (WHERE a > 0), row_number() OVER ()
             FROM t"
        ),
        [
            "returns int8",
            "returns int8",
            "returns text",
            "returns float8",
            "returns int8",
            "returns int8",
        ]
    );
}

/// An aggregate the argument types reach no overload of is not found,
/// counting the ordering arguments of `WITHIN GROUP` among them.
#[test]
fn an_aggregate_without_an_overload_is_not_found() {
    assert_eq!(
        projected_calls(
            "SELECT sum(s), avg(s) OVER (), sum(a) WITHIN GROUP (ORDER BY a),
                    percentile_cont(0.5)
             FROM t"
        ),
        ["not found"; 4]
    );
}

/// A function called in `FROM` is resolved in the scope of its item,
/// laterally or not, and a one-argument call named after a type is a cast.
#[test]
fn a_function_in_from_is_resolved() {
    assert_eq!(
        relation_call("SELECT g FROM generate_series(1, 3) AS g").as_deref(),
        Some("returns int4")
    );
    assert_eq!(
        relation_call("SELECT g FROM t, LATERAL generate_series(1, t.a) AS g").as_deref(),
        Some("returns int4")
    );
    assert_eq!(relation_call("SELECT * FROM absent_function(1)").as_deref(), Some("not found"));
    assert_eq!(
        relation_call("SELECT g FROM generate_series('a'::text, 3) AS g").as_deref(),
        Some("not found")
    );
    assert_eq!(relation_call("SELECT * FROM int8('5') AS x").as_deref(), Some("cast to int8"));
    assert_eq!(relation_call("SELECT a FROM t"), None);
}

/// Calls whose arguments or clauses the context does not type answer no
/// call, so nothing downstream reads them as refused: a whole-row argument,
/// `IGNORE NULLS`, which PostgreSQL 18 has no grammar for, a `SEPARATOR`
/// clause of another dialect, and `COALESCE` in `FROM`, an expression.
#[test]
fn calls_the_context_does_not_type_answer_none() {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema parses");
    for (dialect, sql) in [
        (&PostgreSqlDialect {} as &dyn sqlparser::dialect::Dialect, "SELECT count(t.*) FROM t"),
        (&PostgreSqlDialect {}, "SELECT first_value(a) IGNORE NULLS OVER () FROM t"),
        (&sqlparser::dialect::MySqlDialect {}, "SELECT GROUP_CONCAT(s SEPARATOR ',') FROM t"),
    ] {
        let Statement::Query(query) =
            Parser::parse_sql(dialect, sql).expect("the query parses").remove(0)
        else {
            panic!("a query")
        };
        let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded).expect("typed");
        let SetExpr::Select(select) = query.body.as_ref() else { panic!("a select") };
        let SelectItem::UnnamedExpr(node) = &select.projection[0] else { panic!("a call") };
        assert!(context.call(node).expect("read").is_none(), "{sql}");
    }
    assert_eq!(relation_call("SELECT x FROM coalesce(1, 2) AS x"), None);
    assert_eq!(relation_call("SELECT x FROM t, LATERAL generate_series(t.*) AS x"), None);
}

/// A view holds the overload its `FROM` call runs, and no other.
#[test]
fn a_view_holds_the_overload_its_from_call_runs() {
    let functions = "CREATE FUNCTION f(int4) RETURNS SETOF INT4 LANGUAGE sql AS 'SELECT 1';
                     CREATE FUNCTION f(text) RETURNS SETOF INT4 LANGUAGE sql AS 'SELECT 2';
                     CREATE VIEW v AS SELECT x FROM f(1) AS x;";
    ParserDB::parse::<PostgreSqlDialect>(&format!("{functions} DROP FUNCTION f(text);"))
        .expect("the view runs `f(int4)`");
    let result =
        ParserDB::parse::<PostgreSqlDialect>(&format!("{functions} DROP FUNCTION f(int4);"));
    assert!(
        matches!(&result, Err(Error::FunctionReferenced { function_name }) if function_name == "f"),
        "got {result:?}"
    );
}
