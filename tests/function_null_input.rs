//! What PostgreSQL declares for each builtin, and when a function call is
//! `NULL` because one of its arguments is `NULL`.
#![expect(clippy::expect_used, reason = "test fixtures must parse and resolve")]

use sql_traits::prelude::*;
use sqlparser::{ast::FunctionCalledOnNull, dialect::PostgreSqlDialect};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/function_overloads.rs"));

/// What PostgreSQL 18 declares for every builtin this crate registers.
mod builtins {
    use FunctionBehavior::{Immutable, Stable, Volatile};
    use FunctionCalledOnNull::{CalledOnNullInput as Called, Strict};
    use sql_traits::prelude::FunctionKind::{self, Aggregate, Normal, Window};
    use sqlparser::ast::{FunctionBehavior, FunctionCalledOnNull};

    /// Name, argument types, null-input behavior, the answer at every call
    /// position and one past the last, volatility and kind.
    pub(super) type Row = (
        &'static str,
        &'static [&'static str],
        FunctionCalledOnNull,
        &'static [bool],
        FunctionBehavior,
        FunctionKind,
    );

    // The SQL value functions such as `current_date` have no `pg_proc` row,
    // and the planner holds them stable.
    pub(super) const EXPECTED: &[Row] = &[
        ("length", &["text"], Strict, &[true, false], Immutable, Normal),
        // Not a PostgreSQL function, so nothing vouches for it.
        ("len", &["text"], Called, &[false, false], Volatile, Normal),
        ("char_length", &["text"], Strict, &[true, false], Immutable, Normal),
        ("character_length", &["text"], Strict, &[true, false], Immutable, Normal),
        ("octet_length", &["text"], Strict, &[true, false], Immutable, Normal),
        ("lower", &["text"], Strict, &[true, false], Immutable, Normal),
        ("upper", &["text"], Strict, &[true, false], Immutable, Normal),
        ("btrim", &["text"], Strict, &[true, false], Immutable, Normal),
        ("btrim", &["text", "text"], Strict, &[true, true, false], Immutable, Normal),
        (
            "split_part",
            &["text", "text", "int"],
            Strict,
            &[true, true, true, false],
            Immutable,
            Normal,
        ),
        // `string_to_array('ab', NULL)` is `{a,b}`, and a `NULL` third
        // argument only disables replacement.
        ("string_to_array", &["text", "text"], Called, &[true, false, false], Immutable, Normal),
        (
            "string_to_array",
            &["text", "text", "text"],
            Called,
            &[true, false, false, false],
            Immutable,
            Normal,
        ),
        ("current_setting", &["text"], Strict, &[true, false], Stable, Normal),
        ("current_setting", &["text", "boolean"], Strict, &[true, true, false], Stable, Normal),
        // The variadic array is not `NULL` when one of its elements is.
        ("coalesce", &["any"], Called, &[false, false], Immutable, Normal),
        // `nullif(NULL, 1)` is `NULL`, `nullif(1, NULL)` is 1.
        ("nullif", &["any", "any"], Called, &[true, false, false], Immutable, Normal),
        ("now", &[], Strict, &[false], Stable, Normal),
        ("current_timestamp", &[], Called, &[false], Stable, Normal),
        ("current_date", &[], Called, &[false], Stable, Normal),
        ("current_time", &[], Called, &[false], Stable, Normal),
        ("localtimestamp", &[], Called, &[false], Stable, Normal),
        ("localtime", &[], Called, &[false], Stable, Normal),
        ("gen_random_uuid", &[], Strict, &[false], Volatile, Normal),
        ("uuidv4", &[], Strict, &[false], Volatile, Normal),
        ("uuidv7", &[], Strict, &[false], Volatile, Normal),
        ("uuidv7", &["interval"], Strict, &[true, false], Volatile, Normal),
        ("count", &[], Called, &[false], Immutable, Aggregate),
        ("count", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("sum", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("avg", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("min", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("max", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("array_agg", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("string_agg", &["text", "text"], Called, &[false, false, false], Immutable, Aggregate),
        ("bool_and", &["boolean"], Called, &[false, false], Immutable, Aggregate),
        ("bool_or", &["boolean"], Called, &[false, false], Immutable, Aggregate),
        ("every", &["boolean"], Called, &[false, false], Immutable, Aggregate),
        ("rank", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("dense_rank", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("percent_rank", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("cume_dist", &["any"], Called, &[false, false], Immutable, Aggregate),
        ("row_number", &[], Called, &[false], Immutable, Window),
        ("rank", &[], Called, &[false], Immutable, Window),
        ("dense_rank", &[], Called, &[false], Immutable, Window),
        ("percent_rank", &[], Called, &[false], Immutable, Window),
        ("cume_dist", &[], Called, &[false], Immutable, Window),
        ("ntile", &["int"], Strict, &[true, false], Immutable, Window),
        ("lag", &["any"], Strict, &[true, false], Immutable, Window),
        ("lag", &["any", "int"], Strict, &[true, true, false], Immutable, Window),
        ("lag", &["any", "int", "any"], Strict, &[true, true, true, false], Immutable, Window),
        ("lead", &["any"], Strict, &[true, false], Immutable, Window),
        ("lead", &["any", "int"], Strict, &[true, true, false], Immutable, Window),
        ("lead", &["any", "int", "any"], Strict, &[true, true, true, false], Immutable, Window),
        ("first_value", &["any"], Strict, &[true, false], Immutable, Window),
        ("last_value", &["any"], Strict, &[true, false], Immutable, Window),
        ("nth_value", &["any", "int"], Strict, &[true, true, false], Immutable, Window),
        ("current_user", &[], Strict, &[false], Stable, Normal),
        ("session_user", &[], Strict, &[false], Stable, Normal),
        ("user", &[], Called, &[false], Stable, Normal),
        ("random", &[], Strict, &[false], Volatile, Normal),
        ("random", &["int", "int"], Strict, &[true, true, false], Volatile, Normal),
        ("random", &["bigint", "bigint"], Strict, &[true, true, false], Volatile, Normal),
        ("random", &["numeric", "numeric"], Strict, &[true, true, false], Volatile, Normal),
        ("clock_timestamp", &[], Strict, &[false], Volatile, Normal),
        ("statement_timestamp", &[], Strict, &[false], Stable, Normal),
        ("transaction_timestamp", &[], Strict, &[false], Stable, Normal),
        ("timeofday", &[], Strict, &[false], Volatile, Normal),
        ("md5", &["text"], Strict, &[true, false], Immutable, Normal),
        ("substring", &["text", "int"], Strict, &[true, true, false], Immutable, Normal),
        (
            "substring",
            &["text", "int", "int"],
            Strict,
            &[true, true, true, false],
            Immutable,
            Normal,
        ),
        ("substring", &["text", "text"], Strict, &[true, true, false], Immutable, Normal),
        (
            "substring",
            &["text", "text", "text"],
            Strict,
            &[true, true, true, false],
            Immutable,
            Normal,
        ),
    ];
}

#[test]
fn every_builtin_answers_as_postgres_declares() {
    let database = db("");
    let expected = builtins::EXPECTED;
    for (name, argument_types, behavior, answers, volatility, kind) in expected {
        let function = overload(&database, "pg_catalog", name, argument_types);
        let actual = (0..answers.len())
            .map(|index| function.returns_null_on_null_argument(&database, index))
            .collect::<Vec<_>>();
        assert_eq!(
            (
                function.null_input_behavior(),
                actual.as_slice(),
                function.volatility(),
                function.kind(&database)
            ),
            (behavior.clone(), *answers, volatility.clone(), Ok(*kind)),
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
