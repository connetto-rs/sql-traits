//! Tests that an index whose column list carries an ordering qualifier
//! (`ASC`, `DESC`, `NULLS FIRST`, `NULLS LAST`) is ingested like the same
//! index without one.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::OrderBySort,
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect},
    parser::Parser,
};

/// The observable model of one index: its declared name, the expression it was
/// reduced to, whether that expression is a plain column list, and the table
/// columns it resolves to.
#[derive(Debug, PartialEq, Eq)]
struct IndexModel {
    name: Option<String>,
    expression: String,
    is_simple: bool,
    columns: Vec<String>,
}

fn build(dialect: &dyn Dialect, ddl: &str) -> ParserDB {
    let statements = Parser::parse_sql(dialect, ddl).expect("SQL parses");
    ParserDB::from_statements(statements, "test".to_string()).expect("schema builds")
}

/// Summarizes every index of the database `ddl` builds, or reports why the
/// schema was refused.
fn try_index_models(dialect: &dyn Dialect, ddl: &str) -> Result<Vec<IndexModel>, String> {
    let statements = Parser::parse_sql(dialect, ddl).expect("SQL parses");
    ParserDB::from_statements(statements, "test".to_string())
        .map(|database| index_models(&database))
        .map_err(|error| error.to_string())
}

/// Summarizes every index the database holds, in the order it holds them.
fn index_models(database: &ParserDB) -> Vec<IndexModel> {
    database
        .indexes()
        .map(|index| {
            IndexModel {
                name: index.name().map(ToString::to_string),
                expression: index
                    .expression(database)
                    .expect("index belongs to the database")
                    .to_string(),
                is_simple: index.is_simple(database).expect("index belongs to the database"),
                columns: index
                    .columns(database)
                    .expect("index belongs to the database")
                    .map(|column| column.column_name().to_string())
                    .collect(),
            }
        })
        .collect()
}

/// Summarizes every unique constraint declared on table `t`.
fn unique_index_models(database: &ParserDB) -> Vec<IndexModel> {
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("input declares table t");
    table
        .unique_indices(database)
        .expect("table belongs to the database")
        .map(|unique| {
            IndexModel {
                name: unique.name().map(ToString::to_string),
                expression: unique
                    .expression(database)
                    .expect("constraint belongs to the database")
                    .to_string(),
                is_simple: unique.is_simple(database).expect("constraint belongs to the database"),
                columns: unique
                    .columns(database)
                    .expect("constraint belongs to the database")
                    .map(|column| column.column_name().to_string())
                    .collect(),
            }
        })
        .collect()
}

const TABLE: &str = "CREATE TABLE t (id INT PRIMARY KEY, n INT, m INT, s TEXT);";

/// An ordering qualifier is ingested rather than rejected, and it changes
/// nothing about the resulting schema model: each qualified index below is
/// required to model identically to the unqualified index beside it.
#[test]
fn test_ordering_qualifier_matches_unqualified_index() {
    for (unqualified, qualified) in [
        ("CREATE INDEX i ON t (n);", "CREATE INDEX i ON t (n DESC);"),
        ("CREATE INDEX i ON t (n);", "CREATE INDEX i ON t (n ASC);"),
        ("CREATE INDEX i ON t (n);", "CREATE INDEX i ON t (n NULLS FIRST);"),
        ("CREATE INDEX i ON t (n);", "CREATE INDEX i ON t (n NULLS LAST);"),
        ("CREATE INDEX i ON t (n);", "CREATE INDEX i ON t (n ASC NULLS LAST);"),
        ("CREATE INDEX i ON t (n, m);", "CREATE INDEX i ON t (n, m DESC);"),
        ("CREATE INDEX i ON t (n, m);", "CREATE INDEX i ON t (n DESC, m);"),
        ("CREATE INDEX i ON t (n, m);", "CREATE INDEX i ON t (n DESC, m ASC);"),
        ("CREATE UNIQUE INDEX i ON t (n);", "CREATE UNIQUE INDEX i ON t (n DESC);"),
        ("CREATE INDEX i ON t (lower(s));", "CREATE INDEX i ON t (lower(s) DESC);"),
        ("CREATE INDEX i ON t (s COLLATE \"C\");", "CREATE INDEX i ON t (s COLLATE \"C\" DESC);"),
        (
            "CREATE INDEX i ON t (s text_pattern_ops);",
            "CREATE INDEX i ON t (s text_pattern_ops DESC);",
        ),
        ("CREATE INDEX i ON t (n) INCLUDE (m);", "CREATE INDEX i ON t (n DESC) INCLUDE (m);"),
    ] {
        let expected =
            index_models(&build(&PostgreSqlDialect {}, &format!("{TABLE}\n{unqualified}")));
        assert_eq!(expected.len(), 1, "{unqualified} declares one index");

        assert_eq!(
            try_index_models(&PostgreSqlDialect {}, &format!("{TABLE}\n{qualified}")),
            Ok(expected),
            "{qualified} must model exactly like {unqualified}"
        );
    }
}

/// The index reaches the database under its declared name, which is the
/// assertion the defect report states.
#[test]
fn test_ordered_index_is_named() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT PRIMARY KEY, n INT); CREATE INDEX i ON t (n DESC);",
    );
    let names: Vec<String> =
        database.indexes().filter_map(|i| i.name()).map(ToString::to_string).collect();
    assert_eq!(names, vec!["i".to_string()]);
}

/// A table-level `UNIQUE` whose column list carries an ordering qualifier is
/// recorded rather than dropped. This path reports no error, so a dropped
/// constraint would be silent schema loss.
#[test]
fn test_ordered_unique_constraint_is_recorded() {
    for (dialect, unqualified, qualified) in [
        (
            &PostgreSqlDialect {} as &dyn Dialect,
            "CREATE TABLE t (n INT, m INT, UNIQUE (n, m));",
            "CREATE TABLE t (n INT, m INT, UNIQUE (n, m DESC));",
        ),
        (
            &MySqlDialect {} as &dyn Dialect,
            "CREATE TABLE t (n INT, m INT, UNIQUE KEY u (n, m));",
            "CREATE TABLE t (n INT, m INT, UNIQUE KEY u (n, m DESC));",
        ),
    ] {
        let expected = unique_index_models(&build(dialect, unqualified));
        assert_eq!(expected.len(), 1, "{unqualified} declares one unique constraint");

        assert_eq!(
            unique_index_models(&build(dialect, qualified)),
            expected,
            "{qualified} must model exactly like {unqualified}"
        );
    }
}

/// The qualifier is absent from the derived key expression because it
/// qualifies a key rather than forming part of one, not because it was thrown
/// away: it stays readable on the index node. The equality above would hold
/// just as well of an implementation that discarded it outright, so this pins
/// the half that equality cannot see.
#[test]
fn test_ordering_qualifier_stays_readable_on_the_index_node() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT PRIMARY KEY, n INT, m INT);
         CREATE INDEX i ON t (n DESC NULLS LAST, m ASC);",
    );
    let index = database.indexes().next().expect("input declares an index");

    let declared: Vec<(Option<OrderBySort>, Option<bool>)> = index
        .attribute()
        .columns
        .iter()
        .map(|column| (column.column.options.sort.clone(), column.column.options.nulls_first))
        .collect();

    assert_eq!(
        declared,
        vec![(Some(OrderBySort::Desc), Some(false)), (Some(OrderBySort::Asc), None)]
    );
}
