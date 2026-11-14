//! Tests that `DROP SCHEMA` counts the functions and views a schema holds.
//!
//! PostgreSQL refuses to drop a schema holding a function unless the
//! statement says `CASCADE`, which drops its tables, views, materialized views
//! and functions alike, with what elsewhere reads or calls them.
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

/// A caller outside the schema goes with the function it calls, as
/// PostgreSQL 18 takes the default of `o.b`, `function g()` and `index oi`.
#[test]
fn a_cascade_takes_what_outside_the_schema_calls_its_functions() {
    parse(&format!(
        "{APP} CREATE TABLE public.o (b INT DEFAULT app.f(), c INT);
         CREATE FUNCTION public.g() RETURNS INT LANGUAGE sql IMMUTABLE RETURN app.f();
         CREATE INDEX oi ON public.o ((c + public.g()));
         DROP SCHEMA app CASCADE;
         CREATE FUNCTION public.g() RETURNS INT LANGUAGE sql RETURN 1;
         CREATE INDEX oi ON public.o (c);"
    ))
    .expect("the cascade takes the default, g and oi");
}
