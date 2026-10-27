//! When a function call is `NULL` because one of its arguments is `NULL`.
#![expect(clippy::expect_used, reason = "test fixtures must parse and resolve")]

use sql_traits::prelude::*;
use sqlparser::{ast::FunctionCalledOnNull, dialect::PostgreSqlDialect};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

/// The overload of `name` in `schema` whose arguments are `argument_types`.
fn overload<'db>(
    database: &'db ParserDB,
    schema: &str,
    name: &str,
    argument_types: &[&str],
) -> &'db <ParserDB as DatabaseLike>::Function {
    database
        .functions()
        .find(|function| {
            function.target_name().schema() == Some(schema)
                && function.name() == name
                && function
                    .argument_type_names(database)
                    .map(|declared| declared.to_ascii_lowercase())
                    .eq(argument_types.iter().copied())
        })
        .expect("the overload is registered")
}

#[test]
fn every_builtin_answers_as_postgres_declares() {
    use FunctionCalledOnNull::{CalledOnNullInput as Called, Strict};

    let database = db("");
    // Each row lists the answer at every call position and one past the last.
    let expected: &[(&str, &[&str], FunctionCalledOnNull, &[bool])] = &[
        ("length", &["text"], Strict, &[true, false]),
        ("len", &["text"], Called, &[false, false]),
        ("char_length", &["text"], Strict, &[true, false]),
        ("character_length", &["text"], Strict, &[true, false]),
        ("octet_length", &["text"], Strict, &[true, false]),
        ("lower", &["text"], Strict, &[true, false]),
        ("upper", &["text"], Strict, &[true, false]),
        ("btrim", &["text"], Strict, &[true, false]),
        ("btrim", &["text", "text"], Strict, &[true, true, false]),
        ("split_part", &["text", "text", "int"], Strict, &[true, true, true, false]),
        // `string_to_array('ab', NULL)` is `{a,b}`, and a `NULL` third
        // argument only disables replacement.
        ("string_to_array", &["text", "text"], Called, &[true, false, false]),
        ("string_to_array", &["text", "text", "text"], Called, &[true, false, false, false]),
        ("current_setting", &["text"], Strict, &[true, false]),
        ("current_setting", &["text", "boolean"], Strict, &[true, true, false]),
        // The variadic array is not `NULL` when one of its elements is.
        ("coalesce", &["any"], Called, &[false, false]),
        // `nullif(NULL, 1)` is `NULL`, `nullif(1, NULL)` is 1.
        ("nullif", &["any", "any"], Called, &[true, false, false]),
        ("now", &[], Strict, &[false]),
        ("current_timestamp", &[], Called, &[false]),
        ("current_date", &[], Called, &[false]),
        ("current_time", &[], Called, &[false]),
        ("localtimestamp", &[], Called, &[false]),
        ("localtime", &[], Called, &[false]),
        ("gen_random_uuid", &[], Strict, &[false]),
        ("uuidv4", &[], Strict, &[false]),
        ("uuidv7", &[], Strict, &[false]),
        ("uuidv7", &["interval"], Strict, &[true, false]),
        ("count", &["any"], Called, &[false, false]),
        ("sum", &["any"], Called, &[false, false]),
        ("avg", &["any"], Called, &[false, false]),
        ("min", &["any"], Called, &[false, false]),
        ("max", &["any"], Called, &[false, false]),
        ("current_user", &[], Strict, &[false]),
        ("session_user", &[], Strict, &[false]),
        ("user", &[], Called, &[false]),
    ];

    for (name, argument_types, behavior, answers) in expected {
        let function = overload(&database, "pg_catalog", name, argument_types);
        let actual = (0..answers.len())
            .map(|index| function.returns_null_on_null_argument(&database, index))
            .collect::<Vec<_>>();
        assert_eq!(
            (function.null_input_behavior(), actual.as_slice()),
            (behavior.clone(), *answers),
            "{name}({argument_types:?})"
        );
    }
    let registered = database
        .functions()
        .filter(|function| function.target_name().schema() == Some("pg_catalog"))
        .count();
    assert_eq!(registered, expected.len(), "every registered builtin is listed above");
}

#[test]
fn declared_functions_answer_from_their_null_input_clause() {
    let database = db("
        CREATE FUNCTION strict_fn(a INT, b TEXT) RETURNS INT LANGUAGE sql STRICT
            AS 'SELECT 1';
        CREATE FUNCTION returns_null_fn(a INT) RETURNS INT LANGUAGE sql
            RETURNS NULL ON NULL INPUT AS 'SELECT 1';
        CREATE FUNCTION called_fn(a INT) RETURNS INT LANGUAGE sql
            CALLED ON NULL INPUT AS 'SELECT 1';
        CREATE FUNCTION unstated_fn(a INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
        CREATE FUNCTION out_first(OUT total INT, a INT) LANGUAGE sql STRICT
            AS 'SELECT 1';
        CREATE FUNCTION inout_first(INOUT total INT, a INT) LANGUAGE sql STRICT
            AS 'SELECT 1, 2';
        CREATE FUNCTION strict_variadic(a INT, VARIADIC rest INT[]) RETURNS INT
            LANGUAGE sql STRICT AS 'SELECT 1';
        CREATE FUNCTION strict_none() RETURNS INT LANGUAGE sql STRICT AS 'SELECT 1';
        CREATE FUNCTION string_to_array(s TEXT, d TEXT) RETURNS TEXT[] LANGUAGE sql
            AS 'SELECT ARRAY[]::TEXT[]';
    ");
    let answers = |name: &str, calls: usize| {
        let function = database
            .functions()
            .find(|function| {
                function.name() == name && function.target_name().schema() != Some("pg_catalog")
            })
            .expect("function is declared");
        (0..calls)
            .map(|index| function.returns_null_on_null_argument(&database, index))
            .collect::<Vec<_>>()
    };

    assert_eq!(answers("strict_fn", 3), [true, true, false]);
    assert_eq!(answers("returns_null_fn", 2), [true, false]);
    assert_eq!(answers("called_fn", 2), [false, false]);
    assert_eq!(answers("unstated_fn", 2), [false, false]);
    // An `OUT` parameter is not passed, so the call's first argument is `a`.
    assert_eq!(answers("out_first", 2), [true, false]);
    assert_eq!(answers("inout_first", 3), [true, true, false]);
    assert_eq!(answers("strict_variadic", 3), [true, false, false]);
    assert_eq!(answers("strict_none", 1), [false]);
    // Shadowing a builtin's name does not inherit what the builtin returns.
    assert_eq!(answers("string_to_array", 2), [false, false]);
}

#[test]
fn replacing_a_builtin_drops_its_per_argument_fact() {
    let database = db("
        CREATE OR REPLACE FUNCTION pg_catalog.string_to_array(s TEXT, d TEXT) RETURNS TEXT[]
            LANGUAGE sql AS 'SELECT ARRAY[]::TEXT[]';
    ");
    let function = overload(&database, "pg_catalog", "string_to_array", &["text", "text"]);

    assert!(!function.returns_null_on_null_argument(&database, 0));
}

#[test]
fn alterations_change_the_null_input_behavior() {
    let database = db("
        CREATE FUNCTION loosened(a INT) RETURNS INT LANGUAGE sql STRICT AS 'SELECT 1';
        ALTER FUNCTION loosened(INT) CALLED ON NULL INPUT;
        CREATE FUNCTION tightened(a INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
        ALTER FUNCTION tightened(INT) RETURNS NULL ON NULL INPUT;
        ALTER FUNCTION pg_catalog.lower(TEXT) CALLED ON NULL INPUT;
        ALTER FUNCTION pg_catalog.string_to_array(TEXT, TEXT) STRICT;
    ");
    let declared = |name: &str| {
        let function = database
            .functions()
            .find(|function| function.name() == name)
            .expect("function is declared");
        (function.null_input_behavior(), function.returns_null_on_null_argument(&database, 0))
    };
    let builtin = |name: &str, argument_types: &[&str], index: usize| {
        let function = overload(&database, "pg_catalog", name, argument_types);
        (function.null_input_behavior(), function.returns_null_on_null_argument(&database, index))
    };

    assert_eq!(declared("loosened"), (FunctionCalledOnNull::CalledOnNullInput, false));
    assert_eq!(declared("tightened"), (FunctionCalledOnNull::ReturnsNullOnNullInput, true));
    assert_eq!(builtin("lower", &["text"], 0), (FunctionCalledOnNull::CalledOnNullInput, false));
    assert_eq!(
        builtin("string_to_array", &["text", "text"], 1),
        (FunctionCalledOnNull::Strict, true)
    );
}
