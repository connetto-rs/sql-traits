//! Tests that a stored view's definition is read under the comparison its
//! catalog was parsed with.
//!
//! Measured on SQLite 3.51.1: after `CREATE TABLE "T"("X" INT); CREATE VIEW v
//! AS SELECT x FROM t; INSERT INTO "T" VALUES (1);`, `SELECT x FROM v` returns
//! `1`. PostgreSQL refuses that view, since `t` names no relation there.
#![expect(clippy::expect_used, reason = "the fixtures declare every object a test reads")]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Ident, Query, Statement},
    dialect::SQLiteDialect,
    parser::Parser,
};

fn folded(schema: &str) -> ParserDB {
    ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(schema)
        .expect("schema parses")
}

/// The query `sql` writes, parsed under SQLite.
fn query(sql: &str) -> Box<Query> {
    Parser::parse_sql(&SQLiteDialect {}, sql)
        .expect("query parses")
        .into_iter()
        .find_map(|statement| {
            match statement {
                Statement::Query(query) => Some(query),
                _ => None,
            }
        })
        .expect("a SELECT parses as a query")
}

/// The table `SELECT x FROM v` traces its column `x` to.
fn traced_table(database: &ParserDB) -> Option<String> {
    let query = query("SELECT x FROM v");
    let scope = ColumnScope::from_query(&query, database, IdentifierCase::Folded)
        .expect("the scope builds");
    scope
        .resolve_column(&Expr::Identifier(Ident::new("x")))
        .expect("the column resolves unambiguously")
        .map(|table| table.table_name().to_owned())
}

/// A view reaches a table stored under another spelling of its name.
#[test]
fn a_folded_view_reaches_a_table_quoted_in_another_case() {
    let database = folded(r#"CREATE TABLE "T"(x INT); CREATE VIEW v AS SELECT x FROM t;"#);
    assert_eq!(traced_table(&database).as_deref(), Some("T"));
}

/// A view reaches a column stored under another spelling of its name.
#[test]
fn a_folded_view_reaches_a_column_quoted_in_another_case() {
    let database = folded(r#"CREATE TABLE t("X" INT); CREATE VIEW v AS SELECT x FROM t;"#);
    assert_eq!(traced_table(&database).as_deref(), Some("t"));
}

/// A view reaches through another view stored under another spelling.
#[test]
fn a_folded_view_reaches_through_a_view_quoted_in_another_case() {
    let database = folded(
        r#"CREATE TABLE "T"(x INT);
        CREATE VIEW "Inner" AS SELECT x FROM t;
        CREATE VIEW v AS SELECT x FROM inner;"#,
    );
    assert_eq!(traced_table(&database).as_deref(), Some("T"));
}

/// A qualified column in a view reaches its relation under another spelling.
#[test]
fn a_folded_view_qualifies_a_column_by_another_spelling() {
    let database = folded(r#"CREATE TABLE "T"(x INT); CREATE VIEW v AS SELECT T.x FROM t;"#);
    assert_eq!(traced_table(&database).as_deref(), Some("T"));
}

/// A query's single source table compares under the catalog's rule, the
/// comparison the parse states, since the call takes none of its own.
#[test]
fn a_folded_catalog_finds_the_source_table_of_a_query() {
    let database = folded(r#"CREATE TABLE "T"(id INT PRIMARY KEY, "X" INT);"#);
    let source = query("SELECT t.x FROM t")
        .projection_source_table(&database)
        .expect("the lookup is unambiguous");
    assert_eq!(source.map(TableLike::table_name), Some("T"));
}

/// A catalog parsed under PostgreSQL's rule keeps the quoted spelling apart
/// inside a view, whatever comparison the outer query states.
#[test]
fn an_as_written_view_keeps_a_quoted_table_apart() {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::AsWritten)
        .parse::<SQLiteDialect>(r#"CREATE TABLE "T"(x INT); CREATE VIEW v AS SELECT x FROM t;"#)
        .expect("schema parses");
    assert_eq!(traced_table(&database), None);
}
