//! Checks identifier comparisons at query and stored-definition boundaries.
#![expect(clippy::expect_used, clippy::panic, reason = "tests reject unexpected definitions")]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Ident, Query, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn query(sql: &str) -> Query {
    let Statement::Query(query) =
        Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("query parses").remove(0)
    else {
        panic!("expected a query")
    };
    *query
}

fn name(value: &str, quoted: bool) -> Expr {
    Expr::Identifier(if quoted { Ident::with_quote('"', value) } else { Ident::new(value) })
}

#[test]
fn stored_views_keep_postgres_cte_shadowing() {
    for kind in ["VIEW", "MATERIALIZED VIEW"] {
        let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE t(id INT); CREATE {kind} v AS WITH \"T\" AS (SELECT 1 AS id) SELECT id FROM t;"
        ))
        .expect("schema parses");
        let query = query("SELECT v.id FROM v");
        let reference = Expr::CompoundIdentifier(vec![Ident::new("v"), Ident::new("id")]);
        for case in [IdentifierCase::Folded, IdentifierCase::Exact, IdentifierCase::AsWritten] {
            let scope = ColumnScope::from_query(&query, &db, case).expect("scope builds");
            let definition = scope.resolve_column_definition(&reference).expect("lookup succeeds");
            let Some(ColumnDefinition::Base { table, column }) = definition else {
                panic!("{kind} under {case:?} must bind to t.id, got {definition:?}")
            };
            assert_eq!((table.table_name(), column.column_name()), ("t", "id"));
            assert_eq!(scope.resolve_column(&reference).expect("source resolves"), Some(table));
        }
    }
}

#[test]
fn stored_view_scopes_keep_postgres_input_and_output_names() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t(id INT); CREATE VIEW v AS SELECT id + 1 AS n FROM t;",
    )
    .expect("schema parses");
    let query = query("SELECT v.n FROM v");
    for case in [IdentifierCase::Folded, IdentifierCase::Exact] {
        let scope = ColumnScope::from_query(&query, &db, case).expect("scope builds");
        let reference = Expr::CompoundIdentifier(vec![Ident::new("v"), Ident::new("n")]);
        let Some(ColumnDefinition::Expression { scope: defining, .. }) =
            scope.resolve_column_definition(&reference).expect("lookup succeeds")
        else {
            panic!("expected an expression definition")
        };
        assert!(
            defining
                .resolve_distinct_on_definition(&name("N", true))
                .expect("lookup succeeds")
                .is_none()
        );
        assert!(
            defining
                .resolve_group_by_definition(&name("N", true))
                .expect("lookup succeeds")
                .is_none()
        );
        assert!(
            defining
                .resolve_column_definition(&name("ID", true))
                .expect("lookup succeeds")
                .is_none()
        );
        assert!(
            defining.resolve_column_binding(&name("ID", true)).expect("lookup succeeds").is_none()
        );
        assert!(matches!(
            defining.resolve_column_definition(&name("ID", false)).expect("lookup succeeds"),
            Some(ColumnDefinition::Base { .. })
        ));
        assert!(matches!(
            defining.resolve_distinct_on_definition(&name("n", false)).expect("lookup succeeds"),
            Some(ColumnDefinition::Expression { .. })
        ));
        assert!(matches!(
            defining.resolve_group_by_definition(&name("n", false)).expect("lookup succeeds"),
            Some(ColumnDefinition::Expression { .. })
        ));
    }
}

#[test]
fn exact_recursive_ctes_do_not_invent_case_folded_cycles() {
    let db =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE a(id INT); CREATE TABLE b(id INT);")
            .expect("schema parses");
    let query =
        query("WITH RECURSIVE A AS (SELECT id FROM b), B AS (SELECT id FROM a) SELECT A.id FROM A");
    let reference = Expr::CompoundIdentifier(vec![Ident::new("A"), Ident::new("id")]);
    let scope = ColumnScope::from_query(&query, &db, IdentifierCase::Exact).expect("scope builds");
    let definition = scope.resolve_column_definition(&reference).expect("lookup succeeds");
    let Some(ColumnDefinition::Base { table, column }) = definition else {
        panic!("Exact must bind to b.id, got {definition:?}")
    };
    assert_eq!((table.table_name(), column.column_name()), ("b", "id"));
    let (_, binding) = scope
        .resolve_column_binding(&reference)
        .expect("lookup succeeds")
        .expect("column resolves");
    assert_eq!(binding.relation(), Some(("A", false)));
    assert_eq!(binding.exposed(), Some(("id", false)));
    for case in [IdentifierCase::AsWritten, IdentifierCase::Folded] {
        let scope = ColumnScope::from_query(&query, &db, case).expect("scope builds");
        assert!(matches!(
            scope.resolve_column_definition(&reference).expect("lookup succeeds"),
            Some(ColumnDefinition::Opaque)
        ));
    }
}

#[test]
fn query_set_output_names_keep_the_callers_comparison() {
    let db =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t(id INT);").expect("schema parses");
    let query = query("SELECT id AS n FROM t UNION ALL SELECT id FROM t");
    for case in [IdentifierCase::Folded, IdentifierCase::Exact, IdentifierCase::AsWritten] {
        let scope = ColumnScope::from_query(&query, &db, case).expect("scope builds");
        let outputs = scope.scope_for_query(&query).expect("query is recorded");
        let definition =
            outputs.resolve_order_by_definition(&name("N", true)).expect("lookup succeeds");
        if case == IdentifierCase::Folded {
            assert!(matches!(definition, Some(ColumnDefinition::SetOperation { .. })));
        } else {
            assert!(definition.is_none());
        }
    }
}

#[test]
fn nested_stored_cte_scopes_use_as_written() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t(id INT);
         CREATE VIEW v AS WITH c AS (SELECT ID + 1 AS n FROM t) SELECT n FROM c;",
    )
    .expect("schema parses");
    let query = query("SELECT v.n FROM v");
    let reference = Expr::CompoundIdentifier(vec![Ident::new("v"), Ident::new("n")]);
    for case in [IdentifierCase::Folded, IdentifierCase::Exact] {
        let scope = ColumnScope::from_query(&query, &db, case).expect("scope builds");
        let Some(ColumnDefinition::Expression { scope: defining, .. }) =
            scope.resolve_column_definition(&reference).expect("lookup succeeds")
        else {
            panic!("expected a stored expression")
        };
        let Some((ColumnDefinition::Base { table, column }, binding)) =
            defining.resolve_column_binding(&name("ID", false)).expect("lookup succeeds")
        else {
            panic!("expected a stored column")
        };
        assert_eq!((table.table_name(), column.column_name()), ("t", "id"));
        assert_eq!(binding.relation(), Some(("t", false)));
        assert!(
            defining
                .resolve_column_definition(&name("ID", true))
                .expect("lookup succeeds")
                .is_none()
        );
    }
}

#[test]
fn stored_set_output_arms_keep_their_own_comparison() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t(id INT);
         CREATE VIEW v AS SELECT ID + 1 AS n FROM t UNION ALL SELECT ID + 2 FROM t;",
    )
    .expect("schema parses");
    let query = query("SELECT v.n FROM v");
    for case in [IdentifierCase::Folded, IdentifierCase::Exact] {
        let scope = ColumnScope::from_query(&query, &db, case).expect("scope builds");
        let outputs = scope.scope_for_query(&query).expect("query is recorded");
        let Some(QueryOutput::Column {
            definition: ColumnDefinition::SetOperation { left, right, .. },
            ..
        }) = outputs.outputs().next()
        else {
            panic!("expected a stored set operation")
        };
        for side in [left, right] {
            let ColumnDefinition::Expression { scope: defining, .. } = side.definition() else {
                panic!("expected a stored expression")
            };
            let definition =
                defining.resolve_column_definition(&name("ID", false)).expect("lookup succeeds");
            assert!(
                matches!(definition, Some(ColumnDefinition::Base { table, column }) if table.table_name() == "t" && column.column_name() == "id")
            );
            assert!(
                defining
                    .resolve_column_definition(&name("ID", true))
                    .expect("lookup succeeds")
                    .is_none()
            );
        }
    }
}
