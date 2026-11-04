//! Checks the comparison a recursive-CTE dependency scan applies at the caller
//! and stored-definition boundary.
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

fn reference(table: &str, column: &str) -> Expr {
    Expr::CompoundIdentifier(vec![Ident::new(table), Ident::new(column)])
}

#[test]
fn nested_cte_shadowing_uses_the_selected_comparison() {
    let db =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t(id INT);").expect("schema parses");
    let sql = "WITH RECURSIVE \
         A AS (WITH b AS (SELECT 1 AS id) SELECT b.id FROM B UNION ALL SELECT A.id FROM A WHERE false), \
         B AS (WITH a AS (SELECT 1 AS id) SELECT a.id FROM A UNION ALL SELECT B.id FROM B WHERE false) \
         SELECT A.id FROM A";
    let input = query(sql);
    for case in [IdentifierCase::Folded, IdentifierCase::AsWritten] {
        let scope = ColumnScope::from_query(&input, &db, case).expect("scope builds");
        let definition =
            scope.resolve_column_definition(&reference("A", "id")).expect("definition resolves");
        let Some(ColumnDefinition::RecursiveUnion { anchor, .. }) = definition else {
            panic!("{case:?} must derive the anchored union, got {definition:?}")
        };
        assert!(matches!(anchor.definition(), ColumnDefinition::Expression { .. }));
    }
    let scope = ColumnScope::from_query(&input, &db, IdentifierCase::Exact).expect("scope builds");
    assert!(
        matches!(
            scope.resolve_column_definition(&reference("A", "id")).expect("definition resolves"),
            Some(ColumnDefinition::Opaque)
        ),
        "Exact must see the mutual cycle"
    );
}

#[test]
fn stored_recursive_cte_scopes_keep_the_postgres_comparison() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE a(id INT); CREATE TABLE b(id INT); \
         CREATE VIEW v AS WITH RECURSIVE A AS (SELECT id FROM b), B AS (SELECT id FROM a) SELECT A.id FROM A;",
    )
    .expect("schema parses");
    let input = query("SELECT v.id FROM v");
    for case in [IdentifierCase::Folded, IdentifierCase::Exact, IdentifierCase::AsWritten] {
        let scope = ColumnScope::from_query(&input, &db, case).expect("scope builds");
        assert!(
            matches!(
                scope
                    .resolve_column_definition(&reference("v", "id"))
                    .expect("definition resolves"),
                Some(ColumnDefinition::Opaque)
            ),
            "{case:?} must keep the stored comparison"
        );
    }
    let direct =
        query("WITH RECURSIVE A AS (SELECT id FROM b), B AS (SELECT id FROM a) SELECT A.id FROM A");
    let scope = ColumnScope::from_query(&direct, &db, IdentifierCase::Exact).expect("scope builds");
    let Some(ColumnDefinition::Base { table, column, .. }) =
        scope.resolve_column_definition(&reference("A", "id")).expect("definition resolves")
    else {
        panic!("the caller's Exact read must bind to b.id")
    };
    assert_eq!((table.table_name(), column.column_name()), ("b", "id"));
}
