//! Tests that a subquery is refused wherever PostgreSQL 18 stores an
//! expression it evaluates without running a query, under the place name its
//! error gives, and accepted in the policies and rules that may run one.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ExpressionPlace},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Each statement storing a subquery, with the place PostgreSQL names.
const REFUSED: &[(&str, ExpressionPlace)] = &[
    ("CREATE TABLE d (a INT CHECK (a IN (SELECT 1)));", ExpressionPlace::CheckConstraint),
    ("CREATE TABLE d (a INT, CHECK (EXISTS (SELECT 1)));", ExpressionPlace::CheckConstraint),
    ("CREATE TABLE d (a INT CHECK (a = ANY (ARRAY(SELECT 1))));", ExpressionPlace::CheckConstraint),
    ("CREATE TABLE d (a INT DEFAULT (SELECT 1));", ExpressionPlace::Default),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS ((SELECT 1)) STORED);",
        ExpressionPlace::Generation,
    ),
    (
        "CREATE TABLE d (a INT, EXCLUDE ((a + (SELECT 1)) WITH =));",
        ExpressionPlace::IndexExpression,
    ),
    (
        "CREATE TABLE d (a INT, EXCLUDE (a WITH =) WHERE (a > (SELECT 1)));",
        ExpressionPlace::IndexPredicate,
    ),
    ("CREATE TABLE d (a INT) PARTITION BY RANGE (((SELECT 1)));", ExpressionPlace::PartitionKey),
    (
        "CREATE TABLE p (a INT) PARTITION BY RANGE (a);
         CREATE TABLE d PARTITION OF p FOR VALUES FROM ((SELECT 1)) TO (10);",
        ExpressionPlace::PartitionBound,
    ),
    (
        "CREATE TABLE p (a INT) PARTITION BY LIST (a);
         CREATE TABLE d (a INT);
         ALTER TABLE p ATTACH PARTITION d FOR VALUES IN ((SELECT 1));",
        ExpressionPlace::PartitionBound,
    ),
    ("CREATE TABLE d (a INT); CREATE INDEX ON d (((SELECT 1)));", ExpressionPlace::IndexExpression),
    (
        "CREATE TABLE d (a INT); CREATE INDEX ON d (a) WHERE a > (SELECT 1);",
        ExpressionPlace::IndexPredicate,
    ),
    ("CREATE DOMAIN dm AS INT CHECK (VALUE > (SELECT 1));", ExpressionPlace::CheckConstraint),
    ("CREATE DOMAIN dm AS INT DEFAULT (SELECT 1);", ExpressionPlace::Default),
    (
        "CREATE TABLE d (a INT); ALTER TABLE d ALTER COLUMN a SET DEFAULT (SELECT 1);",
        ExpressionPlace::Default,
    ),
    (
        "CREATE TABLE d (a INT); ALTER TABLE d ADD CHECK (a > (SELECT 1));",
        ExpressionPlace::CheckConstraint,
    ),
    (
        "CREATE TABLE d (a INT); ALTER TABLE d ADD COLUMN b INT DEFAULT (SELECT 1);",
        ExpressionPlace::Default,
    ),
    (
        "CREATE TABLE d (a INT); ALTER TABLE d ALTER COLUMN a TYPE int8 USING (SELECT 1);",
        ExpressionPlace::Transform,
    ),
    (
        "CREATE TABLE d (a INT);
         CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TRIGGER t BEFORE INSERT ON d FOR EACH ROW WHEN (NEW.a > (SELECT 1))
             EXECUTE FUNCTION trg();",
        ExpressionPlace::TriggerWhen,
    ),
];

/// Every stored expression PostgreSQL evaluates without running a query
/// refuses a subquery, naming its place.
#[test]
fn a_stored_expression_refuses_a_subquery() {
    for (sql, place) in REFUSED {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::SubqueryInExpression { place: refused }) if refused == place),
            "{sql}: {result:?}"
        );
    }
}

/// The error reads as PostgreSQL's.
#[test]
fn the_refusal_reads_as_postgresql_writes_it() {
    let messages = REFUSED
        .iter()
        .map(|(sql, _)| parse(sql).map(|_| ()).expect_err("the subquery is refused").to_string());
    let expected = [
        "cannot use subquery in check constraint",
        "cannot use subquery in check constraint",
        "cannot use subquery in check constraint",
        "cannot use subquery in DEFAULT expression",
        "cannot use subquery in column generation expression",
        "cannot use subquery in index expression",
        "cannot use subquery in index predicate",
        "cannot use subquery in partition key expression",
        "cannot use subquery in partition bound",
        "cannot use subquery in partition bound",
        "cannot use subquery in index expression",
        "cannot use subquery in index predicate",
        "cannot use subquery in check constraint",
        "cannot use subquery in DEFAULT expression",
        "cannot use subquery in DEFAULT expression",
        "cannot use subquery in check constraint",
        "cannot use subquery in DEFAULT expression",
        "cannot use subquery in transform expression",
        "cannot use subquery in trigger WHEN condition",
    ];
    assert_eq!(messages.collect::<Vec<_>>(), expected);
}

/// A policy and a rule condition run their subqueries, and a statement
/// PostgreSQL skips never reaches its expressions.
#[test]
fn a_subquery_stays_where_postgresql_runs_or_skips_it() {
    for sql in [
        "CREATE TABLE q (x INT); CREATE TABLE d (a INT);
         CREATE POLICY pl ON d USING (a IN (SELECT x FROM q));",
        "CREATE TABLE d (a INT);
         CREATE RULE r AS ON INSERT TO d WHERE NEW.a > (SELECT 1) DO INSTEAD NOTHING;",
        "CREATE TABLE d (a INT); CREATE TABLE IF NOT EXISTS d (a INT CHECK ((SELECT 1) > 0));",
        "ALTER TABLE IF EXISTS missing ADD CHECK ((SELECT 1) > 0);",
        "CREATE TABLE d (a INT); ALTER TABLE d ADD COLUMN IF NOT EXISTS a INT DEFAULT (SELECT 1);",
    ] {
        let result = parse(sql);
        assert!(result.is_ok(), "{sql}: {result:?}");
    }
}
