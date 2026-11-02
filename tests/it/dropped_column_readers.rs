//! Tests that `ALTER TABLE ... DROP COLUMN` honours the views, materialized
//! views and bound function bodies reading the column.
//!
//! PostgreSQL records a dependency on every column a view or a `RETURN` or
//! `BEGIN ATOMIC` body names, so dropping the column is refused while one
//! names it, and `CASCADE` drops the reader along with whatever depends on
//! the reader in turn. A reader naming only other columns, or the row as a
//! whole, holds no such dependency. The expected outcomes are the ones
//! PostgreSQL 18 gives for the same statements.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::column_names::column_names;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Every view and materialized view name, sorted.
fn view_names(database: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> = database
        .views()
        .map(|view| view.view_name().to_owned())
        .chain(database.materialized_views().map(|view| view.view_name().to_owned()))
        .collect();
    names.sort();
    names
}

/// Every function name, sorted.
fn function_names(database: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> =
        database.functions().map(|function| function.name().to_owned()).collect();
    names.sort();
    names
}

const TABLE: &str = "CREATE TABLE t (a INT, b INT); CREATE TABLE u (a INT, z INT);";

#[test]
fn a_reader_naming_the_column_refuses_the_drop() {
    for reader in [
        "CREATE VIEW v AS SELECT a, b FROM t;",
        "CREATE VIEW v AS SELECT * FROM t;",
        "CREATE VIEW v AS SELECT b FROM t WHERE a > 0;",
        "CREATE MATERIALIZED VIEW v AS SELECT a FROM t;",
        "CREATE VIEW v AS WITH x AS (SELECT a FROM t) SELECT 1 AS one FROM x;",
        "CREATE VIEW v AS SELECT z FROM u WHERE EXISTS (SELECT 1 FROM t WHERE t.a = u.z);",
        "CREATE VIEW v AS SELECT b FROM t JOIN u USING (a);",
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t LIMIT 1);",
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;",
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO t (a) VALUES (1); END;",
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC UPDATE t SET a = 1; END;",
    ] {
        let sql = format!("{TABLE} {reader} ALTER TABLE t DROP COLUMN a;");
        let error = parse(&sql).expect_err(reader);
        assert!(
            matches!(&error, Error::ColumnReferenced { table_name, column_name }
                if table_name == "t" && column_name == "a"),
            "{reader}: {error:?}"
        );
    }
}

#[test]
fn a_reader_not_naming_the_column_lets_it_go() {
    for reader in [
        "CREATE VIEW v AS SELECT b FROM t;",
        "CREATE VIEW v AS SELECT count(*) AS n FROM t;",
        "CREATE VIEW v AS SELECT a FROM u;",
        "CREATE VIEW v AS SELECT x.q FROM t AS x (p, q);",
        "CREATE VIEW v AS SELECT t FROM t;",
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT a FROM t';",
        "CREATE FUNCTION f() RETURNS VOID LANGUAGE sql BEGIN ATOMIC INSERT INTO t (b) VALUES (1); END;",
    ] {
        let sql = format!("{TABLE} {reader} ALTER TABLE t DROP COLUMN a;");
        let database = parse(&sql).expect(reader);
        assert_eq!(column_names(&database, "t"), ["b"], "{reader}");
    }
}

#[test]
fn a_view_on_a_table_inheriting_the_column_refuses_the_drop() {
    let error = parse(
        "CREATE TABLE p (a INT, b INT);
         CREATE TABLE c () INHERITS (p);
         CREATE VIEW v AS SELECT a FROM c;
         ALTER TABLE p DROP COLUMN a;",
    )
    .expect_err("the child's column goes with the parent's");
    assert!(matches!(error, Error::ColumnReferenced { .. }), "{error:?}");
}

#[test]
fn cascade_drops_the_readers_and_what_reads_them() {
    let database = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v AS SELECT a, b FROM t;
         CREATE VIEW w AS SELECT b FROM v;
         CREATE MATERIALIZED VIEW m AS SELECT * FROM v;
         CREATE VIEW keep AS SELECT b FROM t;
         ALTER TABLE t DROP COLUMN a CASCADE;",
    )
    .expect("CASCADE drops the readers");
    assert_eq!(view_names(&database), ["keep"]);
    assert_eq!(column_names(&database, "t"), ["b"]);
}

#[test]
fn cascade_drops_a_reading_function_and_its_callers() {
    let database = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;
         CREATE FUNCTION g() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT f(); END;
         CREATE FUNCTION h() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT b FROM t; END;
         ALTER TABLE t DROP COLUMN a CASCADE;",
    )
    .expect("CASCADE drops the reading functions");
    let created: Vec<String> = function_names(&database)
        .into_iter()
        .filter(|name| ["f", "g", "h"].contains(&name.as_str()))
        .collect();
    assert_eq!(created, ["h"]);
}

#[test]
fn cascade_drops_the_rules_on_a_dropped_view() {
    let database = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v AS SELECT a, b FROM t;
         CREATE RULE r AS ON DELETE TO v DO INSTEAD NOTHING;
         ALTER TABLE t DROP COLUMN a CASCADE;",
    )
    .expect("CASCADE drops the view and its rule");
    assert_eq!(database.rules().count(), 0);
}

#[test]
fn each_clause_answers_to_its_own_cascade() {
    let database = parse(
        "CREATE TABLE t (a INT, b INT, c INT);
         CREATE VIEW v AS SELECT b FROM t;
         ALTER TABLE t DROP COLUMN a, DROP COLUMN b CASCADE;",
    )
    .expect("the second clause cascades");
    assert_eq!(database.views().count(), 0);
    assert_eq!(column_names(&database, "t"), ["c"]);

    let error = parse(
        "CREATE TABLE t (a INT, b INT, c INT);
         CREATE VIEW v AS SELECT b FROM t;
         ALTER TABLE t DROP COLUMN a CASCADE, DROP COLUMN b;",
    )
    .expect_err("the second clause does not cascade");
    assert!(
        matches!(&error, Error::ColumnReferenced { column_name, .. } if column_name == "b"),
        "{error:?}"
    );
}
