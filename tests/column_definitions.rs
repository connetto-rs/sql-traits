//! Integration tests for derived column definitions.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]

use core::{fmt::Write as _, ops::ControlFlow};

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{
        Expr, FunctionArg, FunctionArgExpr, FunctionArguments, InTarget, Query, Select, SelectItem,
        SetExpr, SetOperator, Value, Visit, VisitMut, Visitor, VisitorMut,
    },
    dialect::{
        BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, GenericDialect,
        HiveDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, RedshiftSqlDialect,
        SQLiteDialect, SnowflakeDialect,
    },
};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/query_parsing.rs"));

const SCHEMA: &str = "
    CREATE TABLE a(payload TEXT, id INT);
    CREATE TABLE b(payload JSON, id INT);
";

fn schema_db() -> ParserDB {
    ParserDB::parse::<GenericDialect>(SCHEMA).expect("schema parses")
}

fn query(sql: &str) -> Query {
    query_with(&GenericDialect {}, sql)
}

fn reference(reference: &str) -> Expr {
    let query = query(&format!("SELECT {reference} FROM placeholder"));
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    match &select.projection[0] {
        SelectItem::UnnamedExpr(expression) => expression.clone(),
        other => panic!("expected an expression projection, got {other:?}"),
    }
}

fn scope_with_independent_lifetimes<'query, 'database>(
    query: &'query Query,
    database: &'database ParserDB,
) -> ColumnScope<'query, 'database, ParserDB> {
    ColumnScope::from_query(query, database, IdentifierCase::AsWritten).expect("scope builds")
}

fn assert_copy<T: Copy>(_: T) {}

#[test]
fn recursive_cte_exposes_the_anchor_and_recursive_definitions() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE categories(id INT, parent_id INT);")
        .expect("schema parses");
    let query = query(
        "WITH RECURSIVE tree AS (\
            SELECT id, 0 AS depth FROM categories \
            UNION ALL \
            SELECT c.id, t.depth + 1 FROM categories c \
            JOIN tree t ON c.parent_id = t.id\
         ) SELECT t.depth FROM tree t",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(output_reference) = &select.projection[0] else {
        panic!("expected an expression projection")
    };
    let Some(ColumnDefinition::RecursiveUnion { anchor, recursive }) =
        scope.resolve_column_definition(output_reference).expect("definition resolves")
    else {
        panic!("expected a recursive union definition")
    };
    let ColumnDefinition::Expression { expression: anchor, .. } = anchor.definition() else {
        panic!("expected an anchor expression")
    };
    let ColumnDefinition::Expression { expression: recursive_expression, scope: recursive_scope } =
        recursive.definition()
    else {
        panic!("expected a recursive expression")
    };
    assert_eq!(anchor.to_string(), "0");
    assert_eq!(recursive_expression.to_string(), "t.depth + 1");
    assert!(matches!(
        recursive_scope
            .resolve_column_definition(&reference("t.depth"))
            .expect("seeded input resolves"),
        Some(ColumnDefinition::Expression { expression, .. })
            if expression.to_string() == "0"
    ));
}
#[test]
fn recursive_ctes_with_non_enumerable_arms_remain_opaque() {
    let db = schema_db();
    for sql in [
        "WITH RECURSIVE c(x) AS (\
             VALUES (1) UNION ALL SELECT x FROM c\
         ) SELECT c.x FROM c",
        "WITH RECURSIVE c(x) AS (\
             SELECT 1 UNION ALL VALUES (2)\
         ) SELECT c.x FROM c",
        "WITH RECURSIVE c(x, y) AS (\
             SELECT 1 UNION ALL SELECT x FROM c\
         ) SELECT c.x FROM c",
    ] {
        let query = query(sql);
        let scope =
            ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
        assert!(matches!(
            scope.resolve_column_definition(&reference("c.x")).expect("definition resolves"),
            Some(ColumnDefinition::Opaque)
        ));
    }
}

#[test]
fn nested_set_operations_preserve_every_definition() {
    let db = schema_db();
    let query = query(
        "WITH v AS (\
            SELECT 1 AS n \
            UNION ALL (SELECT 2 INTERSECT SELECT 3)\
         ) SELECT v.n FROM v",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
        panic!("expected an expression projection")
    };
    let Some(ColumnDefinition::SetOperation { operator, left, right }) =
        scope.resolve_column_definition(reference).expect("definition resolves")
    else {
        panic!("expected a set operation definition")
    };
    assert_eq!(operator, SetOperator::Union);
    assert!(matches!(
        left.definition(),
        ColumnDefinition::Expression { expression, .. } if expression.to_string() == "1"
    ));
    let ColumnDefinition::SetOperation { operator, left, right } = right.definition() else {
        panic!("expected a nested set operation")
    };
    assert_eq!(operator, SetOperator::Intersect);
    assert!(matches!(
        left.definition(),
        ColumnDefinition::Expression { expression, .. } if expression.to_string() == "2"
    ));
    assert!(matches!(
        right.definition(),
        ColumnDefinition::Expression { expression, .. } if expression.to_string() == "3"
    ));
}

#[test]
fn base_definition_carries_the_table_and_original_column() {
    let db = schema_db();
    let query = query("WITH v AS (SELECT payload AS copy FROM b) SELECT v.copy FROM v");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
        panic!("expected an expression projection")
    };
    let Some(ColumnDefinition::Base { table, column, .. }) =
        scope.resolve_column_definition(reference).expect("definition resolves")
    else {
        panic!("expected a base column definition")
    };
    assert_eq!(table.table_name(), "b");
    assert_eq!(column.column_name(), "payload");
}

#[test]
fn query_and_database_lifetimes_are_independent() {
    let db = schema_db();
    let query = query("SELECT payload FROM b");
    let scope = scope_with_independent_lifetimes(&query, &db);
    assert!(matches!(
        scope.resolve_column_definition(&reference("payload")).expect("definition resolves"),
        Some(ColumnDefinition::Base { .. })
    ));
}

#[test]
fn table_scope_returns_base_definitions() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE docs(body TEXT);
         CREATE TABLE \"Docs\" (\"Body\" INT);
         CREATE TABLE app.docs(body TEXT);",
    )
    .expect("schema parses");
    for (schema, (table_name, quoted), references) in [
        (None, ("docs", false), &["body", "docs.body"][..]),
        (None, ("Docs", true), &["\"Body\"", "\"Docs\".\"Body\""][..]),
        (Some("app"), ("docs", false), &["body", "docs.body", "app.docs.body"][..]),
    ] {
        let target = match schema {
            Some(schema) => TargetName::new(table_name, quoted).with_schema(schema, false),
            None => TargetName::new(table_name, quoted),
        };
        let table = db
            .table_by_target(target, IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists");
        let scope = ColumnScope::for_table(table, &db, IdentifierCase::AsWritten);
        for reference_sql in references {
            let Some(ColumnDefinition::Base { table: resolved_table, column, .. }) = scope
                .resolve_column_definition(&reference(reference_sql))
                .expect("definition resolves")
            else {
                panic!("expected a base column definition")
            };
            assert!(core::ptr::eq(resolved_table, table));
            assert_eq!(
                column.column_name(),
                reference_sql.rsplit('.').next().unwrap().trim_matches('"')
            );
        }
    }
}

#[test]
fn computed_definitions_cross_every_derived_relation_kind() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE invoices(amount NUMERIC);
         CREATE VIEW doubled_view AS SELECT amount * 2 AS doubled FROM invoices;
         CREATE MATERIALIZED VIEW doubled_snapshot AS
             SELECT amount * 2 AS doubled FROM invoices;",
    )
    .expect("schema parses");
    for sql in [
        "WITH doubled_rows AS (\
            SELECT amount * 2 AS doubled FROM invoices\
         ) SELECT doubled FROM doubled_rows",
        "SELECT doubled FROM (SELECT amount * 2 AS doubled FROM invoices) doubled_rows",
        "SELECT doubled FROM doubled_view",
        "SELECT doubled FROM doubled_snapshot",
    ] {
        let query = query(sql);
        let scope =
            ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected a plain SELECT body")
        };
        let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
            panic!("expected an expression projection")
        };
        assert!(matches!(
            scope.resolve_column_definition(reference).expect("definition resolves"),
            Some(ColumnDefinition::Expression { expression, .. })
                if expression.to_string() == "amount * 2"
        ));
    }
}

#[test]
fn expression_definition_resolves_its_base_inputs() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE invoices(amount NUMERIC);")
        .expect("schema parses");
    let query = query("SELECT v.doubled FROM (SELECT amount * 2 AS doubled FROM invoices) v");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
        scope.resolve_column_definition(&reference("v.doubled")).expect("definition resolves")
    else {
        panic!("expected an expression definition")
    };
    assert_eq!(expression.to_string(), "amount * 2");
    let Some(ColumnDefinition::Base { table, column, .. }) =
        defining_scope.resolve_column_definition(&reference("amount")).expect("input resolves")
    else {
        panic!("expected a base input")
    };
    assert_eq!(table.table_name(), "invoices");
    assert_eq!(column.column_name(), "amount");
}

#[test]
fn opaque_relations_are_distinct_from_missing_columns() {
    let db = schema_db();
    let opaque = query("SELECT g.value FROM generate_series(1, 10) g");
    let opaque_scope =
        ColumnScope::from_query(&opaque, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(select) = opaque.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
        panic!("expected an expression projection")
    };
    assert!(matches!(
        opaque_scope.resolve_column_definition(reference).expect("definition resolves"),
        Some(ColumnDefinition::Opaque)
    ));

    let missing = query("SELECT no.value FROM b");
    let missing_scope =
        ColumnScope::from_query(&missing, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(select) = missing.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    let SelectItem::UnnamedExpr(reference) = &select.projection[0] else {
        panic!("expected an expression projection")
    };
    assert!(
        missing_scope.resolve_column_definition(reference).expect("definition resolves").is_none()
    );
}

fn assert_opaque_resolution(from_clause: &str, reference_sql: &str, expected: bool) {
    let db = schema_db();
    let query = query(&format!("SELECT {reference_sql} FROM {from_clause}"));
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let definition =
        scope.resolve_column_definition(&reference(reference_sql)).expect("definition resolves");
    match definition {
        Some(ColumnDefinition::Opaque) => {
            assert!(expected, "{reference_sql} unexpectedly resolved opaque from {from_clause}");
        }
        None => {
            assert!(!expected, "{reference_sql} unexpectedly had no definition from {from_clause}");
        }
        Some(_) => panic!("expected an opaque or missing definition"),
    }
}

#[test]
fn opaque_relation_qualifiers_match_exactly() {
    for from_clause in
        ["generate_series(1, 10) AS g", "TABLE(FUN(1)) AS g", "LATERAL generate_series(1, 10) AS g"]
    {
        for (reference_sql, expected) in
            [("value", true), ("g.value", true), ("other.value", false)]
        {
            assert_opaque_resolution(from_clause, reference_sql, expected);
        }
    }
    assert_opaque_resolution("TABLE(FUN(1))", "FUN.value", true);
    assert_opaque_resolution("TABLE(1)", "value", true);
    assert_opaque_resolution("LATERAL generate_series(1)", "generate_series.value", true);
}

#[test]
fn opaque_alias_shadows_the_parent_scope() {
    let db =
        ParserDB::parse::<GenericDialect>("CREATE TABLE a (payload INT)").expect("schema parses");
    let query = query(
        "SELECT d.nested_payload FROM (\
             SELECT u.payload, \
                    (SELECT u.payload FROM UNNEST(ARRAY[1]) AS u(payload)) \
             FROM a AS u\
         ) AS d(outer_payload, nested_payload)",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) = scope
        .resolve_column_definition(&reference("d.nested_payload"))
        .expect("output definition resolves")
    else {
        panic!("expected a scalar subquery definition")
    };
    let nested_scope = defining_scope
        .scope_for_select(scalar_nested_select(expression))
        .expect("scalar subquery scope is indexed");
    match nested_scope
        .resolve_column_definition(&reference("u.payload"))
        .expect("definition resolves")
    {
        Some(ColumnDefinition::Opaque) => {}
        Some(ColumnDefinition::Base { table, .. }) => {
            panic!("expected an opaque definition, got base table {}", table.table_name())
        }
        Some(_) => panic!("expected an opaque definition"),
        None => panic!("expected an opaque definition, got no definition"),
    }
}
#[test]
fn opaque_relation_qualifiers_preserve_quotes() {
    for (reference_sql, expected) in [("\"G\".value", true), ("G.value", false), ("g.value", false)]
    {
        assert_opaque_resolution("generate_series(1, 10) AS \"G\"", reference_sql, expected);
    }
}

#[test]
fn unaliased_nested_join_preserves_outer_correlation() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a (payload TEXT);
         CREATE TABLE b (id INT);",
    )
    .expect("schema parses");
    let query = query(
        "SELECT d.nested_payload FROM (\
             SELECT (SELECT a.payload FROM (b JOIN b AS b2 ON true)) \
             FROM a\
         ) AS d(nested_payload)",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) = scope
        .resolve_column_definition(&reference("d.nested_payload"))
        .expect("output definition resolves")
    else {
        panic!("expected a scalar subquery definition")
    };
    let nested_scope = defining_scope
        .scope_for_select(scalar_nested_select(expression))
        .expect("scalar subquery scope is indexed");
    match nested_scope
        .resolve_column_definition(&reference("a.payload"))
        .expect("definition resolves")
    {
        Some(ColumnDefinition::Base { table, .. }) => assert_eq!(table.table_name(), "a"),
        Some(ColumnDefinition::Opaque) => {
            panic!("expected base table a, got an opaque definition")
        }
        Some(_) => panic!("expected base table a"),
        None => panic!("expected base table a, got no definition"),
    }
}

#[test]
fn nested_join_opaque_qualifiers_match_exactly() {
    for (reference_sql, expected) in
        [("b.value", true), ("b2.value", true), ("unknown.value", false)]
    {
        assert_opaque_resolution("(b JOIN b AS b2 ON true)", reference_sql, expected);
    }
    for (reference_sql, expected) in
        [("joined.value", true), ("b.value", false), ("b2.value", false)]
    {
        assert_opaque_resolution("(b JOIN b AS b2 ON true) AS joined", reference_sql, expected);
    }
    for (reference_sql, expected) in
        [("\"B2\".value", true), ("B2.value", false), ("b2.value", false)]
    {
        assert_opaque_resolution("(b JOIN b AS \"B2\" ON true)", reference_sql, expected);
    }
}

#[test]
fn three_part_opaque_qualifiers_require_matching_schema_and_relation() {
    for (from_clause, reference_sql, expected) in [
        ("generate_series(1, 10) AS g", "public.g.value", false),
        ("missing_schema.missing_table", "missing_schema.missing_table.value", true),
        ("missing_schema.missing_table", "other_schema.missing_table.value", false),
    ] {
        assert_opaque_resolution(from_clause, reference_sql, expected);
    }
}
#[test]
fn non_enumerable_set_bodies_remain_opaque() {
    for from_clause in
        ["(SELECT 1 AS x UNION ALL VALUES (2)) AS d", "(VALUES (1) UNION ALL SELECT 2) AS d(x)"]
    {
        assert_opaque_resolution(from_clause, "d.x", true);
    }

    let db =
        ParserDB::parse::<GenericDialect>("CREATE VIEW v(x) AS VALUES (1);").expect("view parses");
    let query = query("SELECT v.x FROM v");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert!(matches!(
        scope.resolve_column_definition(&reference("v.x")).expect("definition resolves"),
        Some(ColumnDefinition::Opaque)
    ));
}

#[test]
fn non_select_queries_have_no_projection_source() {
    let db = schema_db();
    assert!(query("VALUES (1)").projection_source_table(&db).expect("source resolves").is_none());
}
#[test]
fn projection_shapes_do_not_invent_row_identity() {
    let db = schema_db();
    for query in [
        query_with(&BigQueryDialect {}, "SELECT myfunc().* FROM a"),
        query("SELECT stack(2, 1, 2) AS (x, y) FROM a"),
    ] {
        assert!(query.projection_source_table(&db).expect("source resolves").is_none());
        let scope =
            ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
        assert!(
            scope
                .resolve_column_definition(&reference("x"))
                .expect("definition resolves")
                .is_none()
        );
    }

    let query = query("SELECT public.a.* FROM a");
    let table =
        query.projection_source_table(&db).expect("source resolves").expect("source exists");
    assert_eq!(table.table_name(), "a");
}

fn expression_input_table(
    db: &ParserDB,
    sql: &str,
    output_reference: &str,
    input_reference: &str,
) -> Option<String> {
    let query = query(sql);
    let scope =
        ColumnScope::from_query(&query, db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { scope: defining_scope, .. }) = scope
        .resolve_column_definition(&reference(output_reference))
        .expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    match defining_scope
        .resolve_column_definition(&reference(input_reference))
        .expect("input definition resolves")
    {
        Some(ColumnDefinition::Base { table, .. }) => Some(table.table_name().to_owned()),
        None => None,
        Some(_) => panic!("expected a base or missing input definition"),
    }
}

#[test]
fn parent_lookup_obeys_local_shadowing_and_missing_columns() {
    let db = schema_db();
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a AS b, \
             LATERAL (SELECT b.payload || '' AS x FROM b AS b) AS d",
            "d.x",
            "b.payload",
        )
        .as_deref(),
        Some("b")
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a AS b, \
             LATERAL (\
                 SELECT b.payload || '' AS x FROM (SELECT id FROM b) AS b\
             ) AS d",
            "d.x",
            "b.payload",
        ),
        None
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a AS outer_a, \
             LATERAL (SELECT outer_a.payload || '' AS x FROM b AS local_b) AS d",
            "d.x",
            "outer_a.payload",
        )
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a, \
             LATERAL (\
                 SELECT payload || '' AS x FROM (SELECT id FROM b) AS local_b\
             ) AS d",
            "d.x",
            "payload",
        )
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a, \
             LATERAL (\
                 SELECT public.a.payload || '' AS x \
                 FROM missing_schema.missing_table\
             ) AS d",
            "d.x",
            "public.a.payload",
        )
        .as_deref(),
        Some("a")
    );
}

#[test]
fn lateral_subqueries_see_only_prior_from_entries() {
    let db = schema_db();
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a, LATERAL (SELECT a.payload || '' AS x) AS d",
            "d.x",
            "a.payload",
        )
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM LATERAL (SELECT a.payload || '' AS x) AS d, a",
            "d.x",
            "a.payload",
        ),
        None
    );
}
#[test]
fn outer_join_nullability_keeps_column_definitions() {
    let db = schema_db();
    let query = query(
        "SELECT d.id FROM \
         (SELECT id FROM a) AS d \
         RIGHT JOIN b ON true",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Base { table, .. }) =
        scope.resolve_column_definition(&reference("d.id")).expect("definition resolves")
    else {
        panic!("expected a base definition")
    };
    assert_eq!(table.table_name(), "a");
}

#[test]
fn non_lateral_subqueries_skip_siblings_and_reach_grandparents() {
    let db = schema_db();
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a, (SELECT a.payload || '' AS x) AS d",
            "d.x",
            "a.payload",
        ),
        None
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a AS outer_a, \
             LATERAL (\
                 SELECT inner_d.x \
                 FROM b AS local_b, \
                      (SELECT outer_a.payload || '' AS x) AS inner_d\
             ) AS d",
            "d.x",
            "outer_a.payload",
        )
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a AS outer_a, \
             LATERAL (\
                 SELECT inner_d.x \
                 FROM b AS local_b, \
                      (SELECT local_b.payload || '' AS x) AS inner_d\
             ) AS d",
            "d.x",
            "local_b.payload",
        ),
        None
    );
}

#[test]
fn nested_ctes_inherit_their_query_parent() {
    let db = schema_db();
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a, \
             LATERAL (\
                 WITH c AS (SELECT a.payload || '' AS x) \
                 SELECT c.x FROM c\
             ) AS d",
            "d.x",
            "a.payload",
        )
        .as_deref(),
        Some("a")
    );
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT d.x FROM a AS outer_a, \
             LATERAL (\
                 WITH c AS (SELECT outer_a.payload || '' AS x) \
                 SELECT c.x FROM b AS outer_a, c\
             ) AS d",
            "d.x",
            "outer_a.payload",
        )
        .as_deref(),
        Some("a")
    );
}
#[test]
fn scalar_subquery_ctes_index_their_definitions_and_body() {
    let db = schema_db();
    let query = query(
        "SELECT d.x FROM (\
             SELECT (\
                 WITH c AS (SELECT a.payload FROM a) \
                 SELECT c.payload FROM c\
             ) AS x\
         ) AS d",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let Expr::Subquery(nested_query) = expression else { panic!("expected a scalar subquery") };
    let with = nested_query.with.as_ref().expect("expected a CTE");
    for (select, input_reference) in [
        (select_body(&with.cte_tables[0].query), "a.payload"),
        (select_body(nested_query), "c.payload"),
    ] {
        let nested_scope =
            defining_scope.scope_for_select(select).expect("nested scope is indexed");
        let Some(ColumnDefinition::Base { table, .. }) = nested_scope
            .resolve_column_definition(&reference(input_reference))
            .expect("input resolves")
        else {
            panic!("expected a base input")
        };
        assert_eq!(table.table_name(), "a");
    }
}

#[test]
fn stored_views_do_not_capture_the_calling_scope() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(payload TEXT);
         CREATE VIEW detached AS SELECT caller.payload || '' AS x;",
    )
    .expect("schema parses");
    assert_eq!(
        expression_input_table(
            &db,
            "SELECT detached.x FROM a AS caller, detached",
            "detached.x",
            "caller.payload",
        ),
        None
    );
}

fn select_body(query: &Query) -> &sqlparser::ast::Select {
    let SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected a plain SELECT body")
    };
    select
}

fn scalar_nested_select(expression: &Expr) -> &sqlparser::ast::Select {
    let Expr::Subquery(query) = expression else { panic!("expected a scalar subquery") };
    select_body(query)
}

fn exists_nested_select(expression: &Expr) -> &sqlparser::ast::Select {
    let Expr::Exists { subquery, .. } = expression else { panic!("expected an EXISTS expression") };
    select_body(subquery)
}

fn in_nested_select(expression: &Expr) -> &sqlparser::ast::Select {
    let Expr::In { target: InTarget::Subquery(subquery), .. } = expression else {
        panic!("expected an IN subquery")
    };
    select_body(subquery)
}

fn binary_nested_select(expression: &Expr) -> &sqlparser::ast::Select {
    let Expr::BinaryOp { left, .. } = expression else { panic!("expected a binary expression") };
    scalar_nested_select(left)
}

fn function_nested_select(expression: &Expr) -> &sqlparser::ast::Select {
    let Expr::Function(function) = expression else { panic!("expected a function") };
    let FunctionArguments::List(arguments) = &function.args else {
        panic!("expected function arguments")
    };
    let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Subquery(query)))) =
        arguments.args.first()
    else {
        panic!("expected a scalar subquery argument")
    };
    select_body(query)
}

fn indexed_nested_input_table(
    db: &ParserDB,
    sql: &str,
    output_reference: &str,
    input_reference: &str,
    nested_select: for<'expression> fn(&'expression Expr) -> &'expression sqlparser::ast::Select,
) -> Option<String> {
    let query = query(sql);
    let scope =
        ColumnScope::from_query(&query, db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) = scope
        .resolve_column_definition(&reference(output_reference))
        .expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let nested_scope = defining_scope.scope_for_select(nested_select(expression))?;
    match nested_scope
        .resolve_column_definition(&reference(input_reference))
        .expect("nested input resolves")
    {
        Some(ColumnDefinition::Base { table, .. }) => Some(table.table_name().to_owned()),
        None => None,
        Some(_) => panic!("expected a base or missing nested input"),
    }
}

struct IndexedSelectOracle<'scope, 'query, 'database> {
    scope: ColumnDefinitionScope<'scope, 'query, 'database, ParserDB>,
    select_count: usize,
    resolve_input: bool,
    injected_only: bool,
    context: Option<String>,
}

fn is_injected_select(select: &Select) -> bool {
    matches!(
        select.projection.as_slice(),
        [SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts))]
            if matches!(parts.as_slice(), [table, column]
                if table.value == "a" && column.value == "payload")
    )
}

impl Visitor for IndexedSelectOracle<'_, '_, '_> {
    type Break = ();

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
        if self.injected_only && !is_injected_select(select) {
            return ControlFlow::Continue(());
        }
        let nested_scope = self.scope.scope_for_select(select).unwrap_or_else(|| {
            let context = self.context.as_deref().unwrap_or("expression");
            panic!("nested scope {} is indexed for `{select}` in `{context}`", self.select_count)
        });
        if self.resolve_input {
            let Some(ColumnDefinition::Base { table, .. }) = nested_scope
                .resolve_column_definition(&reference("a.payload"))
                .expect("nested input resolves")
            else {
                panic!("expected a base nested input")
            };
            assert_eq!(table.table_name(), "a");
        }
        self.select_count += 1;
        ControlFlow::Continue(())
    }
}

fn assert_expression_nested_selects_indexed(
    query: &Query,
    expected_select_count: usize,
    resolve_input: bool,
) {
    let db = schema_db();
    let scope =
        ColumnScope::from_query(query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let mut oracle = IndexedSelectOracle {
        scope: defining_scope,
        select_count: 0,
        resolve_input,
        injected_only: false,
        context: None,
    };
    assert!(matches!(expression.visit(&mut oracle), ControlFlow::Continue(())));
    assert_eq!(oracle.select_count, expected_select_count);
}

#[test]
fn query_level_order_by_nested_select_is_indexed() {
    let query = query(
        "SELECT d.x FROM (\
             SELECT (SELECT a.payload FROM a ORDER BY (SELECT b.id FROM b)) AS x\
         ) AS d",
    );
    assert_expression_nested_selects_indexed(&query, 2, true);
}

#[derive(Default)]
struct NestedQueryInjector {
    query_count: usize,
    correlated: bool,
}

impl NestedQueryInjector {
    fn correlated() -> Self {
        Self { query_count: 0, correlated: true }
    }
}

impl VisitorMut for NestedQueryInjector {
    type Break = ();

    fn pre_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Self::Break> {
        let is_needle = match expression {
            Expr::Identifier(identifier) => identifier.value == "needle",
            Expr::Value(value) => {
                matches!(&value.value, Value::SingleQuotedString(text) if text == "needle")
            }
            _ => false,
        };
        if !is_needle {
            return ControlFlow::Continue(());
        }
        let sql = if self.correlated { "SELECT a.payload" } else { "SELECT a.payload FROM a" };
        *expression = Expr::Subquery(Box::new(query(sql)));
        self.query_count += 1;
        ControlFlow::Continue(())
    }
}

fn inject_nested_queries(mut query: Query, correlated: bool) -> (Query, usize) {
    let mut injector =
        if correlated { NestedQueryInjector::correlated() } else { NestedQueryInjector::default() };
    assert!(matches!(VisitMut::visit(&mut query, &mut injector), ControlFlow::Continue(())));
    assert!(injector.query_count > 0);
    (query, injector.query_count)
}

fn assert_expression_children_indexed(query: Query) {
    let (query, query_count) = inject_nested_queries(query, false);
    assert_expression_nested_selects_indexed(&query, query_count, true);
}

fn assert_injected_container_children_indexed(query: &Query, query_count: usize) {
    let db = schema_db();
    let scope =
        ColumnScope::from_query(query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { scope: defining_scope, .. }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition for `{query}`")
    };
    let root = select_body(query);
    let sqlparser::ast::TableFactor::Derived { subquery, .. } = &root.from[0].relation else {
        panic!("expected a derived relation")
    };
    let mut oracle = IndexedSelectOracle {
        scope: defining_scope,
        select_count: 0,
        resolve_input: true,
        injected_only: true,
        context: Some(query.to_string()),
    };
    assert!(matches!(subquery.visit(&mut oracle), ControlFlow::Continue(())));
    assert_eq!(oracle.select_count, query_count, "{query}");
}

fn assert_container_fixtures(fixtures: Vec<(Query, bool)>) {
    let fixtures: Vec<_> = fixtures
        .into_iter()
        .map(|(query, correlated)| inject_nested_queries(query, correlated))
        .collect();
    for (query, query_count) in fixtures {
        assert_injected_container_children_indexed(&query, query_count);
    }
}

fn assert_dialect_expression_children_indexed<D: Dialect>(dialect: &D, expressions: &[&str]) {
    for expression in expressions {
        let query =
            query_with(dialect, &format!("SELECT d.x FROM (SELECT {expression} AS x) AS d"));
        assert_expression_children_indexed(query);
    }
}

#[test]
fn omitted_query_containers_follow_the_sqlparser_visitor() {
    let mut fixtures: Vec<_> = [
        "SELECT d.x FROM (SELECT needle AS x FROM a ORDER BY needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a ORDER BY ALL) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a LIMIT needle OFFSET needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a LIMIT needle, needle) AS d",
    ]
    .into_iter()
    .map(|sql| (query(sql), true))
    .collect();
    fixtures.push((
        query_with(
            &PostgreSqlDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a \
                 FETCH FIRST 'needle' ROWS ONLY\
             ) AS d",
        ),
        true,
    ));
    fixtures.push((
        query_with(
            &ClickHouseDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a \
                 SETTINGS max_threads = needle\
             ) AS d",
        ),
        true,
    ));
    fixtures.push((
        query_with(
            &ClickHouseDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a \
                 ORDER BY needle WITH FILL INTERPOLATE (value AS needle)\
             ) AS d",
        ),
        true,
    ));
    fixtures.extend(
        [
            "LIMIT needle OFFSET needle",
            "WHERE needle",
            "ORDER BY needle",
            "SELECT needle AS x",
            "EXTEND needle AS y",
            "SET x = needle",
            "AGGREGATE SUM(needle) GROUP BY needle",
            "TABLESAMPLE SYSTEM (needle) OFFSET needle",
            "CALL f(needle)",
            "PIVOT(SUM(needle) FOR id IN (1))",
            "UNPIVOT(revenue FOR quarter IN (Q1, Q2))",
            "JOIN b ON needle",
            "UNION ALL (SELECT needle FROM a)",
            "INTERSECT DISTINCT (SELECT needle FROM a)",
            "EXCEPT DISTINCT (SELECT needle FROM a)",
        ]
        .into_iter()
        .map(|pipe| {
            (query(&format!("SELECT d.x FROM (SELECT needle x FROM a |> {pipe}) AS d")), true)
        }),
    );
    let (values, values_count) = inject_nested_queries(
        query("SELECT d.x FROM (SELECT (VALUES (needle)) AS x FROM a) AS d"),
        false,
    );
    assert_container_fixtures(fixtures);
    assert_expression_nested_selects_indexed(&values, values_count, true);
}

#[test]
fn omitted_select_containers_follow_the_sqlparser_visitor() {
    let mut fixtures: Vec<_> = [
        "SELECT d.x FROM (SELECT DISTINCT ON (needle) needle AS x FROM a) AS d",
        "SELECT d.x FROM (SELECT needle AS x, a.* FROM a) AS d",
        "SELECT d.x FROM (\
             SELECT (\
                 SELECT a.id FROM a \
                 START WITH needle CONNECT BY needle = PRIOR untouched\
             ) AS x\
         ) AS d",
        "SELECT d.x FROM (SELECT TOP (needle) needle AS x FROM a) AS d",
        "SELECT d.x FROM (\
             SELECT needle AS x FROM a \
             WINDOW w AS (\
                 PARTITION BY needle ORDER BY needle \
                 ROWS BETWEEN needle PRECEDING AND needle FOLLOWING\
             )\
         ) AS d",
    ]
    .into_iter()
    .map(|sql| (query(sql), true))
    .collect();
    fixtures.push((
        query_with(
            &HiveDialect {},
            "SELECT d.x FROM (\
                 SELECT (\
                     SELECT a.id FROM a \
                     LATERAL VIEW explode(needle) t AS value\
                 ) AS x\
             ) AS d",
        ),
        true,
    ));
    assert_container_fixtures(fixtures);
}

#[allow(clippy::too_many_lines)]
#[test]
fn omitted_relation_containers_follow_the_sqlparser_visitor() {
    let mut fixtures: Vec<_> = [
        "SELECT d.x FROM (SELECT needle AS x FROM a JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a INNER JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a LEFT OUTER JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a RIGHT OUTER JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a FULL OUTER JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a CROSS JOIN b) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a CROSS APPLY b) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a OUTER APPLY b) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a SEMI JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a LEFT SEMI JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a RIGHT SEMI JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a ANTI JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a LEFT ANTI JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a RIGHT ANTI JOIN b ON needle) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a ARRAY JOIN b) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a LEFT ARRAY JOIN b) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a INNER ARRAY JOIN b) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a, f(needle) AS f) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a, TABLE(needle) AS f) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a, LATERAL f(needle) AS f) AS d",
        "SELECT d.x FROM (SELECT needle AS x FROM a, UNNEST(ARRAY[needle]) AS u) AS d",
    ]
    .into_iter()
    .map(|sql| (query(sql), true))
    .collect();
    fixtures.extend(
        [
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM a PIVOT(SUM(needle) FOR needle IN (needle))\
             ) AS d",
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM a UNPIVOT(needle FOR kind IN (needle))\
             ) AS d",
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM a MATCH_RECOGNIZE(\
                     PARTITION BY needle \
                     ORDER BY needle \
                     MEASURES needle AS value \
                     PATTERN (x) \
                     DEFINE x AS needle\
                 )\
             ) AS d",
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM XMLTABLE(\
                     needle PASSING needle \
                     COLUMNS value TEXT PATH needle DEFAULT needle\
                 ) AS xt\
             ) AS d",
        ]
        .into_iter()
        .map(|sql| (query(sql), false)),
    );
    fixtures.push((
        query(
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM XMLTABLE(\
                     XMLNAMESPACES('needle' AS n), needle PASSING needle \
                     COLUMNS ordinal FOR ORDINALITY\
                 ) AS xt\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &PostgreSqlDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM a TABLESAMPLE SYSTEM (needle) OFFSET needle\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &SnowflakeDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM SEMANTIC_VIEW(\
                     model \
                     DIMENSIONS needle \
                     METRICS needle \
                     FACTS needle \
                     WHERE needle\
                 ) AS sv\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &BigQueryDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM a FOR SYSTEM_TIME AS OF needle\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &ClickHouseDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM table_function(needle, SETTINGS setting = needle)\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query(
            "SELECT d.x FROM (\
                 SELECT needle x \
                 FROM a AS sampled TABLESAMPLE SYSTEM (needle)\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &MySqlDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM JSON_TABLE(needle, '$[*]' COLUMNS(value INT PATH '$')) AS jt\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &MsSqlDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x \
                 FROM OPENJSON(needle) WITH (value INT) AS jt\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &RedshiftSqlDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a, UNPIVOT needle AS value AT attribute\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &MySqlDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a STRAIGHT_JOIN b ON needle\
             ) AS d",
        ),
        true,
    ));
    fixtures.push((
        query_with(
            &SnowflakeDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a ASOF JOIN b \
                 MATCH_CONDITION (needle >= needle) ON needle\
             ) AS d",
        ),
        true,
    ));
    fixtures.push((
        query_with(
            &SnowflakeDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a CHANGES(INFORMATION => DEFAULT) \
                 AT(TIMESTAMP => TO_TIMESTAMP_TZ('needle')) \
                 END(TIMESTAMP => TO_TIMESTAMP_TZ('needle'))\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &SnowflakeDialect {},
            "SELECT d.x FROM (SELECT needle AS x FROM a[needle]) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &SnowflakeDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a \
                 PIVOT(SUM(needle) FOR id IN (ANY ORDER BY needle))\
             ) AS d",
        ),
        false,
    ));
    fixtures.push((
        query_with(
            &SnowflakeDialect {},
            "SELECT d.x FROM (\
                 SELECT needle AS x FROM a \
                 PIVOT(SUM(needle) FOR id IN (SELECT needle FROM a))\
             ) AS d",
        ),
        false,
    ));
    assert_container_fixtures(fixtures);
}

#[test]
fn expression_container_families_follow_the_sqlparser_visitor() {
    for expression in [
        "NOT (SELECT a.payload FROM a)",
        "CAST((SELECT a.payload FROM a) AS TEXT)",
        "(SELECT a.payload FROM a) IS NULL",
        "(SELECT a.payload FROM a) IS DISTINCT FROM ''",
        "(SELECT a.payload FROM a) AT TIME ZONE 'UTC'",
        "(SELECT a.payload FROM a) BETWEEN '' AND ''",
        "1 IN (0, (SELECT a.payload FROM a))",
        "POSITION((SELECT a.payload FROM a) IN 'text')",
        "SUBSTRING('text' FROM (SELECT a.id FROM a))",
        "CONVERT((SELECT a.payload FROM a), TEXT)",
        "ARRAY[0][(SELECT a.id FROM a)]",
        "ARRAY[0][(SELECT a.id FROM a):]",
        "payload:[(SELECT a.payload FROM a)]",
        "TRIM((SELECT a.payload FROM a))",
        "OVERLAY('text' PLACING (SELECT a.payload FROM a) FROM 1)",
        "CASE WHEN true THEN (SELECT a.payload FROM a) ELSE '' END",
        "ARRAY[(SELECT a.payload FROM a)]",
        "((SELECT a.payload FROM a), '')",
        "COALESCE((SELECT a.payload FROM a), '')",
        "COUNT(*) FILTER (WHERE EXISTS (SELECT 1 FROM a))",
        "COUNT(*) OVER (PARTITION BY (SELECT a.payload FROM a))",
        "PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY (SELECT a.payload FROM a))",
        "{'key': (SELECT a.payload FROM a)}",
        "MAP {'key': (SELECT a.payload FROM a)}",
        "ARRAY(SELECT a.payload FROM a)",
        "COUNT(1 ORDER BY (SELECT a.id FROM a))",
        "COUNT(*) OVER (ORDER BY (SELECT a.id FROM a))",
        "COUNT(*) OVER (ROWS BETWEEN (SELECT a.id FROM a) PRECEDING AND CURRENT ROW)",
        "((SELECT a.payload FROM a))",
    ] {
        let query = query(&format!("SELECT d.x FROM (SELECT {expression} AS x) AS d"));
        assert_expression_nested_selects_indexed(&query, 1, true);
    }
}

#[test]
fn expression_container_children_follow_the_sqlparser_visitor() {
    for expression in [
        "NOT needle",
        "CAST(needle AS TEXT)",
        "needle IS FALSE",
        "needle IS NOT FALSE",
        "needle IS TRUE",
        "needle IS NOT TRUE",
        "needle IS NOT NULL",
        "needle IS UNKNOWN",
        "needle IS NOT UNKNOWN",
        "needle IS JSON",
        "needle IS NORMALIZED",
        "needle IS NULL",
        "needle IS DISTINCT FROM needle",
        "needle IS NOT DISTINCT FROM needle",
        "needle + needle",
        "needle LIKE needle",
        "needle ILIKE needle",
        "needle SIMILAR TO needle",
        "needle RLIKE needle",
        "needle = ANY(needle)",
        "needle = ALL(needle)",
        "needle IN UNNEST(needle)",
        "needle MEMBER OF (needle)",
        "needle AT TIME ZONE needle",
        "needle BETWEEN needle AND needle",
        "needle IN (needle, needle)",
        "POSITION(needle IN needle)",
        "EXTRACT(YEAR FROM needle)",
        "CEIL(needle)",
        "FLOOR(needle)",
        "needle COLLATE utf8",
        "INTERVAL needle DAY",
        "needle -> needle",
        "SUBSTRING(needle FROM needle FOR needle)",
        "CONVERT(needle, TEXT)",
        "ARRAY[needle][needle:needle:needle]",
        "needle:[needle]",
        "TRIM(needle FROM needle)",
        "needle:field",
        "OVERLAY(needle PLACING needle FROM needle FOR needle)",
        "CASE needle WHEN needle THEN needle ELSE needle END",
        "TRIM(needle, needle, needle)",
        "ARRAY[needle, needle]",
        "(needle, needle)",
        "(needle, untouched)",
        "(needle, a.payload)",
        "(needle, 1)",
        "(needle, DATE '2020-01-01')",
        "STRUCT(needle AS value)",
        "COALESCE(needle, needle)",
        "f(named => needle)",
        "f(needle).field",
        "f(needle, *)",
        "f(needle, a.*)",
        "f(needle := needle)",
        "ARRAY_AGG(needle ORDER BY needle LIMIT needle)",
        "ANY_VALUE(needle HAVING MAX needle)",
        "LISTAGG(needle ON OVERFLOW ERROR)",
        "LISTAGG(needle, ', ' ON OVERFLOW TRUNCATE '...' WITHOUT COUNT) WITHIN GROUP (ORDER BY needle)",
        "SUM(needle WHERE needle)",
        "GROUP_CONCAT(needle SEPARATOR ',')",
        "FIRST_VALUE(needle IGNORE NULLS) OVER ()",
        "f(* REPLACE (needle AS value))",
        "f(needle ORDER BY needle WITH FILL FROM needle TO needle STEP needle)",
        "COUNT(*) FILTER (WHERE needle)",
        "COUNT(*) OVER (PARTITION BY needle ORDER BY needle ROWS BETWEEN needle PRECEDING AND needle FOLLOWING)",
        "COUNT(needle) OVER named_window",
        "PERCENTILE_CONT(needle) WITHIN GROUP (ORDER BY needle)",
        "{'one': needle, 'two': needle}",
        "MAP {needle: needle}",
        "COUNT(needle ORDER BY needle)",
    ] {
        let query = query(&format!("SELECT d.x FROM (SELECT {expression} AS x) AS d"));
        assert_expression_children_indexed(query);
    }
}

#[test]
fn non_select_nested_queries_follow_the_sqlparser_visitor() {
    for (expression, select_count) in [
        ("EXISTS (VALUES (1))", 0),
        ("(WITH a AS (SELECT a.payload FROM a) (SELECT a.payload FROM a))", 2),
    ] {
        let query = query(&format!("SELECT d.x FROM (SELECT {expression} AS x) AS d"));
        assert_expression_nested_selects_indexed(&query, select_count, false);
    }
}

#[test]
fn in_table_targets_index_nested_queries_in_operands_and_arguments() {
    for predicate in [
        "needle IN b",
        "f(needle) IN json_each(needle, needle)",
        "f(needle) NOT IN json_each(needle)",
        "needle IN json_each()",
    ] {
        let query = query_with(
            &SQLiteDialect {},
            &format!("SELECT d.x FROM (SELECT needle AS x FROM a WHERE {predicate}) AS d"),
        );
        let (query, query_count) = inject_nested_queries(query, true);
        assert_injected_container_children_indexed(&query, query_count);
    }
}

#[test]
fn dialect_specific_expression_children_follow_the_sqlparser_visitor() {
    assert_dialect_expression_children_indexed(
        &PostgreSqlDialect {},
        &[
            "JSON_OBJECT('name' VALUE needle NULL ON NULL)",
            "JSON_OBJECT('name' VALUE needle RETURNING JSONB)",
        ],
    );
    assert_dialect_expression_children_indexed(
        &SnowflakeDialect {},
        &["CONNECT_BY_ROOT needle", "needle (+)"],
    );
    assert_dialect_expression_children_indexed(&DatabricksDialect {}, &["f(x -> needle)"]);
}

#[test]
fn hive_clause_children_follow_the_sqlparser_visitor() {
    for (clause, expected_queries) in [
        ("GROUP BY ALL", 1),
        ("CLUSTER BY needle", 2),
        ("DISTRIBUTE BY needle", 2),
        ("SORT BY needle", 2),
    ] {
        let mut query = query_with(
            &HiveDialect {},
            &format!("SELECT d.x FROM (SELECT needle AS x {clause}) AS d"),
        );
        let mut injector = NestedQueryInjector::default();
        assert!(matches!(VisitMut::visit(&mut query, &mut injector), ControlFlow::Continue(())));
        assert_eq!(injector.query_count, expected_queries);

        let db = schema_db();
        let scope =
            ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
        let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
            scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
        else {
            panic!("expected an expression definition")
        };
        let root = select_body(&query);
        let sqlparser::ast::TableFactor::Derived { subquery, .. } = &root.from[0].relation else {
            panic!("expected a derived relation")
        };
        let defining_select = select_body(subquery);
        let mut oracle = IndexedSelectOracle {
            scope: defining_scope,
            select_count: 0,
            resolve_input: true,
            injected_only: false,
            context: None,
        };
        assert!(matches!(expression.visit(&mut oracle), ControlFlow::Continue(())));
        for clause_expression in
            defining_select.cluster_by.iter().chain(&defining_select.distribute_by)
        {
            assert!(matches!(clause_expression.visit(&mut oracle), ControlFlow::Continue(())));
        }
        for order in &defining_select.sort_by {
            assert!(matches!(order.expr.visit(&mut oracle), ControlFlow::Continue(())));
        }
        assert_eq!(oracle.select_count, injector.query_count);
    }
}

#[test]
fn grouping_expression_families_index_every_nested_select() {
    let mut query = query(
        "SELECT d.x FROM (\
             SELECT (\
                 SELECT a.payload FROM a \
                 GROUP BY GROUPING SETS ((a.payload, needle)), CUBE((needle)), ROLLUP((needle))\
             ) AS x\
         ) AS d",
    );
    let mut injector = NestedQueryInjector::default();
    assert!(matches!(VisitMut::visit(&mut query, &mut injector), ControlFlow::Continue(())));
    assert_eq!(injector.query_count, 3);
    assert_expression_nested_selects_indexed(&query, injector.query_count + 1, true);
}

#[test]
fn scalar_subquery_scopes_resolve_local_and_correlated_inputs() {
    let db = schema_db();
    for (sql, expected_table) in [
        (
            "SELECT d.x FROM \
             (SELECT (SELECT a.payload FROM a) AS x) AS d",
            "a",
        ),
        (
            "SELECT d.x FROM \
             (SELECT (SELECT a.payload) AS x FROM a) AS d",
            "a",
        ),
        (
            "SELECT d.x FROM \
             (SELECT (SELECT a.payload FROM b AS a) AS x FROM a) AS d",
            "b",
        ),
    ] {
        assert_eq!(
            indexed_nested_input_table(&db, sql, "d.x", "a.payload", scalar_nested_select,)
                .as_deref(),
            Some(expected_table)
        );
    }
}

#[test]
fn set_operation_arms_have_distinct_nested_scopes() {
    let db = schema_db();
    let query = query(
        "SELECT d.x FROM \
         (SELECT (SELECT payload FROM a UNION ALL SELECT payload FROM b) AS x) AS d",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let Expr::Subquery(query) = expression else { panic!("expected a scalar subquery") };
    let SetExpr::SetOperation { left, right, .. } = query.body.as_ref() else {
        panic!("expected a set operation")
    };
    for (arm, expected_table) in [(left, "a"), (right, "b")] {
        let SetExpr::Select(select) = arm.as_ref() else { panic!("expected a SELECT arm") };
        let nested_scope =
            defining_scope.scope_for_select(select).expect("nested scope is indexed");
        let Some(ColumnDefinition::Base { table, .. }) = nested_scope
            .resolve_column_definition(&reference("payload"))
            .expect("arm input resolves")
        else {
            panic!("expected a base arm input")
        };
        assert_eq!(table.table_name(), expected_table);
    }
    let SetExpr::Select(left_select) = left.as_ref() else { panic!("expected a left SELECT arm") };
    let SetExpr::Select(right_select) = right.as_ref() else {
        panic!("expected a right SELECT arm")
    };
    let left_scope = defining_scope.scope_for_select(left_select).expect("left scope is indexed");
    assert!(left_scope.scope_for_select(right_select).is_none());
}

#[test]
fn query_expression_variants_share_the_nested_scope_index() {
    let db = schema_db();
    for (sql, input_reference, nested_select, expected_table) in [
        (
            "SELECT d.x FROM \
             (SELECT EXISTS (SELECT 1 FROM a) AS x) AS d",
            "a.payload",
            exists_nested_select as for<'expression> fn(&'expression Expr) -> &'expression _,
            "a",
        ),
        (
            "SELECT d.x FROM \
             (SELECT 1 IN (SELECT id FROM b) AS x) AS d",
            "b.payload",
            in_nested_select,
            "b",
        ),
        (
            "SELECT d.x FROM \
             (SELECT (SELECT id FROM a) = 1 AS x) AS d",
            "a.payload",
            binary_nested_select,
            "a",
        ),
        (
            "SELECT d.x FROM \
             (SELECT COALESCE((SELECT a.payload FROM a), '') AS x) AS d",
            "a.payload",
            function_nested_select,
            "a",
        ),
    ] {
        assert_eq!(
            indexed_nested_input_table(&db, sql, "d.x", input_reference, nested_select,).as_deref(),
            Some(expected_table)
        );
    }
}

#[test]
fn stored_view_nested_scope_uses_database_ast_references() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(payload TEXT);
         CREATE VIEW nested_view AS (SELECT (SELECT a.payload FROM a) AS x);",
    )
    .expect("schema parses");
    assert_eq!(
        indexed_nested_input_table(
            &db,
            "SELECT nested_view.x FROM nested_view",
            "nested_view.x",
            "a.payload",
            scalar_nested_select,
        )
        .as_deref(),
        Some("a")
    );
}

#[test]
fn stored_view_query_containers_use_database_ast_references() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(payload TEXT);
         CREATE TABLE b(payload TEXT);
         CREATE VIEW nested_containers AS
         SELECT (SELECT a.payload FROM a) AS x
         FROM a
         JOIN b ON EXISTS (SELECT a.payload FROM a)
         CROSS JOIN UNNEST(ARRAY[(SELECT a.payload FROM a)]) AS u
         WHERE EXISTS (VALUES ((SELECT a.payload FROM a)))
         ORDER BY (SELECT a.payload FROM a)
         LIMIT (SELECT a.payload FROM a)
         OFFSET (SELECT a.payload FROM a);",
    )
    .expect("schema parses");
    let query = query("SELECT nested_containers.x FROM nested_containers");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { scope: defining_scope, .. }) = scope
        .resolve_column_definition(&reference("nested_containers.x"))
        .expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let definition = db
        .view_by_target(TargetName::new("nested_containers", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("view exists")
        .definition();
    let mut oracle = IndexedSelectOracle {
        scope: defining_scope,
        select_count: 0,
        resolve_input: true,
        injected_only: true,
        context: Some(definition.to_string()),
    };
    assert!(matches!(definition.visit(&mut oracle), ControlFlow::Continue(())));
    assert_eq!(oracle.select_count, 7);
}
#[test]
fn stored_view_table_functions_use_database_ast_references() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE a(payload TEXT);
         CREATE VIEW table_function_view AS \
         SELECT g.value FROM TABLE(FUN(1)) AS g;",
    )
    .expect("schema parses");
    let query = query("SELECT table_function_view.value FROM table_function_view");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert!(matches!(
        scope
            .resolve_column_definition(&reference("table_function_view.value"))
            .expect("definition resolves"),
        Some(ColumnDefinition::Opaque)
    ));
}

#[test]
fn nested_scope_index_recurses_through_nested_queries() {
    let db = schema_db();
    let query = query(
        "SELECT d.x FROM \
         (SELECT (SELECT (SELECT a.payload FROM a)) AS x) AS d",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let first_select = scalar_nested_select(expression);
    let first_scope =
        defining_scope.scope_for_select(first_select).expect("first nested scope is indexed");
    let SelectItem::UnnamedExpr(second_expression) = &first_select.projection[0] else {
        panic!("expected a nested expression")
    };
    let second_select = scalar_nested_select(second_expression);
    let second_scope =
        first_scope.scope_for_select(second_select).expect("second nested scope is indexed");
    let Some(ColumnDefinition::Base { table, .. }) = second_scope
        .resolve_column_definition(&reference("a.payload"))
        .expect("nested input resolves")
    else {
        panic!("expected a base nested input")
    };
    assert_eq!(table.table_name(), "a");
}

#[test]
fn nested_scope_index_includes_selection_expressions() {
    let db = schema_db();
    let query = query(
        "SELECT d.x FROM (\
             SELECT a.payload || '' AS x FROM a \
             WHERE EXISTS (SELECT 1 FROM b WHERE b.id = a.id)\
         ) AS d",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { scope: defining_scope, .. }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    let SetExpr::Select(defining_select) = ({
        let SetExpr::Select(root) = query.body.as_ref() else { panic!("expected a root SELECT") };
        let sqlparser::ast::TableFactor::Derived { subquery, .. } = &root.from[0].relation else {
            panic!("expected a derived relation")
        };
        subquery.body.as_ref()
    }) else {
        panic!("expected a defining SELECT")
    };
    let Some(Expr::Exists { subquery, .. }) = &defining_select.selection else {
        panic!("expected an EXISTS predicate")
    };
    let nested_select = select_body(subquery);
    let nested_scope =
        defining_scope.scope_for_select(nested_select).expect("predicate scope is indexed");
    for (input_reference, expected_table) in [("b.payload", "b"), ("a.payload", "a")] {
        let Some(ColumnDefinition::Base { table, .. }) = nested_scope
            .resolve_column_definition(&reference(input_reference))
            .expect("predicate input resolves")
        else {
            panic!("expected a base predicate input")
        };
        assert_eq!(table.table_name(), expected_table);
    }
}

#[test]
fn alias_lists_and_quoted_names_preserve_expression_scopes() {
    let db = schema_db();
    for sql in [
        "SELECT d.\"X\" FROM \
         (SELECT a.payload || '' FROM a) AS d(\"X\")",
        "WITH d(\"X\") AS (SELECT a.payload || '' FROM a) \
         SELECT d.\"X\" FROM d",
    ] {
        assert_eq!(expression_input_table(&db, sql, "d.\"X\"", "a.payload").as_deref(), Some("a"));
    }

    let quoted_db = ParserDB::parse::<GenericDialect>("CREATE TABLE \"A\"(\"Payload\" TEXT);")
        .expect("quoted schema parses");
    assert_eq!(
        expression_input_table(
            &quoted_db,
            "SELECT d.\"X\" FROM \
             (SELECT \"A\".\"Payload\" || '' FROM \"A\") AS d(\"X\")",
            "d.\"X\"",
            "\"A\".\"Payload\"",
        )
        .as_deref(),
        Some("A")
    );
}

#[test]
fn expression_scopes_report_local_ambiguity() {
    let db = schema_db();
    let ambiguous_bare = query("SELECT d.x FROM (SELECT payload || '' AS x FROM a, b) AS d");
    let scope = ColumnScope::from_query(&ambiguous_bare, &db, IdentifierCase::AsWritten)
        .expect("scope builds");
    let Some(ColumnDefinition::Expression { scope: defining_scope, .. }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    assert!(matches!(
        defining_scope.resolve_column_definition(&reference("payload")),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));

    let ambiguous_qualified = query(
        "SELECT d.payload FROM \
         (SELECT a.payload, b.payload FROM a, b) AS d",
    );
    let scope = ColumnScope::from_query(&ambiguous_qualified, &db, IdentifierCase::AsWritten)
        .expect("scope builds");
    assert!(matches!(
        scope.resolve_column_definition(&reference("d.payload")),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
}

#[test]
fn expression_without_relations_has_an_empty_scope() {
    let db = schema_db();
    let query = query("SELECT d.x FROM (SELECT 1 + 2 AS x) AS d");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Expression { expression, scope: defining_scope }) =
        scope.resolve_column_definition(&reference("d.x")).expect("output definition resolves")
    else {
        panic!("expected an expression definition")
    };
    assert_eq!(expression.to_string(), "1 + 2");
    assert!(
        defining_scope
            .resolve_column_definition(&reference("missing"))
            .expect("missing input resolves")
            .is_none()
    );
}

#[test]
fn wildcard_definitions_retain_exact_base_columns() {
    let db = schema_db();
    for (sql, expected_table) in [
        ("SELECT d.payload FROM (SELECT * FROM a) AS d", "a"),
        ("SELECT d.payload FROM (SELECT a.* FROM a, b) AS d", "a"),
    ] {
        let query = query(sql);
        let scope =
            ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
        let Some(ColumnDefinition::Base { table, column, .. }) =
            scope.resolve_column_definition(&reference("d.payload")).expect("definition resolves")
        else {
            panic!("expected a base wildcard definition")
        };
        assert_eq!(table.table_name(), expected_table);
        assert_eq!(column.column_name(), "payload");
    }
}

#[test]
fn merged_wildcard_columns_are_opaque() {
    let db = schema_db();
    let query = query("SELECT d.id FROM (SELECT * FROM a JOIN b USING (id)) AS d");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert!(matches!(
        scope.resolve_column_definition(&reference("d.id")).expect("definition resolves"),
        Some(ColumnDefinition::Opaque)
    ));
}

#[test]
fn three_part_references_return_exact_base_definitions() {
    let db = schema_db();
    let unquoted = query("SELECT public.a.payload FROM a");
    let scope =
        ColumnScope::from_query(&unquoted, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::Base { table, column, .. }) = scope
        .resolve_column_definition(&reference("public.a.payload"))
        .expect("definition resolves")
    else {
        panic!("expected a three-part base definition")
    };
    assert_eq!(table.table_name(), "a");
    assert_eq!(column.column_name(), "payload");

    let quoted_db = ParserDB::parse::<GenericDialect>(
        "CREATE SCHEMA \"S\"; CREATE TABLE \"S\".\"T\"(\"C\" INT);",
    )
    .expect("quoted schema parses");
    let quoted = query("SELECT \"S\".\"T\".\"C\" FROM \"S\".\"T\"");
    let scope = ColumnScope::from_query(&quoted, &quoted_db, IdentifierCase::AsWritten)
        .expect("scope builds");
    let Some(ColumnDefinition::Base { table, column, .. }) = scope
        .resolve_column_definition(&reference("\"S\".\"T\".\"C\""))
        .expect("definition resolves")
    else {
        panic!("expected a quoted three-part base definition")
    };
    assert_eq!(table.table_name(), "T");
    assert_eq!(column.column_name(), "C");
}

#[test]
fn set_operations_preserve_mixed_base_and_expression_children() {
    let db = schema_db();
    let query = query(
        "SELECT d.payload FROM (\
             SELECT payload FROM a \
             UNION ALL \
             SELECT payload || '' FROM a\
         ) AS d",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::SetOperation { left, right, .. }) =
        scope.resolve_column_definition(&reference("d.payload")).expect("definition resolves")
    else {
        panic!("expected a set operation definition")
    };
    assert_copy(left);
    assert_copy(right);
    assert_eq!(format!("{left:?}"), format!("{:?}", Clone::clone(&left)));
    assert!(matches!(left.definition(), ColumnDefinition::Base { .. }));
    let ColumnDefinition::Expression { expression, scope: expression_scope } = right.definition()
    else {
        panic!("expected an expression definition")
    };
    assert_eq!(expression.to_string(), "payload || ''");
    assert_copy(expression_scope);
    assert_eq!(format!("{expression_scope:?}"), format!("{:?}", Clone::clone(&expression_scope)));
}

#[test]
fn alias_lists_relabel_set_operation_definitions() {
    let db = schema_db();
    let query = query(
        "SELECT d.x FROM (\
             SELECT payload FROM a \
             UNION ALL \
             SELECT payload FROM b\
         ) AS d(x)",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let Some(ColumnDefinition::SetOperation { left, right, .. }) =
        scope.resolve_column_definition(&reference("d.x")).expect("definition resolves")
    else {
        panic!("expected a relabeled set definition")
    };
    let ColumnDefinition::Base { table: left_table, .. } = left.definition() else {
        panic!("expected a left base definition")
    };
    let ColumnDefinition::Base { table: right_table, .. } = right.definition() else {
        panic!("expected a right base definition")
    };
    assert_eq!(left_table.table_name(), "a");
    assert_eq!(right_table.table_name(), "b");
}

#[test]
fn mutually_recursive_ctes_remain_opaque() {
    let db = schema_db();
    let query = query(
        "WITH RECURSIVE \
             x(v) AS (SELECT y.v FROM y), \
             y(v) AS (SELECT x.v FROM x) \
         SELECT x.v FROM x",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert!(matches!(
        scope.resolve_column_definition(&reference("x.v")).expect("definition resolves"),
        Some(ColumnDefinition::Opaque)
    ));
}

#[test]
fn wide_wildcard_chains_become_opaque_at_the_width_limit() {
    let db =
        ParserDB::parse::<GenericDialect>("CREATE TABLE seed(value INT);").expect("schema parses");
    let mut sql = String::from("WITH c0 AS (SELECT * FROM seed)");
    for level in 1..=13 {
        let previous = level - 1;
        write!(
            &mut sql,
            ", c{level} AS (SELECT * FROM c{previous} AS left_side, \
             c{previous} AS right_side)"
        )
        .expect("string writes");
    }
    sql.push_str(" SELECT c13.value FROM c13");
    let query = query(&sql);
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    assert!(matches!(
        scope.resolve_column_definition(&reference("c13.value")).expect("definition resolves"),
        Some(ColumnDefinition::Opaque)
    ));
}

fn definition_kind(scope: ColumnDefinitionScope<'_, '_, '_, ParserDB>, sql: &str) -> String {
    match scope.resolve_column_definition(&reference(sql)).expect("definition resolves") {
        Some(ColumnDefinition::Base { table, .. }) => format!("base {}", table.table_name()),
        Some(ColumnDefinition::Opaque) => "opaque".to_owned(),
        Some(_) => "other".to_owned(),
        None => "missing".to_owned(),
    }
}

#[test]
fn query_root_set_operands_resolve_in_their_own_scopes() {
    let db = schema_db();
    let query = query("SELECT id FROM a UNION ALL SELECT id FROM b");
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::SetOperation { left, right, .. } = query.body.as_ref() else {
        panic!("expected a set operation body")
    };
    for (operand, expected) in [(left, "base a"), (right, "base b")] {
        let SetExpr::Select(select) = operand.as_ref() else { panic!("expected a SELECT operand") };
        let operand = scope.scope_for_select(select).expect("operand scope is indexed");
        assert_eq!(definition_kind(operand, "id"), expected);
        assert_eq!(definition_kind(operand, "argument_only"), "missing");
    }
    assert!(scope.resolve_column_definition(&reference("id")).expect("root resolves").is_none());
}

#[test]
fn query_root_nested_scopes_keep_local_precedence_and_outer_correlation() {
    let db = schema_db();
    let query = query(
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.id = a.id) \
         UNION ALL \
         (SELECT id FROM b WHERE id IN (SELECT id FROM a WHERE a.payload = b.payload))",
    );
    let scope =
        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::SetOperation { left, right, .. } = query.body.as_ref() else {
        panic!("expected a set operation body")
    };
    let SetExpr::Select(left) = left.as_ref() else { panic!("expected a SELECT operand") };
    let SetExpr::Query(right) = right.as_ref() else { panic!("expected a parenthesized operand") };
    let right = select_body(right);
    let exists = exists_nested_select(left.selection.as_ref().expect("WHERE clause"));
    let in_subquery = in_nested_select(right.selection.as_ref().expect("WHERE clause"));
    for (select, local, outer, outer_table) in
        [(exists, "base b", "a.payload", "base a"), (in_subquery, "base a", "b.payload", "base b")]
    {
        let nested = scope.scope_for_select(select).expect("nested scope is indexed");
        assert_eq!(definition_kind(nested, "id"), local);
        assert_eq!(definition_kind(nested, outer), outer_table);
        assert_eq!(definition_kind(nested, "argument_only"), "missing");
    }
    let right_scope = scope.scope_for_select(right).expect("parenthesized operand is indexed");
    assert_eq!(definition_kind(right_scope, "a.payload"), "missing");
}

#[derive(Default)]
struct SelectCollector {
    selects: Vec<*const Select>,
}

impl SelectCollector {
    fn body(&mut self, body: &SetExpr) {
        match body {
            SetExpr::Select(select) => self.selects.push(select.as_ref()),
            SetExpr::SetOperation { left, right, .. } => {
                self.body(left);
                self.body(right);
            }
            _ => {}
        }
    }
}

impl Visitor for SelectCollector {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.body(&query.body);
        ControlFlow::Continue(())
    }
}

fn input_selects(query: &Query) -> Vec<&Select> {
    let mut collector = SelectCollector::default();
    let _ = query.visit(&mut collector);
    collector
        .selects
        .into_iter()
        // SAFETY: every pointer was taken from a node owned by `query`, which the returned
        // references borrow immutably, so each node stays alive, aligned and unaliased by `&mut`.
        .map(|select| unsafe { &*select })
        .collect()
}

#[test]
fn query_scope_indexes_every_select_of_the_input() {
    let db = schema_db();
    let mut missing = Vec::new();
    for sql in [
        "WITH c AS (SELECT payload FROM a) \
         SELECT (SELECT max(id) FROM b) FROM c \
         WHERE EXISTS (SELECT 1 FROM b WHERE b.payload = c.payload) \
         ORDER BY (SELECT min(id) FROM a)",
        "SELECT d.id FROM (SELECT id FROM a UNION ALL SELECT id FROM b) AS d, \
         LATERAL (SELECT d.id AS x) AS l",
        "SELECT v.x FROM (SELECT 1 AS x UNION ALL VALUES (2)) AS v",
        "SELECT v.x FROM (VALUES (1) UNION ALL SELECT id FROM b) AS v(x)",
        "SELECT v.x FROM (VALUES ((SELECT max(id) FROM a))) AS v(x)",
        "SELECT t.id FROM (SELECT id FROM a START WITH id = 1 CONNECT BY PRIOR id = id) AS t",
        "SELECT id FROM a UNION ALL VALUES (1)",
        "WITH RECURSIVE t(n) AS (\
             SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3 \
             ORDER BY (SELECT min(id) FROM a)\
         ) SELECT n FROM t",
        "WITH RECURSIVE x(v) AS (SELECT y.v FROM y), y(v) AS (SELECT x.v FROM x) \
         SELECT x.v FROM x",
        "SELECT t.x FROM (SELECT * FROM generate_series(1, 3)) AS t",
        "SELECT id FROM a WHERE id IN (SELECT id FROM b UNION SELECT id FROM a)",
    ] {
        let query = query(sql);
        let scope =
            ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).expect("scope builds");
        for select in input_selects(&query) {
            if scope.scope_for_select(select).is_none() {
                missing.push(format!("`{select}` in `{sql}`"));
            }
        }
    }
    assert!(missing.is_empty(), "selects without a scope: {missing:#?}");
}

#[test]
fn lateral_views_and_connect_by_scopes_stay_opaque_for_unmodeled_columns() {
    let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t(id INT, arr INT, col INT);")
        .expect("schema parses");
    let lateral = query_with(
        &HiveDialect {},
        "SELECT x.v FROM (SELECT col AS v FROM t LATERAL VIEW explode(arr) e AS col) AS x",
    );
    let scope =
        ColumnScope::from_query(&lateral, &db, IdentifierCase::AsWritten).expect("scope builds");
    let SetExpr::Select(outer) = lateral.body.as_ref() else { panic!("expected a SELECT body") };
    let sqlparser::ast::TableFactor::Derived { subquery, .. } = &outer.from[0].relation else {
        panic!("expected a derived table")
    };
    let inner = scope.scope_for_select(select_body(subquery)).expect("lateral scope is indexed");
    for (sql, expected) in
        [("col", "opaque"), ("e.col", "opaque"), ("t.id", "base t"), ("other.col", "missing")]
    {
        assert_eq!(definition_kind(inner, sql), expected, "`{sql}`");
    }
    let root = query_with(&HiveDialect {}, "SELECT id FROM t LATERAL VIEW explode(arr) e AS col");
    assert!(root.projection_source_table(&db).expect("source resolves").is_none());

    let hierarchy = query("SELECT id FROM t START WITH id = 1 CONNECT BY PRIOR id = id");
    let scope =
        ColumnScope::from_query(&hierarchy, &db, IdentifierCase::AsWritten).expect("scope builds");
    let root = scope.scope_for_select(select_body(&hierarchy)).expect("root scope is indexed");
    for (sql, expected) in [("level", "opaque"), ("t.id", "base t")] {
        assert_eq!(definition_kind(root, sql), expected, "`{sql}`");
    }
}
