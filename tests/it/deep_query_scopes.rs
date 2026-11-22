//! Scoping queries nested far deeper than a thread's stack holds unguarded.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::prelude::*;
use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};

const STACK_SIZE: usize = 2 << 20;

const PARSER_STACK_SIZE: usize = 256 << 20;

/// Wraps `innermost` `depth` times in `wrap`.
fn nested(depth: usize, innermost: &str, wrap: fn(&str) -> String) -> String {
    (0..depth).fold(innermost.to_owned(), |sql, _| wrap(&sql))
}

/// Scopes the query `sql` on a thread whose stack is [`STACK_SIZE`], parsing
/// and dropping it on a larger one.
fn scope(sql: String) {
    std::thread::Builder::new()
        .stack_size(PARSER_STACK_SIZE)
        .spawn(move || {
            let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a int);").unwrap();
            let statement = Parser::new(&PostgreSqlDialect {})
                .with_recursion_limit(usize::MAX)
                .try_with_sql(&sql)
                .unwrap()
                .parse_statement()
                .unwrap();
            let Statement::Query(query) = statement else { panic!("expected a query") };
            std::thread::scope(|threads| {
                std::thread::Builder::new()
                    .stack_size(STACK_SIZE)
                    .spawn_scoped(threads, || {
                        ColumnScope::from_query(&query, &db, IdentifierCase::AsWritten).unwrap();
                    })
                    .unwrap()
                    .join()
                    .unwrap();
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn in_subquery(sql: &str) -> String {
    format!("SELECT a FROM t WHERE a IN ({sql})")
}

fn scalar_subquery(sql: &str) -> String {
    format!("SELECT ({sql}) AS a FROM t")
}

fn derived_table(sql: &str) -> String {
    format!("SELECT a FROM ({sql}) AS d")
}

fn derived_table_projecting_in_subquery(sql: &str) -> String {
    format!("SELECT a FROM (SELECT a IN (SELECT a FROM ({sql}) AS e) AS a FROM t) AS d")
}

fn parenthesized_join(from: &str) -> String {
    format!("({from} CROSS JOIN t AS u)")
}

#[test]
fn in_subqueries_nested_200_deep_scope() {
    scope(nested(200, "SELECT a FROM t", in_subquery));
}

#[test]
fn in_subqueries_nested_2000_deep_scope() {
    scope(nested(2000, "SELECT a FROM t", in_subquery));
}

#[test]
fn scalar_subqueries_nested_2000_deep_scope() {
    scope(nested(2000, "SELECT a FROM t", scalar_subquery));
}

#[test]
fn derived_tables_nested_200_deep_scope() {
    scope(nested(200, "SELECT a FROM t", derived_table));
}

#[test]
fn derived_tables_nested_2000_deep_scope() {
    scope(nested(2000, "SELECT a FROM t", derived_table));
}

#[test]
fn derived_tables_projecting_in_subqueries_nested_2000_deep_scope() {
    scope(nested(2000, "SELECT a FROM t", derived_table_projecting_in_subquery));
}

#[test]
fn parenthesized_joins_nested_2000_deep_scope() {
    scope(format!("SELECT a FROM {}", nested(2000, "t", parenthesized_join)));
}
