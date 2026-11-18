//! Tests that an index reports each key in order with the sort direction, the
//! null ordering and the operator class written for it.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::OrderBySort,
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect},
    parser::Parser,
};

/// One key as text: its expression, its direction, `NULLS FIRST` as `true`,
/// and its operator class.
type Key = (String, Option<OrderBySort>, Option<bool>, Option<String>);

fn build(dialect: &dyn Dialect, ddl: &str) -> ParserDB {
    let statements = Parser::parse_sql(dialect, ddl).expect("SQL parses");
    ParserDB::from_statements(statements, "test".to_string()).expect("schema builds")
}

fn keys<I: IndexLike<DB = ParserDB>>(database: &ParserDB, index: &I) -> Vec<Key> {
    index
        .keys(database)
        .expect("the index is in this database")
        .map(|key| {
            (
                key.expression().to_string(),
                key.order().cloned(),
                key.nulls_first(),
                key.operator_class().map(ToString::to_string),
            )
        })
        .collect()
}

fn index_keys(database: &ParserDB, name: &str) -> Vec<Key> {
    let index =
        database.indexes().find(|index| index.name() == Some(name)).expect("the index exists");
    keys(database, index)
}

fn unique_index_keys(database: &ParserDB) -> Vec<Vec<Key>> {
    database
        .tables()
        .flat_map(|table| table.unique_indices(database).expect("the table is in this database"))
        .map(|index| keys(database, index))
        .collect()
}

fn key(expression: &str) -> Key {
    (expression.to_owned(), None, None, None)
}

/// Indexes PostgreSQL serves different lookups with share one key expression,
/// and only the key options tell them apart.
#[test]
fn indexes_on_one_column_report_the_options_each_key_wrote() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE docs (id INT, title TEXT);
         CREATE INDEX plain ON docs (title);
         CREATE INDEX descending ON docs (title DESC NULLS LAST);
         CREATE INDEX pattern ON docs (title text_pattern_ops);
         CREATE INDEX collated ON docs (title COLLATE \"C\");",
    );
    assert_eq!(index_keys(&database, "plain"), [key("title")]);
    assert_eq!(
        index_keys(&database, "descending"),
        [("title".to_owned(), Some(OrderBySort::Desc), Some(false), None)]
    );
    assert_eq!(
        index_keys(&database, "pattern"),
        [("title".to_owned(), None, None, Some("text_pattern_ops".to_owned()))]
    );
    assert_eq!(index_keys(&database, "collated"), [key("title COLLATE \"C\"")]);
}

/// Each key keeps its own options in written order, an operator class keeps
/// its schema, and a written `ASC` stays apart from the default it agrees with.
#[test]
fn a_multi_key_index_reports_each_key_in_written_order() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE docs (id INT, title TEXT, body TEXT);
         CREATE INDEX mixed ON docs (id ASC, title public.text_pattern_ops NULLS FIRST, (lower(body)) DESC);",
    );
    assert_eq!(
        index_keys(&database, "mixed"),
        [
            ("id".to_owned(), Some(OrderBySort::Asc), None, None),
            ("title".to_owned(), None, Some(true), Some("public.text_pattern_ops".to_owned())),
            ("(lower(body))".to_owned(), Some(OrderBySort::Desc), None, None),
        ]
    );
}

/// A column rename rewrites the key expression and leaves its options alone.
#[test]
fn key_options_follow_a_renamed_column() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE docs (id INT, title TEXT);
         CREATE INDEX descending ON docs (title text_pattern_ops DESC);
         ALTER TABLE docs RENAME COLUMN title TO heading;",
    );
    assert_eq!(
        index_keys(&database, "descending"),
        [(
            "heading".to_owned(),
            Some(OrderBySort::Desc),
            None,
            Some("text_pattern_ops".to_owned())
        )]
    );
}

/// A key constraint declared on a column or in a later `ALTER TABLE` writes no
/// options, so every key answers the defaults.
#[test]
fn a_key_constraint_without_options_reports_the_defaults() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT PRIMARY KEY, a INT UNIQUE, b INT, c INT);
         ALTER TABLE t ADD CONSTRAINT bc UNIQUE (b, c);",
    );
    assert_eq!(
        unique_index_keys(&database),
        [vec![key("id")], vec![key("a")], vec![key("b"), key("c")]]
    );
}

/// MySQL builds a unique key in the order its column list writes, so a
/// descending key part is reported like an index's.
#[test]
fn a_mysql_unique_key_reports_a_descending_key_part() {
    let database =
        build(&MySqlDialect {}, "CREATE TABLE t (n INT, m INT, UNIQUE KEY u (n, m DESC));");
    assert_eq!(
        unique_index_keys(&database),
        [vec![key("n"), ("m".to_owned(), Some(OrderBySort::Desc), None, None)]]
    );
}
