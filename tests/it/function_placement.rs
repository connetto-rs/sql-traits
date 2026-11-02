//! Tests that an unqualified function name walks the search path, as
//! PostgreSQL walks it.
//!
//! `CREATE FUNCTION f()` lands in the first schema on the path that exists,
//! and `DROP FUNCTION`, `ALTER FUNCTION` and `GRANT ... ON FUNCTION` written
//! without a schema reach the function in the earliest schema on the path
//! holding one, an earlier function hiding a later one taking the same
//! arguments. The expectations below were read from PostgreSQL 16.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{ast::FunctionSecurity, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Every function named `name`, as `schema.name(argument types)`, sorted.
fn signatures(db: &ParserDB, name: &str) -> Vec<String> {
    let mut signatures: Vec<String> = db
        .functions()
        .filter(|function| function.name() == name)
        .map(|function| {
            let arguments: Vec<String> =
                function.argument_type_names(db).map(|argument| argument.to_string()).collect();
            format!(
                "{}.{name}({})",
                function.target_name().schema().unwrap_or("public"),
                arguments.join(", ")
            )
        })
        .collect();
    signatures.sort();
    signatures
}

const SCHEMAS: &str = "CREATE SCHEMA app; CREATE SCHEMA app2;";

#[test]
fn a_function_lands_in_the_first_schema_on_the_path_that_exists() {
    let db = parse(&format!(
        "{SCHEMAS}
         SET search_path TO app, public;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         SET search_path TO nowhere, app2, public;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 2';
         SET search_path TO pg_temp, app;
         CREATE FUNCTION g() RETURNS INT AS 'SELECT 3';
         SET search_path TO public, app;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 4';"
    ))
    .expect("every function lands somewhere");
    assert_eq!(signatures(&db, "f"), ["app.f()", "app2.f()", "public.f()"]);
    assert_eq!(signatures(&db, "g"), ["pg_temp.g()"]);

    let result = parse("SET search_path TO ''; CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';");
    assert!(
        matches!(
            &result,
            Err(Error::NoSchemaSelectedForRelation { object_kind: ObjectKind::Function, relation_name })
                if relation_name == "f"
        ),
        "got {result:?}"
    );
}

/// `CREATE OR REPLACE` replaces the function in the schema it would create
/// one in, leaving a function of that signature later on the path alone.
#[test]
fn a_replacement_reaches_the_function_where_it_lands() {
    let db = parse(&format!(
        "{SCHEMAS}
         SET search_path TO app;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION public.f() RETURNS INT AS 'SELECT 9';
         SET search_path TO app, public;
         CREATE OR REPLACE FUNCTION f() RETURNS INT AS 'SELECT 10';"
    ))
    .expect("the replacement is accepted");
    assert_eq!(signatures(&db, "f"), ["app.f()", "public.f()"]);
    let replaced = db
        .functions()
        .find(|function| function.name() == "f" && function.target_name().schema() == Some("app"))
        .expect("app.f is held");
    assert!(replaced.to_string().contains("'SELECT 10'"), "{replaced}");
}

/// The fixture three schemas each hold an `f()` in, `public` also an
/// `f(INT)`.
const OVERLOADED: &str = "CREATE SCHEMA app; CREATE SCHEMA app2;
     CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
     CREATE FUNCTION app2.f() RETURNS INT AS 'SELECT 2';
     CREATE FUNCTION public.f() RETURNS INT AS 'SELECT 3';";

#[test]
fn a_drop_reaches_the_earliest_function_on_the_path() {
    let path = format!("{OVERLOADED} SET search_path TO app, public;");
    let dropped = parse(&format!("{path} DROP FUNCTION f();")).expect("app.f is dropped");
    assert_eq!(signatures(&dropped, "f"), ["app2.f()", "public.f()"]);

    let bare = parse(&format!("{path} DROP FUNCTION f;")).expect("app.f hides public.f");
    assert_eq!(signatures(&bare, "f"), ["app2.f()", "public.f()"]);

    let overloads = format!("{path} CREATE FUNCTION public.f(x INT) RETURNS INT AS 'SELECT x';");
    let result = parse(&format!("{overloads} DROP FUNCTION f;"));
    assert!(matches!(result, Err(Error::AmbiguousDropFunction { .. })), "got {result:?}");
    let one =
        parse(&format!("{overloads} DROP FUNCTION f(INT);")).expect("public.f(INT) is reached");
    assert_eq!(signatures(&one, "f"), ["app.f()", "app2.f()", "public.f()"]);

    let off_path = parse(&format!(
        "{OVERLOADED} CREATE FUNCTION public.f(x INT) RETURNS INT AS 'SELECT x';
         SET search_path TO app2;
         DROP FUNCTION f(INT);"
    ));
    assert!(matches!(off_path, Err(Error::DropFunctionNotFound { .. })), "got {off_path:?}");
}

#[test]
fn an_alter_and_a_grant_reach_the_function_on_the_path() {
    let db = parse(&format!(
        "{OVERLOADED}
         CREATE ROLE reader;
         REVOKE EXECUTE ON FUNCTION app.f(), app2.f(), public.f() FROM PUBLIC;
         SET search_path TO app2;
         ALTER FUNCTION f() SECURITY DEFINER;
         GRANT EXECUTE ON FUNCTION f() TO reader;
         RESET search_path;"
    ))
    .expect("the statements are accepted");
    let reader = db.role("reader").expect("the role is declared");
    let mut reached = 0;
    for function in db.functions().filter(|function| function.name() == "f") {
        let in_app2 = function.target_name().schema() == Some("app2");
        reached += 1;
        assert_eq!(function.security_mode() == FunctionSecurity::Definer, in_app2, "{function}");
        assert_eq!(function.can_execute(reader, &db), Ok(in_app2), "{function}");
    }
    assert_eq!(reached, 3);
}

/// A function created through the path is created in its schema, so the
/// defaults recorded for that schema apply.
#[test]
fn defaults_for_the_schema_a_function_lands_in_apply() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE ROLE reader;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES IN SCHEMA app GRANT EXECUTE ON FUNCTIONS TO reader;
         SET search_path TO app;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         SET search_path TO public;
         CREATE FUNCTION g() RETURNS INT AS 'SELECT 1';",
    )
    .expect("the functions are created");
    let reader = db.role("reader").expect("the role is declared");
    let can_execute = |name: &str| {
        let function = db.functions().find(|function| function.name() == name).expect("held");
        function.can_execute(reader, &db)
    };
    assert_eq!(can_execute("f"), Ok(true));
    assert_eq!(can_execute("g"), Ok(false));
}
