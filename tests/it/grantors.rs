//! The grantor PostgreSQL records on each privilege a `GRANT` gives, as
//! `GrantLike::granted_by` answers it.
//!
//! Every expectation was measured on PostgreSQL 18.6, with the input applied
//! as the bootstrap superuser `postgres`.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{ast::Grant, dialect::PostgreSqlDialect};

const ROLES: &str = "CREATE ROLE owner_r; CREATE ROLE other_r; CREATE ROLE member_r;
     CREATE ROLE reader; CREATE ROLE boss SUPERUSER; GRANT owner_r TO member_r;
     CREATE SCHEMA app;";

/// Parses `sql` after [`ROLES`], applied as `postgres`.
fn parse(sql: &str) -> ParserDB {
    ParseOptions::default()
        .with_session_role("postgres")
        .parse::<PostgreSqlDialect>(&format!("{ROLES} {sql}"))
        .expect("schema parses")
}

/// Each grant in `grants` as its grantees and the grantor it answers.
fn grantors<'db>(
    grants: impl Iterator<Item = &'db Grant>,
    database: &'db ParserDB,
) -> Vec<(String, Option<&'db str>)> {
    grants
        .map(|grant| {
            let grantees: Vec<String> = grant.grantees(database).map(ToString::to_string).collect();
            (grantees.join(", "), grant.granted_by(database))
        })
        .collect()
}

fn table_grantors(database: &ParserDB) -> Vec<(String, Option<&str>)> {
    grantors(database.table_grants(), database)
}

fn column_grantors(database: &ParserDB) -> Vec<(String, Option<&str>)> {
    grantors(database.column_grants(), database)
}

#[test]
fn the_owner_granting_records_itself() {
    let database = parse("CREATE TABLE app.lines (id INT); GRANT SELECT ON app.lines TO reader;");
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("postgres"))]);
}

#[test]
fn no_acting_role_answers_no_grantor() {
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{ROLES} CREATE TABLE app.lines (id INT); GRANT SELECT ON app.lines TO reader;"
    ))
    .expect("schema parses");
    assert_eq!(table_grantors(&database), [("reader".to_owned(), None)]);
}

#[test]
fn a_superuser_grants_as_the_owner() {
    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT); RESET ROLE;
         GRANT SELECT ON app.t TO reader;",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);

    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT); RESET ROLE;
         SET ROLE boss; GRANT SELECT ON app.t TO reader GRANTED BY boss; RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("reader".to_owned(), Some("owner_r"))],
        "a written grantor is the acting role, not the recorded one"
    );
}

#[test]
fn a_member_of_the_owner_grants_as_the_owner_unless_it_holds_the_option() {
    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT); RESET ROLE;
         SET ROLE member_r; GRANT SELECT ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);

    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT);
         GRANT SELECT ON app.t TO member_r WITH GRANT OPTION; RESET ROLE;
         SET ROLE member_r; GRANT SELECT ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("member_r".to_owned(), Some("owner_r")), ("reader".to_owned(), Some("member_r"))]
    );
}

#[test]
fn a_grant_option_holder_records_itself_on_the_table_and_its_columns() {
    let database = parse(
        "CREATE TABLE app.t (id INT, c INT);
         GRANT UPDATE ON app.t TO other_r WITH GRANT OPTION;
         SET ROLE other_r; GRANT UPDATE ON app.t TO reader; GRANT UPDATE (c) ON app.t TO member_r;
         RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("other_r".to_owned(), Some("postgres")), ("reader".to_owned(), Some("other_r"))]
    );
    assert_eq!(column_grantors(&database), [("member_r".to_owned(), Some("other_r"))]);

    let database = parse(
        "CREATE TABLE app.t (id INT, c INT);
         GRANT SELECT (c) ON app.t TO other_r WITH GRANT OPTION;
         SET ROLE other_r; GRANT SELECT (c) ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(
        column_grantors(&database),
        [("other_r".to_owned(), Some("postgres")), ("reader".to_owned(), Some("other_r"))]
    );
}

#[test]
fn a_schema_grant_records_the_schema_owner() {
    let database =
        parse("CREATE SCHEMA owned AUTHORIZATION owner_r; GRANT USAGE ON SCHEMA owned TO reader;");
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);
}

#[test]
fn a_default_privilege_records_the_owner_it_belongs_to() {
    let database = parse(
        "ALTER DEFAULT PRIVILEGES FOR ROLE owner_r GRANT SELECT ON TABLES TO reader;
         SET ROLE owner_r; CREATE TABLE app.t (id INT); RESET ROLE;",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);
}

#[test]
fn granted_by_current_user_names_the_role_set() {
    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT);
         GRANT SELECT ON app.t TO reader GRANTED BY CURRENT_USER; RESET ROLE;",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);
}

#[test]
fn the_grantor_follows_an_owner_change_and_a_rename() {
    let database = parse(
        "CREATE VIEW app.v AS SELECT 1 AS x; GRANT SELECT ON app.v TO reader;
         ALTER VIEW app.v OWNER TO owner_r;",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);

    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT); GRANT SELECT ON app.t TO reader; RESET ROLE;
         ALTER ROLE owner_r RENAME TO \"Keeper\";",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("Keeper"))]);
}
