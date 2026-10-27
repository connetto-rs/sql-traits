//! Integration tests for PostgreSQL select-list output names in clause lookups.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking on unexpected lookups"
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{
        Distinct, Expr, GroupByExpr, OrderByKind, Query, Select, SelectItem, SetExpr, TableFactor,
    },
    dialect::PostgreSqlDialect,
};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/query_parsing.rs"));

const SCHEMA: &str = "
    CREATE TABLE a(id INT, row_alias INT);
    CREATE TABLE b(id INT);
";

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

fn query(sql: &str) -> Query {
    query_with(&PostgreSqlDialect {}, sql)
}

fn order_key(query: &Query) -> &Expr {
    let order_by = query.order_by.as_ref().expect("an ORDER BY clause");
    let OrderByKind::Expressions(keys) = &order_by.kind else { panic!("expected sort keys") };
    &keys[0].expr
}

fn derived_query(query: &Query) -> &Query {
    let TableFactor::Derived { subquery, .. } = &body_select(query).from[0].relation else {
        panic!("expected a derived table")
    };
    subquery
}

fn body_select(query: &Query) -> &Select {
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a SELECT body") };
    select
}

fn projected_subquery(query: &Query) -> &Query {
    let SelectItem::UnnamedExpr(Expr::Subquery(subquery)) = &body_select(query).projection[0]
    else {
        panic!("expected a scalar subquery projection")
    };
    subquery
}

fn where_subquery(query: &Query) -> &Query {
    let Some(Expr::BinaryOp { right, .. }) = &body_select(query).selection else {
        panic!("expected a comparison filter")
    };
    let Expr::Subquery(subquery) = right.as_ref() else { panic!("expected a scalar subquery") };
    subquery
}

fn left_operand(query: &Query) -> &Query {
    let SetExpr::SetOperation { left, .. } = query.body.as_ref() else {
        panic!("expected a set operation")
    };
    let SetExpr::Query(operand) = left.as_ref() else { panic!("expected a parenthesized operand") };
    operand
}

fn cte_body(query: &Query) -> &Query {
    &query.with.as_ref().expect("a WITH clause").cte_tables[0].query
}

fn select_key(
    sql: &str,
    pick: fn(&Query) -> &Query,
    resolve: fn(
        &ColumnDefinitionScope<'_, '_, '_, ParserDB>,
        &Select,
    ) -> Result<String, LookupError>,
) -> Result<String, LookupError> {
    let database = database();
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let select = body_select(pick(&query));
    resolve(&scope.scope_for_select(select).expect("select is recorded"), select)
}

fn group_by_in(sql: &str, pick: fn(&Query) -> &Query) -> Result<String, LookupError> {
    select_key(sql, pick, |scope, select| {
        let GroupByExpr::Expressions(keys, _) = &select.group_by else { panic!("expected keys") };
        scope.resolve_group_by_definition(&keys[0]).map(|definition| describe(definition.as_ref()))
    })
}

fn distinct_on(sql: &str) -> Result<String, LookupError> {
    select_key(
        sql,
        |query| query,
        |scope, select| {
            let Some(Distinct::On(keys)) = &select.distinct else { panic!("expected DISTINCT ON") };
            scope
                .resolve_distinct_on_definition(&keys[0])
                .map(|definition| describe(definition.as_ref()))
        },
    )
}

fn describe(definition: Option<&ColumnDefinition<'_, '_, '_, ParserDB>>) -> String {
    match definition {
        None => "none".to_owned(),
        Some(ColumnDefinition::Base { table, column }) => {
            format!("{}.{}", table.table_name(), column.column_name())
        }
        Some(ColumnDefinition::Expression { expression, .. }) => format!("expression {expression}"),
        Some(ColumnDefinition::SetOperation { operator, left, right }) => {
            format!(
                "{operator}({}, {})",
                describe(Some(&left.definition())),
                describe(Some(&right.definition()))
            )
        }
        Some(ColumnDefinition::RecursiveUnion { .. }) => "recursive".to_owned(),
        Some(ColumnDefinition::Opaque) => "opaque".to_owned(),
    }
}

fn order_by_in(sql: &str, pick: fn(&Query) -> &Query) -> Result<String, LookupError> {
    let database = database();
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let target = pick(&query);
    let order = scope.scope_for_query(target).expect("query is recorded");
    order
        .resolve_order_by_definition(order_key(target))
        .map(|definition| describe(definition.as_ref()))
}

fn order_by(sql: &str) -> Result<String, LookupError> {
    order_by_in(sql, |query| query)
}

#[test]
fn an_order_by_alias_resolves_to_its_projection_and_a_parameter_name_does_not() {
    let sql = "SELECT id AS row_alias FROM b ORDER BY row_alias LIMIT 1";
    assert_eq!(order_by(sql).unwrap(), "b.id");
    assert_eq!(order_by("SELECT id AS row_alias FROM b ORDER BY id").unwrap(), "b.id");
    assert_eq!(order_by("SELECT id AS row_alias FROM b ORDER BY argument_only").unwrap(), "none");
}

#[test]
fn a_bare_output_name_wins_over_an_input_column_only_as_the_whole_key() {
    assert_eq!(order_by("SELECT id AS row_alias FROM a ORDER BY row_alias").unwrap(), "a.id");
    assert_eq!(order_by("SELECT id AS row_alias FROM a ORDER BY (row_alias)").unwrap(), "a.id");
    assert_eq!(order_by("SELECT id AS row_alias FROM a ORDER BY row_alias + 1").unwrap(), "none");
    assert_eq!(
        order_by("SELECT id AS row_alias FROM a ORDER BY a.row_alias").unwrap(),
        "a.row_alias"
    );
}

#[test]
fn set_operations_order_by_their_output_names_alone() {
    let sql = "SELECT id AS x FROM a UNION SELECT id FROM b ORDER BY x";
    assert_eq!(order_by(sql).unwrap(), "UNION(a.id, b.id)");
    assert_eq!(
        order_by("SELECT id AS x FROM a UNION SELECT id FROM b ORDER BY id").unwrap(),
        "none"
    );
    assert_eq!(order_by("SELECT id AS x FROM a UNION SELECT 3 AS y ORDER BY y").unwrap(), "none");
    assert_eq!(
        order_by("SELECT id AS x FROM a UNION SELECT id FROM b EXCEPT SELECT id FROM a ORDER BY x")
            .unwrap(),
        "EXCEPT(UNION(a.id, b.id), a.id)"
    );
    assert_eq!(
        order_by("SELECT id AS x FROM a UNION SELECT * FROM generate_series(1, 2) AS g ORDER BY x")
            .unwrap(),
        "opaque"
    );
    assert_eq!(
        order_by("VALUES (1) UNION SELECT id FROM b ORDER BY column1").unwrap(),
        "UNION(opaque, b.id)"
    );
    assert_eq!(
        order_by("SELECT * FROM generate_series(1, 2) AS g UNION SELECT id FROM b ORDER BY id")
            .unwrap(),
        "opaque"
    );
    assert_eq!(order_by("(TABLE b) UNION SELECT id FROM a ORDER BY id").unwrap(), "opaque");
    assert_eq!(
        order_by("SELECT id AS x FROM a UNION SELECT id FROM b ORDER BY x + 1").unwrap(),
        "none"
    );
}

#[test]
fn parenthesized_bodies_keep_their_input_columns() {
    assert_eq!(order_by("(SELECT id AS x FROM a) ORDER BY x").unwrap(), "a.id");
    assert_eq!(order_by("(SELECT id AS x FROM a) ORDER BY row_alias").unwrap(), "a.row_alias");
}

#[test]
fn duplicate_output_names_are_ambiguous_only_for_different_values() {
    assert_eq!(order_by("SELECT id AS x, a.id AS x FROM a ORDER BY x").unwrap(), "a.id");
    assert!(matches!(
        order_by("SELECT id AS x, id + 1 AS x FROM a ORDER BY x"),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
    assert_eq!(
        order_by("SELECT *, id + 1 AS id FROM b ORDER BY id"),
        Err(LookupError::AmbiguousTableLookup {
            object_name: "id".to_owned(),
            candidates: vec!["id".to_owned(), "id + 1".to_owned()],
        })
    );
}

#[test]
fn quoted_output_names_compare_exactly() {
    assert_eq!(order_by("SELECT id AS \"X\" FROM a ORDER BY \"X\"").unwrap(), "a.id");
    assert_eq!(order_by("SELECT id AS \"X\" FROM a ORDER BY x").unwrap(), "none");
    assert_eq!(order_by("SELECT id AS x FROM a ORDER BY \"x\"").unwrap(), "a.id");
}

#[test]
fn unaliased_expressions_carry_postgres_implicit_names() {
    for (projection, key) in [
        ("count(*)", "count"),
        ("1::int", "int4"),
        ("1::double precision", "float8"),
        ("'a'::character varying(3)", "varchar"),
        ("'{}'::text[]", "text"),
        ("id::text", "id"),
        ("(id + 1)::text", "text"),
        ("CASE WHEN true THEN 1 END", "case"),
        ("CASE WHEN true THEN 1 ELSE id END", "id"),
        ("ARRAY[1]", "array"),
        ("(1, 2)", "row"),
        ("EXISTS (SELECT 1)", "\"exists\""),
        ("(SELECT 1 AS q)", "q"),
        ("trim(leading 'a' from 'abc')", "ltrim"),
        ("substr('abc', 1)", "substr"),
        ("id + 1", "\"?column?\""),
        (
            "(DATE '2020-01-01', DATE '2020-02-01') OVERLAPS (DATE '2020-01-15', DATE '2020-03-01')",
            "overlaps",
        ),
        ("(SELECT 1 AS q UNION SELECT 2)", "q"),
        ("(WITH c AS (SELECT 1) (SELECT 1 AS q))", "q"),
        ("(SELECT id + 1 FROM b)", "\"?column?\""),
        ("(WITH c AS (SELECT 1) VALUES (1))", "column1"),
        ("(ROW(1, 2)).f1", "f1"),
        ("(ARRAY[1])[1]", "\"array\""),
        ("DATE '2020-01-01'", "\"date\""),
        ("INTERVAL '1 day'", "\"interval\""),
        ("now() AT TIME ZONE 'UTC'", "\"timezone\""),
        ("extract(year FROM DATE '2020-01-01')", "\"extract\""),
        ("ceil(1.5)", "\"ceil\""),
        ("floor(1.5)", "\"floor\""),
        ("position('a' IN 'abc')", "\"position\""),
        ("overlay('abc' PLACING 'x' FROM 1)", "\"overlay\""),
        ("substring('abc' FROM 1 FOR 1)", "\"substring\""),
        ("trim(trailing 'a' FROM 'abc')", "\"rtrim\""),
        ("trim('a')", "\"btrim\""),
        ("1::smallint", "int2"),
        ("1::bigint", "int8"),
        ("1::numeric", "\"numeric\""),
        ("true::boolean", "bool"),
        ("1::float(10)", "float4"),
        ("1::real", "float4"),
        ("'2020-01-01'::date", "\"date\""),
        ("'{}'::json", "\"json\""),
        ("'{}'::jsonb", "\"jsonb\""),
        ("'a'::char", "bpchar"),
        ("'a'::bytea", "\"bytea\""),
        ("'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid", "\"uuid\""),
        ("'1 day'::interval", "\"interval\""),
        ("'1'::bit(1)", "\"bit\""),
        ("'1'::varbit", "varbit"),
        ("'10:00'::time", "\"time\""),
        ("'10:00'::timetz", "timetz"),
        ("'2020-01-01'::timestamp", "\"timestamp\""),
        ("'2020-01-01'::timestamptz", "timestamptz"),
        ("'a'::name", "\"name\""),
        ("'a'::tsvector", "tsvector"),
        ("'pg_class'::regclass", "regclass"),
        ("'(1,2)'::point", "point"),
        ("'a'::tsquery", "tsquery"),
        ("point '(1,2)'", "point"),
        ("line '{1,2,3}'", "line"),
        ("lseg '[(1,1),(2,2)]'", "lseg"),
        ("box '((1,1),(2,2))'", "box"),
        ("path '[(1,1),(2,2)]'", "path"),
        ("polygon '((1,1),(2,2),(3,1))'", "polygon"),
        ("circle '<(1,1),1>'", "circle"),
    ] {
        let sql = format!("SELECT {projection} FROM b ORDER BY {key}");
        let resolved = order_by(&sql).unwrap();
        assert!(resolved.starts_with("expression"), "`{sql}` resolved to {resolved}");
    }
    assert_eq!(order_by("SELECT id + 1 FROM b ORDER BY argument_only").unwrap(), "none");
}

#[test]
fn unknown_output_names_answer_opaque() {
    assert_eq!(
        order_by("SELECT * FROM generate_series(1, 2) AS g ORDER BY argument_only").unwrap(),
        "opaque"
    );
    assert_eq!(
        order_by("SELECT 'a' IS NORMALIZED FROM b ORDER BY argument_only").unwrap(),
        "opaque"
    );
    assert_eq!(order_by("SELECT 'a' IS NORMALIZED, id AS x FROM b ORDER BY x").unwrap(), "b.id");
    for projection in [
        "'a'::nvarchar",
        "(WITH c AS (SELECT 1) TABLE b)",
        "(SELECT * FROM b)",
        "(SELECT 'a' IS NORMALIZED)",
    ] {
        let sql = format!("SELECT {projection} FROM b ORDER BY argument_only");
        assert_eq!(order_by(&sql).unwrap(), "opaque", "`{sql}`");
    }
}

#[test]
fn wildcards_expose_their_columns_as_output_names() {
    assert_eq!(order_by("SELECT * FROM b ORDER BY id").unwrap(), "b.id");
    assert_eq!(order_by("SELECT b.* FROM b ORDER BY argument_only").unwrap(), "none");
}

#[test]
fn every_nested_query_resolves_its_own_order_by() {
    let sql = "SELECT d.x FROM (SELECT id AS x FROM a ORDER BY x LIMIT 1) AS d";
    assert_eq!(order_by_in(sql, derived_query).unwrap(), "a.id");
    let sql = "SELECT id FROM a WHERE id = (SELECT id AS x FROM b ORDER BY x LIMIT 1)";
    assert_eq!(order_by_in(sql, where_subquery).unwrap(), "b.id");
    let sql = "WITH c AS (SELECT id AS x FROM b ORDER BY x LIMIT 1) SELECT x FROM c";
    assert_eq!(order_by_in(sql, cte_body).unwrap(), "b.id");
    let sql = "(SELECT id AS x FROM a ORDER BY x LIMIT 1) UNION SELECT id FROM b";
    assert_eq!(order_by_in(sql, left_operand).unwrap(), "a.id");
    let sql = "SELECT (SELECT id AS q FROM b ORDER BY row_alias, q LIMIT 1) FROM a";
    assert_eq!(order_by_in(sql, projected_subquery).unwrap(), "a.row_alias");
}

#[test]
fn group_by_prefers_a_local_from_column_then_an_output_name_then_an_outer_column() {
    let group_by = |sql| group_by_in(sql, |query| query);
    assert_eq!(
        group_by("SELECT id AS row_alias FROM a GROUP BY row_alias").unwrap(),
        "a.row_alias"
    );
    assert_eq!(group_by("SELECT id AS x FROM a GROUP BY x").unwrap(), "a.id");
    assert_eq!(group_by("SELECT id AS x FROM a GROUP BY (x)").unwrap(), "a.id");
    assert_eq!(group_by("SELECT id AS x FROM a GROUP BY argument_only").unwrap(), "none");
    assert_eq!(group_by("SELECT row_alias AS id FROM a GROUP BY a.id").unwrap(), "a.id");
    assert!(matches!(
        group_by("SELECT id AS x, id + 1 AS x FROM a GROUP BY x"),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
    let sql = "SELECT (SELECT id AS row_alias FROM b GROUP BY row_alias LIMIT 1) FROM a";
    assert_eq!(group_by_in(sql, projected_subquery).unwrap(), "b.id");
    let sql = "SELECT (SELECT count(*) FROM b GROUP BY row_alias) FROM a";
    assert_eq!(group_by_in(sql, projected_subquery).unwrap(), "a.row_alias");
}

#[test]
fn distinct_on_prefers_an_output_name_as_the_whole_key() {
    let sql = "SELECT DISTINCT ON (row_alias) id AS row_alias FROM a ORDER BY row_alias";
    assert_eq!(distinct_on(sql).unwrap(), "a.id");
    let sql = "SELECT DISTINCT ON (row_alias + 1) id AS row_alias FROM a";
    assert_eq!(distinct_on(sql).unwrap(), "none");
}
