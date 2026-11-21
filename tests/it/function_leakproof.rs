//! Effective function leakproof status across declarations, replacements and
//! alterations.
#![expect(clippy::expect_used, reason = "test fixtures must parse and resolve")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::function_overloads::overload;

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn leakproof<DB: DatabaseLike>(database: &DB, name: &str) -> bool {
    database
        .functions()
        .find(|function| function.name() == name)
        .expect("function exists")
        .is_leakproof()
}

#[test]
fn declarations_expose_leakproof_status_to_generic_consumers() {
    let database = db("CREATE FUNCTION omitted() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION marked() RETURNS INT LANGUAGE sql LEAKPROOF AS 'SELECT 1';
         CREATE FUNCTION unmarked() RETURNS INT LANGUAGE sql NOT LEAKPROOF AS 'SELECT 1';
         CREATE FUNCTION public.reveal(value integer) RETURNS boolean
             LANGUAGE plpgsql IMMUTABLE LEAKPROOF COST 1
             AS $$BEGIN IF value <> 1 THEN RAISE EXCEPTION 'hidden value = %', value; END IF; RETURN true; END$$;");

    for (name, expected) in
        [("omitted", false), ("marked", true), ("unmarked", false), ("reveal", true)]
    {
        assert_eq!(leakproof(&database, name), expected, "{name}");
    }
}

#[test]
fn alterations_follow_statement_and_action_order() {
    for (declared, alterations, expected) in [
        ("NOT LEAKPROOF", "ALTER FUNCTION reveal(integer) LEAKPROOF;", true),
        ("LEAKPROOF", "ALTER FUNCTION reveal(integer) NOT LEAKPROOF;", false),
        ("", "ALTER FUNCTION reveal(integer) COST 5 LEAKPROOF;", true),
        ("", "ALTER FUNCTION reveal LEAKPROOF; ALTER FUNCTION reveal(INT) NOT LEAKPROOF;", false),
        ("LEAKPROOF", "ALTER FUNCTION reveal(integer) STABLE;", true),
    ] {
        let database = db(&format!(
            "CREATE FUNCTION reveal(value integer) RETURNS BOOLEAN LANGUAGE sql {declared}
                 AS 'SELECT true';
             {alterations}"
        ));

        assert_eq!(leakproof(&database, "reveal"), expected, "{declared} then {alterations}");
    }
}

#[test]
fn a_replacement_takes_its_own_declaration() {
    let database = db("CREATE FUNCTION reveal(value INT) RETURNS BOOLEAN LANGUAGE sql LEAKPROOF
             AS 'SELECT true';
         CREATE OR REPLACE FUNCTION reveal(value INT) RETURNS BOOLEAN LANGUAGE sql
             AS 'SELECT false';");

    assert!(!leakproof(&database, "reveal"));
}

#[test]
fn an_alteration_changes_only_the_selected_overload() {
    let database = db("CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT) RETURNS INT LANGUAGE sql AS 'SELECT 2';
         ALTER FUNCTION f(INTEGER) LEAKPROOF;");
    let functions: Vec<_> = database
        .functions()
        .filter(|function| function.name() == "f")
        .map(|function| {
            (function.normalized_argument_type_names(&database), function.is_leakproof())
        })
        .collect();

    assert_eq!(functions, [(vec!["INT".into()], true), (vec!["TEXT".into()], false)]);
}

#[test]
fn an_alteration_of_an_absent_signature_is_rejected() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f(TEXT) LEAKPROOF;",
    );

    assert!(
        matches!(result, Err(Error::AlterFunctionNotFound { function_name, .. }) if function_name == "f")
    );
}

#[test]
fn policy_calls_observe_the_altered_function() {
    let database = db("CREATE TABLE secrets (value INT);
         CREATE FUNCTION reveal(value INT) RETURNS BOOLEAN LANGUAGE sql NOT LEAKPROOF
             AS 'SELECT true';
         CREATE POLICY visible ON secrets USING (reveal(value));
         ALTER FUNCTION reveal(INT) LEAKPROOF;");
    let table = database.tables().next().expect("table exists");
    let policy = table.policies(&database).expect("policies resolve").next().expect("policy");
    let called = policy
        .using_functions(&database)
        .expect("using functions resolve")
        .next()
        .expect("reveal is called");

    assert!(called.is_leakproof());
}

#[test]
fn builtins_answer_their_catalog_status() {
    let database = db("CREATE FUNCTION public.int4eq(a INT, b INT) RETURNS BOOLEAN LANGUAGE sql
             AS 'SELECT true';");

    for (name, arguments, expected) in [
        ("int4eq", &["int", "int"][..], true),
        ("texteq", &["text", "text"][..], true),
        ("lower", &["text"][..], false),
        ("now", &[][..], false),
    ] {
        assert_eq!(
            overload(&database, "pg_catalog", name, arguments).is_leakproof(),
            expected,
            "{name}"
        );
    }
    let shadowing = database
        .functions()
        .find(|function| {
            function.target_name().schema() == Some("public") && function.name() == "int4eq"
        })
        .expect("the shadowing declaration is registered");
    assert!(!shadowing.is_leakproof());
}
