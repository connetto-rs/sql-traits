//! Tests that a read or write of a table without `ONLY` reaches every table
//! below it through `INHERITS` or `PARTITION OF`, and that `ONLY` stops at the
//! named table.
//!
//! Each expectation is the set of tables `EXPLAIN (COSTS OFF)` of the same
//! statement scans or modifies on PostgreSQL 18.6 over [`SCHEMA`]. A
//! partitioned table holds no rows, so PostgreSQL's plan leaves `evt` and
//! `evt_eu` out where the answer here keeps them as tables the read reaches.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Query, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

const SCHEMA: &str = "
    CREATE TABLE docs (id INT PRIMARY KEY, body TEXT);
    CREATE TABLE secret_docs (level INT) INHERITS (docs);
    CREATE TABLE top_secret_docs (code TEXT) INHERITS (secret_docs);
    CREATE TABLE evt (id INT, region TEXT, PRIMARY KEY (id, region)) PARTITION BY LIST (region);
    CREATE TABLE evt_eu PARTITION OF evt FOR VALUES IN ('eu') PARTITION BY RANGE (id);
    CREATE TABLE evt_eu_low PARTITION OF evt_eu FOR VALUES FROM (0) TO (10);
    CREATE TABLE evt_us PARTITION OF evt FOR VALUES IN ('us');
    CREATE VIEW all_docs AS SELECT id FROM docs;
    CREATE VIEW only_docs AS SELECT id FROM ONLY docs;
";

const DOCS_TREE: [&str; 3] = ["docs", "secret_docs", "top_secret_docs"];

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("the schema parses")
}

fn statement(sql: &str) -> Statement {
    let mut statements =
        Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("the statement parses");
    assert_eq!(statements.len(), 1, "{sql}");
    statements.pop().expect("one statement")
}

fn query(sql: &str) -> Query {
    match statement(sql) {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}

/// The tables the statement reaches, sorted by name.
fn reached(sql: &str, database: &ParserDB) -> Vec<String> {
    let tables = match statement(sql) {
        Statement::Query(query) => query.referenced_tables(database),
        Statement::Update(update) => update.referenced_tables(database),
        Statement::Delete(delete) => delete.referenced_tables(database),
        other => panic!("expected a query, an UPDATE or a DELETE, got {other:?}"),
    };
    let mut names: Vec<String> = tables
        .expect("the tables resolve")
        .iter()
        .map(|table| table.table_name().to_owned())
        .collect();
    names.sort();
    names
}

/// The relations the statement names, in the order it names them.
fn named(sql: &str, database: &ParserDB) -> Vec<String> {
    query(sql)
        .referenced_relations(database)
        .expect("the relations resolve")
        .iter()
        .map(|relation| {
            match relation {
                Relation::Table(table) | Relation::ForeignTable(table) => {
                    table.table_name().to_owned()
                }
                Relation::View(view) => view.view_name().to_owned(),
                Relation::MaterializedView(view) => view.view_name().to_owned(),
                Relation::Sequence(sequence) => sequence.sequence_name().to_owned(),
            }
        })
        .collect()
}

fn source(sql: &str, database: &ParserDB) -> Option<String> {
    query(sql)
        .projection_source_table(database)
        .expect("the row-identity question answers")
        .map(|table| table.table_name().to_owned())
}

#[test]
fn a_read_without_only_reaches_every_inheriting_table() {
    let database = database();
    // `Append` over `docs`, `secret_docs` and `top_secret_docs` for both.
    for sql in ["SELECT * FROM docs", "SELECT * FROM docs *"] {
        assert_eq!(reached(sql, &database), DOCS_TREE, "{sql}");
    }
    // `Append` over `secret_docs` and `top_secret_docs`.
    assert_eq!(reached("SELECT * FROM secret_docs", &database), ["secret_docs", "top_secret_docs"]);
}

#[test]
fn only_reads_the_named_table_alone() {
    let database = database();
    // `Seq Scan on docs` for both.
    for sql in ["SELECT * FROM ONLY docs", "SELECT * FROM ONLY (docs)"] {
        assert_eq!(reached(sql, &database), ["docs"], "{sql}");
    }
    // The same table read once with `ONLY` and once without still reaches
    // the whole tree.
    assert_eq!(reached("SELECT * FROM ONLY docs, docs AS every_doc", &database), DOCS_TREE);
}

#[test]
fn a_read_of_a_partitioned_table_reaches_every_partition() {
    let database = database();
    // `Append` over `evt_eu_low` and `evt_us`.
    assert_eq!(reached("SELECT * FROM evt", &database), ["evt", "evt_eu", "evt_eu_low", "evt_us"]);
    // `Result` with `One-Time Filter: false`, since the root holds no rows.
    assert_eq!(reached("SELECT * FROM ONLY evt", &database), ["evt"]);
}

#[test]
fn a_table_reached_twice_is_read_once() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE a (x INT);
         CREATE TABLE b () INHERITS (a);
         CREATE TABLE c () INHERITS (a, b);",
    )
    .expect("the schema parses");
    // `c` inherits from `a` both directly and through `b`. `EXPLAIN` of
    // `SELECT * FROM a` scans `a`, `b` and `c` once each, and with one row in
    // each table the read returns three rows.
    assert_eq!(reached("SELECT * FROM a", &database), ["a", "b", "c"]);
    // Named before its ancestor, `c` is still one table, and `EXPLAIN` scans
    // it once on each side of the join.
    assert_eq!(reached("SELECT * FROM c, a", &database), ["a", "b", "c"]);
}

#[test]
fn a_view_carries_the_marker_its_definition_wrote() {
    let database = database();
    // `Append` over the three tables for `all_docs`, `Seq Scan on docs` for
    // `only_docs`.
    assert_eq!(reached("SELECT * FROM all_docs", &database), DOCS_TREE);
    assert_eq!(reached("SELECT * FROM only_docs", &database), ["docs"]);
}

#[test]
fn the_named_relations_stay_the_ones_written() {
    let database = database();
    for sql in ["SELECT * FROM docs", "SELECT * FROM docs *", "SELECT * FROM ONLY docs"] {
        assert_eq!(named(sql, &database), ["docs"], "{sql}");
    }
}

#[test]
fn update_and_delete_without_only_modify_every_inheriting_table() {
    let database = database();
    // `Update on docs` and `Delete on docs` list `docs`, `secret_docs` and
    // `top_secret_docs` as their targets.
    for sql in [
        "UPDATE docs SET body = 'x'",
        "UPDATE docs * SET body = 'x'",
        "DELETE FROM docs",
        "DELETE FROM docs *",
    ] {
        assert_eq!(reached(sql, &database), DOCS_TREE, "{sql}");
    }
    // `Update on docs` and `Delete on docs` over `Seq Scan on docs` alone.
    for sql in ["UPDATE ONLY docs SET body = 'x'", "DELETE FROM ONLY docs"] {
        assert_eq!(reached(sql, &database), ["docs"], "{sql}");
    }
}

#[test]
fn rows_from_inheriting_tables_carry_no_single_source() {
    let database = database();
    // `secret_docs` and `top_secret_docs` do not receive the primary key of
    // `docs`, so with `(1, 'a')` in `docs` and `(1, 'b', 2)` in `secret_docs`
    // `SELECT id FROM docs` returns `1` twice.
    assert_eq!(source("SELECT id, body FROM docs", &database), None);
    assert_eq!(source("SELECT d.id FROM docs d", &database), None);
    assert_eq!(source("SELECT id, body FROM ONLY docs", &database), Some("docs".to_owned()));
    assert_eq!(
        source("SELECT id FROM top_secret_docs", &database),
        Some("top_secret_docs".to_owned())
    );
    // Partitions share the key of the table they partition.
    assert_eq!(source("SELECT id, region FROM evt", &database), Some("evt".to_owned()));
}
