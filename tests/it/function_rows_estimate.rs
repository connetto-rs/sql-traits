//! The row estimate of a set-returning function, `pg_proc.prorows`, across
//! declarations, replacements, alterations and the builtins.
//!
//! Every expected value and refusal is what PostgreSQL 18.6 stores or reports
//! for the same statements.
#![expect(clippy::expect_used, reason = "test fixtures must parse and resolve")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::function_overloads::overload;

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn estimated_rows<DB: DatabaseLike>(database: &DB, name: &str) -> Option<f32> {
    database
        .functions()
        .find(|function| function.name() == name)
        .expect("function exists")
        .estimated_rows()
}

#[test]
fn declarations_expose_the_estimate_of_sets_only() {
    let database = db("CREATE FUNCTION single() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION unsized() RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION sized() RETURNS SETOF INT LANGUAGE sql ROWS 10 AS 'SELECT 1';
         CREATE FUNCTION fractional() RETURNS SETOF INT LANGUAGE sql ROWS 10.5 AS 'SELECT 1';
         CREATE FUNCTION shaped() RETURNS TABLE (x INT) LANGUAGE sql ROWS 3 AS 'SELECT 1';
         CREATE FUNCTION records(OUT a INT, OUT b INT) RETURNS SETOF record LANGUAGE sql
             ROWS 7 AS 'SELECT 1, 2';
         CREATE FUNCTION written_default() RETURNS SETOF INT LANGUAGE sql ROWS 1000
             AS 'SELECT 1';");

    for (name, expected) in [
        ("single", None),
        ("unsized", Some(1000.0)),
        ("sized", Some(10.0)),
        ("fractional", Some(10.5)),
        ("shaped", Some(3.0)),
        ("records", Some(7.0)),
        ("written_default", Some(1000.0)),
    ] {
        assert_eq!(estimated_rows(&database, name), expected, "{name}");
    }
}

#[test]
fn literals_read_as_postgres_reads_them() {
    // An integer literal that fits `integer` is read whole, and any other
    // literal by `strtod`, which stops at an underscore. The value is then
    // stored as a `float4`.
    for (written, expected) in [
        ("+5", 5.0),
        ("0x10", 16.0),
        ("0o17", 15.0),
        ("0b101", 5.0),
        ("1_000", 1000.0),
        ("0x1_0", 16.0),
        ("00012", 12.0),
        ("2.5e1", 25.0),
        (".5", 0.5),
        ("1.5_0", 1.5),
        ("1_0e3", 1.0),
        ("1_000.5", 1.0),
        ("3_000_000_000", 3.0),
        ("0x1_0000_0000", 1.0),
        ("0x100000000", 4_294_967_296.0),
        ("2147483647", 2_147_483_648.0),
        ("0.123456789", 0.123_456_79),
        ("1e400", f32::INFINITY),
    ] {
        let database = db(&format!(
            "CREATE FUNCTION f() RETURNS SETOF INT LANGUAGE sql ROWS {written} AS 'SELECT 1';"
        ));

        assert_eq!(estimated_rows(&database, "f"), Some(expected), "ROWS {written}");
    }
}

#[test]
fn a_declaration_refuses_an_estimate_postgres_refuses() {
    for (declaration, refused_as_positive) in [
        ("RETURNS SETOF INT LANGUAGE sql ROWS 0", true),
        ("RETURNS SETOF INT LANGUAGE sql ROWS -1", true),
        ("RETURNS SETOF INT LANGUAGE sql ROWS 1e-50", true),
        ("RETURNS SETOF INT LANGUAGE sql ROWS 0o77777777777", true),
        ("RETURNS INT LANGUAGE sql ROWS 0", true),
        ("RETURNS INT LANGUAGE sql ROWS 5", false),
        ("RETURNS TABLE (x INT) LANGUAGE sql ROWS -2", true),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE FUNCTION f() {declaration} AS 'SELECT 1';"
        ));

        if refused_as_positive {
            assert!(
                matches!(&result, Err(Error::FunctionRowsNotPositive { function_name, .. }) if function_name == "f"),
                "{declaration}: got {result:?}"
            );
        } else {
            assert!(
                matches!(&result, Err(Error::FunctionRowsWithoutSet { function_name }) if function_name == "f"),
                "{declaration}: got {result:?}"
            );
        }
    }
}

#[test]
fn a_replacement_takes_its_own_declaration() {
    let database = db("CREATE FUNCTION f() RETURNS SETOF INT LANGUAGE sql ROWS 10 AS 'SELECT 1';
         CREATE OR REPLACE FUNCTION f() RETURNS SETOF INT LANGUAGE sql AS 'SELECT 2';");

    assert_eq!(estimated_rows(&database, "f"), Some(1000.0));
}

#[test]
fn alterations_follow_statement_and_action_order() {
    for (declared, alterations, expected) in [
        ("", "ALTER FUNCTION f() ROWS 42;", 42.0),
        ("ROWS 10", "ALTER FUNCTION f() ROWS 1000;", 1000.0),
        ("", "ALTER FUNCTION f() STABLE ROWS 5 COST 2;", 5.0),
        ("", "ALTER FUNCTION f() ROWS 5; ALTER FUNCTION f ROWS 6;", 6.0),
        ("ROWS 10", "ALTER FUNCTION f() STABLE;", 10.0),
    ] {
        let database = db(&format!(
            "CREATE FUNCTION f() RETURNS SETOF INT LANGUAGE sql {declared} AS 'SELECT 1';
             {alterations}"
        ));

        assert_eq!(estimated_rows(&database, "f"), Some(expected), "{declared} then {alterations}");
    }
}

#[test]
fn an_alteration_changes_only_the_selected_overload() {
    let database = db("CREATE FUNCTION f(x INT) RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT) RETURNS SETOF INT LANGUAGE sql AS 'SELECT 2';
         ALTER FUNCTION f(INTEGER) ROWS 3;");
    let estimates: Vec<_> = database
        .functions()
        .filter(|function| function.name() == "f")
        .map(|function| {
            (function.normalized_argument_type_names(&database), function.estimated_rows())
        })
        .collect();

    assert_eq!(estimates, [(vec!["INT".into()], Some(3.0)), (vec!["TEXT".into()], Some(1000.0))]);
}

#[test]
fn an_alteration_refuses_an_estimate_postgres_refuses() {
    let positive = |result: &Result<ParserDB, Error>| matches!(result, Err(Error::FunctionRowsNotPositive { function_name, .. }) if function_name == "f");
    for (declaration, alteration) in [
        ("RETURNS SETOF INT", "ROWS 0"),
        ("RETURNS SETOF INT", "STABLE ROWS -3"),
        // The estimate is refused before the function is found to return no
        // set.
        ("RETURNS INT", "ROWS 0"),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE FUNCTION f() {declaration} LANGUAGE sql AS 'SELECT 1';
             ALTER FUNCTION f() {alteration};"
        ));

        assert!(positive(&result), "{declaration}, {alteration}: got {result:?}");
    }

    let single = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f() ROWS 5;",
    );
    assert!(
        matches!(&single, Err(Error::FunctionRowsWithoutSet { function_name }) if function_name == "f"),
        "got {single:?}"
    );

    let absent = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(x INT) RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f(TEXT) ROWS 5;",
    );
    assert!(
        matches!(&absent, Err(Error::AlterFunctionNotFound { function_name }) if function_name == "f"),
        "got {absent:?}"
    );
}

#[test]
fn builtins_answer_their_catalog_estimate() {
    let database = db("");

    for (name, arguments, expected) in [
        ("unnest", &["anyarray"][..], Some(100.0)),
        ("regexp_matches", &["text", "text", "text"][..], Some(10.0)),
        ("regexp_matches", &["text", "text"][..], Some(1.0)),
        ("pg_snapshot_xip", &["pg_snapshot"][..], Some(50.0)),
        ("generate_series", &["int", "int"][..], Some(1000.0)),
        ("lower", &["text"][..], None),
    ] {
        assert_eq!(
            overload(&database, "pg_catalog", name, arguments).estimated_rows(),
            expected,
            "{name}({arguments:?})"
        );
    }
}
