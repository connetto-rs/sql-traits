//! Tests that the name a trigger's `EXECUTE FUNCTION` writes reaches the
//! function PostgreSQL's zero-argument lookup finds, whatever overloads that
//! name carries elsewhere.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{Ident, ObjectName},
    dialect::PostgreSqlDialect,
};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn name(parts: &[&str]) -> ObjectName {
    ObjectName::from(parts.iter().copied().map(Ident::new).collect::<Vec<_>>())
}

/// The schema of the function `called` reaches, `None` when it reaches none.
fn reached_schema(db: &ParserDB, called: &[&str]) -> Result<Option<String>, LookupError> {
    Ok(db
        .resolve_trigger_function(&name(called), IdentifierCase::AsWritten)?
        .map(|function| function.target_name().schema().unwrap_or("public").to_owned()))
}

const TRIGGER_BODY: &str = "RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$";

/// `pg_catalog` declares `log` three times, each taking arguments, so none of
/// them is a trigger function's candidate and the declared one answers.
#[test]
fn a_name_overloaded_in_pg_catalog_reaches_the_zero_argument_function() -> Result<(), LookupError> {
    let db = db(&format!("CREATE FUNCTION log() {TRIGGER_BODY};"));

    assert_eq!(reached_schema(&db, &["log"])?.as_deref(), Some("public"));
    assert_eq!(reached_schema(&db, &["public", "log"])?.as_deref(), Some("public"));
    assert_eq!(reached_schema(&db, &["absent"])?, None);
    Ok(())
}

/// Of two zero-argument functions on the path, the earlier schema's answers,
/// and a qualified name reaches its own schema whatever the path says.
#[test]
fn the_search_path_order_decides_and_a_qualifier_overrides_it() -> Result<(), LookupError> {
    let db = db(&format!(
        "CREATE SCHEMA app; CREATE SCHEMA audit;
         CREATE FUNCTION app.log() {TRIGGER_BODY};
         CREATE FUNCTION audit.log() {TRIGGER_BODY};
         SET search_path TO audit, app;"
    ));

    assert_eq!(reached_schema(&db, &["log"])?.as_deref(), Some("audit"));
    assert_eq!(reached_schema(&db, &["app", "log"])?.as_deref(), Some("app"));
    assert_eq!(reached_schema(&db, &["public", "log"])?, None);
    Ok(())
}

/// A function every argument of which has a default takes a call passing
/// none, but the trigger lookup does not fill defaults, so it neither answers
/// nor hides the zero-argument function of a later schema.
#[test]
fn a_function_taking_only_defaulted_arguments_is_passed_over() -> Result<(), LookupError> {
    let db = db(&format!(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.log(level INT DEFAULT 1) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION public.log() {TRIGGER_BODY};
         SET search_path TO app, public;"
    ));

    assert_eq!(reached_schema(&db, &["log"])?.as_deref(), Some("public"));
    assert_eq!(reached_schema(&db, &["app", "log"])?, None);
    Ok(())
}

/// `pg_catalog` is searched first unless the path names it, so its
/// zero-argument `now` hides a declared one until the path puts it later.
#[test]
fn pg_catalog_comes_first_unless_the_path_places_it() -> Result<(), LookupError> {
    let declared = format!("CREATE FUNCTION now() {TRIGGER_BODY};");

    let catalog_first = db(&declared);
    assert_eq!(reached_schema(&catalog_first, &["now"])?.as_deref(), Some("pg_catalog"));

    let catalog_last = db(&format!("{declared} SET search_path TO public, pg_catalog;"));
    assert_eq!(reached_schema(&catalog_last, &["now"])?.as_deref(), Some("public"));
    Ok(())
}

/// A procedure is not a function a trigger can run, so a name reaching one
/// first reaches nothing, as PostgreSQL refuses it rather than looking on.
#[test]
fn a_procedure_reached_first_answers_nothing() -> Result<(), LookupError> {
    let db = db(&format!(
        "CREATE SCHEMA app;
         CREATE PROCEDURE app.log() LANGUAGE plpgsql AS $$ BEGIN END $$;
         CREATE FUNCTION public.log() {TRIGGER_BODY};
         SET search_path TO app, public;"
    ));

    assert_eq!(reached_schema(&db, &["log"])?, None);
    assert_eq!(reached_schema(&db, &["public", "log"])?.as_deref(), Some("public"));
    Ok(())
}

/// A name reaching past a schema into another catalog names no function.
#[test]
fn a_three_part_name_is_refused() {
    let db = db(&format!("CREATE FUNCTION log() {TRIGGER_BODY};"));

    let result = db
        .resolve_trigger_function(&name(&["catalog", "public", "log"]), IdentifierCase::AsWritten);
    assert!(matches!(result, Err(LookupError::InvalidObjectName { .. })), "{result:?}");
}

/// The stored trigger runs the function its written name reaches, the
/// reproduction a consumer holding only that name failed on.
#[test]
fn the_stored_trigger_and_its_written_name_reach_one_function() -> Result<(), LookupError> {
    let db = db("CREATE TABLE t (id INT PRIMARY KEY, c INT);
         CREATE TABLE audit (n TEXT);
         CREATE FUNCTION log() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO audit (n) VALUES (NEW.c::text); RETURN NEW; END $$;
         CREATE TRIGGER tw AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION log();");

    let trigger = db.triggers().next().expect("the trigger is stored");
    let stored = trigger.function(&db).expect("the trigger runs a function");
    let written = db
        .resolve_trigger_function(&name(&["log"]), IdentifierCase::AsWritten)?
        .expect("the written name reaches a function");
    assert!(core::ptr::eq(stored, written));
    Ok(())
}
