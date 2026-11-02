//! Tests that a name a statement builds while it runs is refused rather than
//! recorded under the name of the call that builds it.
//!
//! Some dialects let a name be produced by a call, written `IDENTIFIER('docs')`
//! where a plain `docs` would go, and what it will name is unknown until the
//! statement runs. Reading the producing call's own name recorded objects
//! called `identifier`, which nobody wrote, and those names then answered
//! lookups as though they were real.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::{dialect::SnowflakeDialect, parser::Parser};

fn build(sql: &str) -> Result<ParserDB, Error> {
    let statements = Parser::parse_sql(&SnowflakeDialect {}, sql).expect("the statements parse");
    ParserDB::from_statements(statements, "catalog".to_string())
}

/// Every refusal says the same thing: a part of the name is built when the
/// statement runs.
fn assert_refused(sql: &str) {
    match build(sql) {
        Err(Error::IdentifierLookupError(LookupError::InvalidObjectName {
            object_name,
            reason,
        })) => {
            assert!(
                reason.contains("built when the statement runs"),
                "`{sql}` was refused for another reason: {reason}"
            );
            assert!(
                object_name.contains("IDENTIFIER"),
                "`{sql}` refused without naming the part: {object_name}"
            );
        }
        Err(other) => panic!("`{sql}` was refused for another reason: {other}"),
        Ok(_) => panic!("`{sql}` was accepted"),
    }
}

/// A creation cannot record a name it will only learn at run time.
#[test]
fn a_creation_under_a_run_time_name_is_refused() {
    assert_refused("CREATE TABLE IDENTIFIER('docs') (id INT)");
    assert_refused("CREATE SCHEMA IDENTIFIER('app')");
    assert_refused("CREATE ROLE IDENTIFIER('reader')");
    assert_refused(
        "CREATE TABLE docs (id INT); CREATE VIEW IDENTIFIER('v') AS SELECT id FROM docs",
    );
}

/// A run-time qualifier is refused as unreadable, not reported as a schema
/// called after the producing call.
#[test]
fn a_run_time_qualifier_is_refused_rather_than_read_as_a_schema() {
    assert_refused("CREATE SCHEMA app; CREATE TABLE IDENTIFIER('app').docs (id INT)");
    assert_refused("CREATE TABLE app.IDENTIFIER('docs') (id INT)");
}

/// The statements that already refused keep refusing, and say why the same way.
#[test]
fn a_reference_to_a_run_time_name_stays_refused() {
    assert_refused("CREATE TABLE docs (id INT); CREATE INDEX i ON IDENTIFIER('docs') (id)");
    assert_refused("CREATE TABLE docs (id INT); DROP TABLE IDENTIFIER('docs')");
    assert_refused("CREATE TABLE docs (id INT); ALTER TABLE docs RENAME TO IDENTIFIER('papers')");
    assert_refused(
        "CREATE TABLE docs (id INT); CREATE POLICY p ON IDENTIFIER('docs') USING (true)",
    );
    assert_refused(
        "CREATE TABLE docs (id INT); CREATE ROLE r; GRANT SELECT ON IDENTIFIER('docs') TO r",
    );
}

/// A grant over a schema named at run time is refused rather than recorded as
/// a grant that quietly covers nothing.
#[test]
fn a_schema_wide_grant_over_a_run_time_name_is_refused() {
    assert_refused(
        "CREATE SCHEMA app;
         CREATE ROLE r;
         GRANT SELECT ON ALL TABLES IN SCHEMA IDENTIFIER('app') TO r",
    );
    assert_refused(
        "CREATE SCHEMA app; CREATE ROLE r; GRANT USAGE ON SCHEMA IDENTIFIER('app') TO r",
    );
    assert_refused(
        "CREATE SCHEMA app;
         CREATE ROLE r;
         GRANT SELECT ON ALL SEQUENCES IN SCHEMA IDENTIFIER('app') TO r",
    );
    assert_refused(
        "CREATE SCHEMA app;
         CREATE ROLE r;
         GRANT USAGE ON FUNCTION IDENTIFIER('app').f TO r",
    );
    assert_refused(
        "CREATE SCHEMA app;
         CREATE ROLE r;
         GRANT EXECUTE ON ROUTINE IDENTIFIER('app').f() TO r",
    );
    assert_refused(
        "CREATE SCHEMA app;
         CREATE ROLE r;
         GRANT EXECUTE ON ALL ROUTINES IN SCHEMA IDENTIFIER('app') TO r",
    );
    assert_refused(
        "CREATE SCHEMA app;
         CREATE ROLE r;
         GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA IDENTIFIER('app') TO r",
    );
}

/// Nothing about ordinary names changes, including the quoted spelling of the
/// word the dialect uses for a dynamic name.
#[test]
fn ordinary_names_are_unaffected() {
    let db = build(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE \"IDENTIFIER\" (id INT);",
    )
    .expect("plain names build");

    assert!(
        db.table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
    assert!(
        db.table_by_target(TargetName::new("IDENTIFIER", true), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
}
