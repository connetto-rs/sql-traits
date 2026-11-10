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

    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT);
         GRANT SELECT ON app.t TO other_r WITH GRANT OPTION; RESET ROLE;
         SET ROLE member_r; GRANT SELECT ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("other_r".to_owned(), Some("owner_r")), ("reader".to_owned(), Some("owner_r"))],
        "another role's option leaves the member granting as the owner"
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
fn a_session_principal_in_granted_by_names_the_role_it_stands_for() {
    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT);
         GRANT SELECT ON app.t TO reader GRANTED BY CURRENT_USER; RESET ROLE;",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("owner_r"))]);

    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT); RESET ROLE;
         GRANT SELECT ON app.t TO reader GRANTED BY SESSION_USER;",
    );
    assert_eq!(
        table_grantors(&database),
        [("reader".to_owned(), Some("owner_r"))],
        "the session role is the superuser postgres, which grants as the owner"
    );
}

#[test]
fn all_privileges_are_granted_through_an_option_on_all_of_them() {
    let database = parse(
        "CREATE TABLE app.t (id INT);
         GRANT ALL ON app.t TO other_r WITH GRANT OPTION;
         SET ROLE other_r; GRANT ALL ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("other_r".to_owned(), Some("postgres")), ("reader".to_owned(), Some("other_r"))]
    );
}

#[test]
fn a_role_holding_some_of_the_options_grants_as_itself() {
    let database = parse(
        "CREATE TABLE app.t (id INT, c INT);
         GRANT SELECT (c) ON app.t TO other_r WITH GRANT OPTION;
         GRANT UPDATE ON app.t TO other_r WITH GRANT OPTION;
         SET ROLE other_r; GRANT SELECT, UPDATE ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("other_r".to_owned(), Some("postgres")), ("reader".to_owned(), Some("other_r"))],
        "an option on one column does not cover the whole table"
    );
}

#[test]
fn an_option_reached_along_two_memberships_names_its_holder() {
    let database = parse(
        "CREATE ROLE team_a; CREATE ROLE team_b; CREATE ROLE holders;
         GRANT team_a, team_b TO member_r; GRANT holders TO team_a, team_b;
         CREATE TABLE app.t (id INT);
         GRANT SELECT ON app.t TO holders WITH GRANT OPTION;
         SET ROLE member_r; GRANT SELECT ON app.t TO reader; RESET ROLE;",
    );
    assert_eq!(
        table_grantors(&database),
        [("holders".to_owned(), Some("postgres")), ("reader".to_owned(), Some("holders"))]
    );
}

#[test]
fn a_sequence_grant_records_its_authorization_owner() {
    let database = parse(
        "SET ROLE owner_r; CREATE SEQUENCE app.s; RESET ROLE;
         GRANT USAGE ON SEQUENCE app.s TO reader;",
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
        "CREATE VIEW app.v AS SELECT 1 AS x; GRANT SELECT ON app.v TO reader;
         ALTER VIEW app.v OWNER TO postgres;",
    );
    assert_eq!(
        table_grantors(&database),
        [("reader".to_owned(), Some("postgres"))],
        "handing a view to its owner changes nothing"
    );

    let database = parse(
        "CREATE MATERIALIZED VIEW app.m AS SELECT 1 AS x; GRANT SELECT ON app.m TO reader;
         ALTER MATERIALIZED VIEW app.m OWNER TO owner_r; GRANT SELECT ON app.m TO other_r;",
    );
    assert_eq!(
        table_grantors(&database),
        [("reader".to_owned(), Some("owner_r")), ("other_r".to_owned(), Some("owner_r"))]
    );

    let database = parse(
        "SET ROLE owner_r; CREATE TABLE app.t (id INT); GRANT SELECT ON app.t TO reader; RESET ROLE;
         ALTER ROLE owner_r RENAME TO \"Keeper\";",
    );
    assert_eq!(table_grantors(&database), [("reader".to_owned(), Some("Keeper"))]);
}

#[test]
fn sequence_grantors_follow_session_roles_and_grant_options() {
    let database = parse(
        "SET ROLE owner_r; CREATE SEQUENCE app.s;
         GRANT USAGE, SELECT ON SEQUENCE app.s TO other_r WITH GRANT OPTION;
         RESET ROLE; SET ROLE other_r;
         GRANT USAGE ON SEQUENCE app.s TO reader;
         GRANT SELECT ON app.s TO reader;
         RESET ROLE; SET ROLE member_r;
         GRANT SELECT ON app.s TO reader;",
    );
    assert_eq!(
        table_grantors(&database),
        [
            ("other_r".to_owned(), Some("owner_r")),
            ("reader".to_owned(), Some("other_r")),
            ("reader".to_owned(), Some("other_r")),
            ("reader".to_owned(), Some("owner_r")),
        ]
    );
}

#[test]
fn a_sequence_without_a_known_creator_keeps_its_grantor_unknown() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SEQUENCE s; GRANT USAGE ON SEQUENCE s TO PUBLIC;",
    )
    .expect("schema parses");
    assert_eq!(table_grantors(&database), [("PUBLIC".to_owned(), None)]);
}

#[test]
fn sequence_authorization_is_independent_of_column_ownership() {
    let database = ParseOptions::default()
        .with_session_role("user")
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app; CREATE TABLE app.t (id INT);
             CREATE SEQUENCE app.s OWNED BY app.t.id;
             CREATE SEQUENCE app.free;
             GRANT USAGE ON SEQUENCE app.s, app.free TO PUBLIC;",
        )
        .expect("schema parses");
    assert_eq!(
        table_grantors(&database),
        [("PUBLIC".to_owned(), Some("user")), ("PUBLIC".to_owned(), Some("user"))]
    );
    let owned = database
        .sequence_by_target(
            TargetName::new("s", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("valid name")
        .expect("sequence exists");
    assert!(matches!(owned.owner(&database), Some(SequenceOwner::Column(_))));
}

#[test]
fn implicit_sequences_use_the_table_owner_when_another_role_adds_columns() {
    let database = parse(
        "SET ROLE owner_r;
         CREATE TABLE app.t (id serial, n INT GENERATED ALWAYS AS IDENTITY);
         RESET ROLE;
         ALTER TABLE app.t ADD COLUMN added serial;
         ALTER TABLE app.t ADD COLUMN also_added INT GENERATED ALWAYS AS IDENTITY;
         GRANT USAGE ON ALL SEQUENCES IN SCHEMA app TO reader;",
    );
    assert_eq!(table_grantors(&database), vec![("reader".to_owned(), Some("owner_r")); 4]);
}

#[test]
fn sequence_owners_and_grantors_follow_role_renames() {
    let database = parse(
        "SET ROLE owner_r; CREATE SEQUENCE app.s;
         GRANT USAGE ON SEQUENCE app.s TO reader; RESET ROLE;
         ALTER ROLE owner_r RENAME TO \"Keeper\";
         GRANT SELECT ON SEQUENCE app.s TO other_r;",
    );
    assert_eq!(
        table_grantors(&database),
        [("reader".to_owned(), Some("Keeper")), ("other_r".to_owned(), Some("Keeper"))]
    );
}

#[test]
fn a_sequence_blocks_dropping_its_authorization_owner_until_it_is_dropped() {
    let sql = "CREATE ROLE keeper; SET ROLE keeper; CREATE SEQUENCE s; RESET ROLE;";
    let error = ParseOptions::default()
        .with_session_role("postgres")
        .parse::<PostgreSqlDialect>(&format!("{sql} DROP ROLE keeper;"))
        .expect_err("the sequence depends on its authorization owner");
    assert!(
        matches!(error, sql_traits::errors::Error::RoleReferenced { role_name } if role_name == "keeper")
    );
    ParseOptions::default()
        .with_session_role("postgres")
        .parse::<PostgreSqlDialect>(&format!("{sql} DROP SEQUENCE s; DROP ROLE keeper;"))
        .expect("dropping the sequence releases the role");
}

#[test]
fn attached_sequences_and_their_grants_follow_relation_owner_changes() {
    let database = parse(
        "CREATE TABLE app.t (id serial, n INT GENERATED ALWAYS AS IDENTITY);
         CREATE SEQUENCE app.s OWNED BY app.t.id;
         CREATE VIEW app.v AS SELECT 1 AS id;
         CREATE SEQUENCE app.vs OWNED BY app.v.id;
         CREATE SEQUENCE app.free;
         GRANT USAGE ON ALL SEQUENCES IN SCHEMA app TO reader;
         ALTER TABLE app.t OWNER TO owner_r;
         ALTER VIEW app.v OWNER TO owner_r;
         GRANT SELECT ON SEQUENCE app.s, app.vs TO other_r;",
    );
    let mut grants: Vec<_> = database
        .table_grants()
        .map(|grant| {
            let sequence = grant
                .relations(&database)
                .find_map(|relation| {
                    match relation {
                        Relation::Sequence(sequence) => Some(sequence),
                        _ => None,
                    }
                })
                .expect("sequence target");
            (sequence.sequence_name(), grant.granted_by(&database))
        })
        .collect();
    grants.sort_unstable();
    assert_eq!(
        grants,
        [
            ("free", Some("postgres")),
            ("s", Some("owner_r")),
            ("s", Some("owner_r")),
            ("t_id_seq", Some("owner_r")),
            ("t_n_seq", Some("owner_r")),
            ("vs", Some("owner_r")),
            ("vs", Some("owner_r")),
        ]
    );
}
