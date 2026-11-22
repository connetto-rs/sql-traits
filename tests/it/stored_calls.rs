//! Tests that an aggregate, a window and a set-returning call are refused
//! wherever PostgreSQL 18 stores an expression it evaluates one row at a
//! time, in the order PostgreSQL transforms the expression's nodes.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ExpressionPlace, RefusedCall},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

const TRIGGER_FUNCTION: &str =
    "CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';";

const REFUSED: &[(&str, RefusedCall, ExpressionPlace, &str, &str)] = &[
    (
        "CREATE TABLE d (iv INTERVAL, g TEXT GENERATED ALWAYS AS (min(iv)) STORED);",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (g TEXT GENERATED ALWAYS AS (sum(INTERVAL '1 day')) STORED);",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS (min(i)) VIRTUAL);",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS (row_number() OVER ()) STORED);",
        RefusedCall::Window,
        ExpressionPlace::Generation,
        "42P20",
        "window functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS (count(*) OVER ()) STORED);",
        RefusedCall::Window,
        ExpressionPlace::Generation,
        "42P20",
        "window functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS (generate_series(1, i)) STORED);",
        RefusedCall::SetReturning,
        ExpressionPlace::Generation,
        "0A000",
        "set-returning functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS \
         (percentile_disc(0.5) WITHIN GROUP (ORDER BY i)) STORED);",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS (count(DISTINCT i)) STORED);",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT, g INT GENERATED ALWAYS AS (pg_catalog.min(i)) STORED);",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (i INT CHECK (min(i) > 0));",
        RefusedCall::Aggregate,
        ExpressionPlace::CheckConstraint,
        "42803",
        "aggregate functions are not allowed in check constraints",
    ),
    (
        "CREATE TABLE d (i INT, CHECK (row_number() OVER () > 0));",
        RefusedCall::Window,
        ExpressionPlace::CheckConstraint,
        "42P20",
        "window functions are not allowed in check constraints",
    ),
    (
        "CREATE TABLE d (i INT CHECK (generate_series(1, i) > 0));",
        RefusedCall::SetReturning,
        ExpressionPlace::CheckConstraint,
        "0A000",
        "set-returning functions are not allowed in check constraints",
    ),
    (
        "CREATE FUNCTION f() RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE d (i INT CHECK (f() > 0));",
        RefusedCall::SetReturning,
        ExpressionPlace::CheckConstraint,
        "0A000",
        "set-returning functions are not allowed in check constraints",
    ),
    (
        "CREATE TABLE d (i INT DEFAULT count(*));",
        RefusedCall::Aggregate,
        ExpressionPlace::Default,
        "42803",
        "aggregate functions are not allowed in DEFAULT expressions",
    ),
    (
        "CREATE TABLE d (i INT DEFAULT row_number() OVER ());",
        RefusedCall::Window,
        ExpressionPlace::Default,
        "42P20",
        "window functions are not allowed in DEFAULT expressions",
    ),
    (
        "CREATE FUNCTION f() RETURNS TABLE (x INT) LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE d (i INT DEFAULT f());",
        RefusedCall::SetReturning,
        ExpressionPlace::Default,
        "0A000",
        "set-returning functions are not allowed in DEFAULT expressions",
    ),
    (
        "CREATE TABLE d (i INT); CREATE INDEX ON d ((min(i)));",
        RefusedCall::Aggregate,
        ExpressionPlace::IndexExpression,
        "42803",
        "aggregate functions are not allowed in index expressions",
    ),
    (
        "CREATE TABLE d (i INT); CREATE INDEX ON d (i) WHERE row_number() OVER () > 0;",
        RefusedCall::Window,
        ExpressionPlace::IndexPredicate,
        "42P20",
        "window functions are not allowed in index predicates",
    ),
    (
        "CREATE TABLE d (i INT, EXCLUDE USING btree ((min(i)) WITH =));",
        RefusedCall::Aggregate,
        ExpressionPlace::IndexExpression,
        "42803",
        "aggregate functions are not allowed in index expressions",
    ),
    (
        "CREATE TABLE d (i INT, EXCLUDE USING btree (i WITH =) WHERE (count(*) > 0));",
        RefusedCall::Aggregate,
        ExpressionPlace::IndexPredicate,
        "42803",
        "aggregate functions are not allowed in index predicates",
    ),
    (
        "CREATE TABLE d (i INT) PARTITION BY RANGE ((generate_series(1, i)));",
        RefusedCall::SetReturning,
        ExpressionPlace::PartitionKey,
        "0A000",
        "set-returning functions are not allowed in partition key expressions",
    ),
    (
        "CREATE TABLE p (i INT) PARTITION BY RANGE (i);
         CREATE TABLE d PARTITION OF p FOR VALUES FROM (count(*)) TO (10);",
        RefusedCall::Aggregate,
        ExpressionPlace::PartitionBound,
        "42803",
        "aggregate functions are not allowed in partition bound",
    ),
    (
        "CREATE TABLE p (i INT) PARTITION BY LIST (i);
         CREATE TABLE d PARTITION OF p FOR VALUES IN (row_number() OVER ());",
        RefusedCall::Window,
        ExpressionPlace::PartitionBound,
        "42P20",
        "window functions are not allowed in partition bound",
    ),
    (
        "CREATE TABLE d (i INT); ALTER TABLE d ALTER COLUMN i TYPE INT8 USING min(i);",
        RefusedCall::Aggregate,
        ExpressionPlace::Transform,
        "42803",
        "aggregate functions are not allowed in transform expressions",
    ),
    (
        "CREATE TABLE d (i INT); ALTER TABLE d ALTER COLUMN i SET DEFAULT count(*);",
        RefusedCall::Aggregate,
        ExpressionPlace::Default,
        "42803",
        "aggregate functions are not allowed in DEFAULT expressions",
    ),
    (
        "CREATE TABLE d (i INT); ALTER TABLE d ADD CONSTRAINT k CHECK (min(i) > 0);",
        RefusedCall::Aggregate,
        ExpressionPlace::CheckConstraint,
        "42803",
        "aggregate functions are not allowed in check constraints",
    ),
    (
        "CREATE TABLE d (i INT); ALTER TABLE d ADD COLUMN g INT GENERATED ALWAYS AS (min(i)) STORED;",
        RefusedCall::Aggregate,
        ExpressionPlace::Generation,
        "42803",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE DOMAIN dm AS INT CHECK (generate_series(1, VALUE) > 0);",
        RefusedCall::SetReturning,
        ExpressionPlace::CheckConstraint,
        "0A000",
        "set-returning functions are not allowed in check constraints",
    ),
    (
        "CREATE DOMAIN dm AS INT DEFAULT count(*);",
        RefusedCall::Aggregate,
        ExpressionPlace::Default,
        "42803",
        "aggregate functions are not allowed in DEFAULT expressions",
    ),
];

#[test]
fn a_stored_expression_refuses_an_aggregate_a_window_and_a_set_returning_call() {
    for (sql, call, place, sqlstate, message) in REFUSED {
        let error = parse(sql).map(|_| ()).expect_err(sql);
        assert!(
            matches!(&error, Error::CallInExpression { call: refused, place: at } if refused == call && at == place),
            "{sql}: {error:?}"
        );
        let refusal = error.postgres_error().expect("PostgreSQL refuses the call");
        assert_eq!(refusal.sqlstate().to_string(), *sqlstate, "{sql}");
        assert_eq!(refusal.message(), *message, "{sql}");
        assert_eq!(error.to_string(), *message, "{sql}");
    }
}

#[test]
fn a_trigger_condition_refuses_each_call_postgresql_refuses_there() {
    let refused = [
        (
            "min(NEW.i) > 0",
            "42803",
            "aggregate functions are not allowed in trigger WHEN conditions",
        ),
        (
            "row_number() OVER () > 0",
            "42P20",
            "window functions are not allowed in trigger WHEN conditions",
        ),
        (
            "generate_series(1, NEW.i) > 0",
            "0A000",
            "set-returning functions are not allowed in trigger WHEN conditions",
        ),
    ];
    for (condition, sqlstate, message) in refused {
        let sql = format!(
            "CREATE TABLE d (i INT); {TRIGGER_FUNCTION}
             CREATE TRIGGER t BEFORE INSERT ON d FOR EACH ROW WHEN ({condition})
                 EXECUTE FUNCTION trg();"
        );
        let refusal = parse(&sql)
            .map(|_| ())
            .expect_err(&sql)
            .postgres_error()
            .expect("PostgreSQL refuses the call");
        assert_eq!(
            (refusal.sqlstate().to_string().as_str(), refusal.message()),
            (sqlstate, message),
            "{sql}"
        );
    }
}

const FIRST_REFUSALS: &[(&str, &str)] = &[
    (
        "CREATE TABLE d (a INT GENERATED ALWAYS AS (1) STORED, \
             b INT GENERATED ALWAYS AS (min(a)) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b TEXT GENERATED ALWAYS AS (now()::TEXT || min(a)) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (min(xmin::TEXT::INT)) STORED);",
        r#"cannot use system column "xmin" in column generation expression"#,
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (min(a) + xmin::TEXT::INT) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (xmin::TEXT::INT + (SELECT 1)) STORED);",
        r#"cannot use system column "xmin" in column generation expression"#,
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (d.xmin::TEXT::INT + (SELECT 1)) STORED);",
        r#"cannot use system column "xmin" in column generation expression"#,
    ),
    (
        "CREATE TABLE d (a INT); \
             ALTER TABLE d ADD COLUMN b INT GENERATED ALWAYS AS (min(xmin::TEXT::INT)) STORED;",
        r#"cannot use system column "xmin" in column generation expression"#,
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS \
             (min(a) FILTER (WHERE xmin::TEXT::INT > 0)) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (min((SELECT 1))) STORED);",
        "cannot use subquery in column generation expression",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (min(a) + (SELECT 1)) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (generate_series(1, (SELECT 1))) STORED);",
        "cannot use subquery in column generation expression",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS \
             (count(*) FILTER (WHERE a > (SELECT 1))) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (min(a ORDER BY (SELECT 1))) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS \
             (percentile_disc(0.5) WITHIN GROUP (ORDER BY (SELECT 1))) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS \
             (row_number() OVER (ORDER BY (SELECT 1))) STORED);",
        "window functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS \
             (count(*) FILTER (WHERE a > (SELECT 1)) OVER ()) STORED);",
        "window functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (row_number() OVER () + min(a)) STORED);",
        "window functions are not allowed in column generation expressions",
    ),
    (
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (generate_series(1, min(a))) STORED);",
        "aggregate functions are not allowed in column generation expressions",
    ),
    ("CREATE TABLE d (a INT DEFAULT min(a));", "cannot use column reference in DEFAULT expression"),
    (
        "CREATE TABLE d (a INT DEFAULT count(*) FILTER (WHERE a > 0));",
        "aggregate functions are not allowed in DEFAULT expressions",
    ),
    (
        "CREATE TABLE d (a INT CHECK (min(a) > (SELECT 1)));",
        "aggregate functions are not allowed in check constraints",
    ),
];

/// PostgreSQL transforms a call's arguments before placing the call, and
/// transforms `FILTER`, an aggregate's `ORDER BY` and `WITHIN GROUP` as
/// expressions that may run a subquery or read any column, and `OVER` never.
#[test]
fn the_first_refusal_is_the_one_postgresql_meets_first() {
    for (sql, message) in FIRST_REFUSALS {
        let refusal = parse(sql)
            .map(|_| ())
            .expect_err(sql)
            .postgres_error()
            .expect("PostgreSQL refuses the expression");
        assert_eq!(refusal.message(), *message, "{sql}");
    }
}

/// A clause the function a call selects refuses is refused before the call
/// is placed, and a call the argument count leads to a plain function is
/// placed anywhere.
#[test]
fn a_call_postgresql_places_or_refuses_for_its_clauses_is_not_refused_for_its_place() {
    let refused = [
        (
            "CREATE TABLE d (i INT CHECK (now() OVER () IS NOT NULL));",
            "OVER specified, but now is not a window function nor an aggregate function",
        ),
        (
            "CREATE TABLE d (i INT CHECK (count() > 0));",
            "count(*) must be used to call a parameterless aggregate function",
        ),
        (
            "CREATE TABLE d (i INT CHECK (row_number() > 0));",
            "window function row_number requires an OVER clause",
        ),
        (
            "CREATE TABLE d (i INT CHECK (percentile_disc(0.5) > 0));",
            "function percentile_disc(numeric) does not exist",
        ),
        (
            "CREATE TABLE d (i INT CHECK (min(i) WITHIN GROUP (ORDER BY i) > 0));",
            "function min(integer, integer) does not exist",
        ),
        (
            "CREATE TABLE d (i INT CHECK (now() FILTER (WHERE (SELECT true)) IS NOT NULL));",
            "FILTER specified, but now is not an aggregate function",
        ),
    ];
    for (sql, message) in refused {
        let refusal = parse(sql)
            .map(|_| ())
            .expect_err(sql)
            .postgres_error()
            .expect("PostgreSQL refuses the call");
        assert_eq!(refusal.message(), message, "{sql}");
    }
    for sql in [
        "CREATE FUNCTION min(TEXT, TEXT) RETURNS TEXT LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE TABLE d (a TEXT, b TEXT CHECK (min(a, b) IS NOT NULL));",
        "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (abs(a)) STORED, c INT DEFAULT abs(-1), \
         CHECK (abs(a) > 0));",
        "CREATE TABLE q (x INT); CREATE TABLE d (a INT);
         CREATE POLICY pl ON d USING (a > (SELECT min(x) FROM q));",
    ] {
        let result = parse(sql);
        assert!(result.is_ok(), "{sql}: {result:?}");
    }
}

const MAX_TEXT: &str =
    "CREATE FUNCTION max(TEXT) RETURNS TEXT LANGUAGE sql IMMUTABLE AS 'SELECT $1';";

/// A call whose name and argument count reach functions of different kinds
/// is placed as the function its argument types and the search path select.
#[test]
fn a_call_reaching_functions_of_different_kinds_is_placed_as_the_one_selected() {
    let refused = [
        (
            "CREATE FUNCTION min(TEXT) RETURNS SETOF TEXT LANGUAGE sql AS 'SELECT $1';
             CREATE TABLE d (a TEXT CHECK (min(a) IS NOT NULL));"
                .to_string(),
            "aggregate functions are not allowed in check constraints",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a INT CHECK (max(a) > 0));"),
            "aggregate functions are not allowed in check constraints",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a TEXT CHECK (max(a) > ''));"),
            "aggregate functions are not allowed in check constraints",
        ),
        (
            format!(
                "{MAX_TEXT} CREATE TABLE d (a INT, g INT GENERATED ALWAYS AS (max(a)) STORED);"
            ),
            "aggregate functions are not allowed in column generation expressions",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a INT DEFAULT max(1));"),
            "aggregate functions are not allowed in DEFAULT expressions",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a INT); CREATE INDEX ON d ((max(a)));"),
            "aggregate functions are not allowed in index expressions",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a INT); CREATE INDEX ON d (a) WHERE max(a) > 0;"),
            "aggregate functions are not allowed in index predicates",
        ),
        (
            format!(
                "{MAX_TEXT} {TRIGGER_FUNCTION} CREATE TABLE d (a INT); CREATE INDEX ON d (a);
                 CREATE TRIGGER t BEFORE INSERT ON d FOR EACH ROW WHEN (max(NEW.a) > 0) \
                 EXECUTE FUNCTION trg();"
            ),
            "aggregate functions are not allowed in trigger WHEN conditions",
        ),
        (
            format!("{MAX_TEXT} CREATE DOMAIN dm AS INT CHECK (max(VALUE) > 0);"),
            "aggregate functions are not allowed in check constraints",
        ),
        (
            format!("{MAX_TEXT} CREATE DOMAIN dm AS INT DEFAULT max(1);"),
            "aggregate functions are not allowed in DEFAULT expressions",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a INT) PARTITION BY RANGE ((max(a)));"),
            "aggregate functions are not allowed in partition key expressions",
        ),
        (
            format!("{MAX_TEXT} CREATE TABLE d (a INT, EXCLUDE USING btree ((max(a)) WITH =));"),
            "aggregate functions are not allowed in index expressions",
        ),
    ];
    for (sql, message) in refused {
        let refusal = parse(&sql)
            .map(|_| ())
            .expect_err(&sql)
            .postgres_error()
            .expect("PostgreSQL refuses the call");
        assert_eq!(refusal.message(), message, "{sql}");
    }
    let selected_plain = "CREATE SCHEMA s;
        CREATE FUNCTION s.min(INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT $1';
        SET search_path = s, pg_catalog;
        CREATE TABLE d (a INT CHECK (min(a) > 0));";
    let result = parse(selected_plain);
    assert!(result.is_ok(), "{selected_plain}: {result:?}");
}
