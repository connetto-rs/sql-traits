//! The functions a function body calls, resolved where the body's calls go
//! when it runs, or where a bound body's calls went when it was created.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{BigQueryDialect, MsSqlDialect, PostgreSqlDialect};

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn function<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
}

/// Each function `name` calls, as `schema.name(argument types)`.
fn called_by(database: &ParserDB, name: &str) -> Result<Vec<String>, LookupError> {
    Ok(function(database, name)
        .referenced_functions(database)?
        .into_iter()
        .map(|called| {
            let target = called.target_name();
            format!(
                "{}.{}({})",
                target.schema().unwrap_or("public"),
                called.name(),
                called.argument_type_names(database).collect::<Vec<_>>().join(", ")
            )
        })
        .collect())
}

/// The calls of `name` reaching a function the schema declares.
fn declared_called_by(database: &ParserDB, name: &str) -> Vec<String> {
    let mut called = called_by(database, name).unwrap();
    called.retain(|called| !called.starts_with("pg_catalog."));
    called
}

const MEMBERSHIP: &str = "
    CREATE TABLE members (doc_id TEXT, user_id TEXT);
    CREATE FUNCTION is_member(d TEXT) RETURNS BOOLEAN LANGUAGE sql
        AS $$ SELECT EXISTS (SELECT 1 FROM members WHERE doc_id = d AND user_id = current_user) $$;
    CREATE FUNCTION can_read(d TEXT) RETURNS BOOLEAN LANGUAGE sql
        AS $$ SELECT is_member(d) $$;
    CREATE TABLE docs (id TEXT PRIMARY KEY);
    ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
    CREATE POLICY docs_read ON docs FOR SELECT USING (can_read(id));
";

#[test]
fn a_policy_is_followed_through_the_calls_of_the_functions_it_calls() {
    let database = postgres(MEMBERSHIP);
    assert_eq!(called_by(&database, "can_read").unwrap(), ["public.is_member(TEXT)"]);
    assert_eq!(declared_called_by(&database, "is_member"), Vec::<String>::new());

    let policy = database.policies().next().expect("the policy exists");
    let mut read = Vec::new();
    let mut pending: Vec<_> = policy.using_functions(&database).unwrap().collect();
    while let Some(called) = pending.pop() {
        // A built-in holds no SQL body to follow.
        if called.target_name().schema() == Some("pg_catalog") {
            continue;
        }
        read.extend(
            called.referenced_tables(&database).unwrap().into_iter().map(TableLike::table_name),
        );
        pending.extend(called.referenced_functions(&database).unwrap());
    }
    assert!(read.contains(&"members"), "read {read:?}");
}

const TWO_FS: &str = "
    CREATE SCHEMA app;
    CREATE FUNCTION app.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
    CREATE FUNCTION public.f() RETURNS INT LANGUAGE sql AS 'SELECT 2';
";

#[test]
fn the_function_path_decides_where_an_unqualified_call_goes() {
    let database = postgres(&format!(
        "{TWO_FS}
         CREATE FUNCTION own_path() RETURNS INT LANGUAGE sql SET search_path = app
             AS 'SELECT f()';
         CREATE FUNCTION session_path() RETURNS INT LANGUAGE sql AS 'SELECT f()';
         CREATE FUNCTION emptied() RETURNS INT LANGUAGE sql SET search_path = ''
             AS 'SELECT f()';
         CREATE FUNCTION qualified() RETURNS INT LANGUAGE sql SET search_path = ''
             AS 'SELECT app.f()';"
    ));
    assert_eq!(called_by(&database, "own_path").unwrap(), ["app.f()"]);
    assert_eq!(called_by(&database, "session_path").unwrap(), ["public.f()"]);
    assert_eq!(called_by(&database, "emptied").unwrap(), Vec::<String>::new());
    assert_eq!(called_by(&database, "qualified").unwrap(), ["app.f()"]);
}

#[test]
fn a_path_set_in_the_body_reaches_the_calls_after_it() {
    let database = postgres(&format!(
        "{TWO_FS}
         CREATE FUNCTION switched() RETURNS INT LANGUAGE sql
             AS $$ SET search_path = app; SELECT f(); $$;
         CREATE FUNCTION restored() RETURNS INT LANGUAGE sql SET search_path = app
             AS $$ SELECT f(); RESET search_path; SELECT f(); $$;"
    ));
    assert_eq!(called_by(&database, "switched").unwrap(), ["app.f()"]);
    assert_eq!(called_by(&database, "restored").unwrap(), ["app.f()", "public.f()"]);
}

#[test]
fn a_path_changed_at_run_time_hides_where_later_unqualified_calls_go() {
    let database = postgres(&format!(
        "{TWO_FS}
         CREATE FUNCTION bare_after() RETURNS INT LANGUAGE sql
             AS $$ SELECT set_config('search_path', 'app', true); SELECT f(); $$;
         CREATE FUNCTION qualified_after() RETURNS INT LANGUAGE sql
             AS $$ SELECT set_config('search_path', 'app', true); SELECT app.f(); $$;"
    ));
    assert!(matches!(
        called_by(&database, "bare_after"),
        Err(LookupError::FunctionBodyUnresolvable { statement, .. })
            if statement == "SELECT set_config('search_path', 'app', true)"
    ));
    assert_eq!(declared_called_by(&database, "qualified_after"), ["app.f()"]);
}

#[test]
fn every_overload_accepting_the_argument_count_is_reached() {
    let database = postgres(
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION f(x TEXT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x INT, y INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION g(x INT, y INT DEFAULT 0) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION rows_of(x INT) RETURNS SETOF INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION one() RETURNS INT LANGUAGE sql AS 'SELECT f(1)';
         CREATE FUNCTION nested() RETURNS INT LANGUAGE sql
             AS 'SELECT g(r) FROM rows_of(f(1, 2)) AS r';",
    );
    assert_eq!(called_by(&database, "one").unwrap(), ["public.f(INT)", "public.f(TEXT)"]);
    assert_eq!(
        declared_called_by(&database, "nested"),
        ["public.g(INT, INT)", "public.rows_of(INT)", "public.f(INT, INT)"]
    );
}

#[test]
fn each_function_is_reported_once_in_the_order_first_reached() {
    let database = postgres(
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION g(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION repeated() RETURNS INT LANGUAGE sql AS $$
             SELECT g(1) + f(1);
             SELECT g(2);
         $$;",
    );
    assert_eq!(called_by(&database, "repeated").unwrap(), ["public.g(INT)", "public.f(INT)"]);
}

#[test]
fn calls_in_bodies_the_input_already_parsed_are_resolved() {
    let sql_server = ParserDB::parse::<MsSqlDialect>(
        "CREATE FUNCTION helper() RETURNS INT AS BEGIN RETURN 1; END;
         CREATE FUNCTION counted() RETURNS INT AS BEGIN RETURN (SELECT helper()); END;
         CREATE FUNCTION listed() RETURNS TABLE AS RETURN SELECT helper() AS h;",
    )
    .unwrap();
    assert_eq!(declared_called_by(&sql_server, "counted"), ["public.helper()"]);
    assert_eq!(declared_called_by(&sql_server, "listed"), ["public.helper()"]);

    let big_query = ParserDB::parse::<BigQueryDialect>(
        "CREATE FUNCTION helper() RETURNS INT64 AS (1);
         CREATE FUNCTION counted() RETURNS INT64 AS ((SELECT helper()));
         CREATE FUNCTION described() RETURNS INT64 OPTIONS (description = 'count')
             AS ((SELECT helper()));",
    )
    .unwrap();
    assert_eq!(declared_called_by(&big_query, "counted"), ["public.helper()"]);
    assert_eq!(declared_called_by(&big_query, "described"), ["public.helper()"]);
}

#[test]
fn a_body_whose_calls_cannot_be_known_fails_as_its_reads_do() {
    let database = postgres(
        "CREATE FUNCTION procedural() RETURNS INT LANGUAGE plpgsql
             AS $$ BEGIN RETURN 1; END $$;
         CREATE FUNCTION native() RETURNS INT LANGUAGE c AS 'module', 'symbol';
         CREATE FUNCTION calls() RETURNS INT LANGUAGE sql AS 'CALL elsewhere(); SELECT 1';
         CREATE FUNCTION broken() RETURNS INT LANGUAGE sql AS 'SELECT FROM WHERE (';",
    );
    for name in ["procedural", "native", "calls", "broken"] {
        let function = function(&database, name);
        let calls = function.referenced_functions(&database).expect_err(name);
        let reads = function.referenced_tables(&database).expect_err(name);
        assert_eq!(calls, reads, "{name}");
    }
    assert!(matches!(
        called_by(&database, "procedural"),
        Err(LookupError::FunctionBodyNotSql { language, .. }) if language == "plpgsql"
    ));
}

/// A `RETURN` or `BEGIN ATOMIC` body binds its calls when the function is
/// created, under the session's path, so neither the function's own path
/// nor a function created afterwards reaches it.
#[test]
fn a_bound_body_calls_what_it_reached_at_creation() {
    let database = postgres(&format!(
        "{TWO_FS}
         SET search_path = app;
         CREATE FUNCTION public.returned() RETURNS INT LANGUAGE sql SET search_path = public
             RETURN f();
         CREATE FUNCTION public.atomic() RETURNS INT LANGUAGE sql SET search_path = public
             BEGIN ATOMIC SELECT f(); END;
         CREATE FUNCTION public.quoted() RETURNS INT LANGUAGE sql SET search_path = app
             AS 'SELECT f()';
         CREATE FUNCTION app.f(x INT DEFAULT 0) RETURNS INT LANGUAGE sql AS 'SELECT x';
         SET search_path = public;"
    ));
    assert_eq!(called_by(&database, "returned").unwrap(), ["app.f()"]);
    assert_eq!(called_by(&database, "atomic").unwrap(), ["app.f()"]);
    // A body written as a string finds the overload created after it.
    assert_eq!(called_by(&database, "quoted").unwrap(), ["app.f()", "app.f(INT)"]);
}

#[test]
fn a_function_a_bound_body_calls_is_dropped_only_with_its_caller() {
    let schema = "
        CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
        CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN f();
        CREATE FUNCTION quoted() RETURNS INT LANGUAGE sql AS 'SELECT f()';
        CREATE FUNCTION recursive() RETURNS INT LANGUAGE sql AS 'SELECT 1';
        CREATE OR REPLACE FUNCTION recursive() RETURNS INT LANGUAGE sql RETURN recursive();
    ";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&format!("{schema} DROP FUNCTION f();")),
        Err(Error::FunctionReferenced { function_name }) if function_name == "f"
    ));
    for drop in ["DROP FUNCTION g(), f();", "DROP FUNCTION f(), g();"] {
        let database = postgres(&format!("{schema} {drop}"));
        assert!(database.functions().all(|function| !["f", "g"].contains(&function.name())));
    }
    // A function calling itself does not hold itself in place.
    let database = postgres(&format!("{schema} DROP FUNCTION recursive();"));
    assert!(database.functions().all(|function| function.name() != "recursive"));
}

#[test]
fn a_bound_body_calls_the_node_the_database_holds_after_the_callee_changes() {
    let database = postgres(
        "CREATE TABLE t (id INT);
         CREATE FUNCTION f() RETURNS BIGINT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION counted() RETURNS BIGINT LANGUAGE sql RETURN (SELECT count(*) FROM t);
         CREATE FUNCTION g() RETURNS BIGINT LANGUAGE sql RETURN f() + counted();
         CREATE OR REPLACE FUNCTION f() RETURNS BIGINT LANGUAGE sql AS 'SELECT 2';
         ALTER FUNCTION f() STABLE;
         ALTER TABLE t RENAME TO u;
         CREATE FUNCTION recursive() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE OR REPLACE FUNCTION recursive() RETURNS INT LANGUAGE sql RETURN recursive();",
    );
    let called = function(&database, "g").referenced_functions(&database).unwrap();
    let held: Vec<_> = ["f", "counted"].map(|name| function(&database, name)).into();
    assert_eq!(called.len(), 2);
    for (called, held) in called.iter().zip(&held) {
        assert!(core::ptr::eq(*called, *held), "{} is a stale node", called.name());
    }
    let read = called[1].referenced_tables(&database).unwrap();
    assert_eq!(read.iter().map(|table| TableLike::table_name(*table)).collect::<Vec<_>>(), ["u"]);

    let recursive = function(&database, "recursive");
    let called = recursive.referenced_functions(&database).unwrap();
    assert!(matches!(called.as_slice(), [itself] if core::ptr::eq(*itself, recursive)));
}

#[test]
fn a_cascade_takes_the_bound_callers_of_a_function_it_drops() {
    let database = postgres(
        "CREATE TABLE t (id INT);
         CREATE FUNCTION counted() RETURNS BIGINT LANGUAGE sql RETURN (SELECT count(*) FROM t);
         CREATE FUNCTION caller() RETURNS BIGINT LANGUAGE sql BEGIN ATOMIC SELECT counted(); END;
         CREATE FUNCTION outer_caller() RETURNS BIGINT LANGUAGE sql RETURN caller();
         CREATE FUNCTION quoted() RETURNS BIGINT LANGUAGE sql AS 'SELECT counted()';
         DROP TABLE t CASCADE;",
    );
    let names: Vec<_> = database.functions().map(FunctionLike::name).collect();
    assert!(names.contains(&"quoted"), "{names:?}");
    for dropped in ["counted", "caller", "outer_caller"] {
        assert!(!names.contains(&dropped), "{dropped} survived in {names:?}");
    }
}
