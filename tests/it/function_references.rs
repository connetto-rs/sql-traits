//! Tests that a function counts as in use only where a call can reach it.
//!
//! PostgreSQL resolves a call by schema and by arguments, so a function of the
//! same name in another schema or taking other arguments is free to drop.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn assert_in_use(result: &Result<ParserDB, Error>) {
    assert!(
        matches!(result, Err(Error::FunctionReferenced { function_name }) if function_name == "f"),
        "got {result:?}"
    );
}

#[test]
fn a_function_in_a_schema_no_call_reaches_can_be_dropped() {
    let calls_public = "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION app.f() RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT, CHECK (f()));
         CREATE POLICY p ON t USING (f()) WITH CHECK (f());";
    let dropped = parse(&format!("{calls_public} DROP FUNCTION app.f();"))
        .expect("the unqualified calls reach public.f");
    assert_eq!(dropped.functions().filter(|function| function.name() == "f").count(), 1);
    assert_in_use(&parse(&format!("{calls_public} DROP FUNCTION f();")));

    let calls_app = "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION app.f() RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (app.f());";
    parse(&format!("{calls_app} DROP FUNCTION f();")).expect("the call names app.f");
    assert_in_use(&parse(&format!("{calls_app} DROP FUNCTION app.f();")));
}

/// An unqualified call looks through the search path in force when it was
/// written.
#[test]
fn an_unqualified_call_reaches_the_schemas_on_the_search_path() {
    let result = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS BOOL AS 'SELECT true';
         SET search_path TO app, public;
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f());
         DROP FUNCTION app.f();",
    );
    assert_in_use(&result);
}

/// The number of arguments narrows a call to the overloads that accept it,
/// and argument types are not inferred, so every overload taking that many
/// arguments stays reachable.
#[test]
fn a_call_reaches_the_overloads_that_accept_its_arguments() {
    let overloads = "CREATE FUNCTION f(x INT) RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION f(x TEXT) RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION f(x INT, y INT) RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f(id));";
    parse(&format!("{overloads} DROP FUNCTION f(INT, INT);"))
        .expect("no call passes two arguments");
    assert_in_use(&parse(&format!("{overloads} DROP FUNCTION f(TEXT);")));

    let defaulted = parse(
        "CREATE FUNCTION f(x INT, y INT DEFAULT 0) RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f(id));
         DROP FUNCTION f(INT, INT);",
    );
    assert_in_use(&defaulted);

    let variadic = parse(
        "CREATE FUNCTION f(VARIADIC xs INT[]) RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f(id, id, id));
         DROP FUNCTION f(INT[]);",
    );
    assert_in_use(&variadic);
}

/// A trigger runs a function taking no arguments, found the way a call is.
#[test]
fn a_trigger_reaches_only_the_function_it_runs() {
    let triggered = "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION app.f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1';
         CREATE TABLE t (id INT);
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();";
    parse(&format!("{triggered} DROP FUNCTION app.f();")).expect("the trigger runs public.f");
    parse(&format!("{triggered} DROP FUNCTION f(INT);")).expect("a trigger function takes nothing");
    assert_in_use(&parse(&format!("{triggered} DROP FUNCTION f();")));
}

/// The function a trigger reports is the one it was bound to, however many
/// functions of that name other schemas hold.
#[test]
fn a_trigger_reports_the_function_it_runs() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE t (id INT);
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();",
    )
    .expect("schema parses");
    let trigger = db.triggers().next().expect("the trigger exists");
    let function = trigger.function(&db).expect("the trigger runs a function");

    assert_eq!(function.target_name().schema(), None);
    assert_eq!(function.argument_type_names(&db).count(), 0);

    let spelled_public = parse(
        "CREATE FUNCTION public.f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE t (id INT);
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();",
    )
    .expect("schema parses");
    let trigger = spelled_public.triggers().next().expect("the trigger exists");
    assert!(trigger.function(&spelled_public).is_some(), "public.f is the f a bare name reaches");
}
