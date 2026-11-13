//! Tests that `DROP SCHEMA` counts the functions and views a schema holds.
//!
//! PostgreSQL 16 refuses to drop a schema holding a function unless the
//! statement says `CASCADE`, which drops its tables, views, materialized views
//! and functions alike, with the views elsewhere reading them.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The schema `app` holding a function, a table and views calling and reading
/// one another, and a view outside reading one of them.
const APP: &str = "CREATE SCHEMA app;
     CREATE FUNCTION app.f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE TABLE app.t (a INT DEFAULT app.f());
     CREATE VIEW app.v AS SELECT a, app.f() AS one FROM app.t;
     CREATE MATERIALIZED VIEW app.mv AS SELECT 1 AS a;
     CREATE VIEW public.pv AS SELECT * FROM app.v;";

#[test]
fn a_function_keeps_its_schema_from_a_plain_drop() {
    let result = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         DROP SCHEMA app;",
    );
    assert!(
        matches!(&result, Err(Error::SchemaNotEmpty { schema_name }) if schema_name == "app"),
        "got {result:?}"
    );
}

#[test]
fn a_cascade_drops_the_functions_and_views_of_the_schema() {
    let db = parse(&format!("{APP} DROP SCHEMA app CASCADE;")).expect("the cascade is accepted");
    assert_eq!(db.functions().filter(|function| function.name() == "f").count(), 0);
    assert_eq!(db.views().count(), 0, "v and the view reading it went");
    assert_eq!(db.materialized_views().count(), 0);
    assert_eq!(db.tables().count(), 0);
}

/// PostgreSQL also drops a caller outside the schema, which this model does
/// for no function drop, so the cascade is refused rather than leaving the
/// call bound to a function that is gone.
#[test]
fn a_cascade_reaching_a_call_outside_the_schema_is_refused() {
    let result = parse(&format!(
        "{APP} CREATE TABLE public.o (b INT DEFAULT app.f()); DROP SCHEMA app CASCADE;"
    ));
    assert!(
        matches!(&result, Err(Error::FunctionReferenced { function_name, .. }) if function_name == "f"),
        "got {result:?}"
    );
}
