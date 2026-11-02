//! Tests that a grant naming a column checks it against the table it grants on.
//!
//! `GRANT SELECT (col) ON t` is refused by the database when `t` has no such
//! column, and a grant may list several tables and needs the column on each.
//! Every expectation here was checked against a real PostgreSQL 16 first,
//! including the one that looks like it should fail and does not.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

#[test]
fn a_column_the_table_does_not_have_is_refused() {
    let refused = parse(
        "CREATE ROLE r;
         CREATE TABLE docs (id INT);
         GRANT SELECT (nope) ON docs TO r;",
    );

    assert!(
        matches!(&refused, Err(Error::ColumnNotFoundForGrant { column_name, table_name })
            if column_name == "nope" && table_name == "docs"),
        "got {refused:?}"
    );
}

#[test]
fn a_column_the_table_has_is_accepted() {
    parse(
        "CREATE ROLE r;
         CREATE TABLE docs (id INT, name TEXT);
         GRANT SELECT (id, name) ON docs TO r;",
    )
    .expect("both columns exist");
}

/// A grant may list several tables, and the database wants the column on each,
/// naming the one that lacks it.
#[test]
fn every_listed_table_needs_the_column() {
    parse(
        "CREATE ROLE r;
         CREATE TABLE a (id INT, c INT);
         CREATE TABLE b (id INT, c INT);
         GRANT SELECT (c) ON a, b TO r;",
    )
    .expect("both tables have it");

    let refused = parse(
        "CREATE ROLE r;
         CREATE TABLE a (id INT, c INT);
         CREATE TABLE b (id INT);
         GRANT SELECT (c) ON a, b TO r;",
    );
    assert!(
        matches!(&refused, Err(Error::ColumnNotFoundForGrant { column_name, table_name })
            if column_name == "c" && table_name == "b"),
        "the table that lacks it is the one named, got {refused:?}"
    );
}

/// Quoting decides which column a name reaches, as everywhere else.
#[test]
fn quoting_decides_which_column_the_grant_names() {
    parse(
        "CREATE ROLE r;
         CREATE TABLE docs (\"Id\" INT);
         GRANT SELECT (\"Id\") ON docs TO r;",
    )
    .expect("the quoted name matches the quoted column");

    let refused = parse(
        "CREATE ROLE r;
         CREATE TABLE docs (\"Id\" INT);
         GRANT SELECT (Id) ON docs TO r;",
    );
    assert!(
        matches!(&refused, Err(Error::ColumnNotFoundForGrant { .. })),
        "an unquoted name folds and reaches no column, got {refused:?}"
    );
}

/// The schema-wide form carries no table list to check against, and the
/// database accepts a column list beside it rather than refusing, so this must
/// not refuse either.
#[test]
fn the_schema_wide_form_is_left_alone() {
    parse(
        "CREATE ROLE r;
         CREATE TABLE public.docs (id INT);
         GRANT SELECT (id) ON ALL TABLES IN SCHEMA public TO r;",
    )
    .expect("the database accepts this");
}

/// Under the permissive setting the table may be absent, and then there is
/// nothing to check the columns against.
#[test]
fn an_unresolved_table_leaves_the_columns_unchecked() {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>("CREATE ROLE r; GRANT SELECT (whatever) ON absent TO r;")
        .expect("no table, so no column check");
}

/// A grant naming no columns at all reaches this code and must pass through it.
#[test]
fn a_table_wide_grant_names_no_columns() {
    parse(
        "CREATE ROLE r;
         CREATE TABLE docs (id INT);
         GRANT SELECT ON docs TO r;
         GRANT ALL PRIVILEGES ON docs TO r;",
    )
    .expect("neither names a column");
}

/// A column is validated when the table resolves through the search path, for
/// a grant and for a revoke alike.
#[test]
fn a_column_on_a_path_resolved_table_is_validated() {
    let refused = parse(
        "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE docs (id INT);
         CREATE ROLE r;
         GRANT SELECT (nope) ON docs TO r;",
    );
    assert!(
        matches!(&refused, Err(Error::ColumnNotFoundForGrant { column_name, table_name })
            if column_name == "nope" && table_name == "app.docs"),
        "got {refused:?}"
    );

    let revoked = parse(
        "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE docs (id INT);
         CREATE ROLE r;
         GRANT SELECT (id) ON docs TO r;
         REVOKE SELECT (nope) ON docs FROM r;",
    );
    assert!(
        matches!(&revoked, Err(Error::ColumnNotFoundForGrant { column_name, .. })
            if column_name == "nope"),
        "got {revoked:?}"
    );

    parse(
        "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE docs (id INT);
         CREATE ROLE r;
         GRANT SELECT (id) ON docs TO r;",
    )
    .expect("the column exists on the table the path selects");
}

/// A catalog-qualified name is beyond the strict resolver and keeps the
/// lenient column check, reachable only under the open world since the closed
/// world refuses the name outright.
#[test]
fn a_catalog_qualified_grant_keeps_its_column_check() {
    let refused = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(
        "CREATE TABLE docs (id INT);
             CREATE ROLE r;
             GRANT SELECT (nope) ON cat.public.docs TO r;",
    );
    assert!(
        matches!(&refused, Err(Error::ColumnNotFoundForGrant { column_name, .. })
            if column_name == "nope"),
        "got {refused:?}"
    );
}
