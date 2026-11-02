//! Tests that moving or renaming a function carries every call bound to it.
//!
//! PostgreSQL binds a call to the function it reaches when the call is
//! written, so `ALTER SCHEMA ... RENAME TO`, `ALTER FUNCTION ... RENAME TO` and
//! `ALTER FUNCTION ... SET SCHEMA` leave every default, check, generated
//! column, index, policy, view, rule, domain, trigger and `BEGIN ATOMIC` body
//! calling the same function under its new name. A body written as a string
//! keeps its text. The expectations below were read from PostgreSQL 16.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The schema `s` holding functions every kind of object calls.
const CALLED: &str = "
CREATE ROLE reader;
CREATE SCHEMA s;
CREATE FUNCTION s.f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
CREATE FUNCTION s.f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x';
CREATE FUNCTION s.g() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT s.f(); END;
CREATE FUNCTION s.k() RETURNS INT LANGUAGE sql RETURN s.f();
CREATE FUNCTION s.trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
CREATE TABLE other (x INT CHECK (x > 0));
CREATE TABLE t (a INT DEFAULT s.f(), b INT CHECK (s.f(b) > 0), c INT GENERATED ALWAYS AS (s.f(a)) STORED);
CREATE INDEX t_expr ON t (s.f(a));
CREATE TRIGGER t_trg BEFORE INSERT ON t FOR EACH ROW WHEN (s.f() > 0) EXECUTE FUNCTION s.trg();
CREATE POLICY p ON t USING (s.f() = 1);
CREATE VIEW v AS SELECT s.f() AS one;
CREATE MATERIALIZED VIEW mv AS SELECT s.f() AS one;
CREATE RULE r AS ON DELETE TO t WHERE s.f() > 0 DO INSTEAD NOTHING;
CREATE DOMAIN d AS INT DEFAULT s.f() CHECK (s.f(VALUE) > 0);
CREATE FUNCTION atomic_caller() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT s.f(); END;
CREATE FUNCTION text_caller() RETURNS INT LANGUAGE sql AS 'SELECT s.f()';
GRANT EXECUTE ON FUNCTION s.f() TO reader;
";

/// The text of every object calling into `s`, one line each.
fn call_sites(db: &ParserDB) -> Vec<String> {
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t is held");
    let mut sites = vec![table.to_string()];
    sites.extend(table.indices(db).expect("t is held").map(|index| index.attribute().to_string()));
    sites.extend(db.triggers().map(ToString::to_string));
    sites.extend(db.policies().map(ToString::to_string));
    sites.extend(db.views().map(|view| view.definition().to_string()));
    sites.extend(db.materialized_views().map(|view| view.definition().to_string()));
    sites.extend(db.rules().map(ToString::to_string));
    sites.extend(db.domains().map(ToString::to_string));
    sites.extend(
        db.functions()
            .filter(|function| ["g", "k", "atomic_caller"].contains(&function.name()))
            .map(ToString::to_string),
    );
    sites
}

/// Whether `sql` is refused because a call still reaches function `f`.
fn in_use(sql: &str) -> bool {
    matches!(parse(sql), Err(Error::FunctionReferenced { .. }))
}

#[test]
fn a_schema_rename_moves_its_functions_and_every_call() {
    let renamed = format!("{CALLED} ALTER SCHEMA s RENAME TO s2;");
    let db = parse(&renamed).expect("the rename is accepted");

    let mut moved: Vec<String> = db
        .functions()
        .filter(|function| matches!(function.target_name().schema(), Some("s" | "s2")))
        .map(|function| function.target_name().to_string())
        .collect();
    moved.sort();
    assert_eq!(moved, ["s2.f", "s2.f", "s2.g", "s2.k", "s2.trg"]);

    for site in call_sites(&db) {
        assert!(!site.contains("s.f(") && !site.contains("s.trg("), "{site}");
        assert!(site.contains("s2.f(") || site.contains("s2.trg("), "{site}");
    }
    let text_caller =
        db.functions().find(|function| function.name() == "text_caller").expect("kept");
    assert!(text_caller.to_string().contains("'SELECT s.f()'"), "{text_caller}");

    assert!(in_use(&format!("{renamed} DROP FUNCTION s2.f();")));
    assert!(in_use(&format!("{renamed} DROP FUNCTION s2.f(INT);")));
    assert!(in_use(&format!("{renamed} DROP FUNCTION s2.trg();")));
    assert!(in_use(&format!("{renamed} ALTER TABLE t ADD COLUMN e INT; DROP FUNCTION s2.f();")));

    let reader = db.role("reader").expect("the role is declared");
    let f = db
        .functions()
        .find(|function| function.name() == "f" && function.argument_type_names(&db).count() == 0)
        .expect("s2.f() is held");
    assert!(f.can_execute(reader, &db).expect("f is held"));
}

#[test]
fn a_renamed_function_is_called_by_its_new_name() {
    let renamed = format!("{CALLED} ALTER FUNCTION s.f() RENAME TO h;");
    let db = parse(&renamed).expect("the rename is accepted");

    for site in call_sites(&db) {
        assert!(!site.contains("s.f()"), "{site}");
    }
    let policy = db.policies().next().expect("the policy is kept").to_string();
    assert!(policy.contains("s.h() = 1"), "{policy}");
    let table = call_sites(&db).remove(0);
    assert!(table.contains("DEFAULT s.h()") && table.contains("s.f(b) > 0"), "{table}");

    assert!(in_use(&format!("{renamed} DROP FUNCTION s.h();")));
    parse(&format!(
        "{renamed} CREATE FUNCTION s.f() RETURNS INT AS 'SELECT 3'; DROP FUNCTION s.f();"
    ))
    .expect("no call reaches the new s.f");
}

#[test]
fn a_function_set_into_another_schema_is_called_there() {
    let moved = format!("{CALLED} CREATE SCHEMA o; ALTER FUNCTION s.f() SET SCHEMA o;");
    let db = parse(&moved).expect("the move is accepted");

    let view = db.views().next().expect("the view is kept").definition().to_string();
    assert!(view.contains("o.f()"), "{view}");
    let g = db.functions().find(|function| function.name() == "g").expect("g is kept");
    assert!(g.to_string().contains("o.f()"), "{g}");
    assert!(in_use(&format!("{moved} DROP FUNCTION o.f();")));

    let to_public =
        parse(&format!("{CALLED} ALTER FUNCTION s.trg() SET SCHEMA public;")).expect("moved");
    let trigger = to_public.triggers().next().expect("the trigger is kept").to_string();
    assert!(trigger.contains("EXECUTE FUNCTION public.trg()"), "{trigger}");
    assert!(in_use(&format!(
        "{CALLED} ALTER FUNCTION s.trg() SET SCHEMA public; DROP FUNCTION trg();"
    )));

    let from_public = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE t (a INT);
         CREATE TRIGGER t_trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();
         ALTER FUNCTION trg() SET SCHEMA app;",
    )
    .expect("moved");
    let trigger = from_public.triggers().next().expect("the trigger is kept").to_string();
    assert!(trigger.contains("EXECUTE FUNCTION app.trg()"), "{trigger}");
}

/// An unqualified call names its function by schema once it moves, since the
/// path it was written under may no longer reach it.
#[test]
fn an_unqualified_call_is_qualified_when_its_function_moves() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         SET search_path TO app;
         CREATE VIEW public.v AS SELECT f() AS one;
         RESET search_path;
         ALTER FUNCTION app.f() RENAME TO g;",
    )
    .expect("the rename is accepted");
    let view = db.views().next().expect("the view is kept").definition().to_string();
    assert!(view.contains("app.g()"), "{view}");
}

#[test]
fn a_move_postgresql_refuses_is_refused() {
    let base = "CREATE SCHEMA e; CREATE SCHEMA e2;
         CREATE FUNCTION e.a() RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION e.a(x INT) RETURNS INT AS 'SELECT x';
         CREATE FUNCTION e.b() RETURNS INT AS 'SELECT 2';
         CREATE FUNCTION e2.a() RETURNS INT AS 'SELECT 3';";
    let refused = |statement: &str| parse(&format!("{base} {statement}"));

    for statement in [
        "ALTER FUNCTION e.b() RENAME TO a;",
        "ALTER FUNCTION e.a() RENAME TO a;",
        "ALTER FUNCTION e.a() SET SCHEMA e2;",
    ] {
        let result = refused(statement);
        assert!(
            matches!(&result, Err(Error::FunctionAlreadyExists { function_name }) if function_name == "a"),
            "{statement} got {result:?}"
        );
    }
    let result = refused("ALTER FUNCTION e.a RENAME TO c;");
    assert!(matches!(result, Err(Error::AmbiguousAlterFunction { .. })), "got {result:?}");
    let result = refused("ALTER FUNCTION nope() RENAME TO x;");
    assert!(matches!(result, Err(Error::AlterFunctionNotFound { .. })), "got {result:?}");
    let result = refused("ALTER FUNCTION e.a() SET SCHEMA nowhere;");
    assert!(
        matches!(&result, Err(Error::AlterFunctionSchemaNotFound { schema_name, .. }) if schema_name == "nowhere"),
        "got {result:?}"
    );
    let result = refused("ALTER AGGREGATE e.a(INT) RENAME TO tally;");
    assert!(matches!(result, Err(Error::AggregateAlterationUnsupported { .. })), "got {result:?}");

    refused("ALTER FUNCTION e.a(INT) RENAME TO b;").expect("another signature of b is free");
    refused("ALTER FUNCTION e.a() SET SCHEMA e;").expect("a function may stay where it is");
}

/// A call arity alone cannot pin to one overload stays ambiguous, so moving
/// only some of the functions it may reach is refused.
#[test]
fn a_call_reaching_a_moved_and_a_staying_function_is_refused() {
    let overloads = "CREATE SCHEMA s;
         CREATE FUNCTION s.f(x INT) RETURNS INT AS 'SELECT x';
         CREATE FUNCTION s.f(x TEXT) RETURNS INT AS 'SELECT 1';
         CREATE TABLE t (a INT DEFAULT s.f(1));";
    let result = parse(&format!("{overloads} ALTER FUNCTION s.f(INT) RENAME TO g;"));
    assert!(
        matches!(&result, Err(Error::MovedFunctionCallAmbiguous { function_name, .. }) if function_name == "f"),
        "got {result:?}"
    );
    parse(&format!("{overloads} ALTER SCHEMA s RENAME TO s2;")).expect("both overloads move");

    let across_schemas = "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION public.f() RETURNS INT AS 'SELECT 2';
         SET search_path TO app, public;
         CREATE VIEW public.v AS SELECT f() AS one;
         RESET search_path;";
    let result = parse(&format!("{across_schemas} ALTER SCHEMA app RENAME TO app2;"));
    assert!(matches!(result, Err(Error::MovedFunctionCallAmbiguous { .. })), "got {result:?}");
}
