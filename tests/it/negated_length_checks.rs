//! A length contradiction is a negation only when both bounds belong to the
//! same column, since the lengths of two different columns are unrelated, and
//! only on a column that cannot be `NULL`, since a `NULL` length makes the
//! check `NULL`, which admits the row.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

fn negations(sql: &str, table: &str) -> Vec<bool> {
    let database = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
    database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .check_constraints(&database)
        .expect("table is in this database")
        .map(|check| check.is_negation(&database).expect("check is in this database"))
        .collect()
}

#[test]
fn bounds_on_different_columns_are_not_a_negation() {
    let negations = negations(
        "CREATE TABLE t (a TEXT NOT NULL, b TEXT NOT NULL, CHECK (length(a) < 3 AND length(b) > 10));",
        "t",
    );
    assert_eq!(negations, vec![false]);
}

#[test]
fn close_bounds_on_different_columns_are_not_a_negation() {
    let negations = negations(
        "CREATE TABLE t (a TEXT NOT NULL, b TEXT NOT NULL, CHECK (length(a) < 5 AND length(b) > 2));",
        "t",
    );
    assert_eq!(negations, vec![false]);
}

#[test]
fn bounds_on_one_column_are_a_negation() {
    let negations = negations(
        "CREATE TABLE t (a TEXT NOT NULL CHECK (length(a) < 3 AND length(a) > 10));",
        "t",
    );
    assert_eq!(negations, vec![true]);
}

#[test]
fn bounds_on_one_nullable_column_admit_its_null_rows() {
    let negations =
        negations("CREATE TABLE t (a TEXT CHECK (length(a) < 3 AND length(a) > 10));", "t");
    assert_eq!(negations, vec![false]);
}

#[test]
fn adjacent_inclusive_bounds_on_one_column_are_a_negation() {
    let negations = negations(
        "CREATE TABLE t (a TEXT NOT NULL CHECK (length(a) <= 3 AND length(a) >= 4));",
        "t",
    );
    assert_eq!(negations, vec![true]);
}

#[test]
fn touching_inclusive_bounds_on_one_column_are_not_a_negation() {
    let negations = negations(
        "CREATE TABLE t (a TEXT NOT NULL CHECK (length(a) <= 3 AND length(a) >= 3));",
        "t",
    );
    assert_eq!(negations, vec![false]);
}

#[test]
fn transitive_bounds_are_not_a_negation_on_their_own() {
    let negations = negations(
        "CREATE TABLE t (
            b TEXT NOT NULL CHECK (length(b) < 5),
            a TEXT NOT NULL CHECK (length(a) < length(b) AND length(a) > 10)
        );",
        "t",
    );
    assert_eq!(negations, vec![false, false]);
}

#[test]
fn transitive_bounds_admit_no_row_beside_the_other_check() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (
            b TEXT NOT NULL CHECK (length(b) < 5),
            a TEXT NOT NULL CHECK (length(a) < length(b) AND length(a) > 10)
        );",
    )
    .expect("schema parses");
    let table = database.tables().next().expect("table exists");
    let checks: Vec<_> =
        table.check_constraints(&database).expect("table is in this database").collect();
    let beside = checks[1].truth_under_other_checks(&database).expect("check is in this database");
    assert!(!beside.outcomes.truth, "{beside:?}");
}

#[test]
fn length_resolves_the_overload_its_argument_selects() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE code AS ENUM ('x');
         CREATE TABLE t (
            a TEXT NOT NULL,
            b VARCHAR(8) NOT NULL,
            s LSEG NOT NULL,
            r LSEG NOT NULL,
            c code NOT NULL,
            d code NOT NULL
         );",
    )
    .expect("schema parses");
    let table = database.tables().next().expect("table exists");
    let truth = |sql: &str| {
        let expression = sqlparser::parser::Parser::new(&PostgreSqlDialect {})
            .try_with_sql(sql)
            .expect("expression lexes")
            .parse_expr()
            .expect("expression parses");
        table
            .expression_truth(&database, &expression, IdentifierCase::AsWritten)
            .expect("table is in this database")
    };
    // `length(text)` and `length(lseg)` return `int4` and `float8`, whose
    // comparisons raise nothing.
    for sql in ["length(a) < length(b)", "length(s) < length(r)"] {
        let resolved = truth(sql);
        assert!(!resolved.may_raise, "{sql}: {resolved:?}");
    }
    // No overload takes `code`, so both calls keep an unknown type, whose
    // comparison may raise.
    let unresolved = truth("length(c) < length(d)");
    assert!(unresolved.outcomes.truth && unresolved.outcomes.falsity, "{unresolved:?}");
    assert!(unresolved.may_raise, "{unresolved:?}");
}
