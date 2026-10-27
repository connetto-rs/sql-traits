//! Effective function volatility across declarations and alterations.
#![expect(clippy::expect_used, reason = "test fixtures must parse and resolve")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{FunctionBehavior, FunctionSecurity},
    dialect::PostgreSqlDialect,
};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn volatility<DB: DatabaseLike>(database: &DB, name: &str) -> FunctionBehavior {
    database
        .functions()
        .find(|function| function.name() == name)
        .expect("function exists")
        .volatility()
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/function_overloads.rs"));

#[test]
fn declarations_expose_effective_volatility_to_generic_consumers() {
    let database = db("CREATE FUNCTION omitted() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION immutable_fn() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE FUNCTION stable_fn() RETURNS INT LANGUAGE sql STABLE AS 'SELECT 1';
         CREATE FUNCTION volatile_fn() RETURNS INT LANGUAGE sql VOLATILE AS 'SELECT 1';");

    for (name, expected) in [
        ("omitted", FunctionBehavior::Volatile),
        ("immutable_fn", FunctionBehavior::Immutable),
        ("stable_fn", FunctionBehavior::Stable),
        ("volatile_fn", FunctionBehavior::Volatile),
    ] {
        assert_eq!(volatility(&database, name), expected);
    }
}

#[test]
fn a_declaration_shadowing_a_builtin_keeps_its_own_volatility() {
    let database =
        db("CREATE FUNCTION public.now() RETURNS TIMESTAMPTZ LANGUAGE sql AS 'SELECT 1';");

    assert_eq!(overload(&database, "public", "now", &[]).volatility(), FunctionBehavior::Volatile);
    assert_eq!(
        overload(&database, "pg_catalog", "now", &[]).volatility(),
        FunctionBehavior::Stable
    );
}

#[test]
fn alterations_follow_statement_and_action_order() {
    for (actions, expected) in [
        ("VOLATILE", FunctionBehavior::Volatile),
        ("IMMUTABLE", FunctionBehavior::Immutable),
        ("IMMUTABLE; ALTER FUNCTION policy_helper() STABLE", FunctionBehavior::Stable),
        ("IMMUTABLE STABLE VOLATILE", FunctionBehavior::Volatile),
    ] {
        let database = db(&format!(
            "CREATE FUNCTION policy_helper() RETURNS BOOLEAN LANGUAGE sql STABLE AS 'SELECT true';
             ALTER FUNCTION policy_helper() {actions};"
        ));

        assert_eq!(volatility(&database, "policy_helper"), expected);
    }
}

#[test]
fn volatility_coexists_with_security_configuration_and_ownership() {
    let database = db("CREATE ROLE app_owner;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE STRICT
             SET work_mem TO '64kB' AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO app_owner;
         ALTER FUNCTION f() STABLE SECURITY DEFINER SET search_path TO public;
         ALTER FUNCTION f() RESET work_mem VOLATILE;
         ALTER FUNCTION f() SECURITY INVOKER;");
    let function =
        database.functions().find(|function| function.name() == "f").expect("function exists");

    assert_eq!(function.volatility(), FunctionBehavior::Volatile);
    assert_eq!(function.security_mode(), FunctionSecurity::Invoker);
    assert_eq!(function.owner(&database), Ok(Some("app_owner")));
    assert_eq!(function.null_input_behavior(), sqlparser::ast::FunctionCalledOnNull::Strict);
    assert_eq!(
        function.configuration_parameters().iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["SET search_path = public"]
    );
}

#[test]
fn an_alteration_changes_only_the_selected_overload() {
    let database = db("CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT) RETURNS INT LANGUAGE sql STABLE AS 'SELECT 2';
         ALTER FUNCTION f(INTEGER) VOLATILE;");
    let functions: Vec<_> = database
        .functions()
        .filter(|function| function.name() == "f")
        .map(|function| (function.normalized_argument_type_names(&database), function.volatility()))
        .collect();

    assert_eq!(
        functions,
        [
            (vec!["INT".into()], FunctionBehavior::Volatile),
            (vec!["TEXT".into()], FunctionBehavior::Stable),
        ]
    );
}

#[test]
fn a_bare_name_requires_a_single_matching_function() {
    let database = db("CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         ALTER FUNCTION f STABLE;");
    assert_eq!(volatility(&database, "f"), FunctionBehavior::Stable);

    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT) RETURNS INT LANGUAGE sql AS 'SELECT 2';
         ALTER FUNCTION f STABLE;",
    );
    assert!(
        matches!(result, Err(Error::AmbiguousAlterFunction { function_name }) if function_name == "f")
    );
}

#[test]
fn an_absent_signature_is_rejected() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f(TEXT) STABLE;",
    );

    assert!(
        matches!(result, Err(Error::AlterFunctionNotFound { function_name }) if function_name == "f")
    );
}

#[test]
fn cached_policy_and_check_calls_observe_alterations_and_replacements() {
    for (change, expected) in [
        (
            "ALTER FUNCTION guard(INT) VOLATILE; ALTER FUNCTION guard(INT) IMMUTABLE;",
            FunctionBehavior::Immutable,
        ),
        (
            "ALTER FUNCTION guard(INT) IMMUTABLE;
          CREATE OR REPLACE FUNCTION guard(x INT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';",
            FunctionBehavior::Volatile,
        ),
    ] {
        let database = db(&format!(
            "CREATE FUNCTION guard(x INT) RETURNS BOOLEAN LANGUAGE sql STABLE AS 'SELECT true';
             CREATE TABLE t (id INT, CHECK (guard(id)));
             CREATE POLICY p ON t USING (guard(id)) WITH CHECK (guard(id));
             {change}"
        ));
        assert_eq!(volatility(&database, "guard"), expected);
        let table = database.tables().next().expect("table exists");
        let policy =
            table.policies(&database).expect("policies resolve").next().expect("policy exists");
        let check = table
            .check_constraints(&database)
            .expect("checks resolve")
            .next()
            .expect("check exists");
        assert_eq!(
            policy
                .using_functions(&database)
                .expect("using functions resolve")
                .next()
                .expect("guard exists")
                .volatility(),
            expected
        );
        assert_eq!(
            policy
                .check_functions(&database)
                .expect("check functions resolve")
                .next()
                .expect("guard exists")
                .volatility(),
            expected
        );
        assert_eq!(
            check
                .functions(&database)
                .expect("constraint functions resolve")
                .next()
                .expect("guard exists")
                .volatility(),
            expected
        );
    }
}
