//! Tests that `SELECT ... INTO` creates the table PostgreSQL creates for it.
//!
//! PostgreSQL runs `SELECT ... INTO t` as `CREATE TABLE t AS` over the same
//! query, so each form here is checked against that spelling. Every
//! expectation was measured on PostgreSQL 18.6.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

const SOURCE: &str = "CREATE ROLE r; CREATE SCHEMA s;
    CREATE TABLE src (id INT NOT NULL DEFAULT 1, name TEXT COLLATE \"C\", n SERIAL);";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SOURCE} {sql}"))
}

/// What the created table `t` records: its schema, persistence, owner and
/// columns with their types.
fn shape(db: &ParserDB) -> String {
    let table =
        db.tables().find(|table| table.table_name() == "t").expect("the statement created t");
    let columns: Vec<String> = table
        .columns(db)
        .expect("columns")
        .map(|column| format!("{} {}", column.column_name(), column.data_type(db)))
        .collect();
    format!(
        "schema={:?} temporary={} unlogged={} owner={:?} columns={columns:?}",
        table.table_schema(),
        table.is_temporary(),
        table.is_unlogged(),
        table.owner(db).expect("owner lookup"),
    )
}

#[test]
fn select_into_creates_the_table_create_table_as_creates() {
    for (select_into, create_table_as) in [
        ("SELECT id, name INTO t FROM src;", "CREATE TABLE t AS SELECT id, name FROM src;"),
        ("SELECT * INTO TABLE t FROM src;", "CREATE TABLE t AS SELECT * FROM src;"),
        ("SELECT 1, 'x' AS y INTO t;", "CREATE TABLE t AS SELECT 1, 'x' AS y;"),
        ("SELECT 1 AS a INTO t UNION SELECT 2;", "CREATE TABLE t AS SELECT 1 AS a UNION SELECT 2;"),
        (
            "WITH w AS (SELECT id FROM src) SELECT id INTO t FROM w;",
            "CREATE TABLE t AS WITH w AS (SELECT id FROM src) SELECT id FROM w;",
        ),
        ("(SELECT 1 AS a INTO t);", "CREATE TABLE t AS SELECT 1 AS a;"),
        ("SELECT id INTO s.t FROM src;", "CREATE TABLE s.t AS SELECT id FROM src;"),
        ("SELECT id INTO TEMP t FROM src;", "CREATE TEMP TABLE t AS SELECT id FROM src;"),
        ("SELECT id INTO UNLOGGED t FROM src;", "CREATE UNLOGGED TABLE t AS SELECT id FROM src;"),
        ("SELECT id INTO t FROM src FOR UPDATE;", "CREATE TABLE t AS SELECT id FROM src;"),
        (
            "SET ROLE r; SELECT id INTO t FROM src;",
            "SET ROLE r; CREATE TABLE t AS SELECT id FROM src;",
        ),
    ] {
        let selected = parse(select_into).unwrap_or_else(|error| panic!("{select_into}: {error}"));
        let created = parse(create_table_as).expect("the CREATE TABLE AS spelling parses");
        assert_eq!(shape(&selected), shape(&created), "{select_into}");
    }
}

/// The table is in the catalog for every later statement, as any table is.
#[test]
fn later_statements_reach_the_selected_table() {
    let db = parse(
        "SELECT id INTO t FROM src;
         CREATE INDEX t_id ON t (id);
         GRANT SELECT ON t TO r;
         CREATE VIEW v AS SELECT id FROM t;",
    )
    .expect("every statement finds t");
    assert_eq!(db.views().count(), 1);
}

#[test]
fn select_into_refuses_what_create_table_as_refuses() {
    for (select_into, create_table_as) in [
        ("SELECT 1 AS c, 2 AS c INTO t;", "CREATE TABLE t AS SELECT 1 AS c, 2 AS c;"),
        ("SELECT 1, 2 INTO t;", "CREATE TABLE t AS SELECT 1, 2;"),
        ("SELECT id INTO src FROM src;", "CREATE TABLE src AS SELECT id FROM src;"),
        (
            "CREATE TABLE t (x INT); SELECT 1 AS c, 2 AS c INTO t;",
            "CREATE TABLE t (x INT); CREATE TABLE t AS SELECT 1 AS c, 2 AS c;",
        ),
    ] {
        let refused = parse(select_into).expect_err(select_into);
        let expected = parse(create_table_as).expect_err(create_table_as);
        assert_eq!(refused.to_string(), expected.to_string(), "{select_into}");
    }
    assert!(
        matches!(parse("SELECT 1 AS c, 2 AS c INTO t;"), Err(Error::ColumnAlreadyExists { .. })),
        "PostgreSQL refuses the repeated column"
    );
    for (sql, taken_by_a_table) in [
        ("CREATE TABLE t (x INT); SELECT 1 AS c, 2 AS c INTO t;", true),
        ("CREATE TABLE t (x INT); CREATE TABLE t AS SELECT 1 AS c, 2 AS c;", true),
        ("CREATE VIEW t AS SELECT 1 AS x; SELECT 1 AS c, 2 AS c INTO t;", false),
        ("CREATE VIEW t AS SELECT 1 AS x; CREATE TABLE t AS SELECT 1 AS c, 2 AS c;", false),
    ] {
        let refused = parse(sql);
        let reported_the_name = if taken_by_a_table {
            matches!(
                refused,
                Err(Error::IdentifierLookupError(LookupError::TableLookupConflict { .. }))
            )
        } else {
            matches!(refused, Err(Error::RelationNameAlreadyTaken { .. }))
        };
        assert!(
            reported_the_name,
            "PostgreSQL reports the taken name first: {sql} got {refused:?}"
        );
    }
}

/// `EXPLAIN` plans the statement without running it, so nothing is created.
#[test]
fn explaining_select_into_creates_nothing() {
    let db = parse("EXPLAIN SELECT id INTO t FROM src;").expect("parses");
    assert!(db.tables().all(|table| table.table_name() != "t"));
}

/// MySQL's `SELECT ... INTO` assigns variables and creates no table.
#[test]
fn select_into_creates_a_table_only_under_postgresql() {
    let db = ParserDB::parse::<MySqlDialect>("SELECT 1 INTO @x;").expect("parses");
    assert_eq!(db.tables().count(), 0);
}
