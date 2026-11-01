//! A grant naming columns reaches those columns only, so it answers the
//! column-level privilege question and never the table-level one, as
//! PostgreSQL's `has_table_privilege` keeps the two apart.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{ast::Action, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn table<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
}

/// The table-level privileges `role` holds on `a`, in the order SELECT,
/// INSERT, UPDATE.
fn table_privileges(database: &ParserDB, role: &str) -> [bool; 3] {
    let a = table(database, "a");
    let role = database.role(role).expect("the role exists");
    [
        a.can_select(role, database).expect("the table is in the database"),
        a.can_insert(role, database).expect("the table is in the database"),
        a.can_update(role, database).expect("the table is in the database"),
    ]
}

fn rendered(grants: impl Iterator<Item = impl core::fmt::Display>) -> Vec<String> {
    grants.map(|grant| grant.to_string()).collect()
}

#[test]
fn a_column_grant_holds_no_table_privilege() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT (id), INSERT (b), UPDATE (b) ON a TO r1;",
    );

    assert_eq!(table_privileges(&database, "r1"), [false, false, false]);
}

#[test]
fn a_table_grant_beside_a_column_grant_holds_only_its_own_privilege() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b) ON a TO r1;",
    );

    assert_eq!(table_privileges(&database, "r1"), [true, false, false]);
}

#[test]
fn a_column_grant_reaches_no_member_as_a_table_privilege() {
    let database = parse(
        "CREATE ROLE grp;
         CREATE ROLE member IN ROLE grp;
         CREATE TABLE a (id int, b text);
         GRANT UPDATE (b) ON a TO grp;
         GRANT INSERT (id) ON a TO PUBLIC;",
    );

    assert_eq!(table_privileges(&database, "member"), [false, false, false]);
}

#[test]
fn a_mixed_grant_splits_between_the_table_and_column_stores() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b), DELETE ON a TO r1;",
    );

    assert_eq!(rendered(database.table_grants()), ["GRANT SELECT, DELETE ON a TO r1"]);
    assert_eq!(rendered(database.column_grants()), ["GRANT UPDATE (b) ON a TO r1"]);
    assert_eq!(
        rendered(table(&database, "a").grants(&database).expect("held")),
        ["GRANT SELECT, DELETE ON a TO r1"]
    );
}

#[test]
fn a_whole_relation_grant_stays_out_of_the_column_store() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         CREATE TABLE s.t (id int);
         CREATE SEQUENCE q;
         GRANT ALL PRIVILEGES ON a TO r1;
         GRANT USAGE ON SEQUENCE q TO r1;
         GRANT CREATE ON SCHEMA s TO r1;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO r1;",
    );

    assert_eq!(database.table_grants().count(), 4);
    assert!(!database.has_column_grants());
}

#[test]
fn a_column_grant_stays_out_of_the_table_store() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT REFERENCES (id), SELECT (b) ON a TO r1;",
    );

    assert!(!database.has_table_grants());
    let grant = database.column_grants().next().expect("the column grant");
    let columns: Vec<_> = grant
        .columns(table(&database, "a"), &database)
        .expect("held")
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(columns, ["id", "b"]);
}

#[test]
fn revoking_one_part_of_a_mixed_grant_leaves_the_other() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b) ON a TO r1;
         REVOKE SELECT ON a FROM r1;",
    );
    assert_eq!(table_privileges(&database, "r1"), [false, false, false]);
    assert_eq!(rendered(database.column_grants()), ["GRANT UPDATE (b) ON a TO r1"]);

    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b) ON a TO r1;
         REVOKE UPDATE (b) ON a FROM r1;",
    );
    assert_eq!(table_privileges(&database, "r1"), [true, false, false]);
    assert!(!database.has_column_grants());
}

#[test]
fn a_table_revoke_takes_the_column_grants_of_that_privilege() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT (id), UPDATE (b) ON a TO r1;
         REVOKE UPDATE ON a FROM r1;",
    );

    let remaining: Vec<_> = database
        .column_grants()
        .flat_map(|grant| grant.privileges(&database))
        .map(|action| matches!(action, Action::Select { .. }))
        .collect();
    assert_eq!(remaining, [true]);
}
