//! Integration tests for the data-statement analysis traits
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable)]
//! (`DataStatementLike`, `DQLLike`, `DMLLike`), exercising them through the
//! public prelude exactly as a downstream consumer (subql, connetto) would.

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{Query, Statement},
    dialect::{GenericDialect, MySqlDialect},
    parser::Parser,
};

const SCHEMA: &str = "
    CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL);
    CREATE TABLE orders (
        id INT PRIMARY KEY,
        user_id INT REFERENCES users(id),
        total INT
    );
    CREATE TABLE order_items (order_id INT, sku TEXT, qty INT, PRIMARY KEY (order_id, sku));
    CREATE VIEW open_orders AS SELECT id, user_id, total FROM orders;
    CREATE VIEW open_high AS SELECT user_id FROM open_orders WHERE total > 100;
    CREATE MATERIALIZED VIEW big_orders AS SELECT id, user_id, total FROM orders;
    CREATE VIEW over_big AS SELECT user_id FROM big_orders;
";

fn schema_db() -> ParserDB {
    ParserDB::parse::<GenericDialect>(SCHEMA).expect("schema parses")
}

fn statement(sql: &str) -> Statement {
    let mut statements = Parser::parse_sql(&GenericDialect {}, sql).expect("statement parses");
    statements.pop().expect("one statement")
}

fn query(sql: &str) -> Query {
    match statement(sql) {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}

fn referenced_names<S: DataStatementLike<ParserDB>>(stmt: &S, db: &ParserDB) -> Vec<String> {
    stmt.referenced_tables(db)
        .expect("referenced_tables succeeds")
        .iter()
        .map(|table| table.table_name().to_string())
        .collect()
}

fn referenced_relation_names<S: DataStatementLike<ParserDB>>(
    stmt: &S,
    db: &ParserDB,
) -> Vec<String> {
    stmt.referenced_relations(db)
        .expect("referenced_relations succeeds")
        .iter()
        .map(|relation| {
            match relation {
                Relation::Table(table) => format!("table {}", table.table_name()),
                Relation::View(view) => format!("view {}", view.view_name()),
                Relation::MaterializedView(view) => {
                    format!("materialized view {}", view.view_name())
                }
            }
        })
        .collect()
}

fn projection_name(sql: &str, db: &ParserDB) -> Option<String> {
    query(sql)
        .projection_source_table(db)
        .expect("projection_source_table succeeds")
        .map(|table| table.table_name().to_string())
}

#[test]
fn referenced_tables_for_join_routes_both_base_tables() {
    let db = schema_db();
    let names = referenced_names(
        &query("SELECT o.total FROM orders o JOIN users u ON o.user_id = u.id"),
        &db,
    );
    assert_eq!(names, vec!["orders".to_string(), "users".to_string()]);
}

#[test]
fn referenced_tables_includes_subquery_tables() {
    let db = schema_db();
    let names = referenced_names(
        &query("SELECT id FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 100)"),
        &db,
    );
    assert_eq!(names, vec!["users".to_string(), "orders".to_string()]);
}

#[test]
fn single_table_query_is_eligible_with_primary_key() {
    // The subql eligibility composite: a query whose output rows are rows of a
    // single base table that has a primary key can be re-executed as a
    // PK-keyed patchset.
    let db = schema_db();
    let analyzed = query("SELECT o.id, o.total FROM orders o JOIN users u ON o.user_id = u.id");

    let source = analyzed
        .projection_source_table(&db)
        .expect("projection resolves")
        .expect("single base table");
    assert_eq!(source.table_name(), "orders");
    assert!(source.has_primary_key(&db).expect("has_primary_key succeeds"));

    let pk: Vec<&str> = source
        .primary_key_columns(&db)
        .expect("primary_key_columns succeeds")
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(pk, vec!["id"]);
}

#[test]
fn multi_table_projection_is_not_eligible() {
    let db = schema_db();
    assert_eq!(
        projection_name("SELECT o.id, u.name FROM orders o JOIN users u ON o.user_id = u.id", &db,),
        None,
    );
}

#[test]
fn wildcard_over_join_is_not_eligible() {
    let db = schema_db();
    assert_eq!(
        projection_name("SELECT * FROM orders o JOIN users u ON o.user_id = u.id", &db),
        None,
    );
}

#[test]
fn aggregate_and_group_by_are_not_eligible() {
    let db = schema_db();
    assert_eq!(projection_name("SELECT COUNT(*) FROM orders", &db), None);
    assert_eq!(projection_name("SELECT user_id FROM orders GROUP BY user_id", &db), None);
}

#[test]
fn computed_projection_is_not_eligible() {
    let db = schema_db();
    assert_eq!(projection_name("SELECT total * 2 FROM orders", &db), None);
}

#[test]
fn set_operation_is_not_eligible_but_routes_all_tables() {
    let db = schema_db();
    let sql = "SELECT id FROM users UNION SELECT id FROM orders";
    assert_eq!(projection_name(sql, &db), None);
    assert_eq!(referenced_names(&query(sql), &db), vec!["users".to_string(), "orders".to_string()],);
}

#[test]
fn cte_body_routes_and_row_preserving_reference_resolves() {
    let db = schema_db();
    let sql = "WITH high AS (SELECT id FROM orders WHERE total > 100) SELECT id FROM high";
    // `orders` (CTE body) is routed; `high` (the CTE name) is not a base table.
    assert_eq!(referenced_names(&query(sql), &db), vec!["orders".to_string()]);
    // The CTE passes rows of `orders` through a filter, so the projection is
    // still rows of `orders`.
    assert_eq!(projection_name(sql, &db), Some("orders".to_string()));
}

#[test]
fn grouped_cte_body_is_not_eligible() {
    let db = schema_db();
    // Grouped rows are not orders rows, so re-execution against `orders` is
    // not eligible even though the column passes through.
    let sql = "WITH s AS (SELECT user_id, count(*) AS n FROM orders GROUP BY user_id) \
               SELECT s.user_id FROM s";
    assert_eq!(projection_name(sql, &db), None);
}

#[test]
fn ambiguous_unqualified_column_is_an_error() {
    let db = schema_db();
    let result = query("SELECT id FROM users JOIN orders ON users.id = orders.user_id")
        .projection_source_table(&db);
    assert!(matches!(result, Err(LookupError::AmbiguousTableLookup { .. })), "got {result:?}");
}

#[test]
fn composite_primary_key_table_projection() {
    let db = schema_db();
    let source = projection_name("SELECT order_id, sku, qty FROM order_items", &db);
    assert_eq!(source, Some("order_items".to_string()));

    let table = db
        .table_by_target(TargetName::new("order_items", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap();
    let pk: Vec<&str> = table
        .primary_key_columns(&db)
        .expect("primary_key_columns succeeds")
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(pk, vec!["order_id", "sku"]);
}

#[test]
fn insert_target_kind_and_referenced_tables() {
    let db = schema_db();
    let Statement::Insert(insert) = statement("INSERT INTO orders SELECT * FROM orders") else {
        panic!("expected insert");
    };
    assert_eq!(insert.kind(), DmlKind::Insert);
    assert_eq!(insert.target_table(&db).unwrap().table_name(), "orders");
    assert_eq!(referenced_names(&insert, &db), vec!["orders".to_string()]);
}

#[test]
fn update_target_kind_and_referenced_tables() {
    let db = schema_db();
    let Statement::Update(update) =
        statement("UPDATE orders SET total = 0 FROM users WHERE orders.user_id = users.id")
    else {
        panic!("expected update");
    };
    assert_eq!(update.kind(), DmlKind::Update);
    assert_eq!(update.target_table(&db).unwrap().table_name(), "orders");
    assert_eq!(referenced_names(&update, &db), vec!["orders".to_string(), "users".to_string()],);
}

#[test]
fn delete_target_kind_and_referenced_tables() {
    let db = schema_db();
    let Statement::Delete(delete) =
        statement("DELETE FROM orders WHERE user_id IN (SELECT id FROM users)")
    else {
        panic!("expected delete");
    };
    assert_eq!(delete.kind(), DmlKind::Delete);
    assert_eq!(delete.target_table(&db).unwrap().table_name(), "orders");
    assert_eq!(referenced_names(&delete, &db), vec!["orders".to_string(), "users".to_string()],);
}

#[test]
fn mysql_multi_table_delete_has_no_single_target() {
    let db = schema_db();
    let mut statements = Parser::parse_sql(
        &MySqlDialect {},
        "DELETE users, orders FROM users JOIN orders ON users.id = orders.user_id",
    )
    .expect("statement parses");
    let Statement::Delete(delete) = statements.pop().unwrap() else {
        panic!("expected delete");
    };
    assert!(matches!(delete.target_table(&db), Err(LookupError::InvalidObjectName { .. })));
}

#[test]
fn referenced_relations_names_every_kind_the_statement_reads() {
    let db = schema_db();
    let names =
        referenced_relation_names(&query("SELECT * FROM users, open_orders, big_orders"), &db);
    assert_eq!(
        names,
        vec![
            "table users".to_string(),
            "view open_orders".to_string(),
            "materialized view big_orders".to_string(),
        ],
    );
}

#[test]
fn referenced_tables_expands_a_view_to_its_base_table() {
    let db = schema_db();
    // The statement names the view; the base table is reached through it.
    assert_eq!(
        referenced_names(&query("SELECT * FROM open_orders"), &db),
        vec!["orders".to_string()],
    );
    assert_eq!(
        referenced_relation_names(&query("SELECT * FROM open_orders"), &db),
        vec!["view open_orders".to_string()],
    );
}

#[test]
fn referenced_tables_expands_a_view_over_a_view() {
    let db = schema_db();
    assert_eq!(
        referenced_names(&query("SELECT * FROM open_high"), &db),
        vec!["orders".to_string()],
    );
}

#[test]
fn referenced_tables_expands_a_view_inside_a_subquery() {
    let db = schema_db();
    let names = referenced_names(
        &query("SELECT id FROM users WHERE id IN (SELECT user_id FROM open_orders)"),
        &db,
    );
    assert_eq!(names, vec!["users".to_string(), "orders".to_string()]);
}

#[test]
fn with_item_shadows_a_view_name() {
    let db = schema_db();
    let sql = "WITH open_orders AS (SELECT 1 AS user_id) SELECT user_id FROM open_orders";
    assert_eq!(referenced_names(&query(sql), &db), Vec::<String>::new());
    assert_eq!(referenced_relation_names(&query(sql), &db), Vec::<String>::new());
}

#[test]
fn a_materialized_view_is_not_expanded() {
    let db = schema_db();
    // Reading the materialized view reads its snapshot, never its base table.
    assert_eq!(referenced_names(&query("SELECT * FROM big_orders"), &db), Vec::<String>::new());
    assert_eq!(
        referenced_relation_names(&query("SELECT * FROM big_orders"), &db),
        vec!["materialized view big_orders".to_string()],
    );
    // A plain view reading the materialized view reaches no base table.
    assert_eq!(referenced_names(&query("SELECT * FROM over_big"), &db), Vec::<String>::new());
    assert_eq!(
        referenced_relation_names(&query("SELECT * FROM over_big"), &db),
        vec!["view over_big".to_string()],
    );
}

#[test]
fn referenced_tables_dedupes_a_table_read_directly_and_through_a_view() {
    let db = schema_db();
    assert_eq!(
        referenced_names(&query("SELECT * FROM orders, open_orders"), &db),
        vec!["orders".to_string()],
    );
    // The table reached through two views of it still appears once.
    assert_eq!(
        referenced_names(&query("SELECT * FROM open_orders, open_high"), &db),
        vec!["orders".to_string()],
    );
}

#[test]
fn a_dml_target_resolves_against_the_relation_pool() {
    let db = schema_db();
    let Statement::Insert(insert) = statement("INSERT INTO open_orders (id) VALUES (1)") else {
        panic!("expected insert");
    };
    // The target names the view, and writing through it writes its base
    // table.
    assert_eq!(referenced_relation_names(&insert, &db), vec!["view open_orders".to_string()],);
    assert_eq!(referenced_names(&insert, &db), vec!["orders".to_string()]);
}

#[test]
fn a_view_cycle_terminates_with_no_base_table() {
    let db = ParserDB::parse::<GenericDialect>(
        "
        CREATE TABLE t (id INT);
        CREATE VIEW v1 AS SELECT id FROM t;
        CREATE VIEW v2 AS SELECT id FROM v1;
        CREATE OR REPLACE VIEW v1 AS SELECT id FROM v2;
        ",
    )
    .expect("schema parses");
    assert_eq!(referenced_names(&query("SELECT * FROM v1"), &db), Vec::<String>::new());
    assert_eq!(
        referenced_relation_names(&query("SELECT * FROM v1"), &db),
        vec!["view v1".to_string()],
    );
}

/// Three spellings a case-sensitive server holds apart are one name under
/// PostgreSQL's rule, and the unified resolvers report them as the ambiguity
/// one name is supposed to be, whatever the kinds.
#[test]
fn a_name_held_by_relations_of_every_kind_is_an_error() {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Exact)
        .parse::<GenericDialect>(
            "
            CREATE TABLE docs (id INT PRIMARY KEY);
            CREATE VIEW Docs AS SELECT id FROM docs;
            CREATE MATERIALIZED VIEW DOCS AS SELECT id FROM docs;
            ",
        )
        .expect("a case-sensitive server holds all three spellings");

    let Err(LookupError::AmbiguousTableLookup { candidates, .. }) =
        db.resolve_target_relation(TargetName::new("docs", false), IdentifierCase::AsWritten)
    else {
        panic!("the written name is ambiguous");
    };
    assert_eq!(candidates, vec!["DOCS".to_string(), "Docs".to_string(), "docs".to_string()]);

    // The bound reading takes the same comparison, so it is ambiguous too.
    assert!(
        matches!(
            db.resolve_bound_relation(TargetName::new("docs", false)),
            Err(LookupError::AmbiguousTableLookup { .. })
        ),
        "the bound name is ambiguous too"
    );

    // A statement reading the name carries the error through both accessors.
    assert!(
        matches!(
            query("SELECT * FROM docs").referenced_tables(&db),
            Err(LookupError::AmbiguousTableLookup { .. })
        ),
        "referenced_tables reports the ambiguity"
    );
    assert!(
        matches!(
            query("SELECT * FROM docs").referenced_relations(&db),
            Err(LookupError::AmbiguousTableLookup { .. })
        ),
        "referenced_relations reports the ambiguity"
    );
}
