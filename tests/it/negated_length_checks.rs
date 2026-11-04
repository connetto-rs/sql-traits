//! A length contradiction is a negation only when both bounds belong to the
//! same column, since the lengths of two different columns are unrelated.
//!
//! `is_negation` used to compare the constraint's upper and lower bounds
//! without naming a column, so a check that read two columns answered a
//! negation when one column's lower bound met the other's upper bound.
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
        "CREATE TABLE t (a TEXT, b TEXT, CHECK (length(a) < 3 AND length(b) > 10));",
        "t",
    );
    assert_eq!(negations, vec![false]);
}

#[test]
fn close_bounds_on_different_columns_are_not_a_negation() {
    let negations =
        negations("CREATE TABLE t (a TEXT, b TEXT, CHECK (length(a) < 5 AND length(b) > 2));", "t");
    assert_eq!(negations, vec![false]);
}

#[test]
fn bounds_on_one_column_are_a_negation() {
    let negations =
        negations("CREATE TABLE t (a TEXT CHECK (length(a) < 3 AND length(a) > 10));", "t");
    assert_eq!(negations, vec![true]);
}

#[test]
fn adjacent_inclusive_bounds_on_one_column_are_a_negation() {
    let negations =
        negations("CREATE TABLE t (a TEXT CHECK (length(a) <= 3 AND length(a) >= 4));", "t");
    assert_eq!(negations, vec![true]);
}

#[test]
fn touching_inclusive_bounds_on_one_column_are_not_a_negation() {
    let negations =
        negations("CREATE TABLE t (a TEXT CHECK (length(a) <= 3 AND length(a) >= 3));", "t");
    assert_eq!(negations, vec![false]);
}

#[test]
fn transitive_bounds_on_one_column_are_a_negation() {
    let negations = negations(
        "CREATE TABLE t (b TEXT CHECK (length(b) < 5), a TEXT CHECK (length(a) < length(b) AND length(a) > 10));",
        "t",
    );
    assert_eq!(negations, vec![false, true]);
}
