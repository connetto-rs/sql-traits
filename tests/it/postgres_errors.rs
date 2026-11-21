//! The SQLSTATE and the message `Error::postgres_error` answers, each pinned to
//! what PostgreSQL 18 reports for the same statements.
#![expect(clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::dialect::{Dialect, PostgreSqlDialect};

mod alterations_transactions;
mod inheritance_operators;
mod keys_triggers_grants;
mod lookup_columns;
mod persistence_partitions;
mod roles_functions_schemas;
mod types_settings;
mod views_constraints_functions;
mod written_tokens;

/// Asserts that `ParserDB` refuses `sql` under PostgreSQL with an error
/// answering `sqlstate` and `message`, as PostgreSQL 18 refuses its last
/// statement.
#[track_caller]
fn refuses(sql: &str, sqlstate: &str, message: &str) {
    let Err(error) = ParserDB::parse::<PostgreSqlDialect>(sql) else {
        panic!("ParserDB accepts {sql}");
    };
    let Some(refusal) = error.postgres_error() else {
        panic!("{error:?} answers no PostgreSQL error for {sql}");
    };
    assert_eq!((refusal.sqlstate().as_str(), refusal.message()), (sqlstate, message), "{sql}");
}

/// Asserts that `ParserDB` refuses `sql` under `D` with an error PostgreSQL
/// has no counterpart of.
#[track_caller]
fn refuses_without_counterpart<D: Dialect + Default + 'static>(sql: &str) {
    let Err(error) = ParserDB::parse::<D>(sql) else {
        panic!("ParserDB accepts {sql}");
    };
    assert_eq!(error.postgres_error(), None, "{error:?} for {sql}");
}
