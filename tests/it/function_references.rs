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

/// A trigger name is unique per table, so one name on two tables names two
/// triggers, each with the calls of its own `WHEN` condition.
#[test]
fn a_trigger_name_on_two_tables_names_each_tables_own_trigger() {
    let on_a =
        "CREATE TRIGGER t BEFORE INSERT ON a FOR EACH ROW WHEN (fa() > 0) EXECUTE FUNCTION trg();";
    let on_b =
        "CREATE TRIGGER t BEFORE INSERT ON b FOR EACH ROW WHEN (fb() > 0) EXECUTE FUNCTION trg();";
    for triggers in [format!("{on_a} {on_b}"), format!("{on_b} {on_a}")] {
        let db = parse(&format!(
            "CREATE FUNCTION fa() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
             CREATE FUNCTION fb() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';
             CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
             CREATE TABLE a (id INT);
             CREATE TABLE b (id INT);
             {triggers}"
        ))
        .expect("schema parses");
        for (table, called) in [("a", "fa"), ("b", "fb")] {
            let table = db
                .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
                .expect("the lookup runs")
                .expect("the table exists");
            let trigger = db.trigger("t", table).expect("the table carries t");
            assert_eq!(trigger.table(&db).expect("the trigger's table"), table);
            let calls: Vec<&str> = db
                .trigger_metadata(trigger)
                .expect("the trigger is held")
                .functions()
                .map(FunctionLike::name)
                .collect();
            assert_eq!(calls, [called], "{triggers}");
        }
    }
}

/// The functions `f` and `trg` and the table `t` every dependent below is
/// written against.
const CALLED: &str = "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
     CREATE TABLE t (a INT);";

/// A statement writing a call to `f` into each object PostgreSQL binds the
/// call of at creation.
const DEPENDENTS: [&str; 13] = [
    "CREATE TABLE d (a INT DEFAULT f());",
    "CREATE TABLE d (a INT GENERATED ALWAYS AS (f()) STORED);",
    "ALTER TABLE t ALTER COLUMN a SET DEFAULT f();",
    "ALTER TABLE t ADD COLUMN b INT DEFAULT f();",
    "CREATE INDEX i ON t ((f() + a));",
    "CREATE INDEX i ON t (a) WHERE f() > 0;",
    "CREATE VIEW v AS SELECT f() AS one;",
    "CREATE MATERIALIZED VIEW v AS SELECT f() AS one;",
    "CREATE RULE r AS ON INSERT TO t WHERE f() > 0 DO INSTEAD NOTHING;",
    "CREATE RULE r AS ON INSERT TO t DO INSTEAD SELECT f();",
    "CREATE DOMAIN d AS INT CHECK (VALUE > f());",
    "CREATE DOMAIN d AS INT DEFAULT f();",
    "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (f() > 0) EXECUTE FUNCTION trg();",
];

/// Each object a call is bound into holds the function, so dropping it is
/// refused while the object remains.
#[test]
fn every_bound_call_holds_its_function() {
    for dependent in DEPENDENTS {
        parse(&format!("{CALLED} {dependent}")).expect(dependent);
        let result = parse(&format!("{CALLED} {dependent} DROP FUNCTION f();"));
        assert!(
            matches!(&result, Err(Error::FunctionReferenced { function_name }) if function_name == "f"),
            "{dependent} got {result:?}"
        );
    }
}

/// Replacing or altering the function keeps it the one each object calls.
#[test]
fn a_replaced_function_stays_held() {
    for dependent in DEPENDENTS {
        for change in [
            "CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';",
            "ALTER FUNCTION f() STABLE;",
        ] {
            let result = parse(&format!("{CALLED} {dependent} {change} DROP FUNCTION f();"));
            assert!(
                matches!(&result, Err(Error::FunctionReferenced { .. })),
                "{dependent} {change} got {result:?}"
            );
        }
    }
}

/// A call binds under the search path of the statement writing it, and keeps
/// that function when the path changes and another function of the name
/// appears.
#[test]
fn a_bound_call_keeps_the_function_its_path_reached() {
    let bound = "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         SET search_path TO app, public;
         CREATE TABLE d (a INT DEFAULT f());
         CREATE VIEW v AS SELECT f() AS one;
         CREATE DOMAIN dd AS INT DEFAULT f();
         RESET search_path;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 2';";
    parse(&format!("{bound} DROP FUNCTION public.f();")).expect("every call reached app.f");
    assert_in_use(&parse(&format!("{bound} DROP FUNCTION app.f();")));
}

/// Removing the call, or the object holding it, frees the function.
#[test]
fn removing_the_call_frees_the_function() {
    for (dependent, release) in [
        ("CREATE TABLE d (a INT DEFAULT f());", "ALTER TABLE d ALTER COLUMN a DROP DEFAULT;"),
        ("CREATE TABLE d (a INT DEFAULT f());", "DROP TABLE d;"),
        ("CREATE INDEX i ON t ((f() + a));", "DROP INDEX i;"),
        ("CREATE VIEW v AS SELECT f() AS one;", "CREATE OR REPLACE VIEW v AS SELECT 1 AS one;"),
        ("CREATE MATERIALIZED VIEW v AS SELECT f() AS one;", "DROP MATERIALIZED VIEW v;"),
        ("CREATE DOMAIN d AS INT DEFAULT f();", "DROP DOMAIN d;"),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (f() > 0) EXECUTE FUNCTION trg();",
            "DROP TRIGGER tr ON t;",
        ),
    ] {
        let result = parse(&format!("{CALLED} {dependent} {release} DROP FUNCTION f();"));
        assert!(result.is_ok(), "{dependent} {release} got {result:?}");
    }
}

/// A table rebuilt under another search path keeps the functions its checks,
/// defaults and indexes were bound to, and a call it writes anew binds under
/// the path in force.
#[test]
fn a_rebuilt_table_keeps_its_bound_calls() {
    let bound = "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         SET search_path TO app, public;
         CREATE TABLE d (a INT DEFAULT f(), b INT CHECK (b > f()), c INT, CHECK (c > f()));
         CREATE INDEX i ON d ((f() + a));
         RESET search_path;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 2';";
    for rebuild in [
        "ALTER TABLE app.d ADD COLUMN e INT;",
        "ALTER TABLE app.d RENAME COLUMN a TO z;",
        "ALTER TABLE app.d ALTER COLUMN c SET DEFAULT f();",
    ] {
        let rebuilt = format!("{bound} {rebuild}");
        assert_in_use(&parse(&format!("{rebuilt} DROP FUNCTION app.f();")));
        let public = parse(&format!("{rebuilt} DROP FUNCTION public.f();"));
        assert_eq!(public.is_ok(), !rebuild.contains("SET DEFAULT"), "{rebuild} got {public:?}");
    }
}
