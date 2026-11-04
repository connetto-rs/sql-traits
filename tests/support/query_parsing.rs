//! Parses one query out of a single statement.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test helper fails loudly on a broken premise"
)]

use sqlparser::{
    ast::{Query, Statement},
    dialect::Dialect,
    parser::Parser,
};

/// The query `sql` holds as its only statement.
pub(crate) fn query_with(dialect: &dyn Dialect, sql: &str) -> Query {
    let mut statements = Parser::parse_sql(dialect, sql)
        .unwrap_or_else(|error| panic!("statement `{sql}` parses: {error}"));
    match statements.pop().expect("one statement") {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}
