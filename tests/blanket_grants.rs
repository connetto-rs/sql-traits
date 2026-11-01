//! A relation grant reaches the relations it named or swept when it ran, one
//! access list per relation as PostgreSQL keeps them. `ON ALL TABLES IN
//! SCHEMA` and `ON ALL SEQUENCES IN SCHEMA` reach what their schemas held at
//! that moment, and a `DROP` or `REVOKE` on one relation leaves the others a
//! statement named as they stood.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/table_lookup.rs"));

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn can_select(database: &ParserDB, name: &str, role: &str) -> bool {
    table(database, None, name)
        .can_select(database.role(role).expect("the role exists"), database)
        .expect("the table is in the database")
}

/// The names of every relation of `kind` some grant reaches, sorted.
fn granted(
    database: &ParserDB,
    kind: fn(&Relation<'_, ParserDB>) -> Option<String>,
) -> Vec<String> {
    let mut names: Vec<String> = database
        .table_grants()
        .flat_map(|grant| grant.relations(database))
        .filter_map(|relation| kind(&relation))
        .collect();
    names.sort_unstable();
    names
}

fn sequence_name(relation: &Relation<'_, ParserDB>) -> Option<String> {
    match relation {
        Relation::Sequence(sequence) => Some(sequence.sequence_name().to_owned()),
        _ => None,
    }
}

fn relation_name(relation: &Relation<'_, ParserDB>) -> Option<String> {
    match relation {
        Relation::Table(table) => Some(table.table_name().to_owned()),
        Relation::View(view) => Some(view.view_name().to_owned()),
        Relation::MaterializedView(view) => Some(view.view_name().to_owned()),
        Relation::Sequence(_) => None,
    }
}

#[test]
fn a_blanket_table_grant_leaves_a_later_table_out() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO r1;
         CREATE TABLE late (id int);",
    );

    assert!(can_select(&database, "a", "r1"));
    assert!(!can_select(&database, "late", "r1"));
    let late = table(&database, None, "late");
    assert!(database.table_grants().all(|grant| !grant.applies_to_table(late, &database)));
    assert_eq!(granted(&database, relation_name), ["a"]);
}

#[test]
fn a_blanket_table_grant_reaches_the_views_existing_then() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         CREATE VIEW v AS SELECT id FROM a;
         CREATE MATERIALIZED VIEW m AS SELECT id FROM a;
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO r1;
         CREATE VIEW late_v AS SELECT id FROM a;",
    );

    assert_eq!(granted(&database, relation_name), ["a", "m", "v"]);
}

#[test]
fn a_blanket_sequence_grant_leaves_a_later_sequence_out() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE SEQUENCE s1;
         GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO r1;
         CREATE SEQUENCE s2;",
    );

    assert_eq!(granted(&database, sequence_name), ["s1"]);
}

#[test]
fn a_blanket_grant_on_an_empty_schema_grants_nothing() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE ROLE r1;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO r1;
         CREATE TABLE s.late (id int);",
    );

    assert_eq!(database.table_grants().count(), 0);
}

/// PostgreSQL takes from every relation existing when the `REVOKE` runs,
/// whichever statement granted it.
#[test]
fn a_blanket_revoke_takes_from_a_table_granted_by_name() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO r1;
         CREATE TABLE late (id int);
         GRANT SELECT ON late TO r1;
         REVOKE SELECT ON ALL TABLES IN SCHEMA public FROM r1;",
    );

    assert!(!can_select(&database, "a", "r1"));
    assert!(!can_select(&database, "late", "r1"));
}

#[test]
fn a_revoke_on_one_table_keeps_a_blanket_grant_on_the_others() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         CREATE TABLE b (id int);
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO r1;
         REVOKE SELECT ON b FROM r1;",
    );

    assert!(can_select(&database, "a", "r1"));
    assert!(!can_select(&database, "b", "r1"));
}

#[test]
fn dropping_one_listed_table_keeps_the_grant_on_the_other() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         CREATE TABLE b (id int);
         GRANT SELECT ON b, a TO r1;
         DROP TABLE b;",
    );

    assert!(can_select(&database, "a", "r1"));
}

#[test]
fn revoking_one_listed_table_keeps_the_grant_on_the_other() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         CREATE TABLE b (id int);
         GRANT SELECT ON b, a TO r1;
         REVOKE SELECT ON b FROM r1;",
    );

    assert!(can_select(&database, "a", "r1"));
    assert!(!can_select(&database, "b", "r1"));
}

/// PostgreSQL refuses the whole statement when one table the blanket form
/// reaches lacks a listed column.
#[test]
fn a_blanket_column_grant_needs_the_column_on_every_table() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE ROLE r1;
         CREATE TABLE b (id int);
         CREATE TABLE c (other int);
         GRANT SELECT (id) ON ALL TABLES IN SCHEMA public TO r1;",
    );

    assert!(matches!(
        result,
        Err(Error::ColumnNotFoundForGrant { column_name, table_name })
            if column_name == "id" && table_name == "c"
    ));
}

/// PostgreSQL grants a relation written twice in one statement once, under
/// any spelling that folds to the same name.
#[test]
fn a_relation_written_twice_is_granted_once() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int);
         GRANT SELECT ON a, A, public.a TO r1;",
    );

    assert_eq!(database.table_grants().count(), 1);
}

/// A bare table lives in `public`, so `public`'s blanket grant reaches it
/// even when a later search path puts a namesake first.
#[test]
fn a_blanket_grant_keeps_reaching_the_table_it_swept_under_a_later_path() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE SCHEMA s;
         CREATE TABLE a (id int);
         CREATE TABLE s.a (id int);
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO r1;
         SET search_path = s, public;",
    );
    let r1 = database.role("r1").expect("the role exists");

    assert_eq!(table(&database, None, "a").can_select(r1, &database), Ok(true));
    assert_eq!(table(&database, Some("s"), "a").can_select(r1, &database), Ok(false));
}
