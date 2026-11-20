//! Tests that a column reference is refused in the `DEFAULT` of a column or a
//! domain, as PostgreSQL 18 refuses it, and that the names a default may hold
//! without reading a column stay accepted.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ExpressionPlace},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Each statement whose default reads a column, with the reference as written.
const REFUSED: &[(&str, &str)] = &[
    ("CREATE TABLE people (mid TEXT, label TEXT DEFAULT (mid || '!'));", "mid"),
    (
        "CREATE TABLE people (mid TEXT, label TEXT);
         ALTER TABLE people ALTER COLUMN label SET DEFAULT mid;",
        "mid",
    ),
    (
        "CREATE TABLE people (mid TEXT); ALTER TABLE people ADD COLUMN label TEXT DEFAULT mid;",
        "mid",
    ),
    ("CREATE DOMAIN d AS TEXT DEFAULT (VALUE || '!');", "VALUE"),
    ("CREATE DOMAIN d AS TEXT DEFAULT \"VALUE\";", "\"VALUE\""),
    ("CREATE TABLE t (a INT DEFAULT nope);", "nope"),
    ("CREATE TABLE t (a TEXT DEFAULT \"current_role\");", "\"current_role\""),
    ("CREATE TABLE t (a INT DEFAULT t.a);", "t.a"),
    ("CREATE TABLE t (a TEXT DEFAULT t.*);", "t.*"),
    ("CREATE TABLE t (a TEXT, b TEXT DEFAULT coalesce(a, 'x'));", "a"),
    ("CREATE TABLE t (a INT DEFAULT (nope + (SELECT 1)));", "nope"),
];

/// A default reading a column is refused, naming the reference as written.
#[test]
fn a_default_refuses_a_column_reference() {
    for (sql, reference) in REFUSED {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::ColumnReferenceInDefault { reference: refused }) if refused == reference),
            "{sql}: {result:?}"
        );
    }
}

/// The error reads as PostgreSQL's, with the reference it names.
#[test]
fn the_refusal_names_the_reference() {
    let message =
        parse(REFUSED[0].0).map(|_| ()).expect_err("the reference is refused").to_string();
    assert_eq!(message, "cannot use column reference `mid` in DEFAULT expression");
}

/// PostgreSQL meets a subquery written before a column reference first, and
/// refuses the subquery.
#[test]
fn the_first_refusal_written_is_the_one_given() {
    let result = parse("CREATE TABLE t (a INT DEFAULT ((SELECT 1) + nope));");
    assert!(
        matches!(&result, Err(Error::SubqueryInExpression { place: ExpressionPlace::Default })),
        "{result:?}"
    );
}

/// The niladic functions SQL spells without parentheses, a named argument's
/// label and a field after a dot read no column, so PostgreSQL stores them.
#[test]
fn a_default_reading_no_column_is_accepted() {
    let statements = [
        "CREATE TABLE t (
             a TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
             b TEXT DEFAULT CURRENT_USER,
             c TEXT DEFAULT USER,
             d TEXT DEFAULT SESSION_USER,
             e TEXT DEFAULT CURRENT_ROLE,
             f TEXT DEFAULT CURRENT_CATALOG,
             g TEXT DEFAULT CURRENT_SCHEMA,
             h TIMESTAMP DEFAULT LOCALTIMESTAMP,
             i DATE DEFAULT CURRENT_DATE,
             j TEXT DEFAULT SYSTEM_USER,
             k TIME DEFAULT LOCALTIME,
             l TIMETZ DEFAULT CURRENT_TIME
         );",
        "CREATE TABLE t (
             a INT DEFAULT EXTRACT(YEAR FROM now())::INT,
             b TIMESTAMP DEFAULT (now() AT TIME ZONE 'utc'::TEXT),
             c TEXT[] DEFAULT ARRAY[]::TEXT[],
             d INTERVAL DEFAULT make_interval(days => 1),
             e TEXT DEFAULT CASE WHEN true THEN 'x' END,
             f TEXT DEFAULT (CURRENT_USER)::TEXT
         );",
        "CREATE TYPE pair AS (f1 INT, f2 INT);
         CREATE TABLE t (a INT DEFAULT (ROW(1, 2)::pair).f1);",
        "CREATE SEQUENCE s; CREATE TABLE t (a BIGINT DEFAULT nextval('s'::regclass));",
        "CREATE DOMAIN d AS TEXT DEFAULT 'x' CHECK (VALUE <> '');",
        "CREATE TABLE people (mid TEXT);
         ALTER TABLE people ADD COLUMN IF NOT EXISTS mid TEXT DEFAULT mid;",
    ];
    let refused: Vec<_> = statements
        .into_iter()
        .filter_map(|sql| parse(sql).err().map(|error| format!("{sql}: {error:?}")))
        .collect();
    assert!(refused.is_empty(), "{refused:#?}");
}
