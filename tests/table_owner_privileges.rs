//! A table's owner holds every table privilege through an entry of its own,
//! which it can revoke and grant back, and an owner change hands that entry
//! and every grant naming the old owner on the table to the new owner, as
//! PostgreSQL's `aclnewowner` does.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/table_lookup.rs"));

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn can_select(database: &ParserDB, schema: Option<&str>, name: &str, role: &str) -> bool {
    table(database, schema, name)
        .can_select(database.role(role).expect("the role exists"), database)
        .expect("the table is in the database")
}

#[test]
fn the_owner_and_its_inheriting_members_hold_every_privilege() {
    let database = parse(
        "CREATE ROLE owners;
         CREATE ROLE maintainer IN ROLE owners;
         CREATE ROLE detached;
         GRANT owners TO detached WITH INHERIT FALSE;
         CREATE ROLE outsider;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO owners;",
    );
    let t = table(&database, None, "t");

    for role in ["owners", "maintainer"] {
        let role = database.role(role).expect("the role exists");
        assert_eq!(t.can_select(role, &database), Ok(true));
        assert_eq!(t.can_insert(role, &database), Ok(true));
        assert_eq!(t.can_update(role, &database), Ok(true));
        assert_eq!(t.can_delete(role, &database), Ok(true));
        assert_eq!(t.can_truncate(role, &database), Ok(true));
    }
    assert!(!can_select(&database, None, "t", "detached"));
    assert!(!can_select(&database, None, "t", "outsider"));
}

/// Without `ALTER TABLE ... OWNER TO` the owner is whoever ran `CREATE
/// TABLE`, which the schema never says.
#[test]
fn a_table_without_a_recorded_owner_grants_nothing_through_ownership() {
    let database = parse(
        "CREATE ROLE postgres;
         CREATE TABLE t (id INTEGER PRIMARY KEY);",
    );

    assert_eq!(table(&database, None, "t").owner_privileges(&database).map(Iterator::count), Ok(7));
    assert!(!can_select(&database, None, "t", "postgres"));
}

#[test]
fn the_owner_can_revoke_its_own_privileges_and_grant_them_back() {
    let revoked = parse(
        "CREATE ROLE owners;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO owners;
         REVOKE SELECT ON t FROM owners;",
    );
    let owners = revoked.role("owners").expect("the role exists");
    let t = table(&revoked, None, "t");
    assert_eq!(t.can_select(owners, &revoked), Ok(false));
    assert_eq!(t.can_insert(owners, &revoked), Ok(true));

    let restored = parse(
        "CREATE ROLE owners;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO owners;
         REVOKE SELECT ON t FROM owners;
         GRANT SELECT ON t TO owners;",
    );
    assert!(can_select(&restored, None, "t", "owners"));
}

#[test]
fn only_a_whole_table_revoke_of_the_privilege_itself_takes_it() {
    let database = parse(
        "CREATE ROLE owners;
         CREATE TABLE emptied (id INTEGER PRIMARY KEY);
         CREATE TABLE option_only (id INTEGER PRIMARY KEY);
         CREATE TABLE one_column (id INTEGER PRIMARY KEY);
         ALTER TABLE emptied OWNER TO owners;
         ALTER TABLE option_only OWNER TO owners;
         ALTER TABLE one_column OWNER TO owners;
         REVOKE ALL PRIVILEGES ON emptied FROM owners;
         REVOKE GRANT OPTION FOR SELECT ON option_only FROM owners;
         REVOKE SELECT (id) ON one_column FROM owners;",
    );

    assert_eq!(
        table(&database, None, "emptied").owner_privileges(&database).map(Iterator::count),
        Ok(0)
    );
    assert!(!can_select(&database, None, "emptied", "owners"));
    assert!(can_select(&database, None, "option_only", "owners"));
    assert!(can_select(&database, None, "one_column", "owners"));
}

/// `ALL TABLES IN SCHEMA` reaches the tables the schema holds when the revoke
/// runs, and only the owner's entry on those the role owns.
#[test]
fn a_schema_wide_revoke_takes_the_entry_of_the_tables_then_owned() {
    let database = parse(
        "CREATE ROLE owners;
         CREATE ROLE others;
         CREATE SCHEMA app;
         CREATE TABLE app.owned (id INTEGER PRIMARY KEY);
         CREATE TABLE app.foreign_owned (id INTEGER PRIMARY KEY);
         ALTER TABLE app.owned OWNER TO owners;
         ALTER TABLE app.foreign_owned OWNER TO others;
         REVOKE SELECT ON ALL TABLES IN SCHEMA app FROM owners;
         CREATE TABLE app.later (id INTEGER PRIMARY KEY);
         ALTER TABLE app.later OWNER TO owners;",
    );

    assert!(!can_select(&database, Some("app"), "owned", "owners"));
    assert!(can_select(&database, Some("app"), "foreign_owned", "others"));
    assert!(can_select(&database, Some("app"), "later", "owners"));
}

#[test]
fn an_owner_change_hands_the_entry_over_as_it_stands() {
    let database = parse(
        "CREATE ROLE previous;
         CREATE ROLE next;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO previous;
         REVOKE TRUNCATE ON t FROM previous;
         ALTER TABLE t OWNER TO next;",
    );
    let t = table(&database, None, "t");
    let previous = database.role("previous").expect("the role exists");
    let next = database.role("next").expect("the role exists");

    assert_eq!(t.can_select(previous, &database), Ok(false));
    assert_eq!(t.can_select(next, &database), Ok(true));
    assert_eq!(t.can_truncate(next, &database), Ok(false));
}

/// A grant naming the old owner, as grantee or as grantor, moves to the new
/// owner on the table that changed hands and stays on every other table.
#[test]
fn an_owner_change_moves_the_grants_naming_the_old_owner_on_that_table() {
    let database = parse(
        "CREATE ROLE previous;
         CREATE ROLE next;
         CREATE ROLE reader;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE TABLE u (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO previous;
         REVOKE SELECT ON t FROM previous;
         GRANT SELECT ON t, u TO previous;
         GRANT SELECT ON t TO reader GRANTED BY previous;
         ALTER TABLE t OWNER TO next;",
    );

    assert!(can_select(&database, None, "t", "next"));
    assert!(!can_select(&database, None, "t", "previous"));
    assert!(can_select(&database, None, "u", "previous"));
    assert!(can_select(&database, None, "t", "reader"));

    let t = table(&database, None, "t");
    let grantors: Vec<_> = t
        .grants(&database)
        .expect("the table is in the database")
        .filter(|grant| grant.applies_to_role(database.role("reader").expect("the role exists")))
        .map(|grant| grant.granted_by(&database).map(RoleLike::name))
        .collect();
    assert_eq!(grantors, [Some("next")]);
}

/// A schema-wide grant naming the old owner stops reaching the table that
/// changed hands for that role, and keeps reaching it for everyone else.
#[test]
fn an_owner_change_carves_the_table_out_of_a_schema_wide_grant() {
    let database = parse(
        "CREATE ROLE previous;
         CREATE ROLE next;
         CREATE ROLE reader;
         CREATE SCHEMA app;
         CREATE TABLE app.t (id INTEGER PRIMARY KEY);
         CREATE TABLE app.u (id INTEGER PRIMARY KEY);
         ALTER TABLE app.t OWNER TO previous;
         REVOKE ALL ON app.t FROM previous;
         GRANT SELECT ON ALL TABLES IN SCHEMA app TO previous, reader;
         ALTER TABLE app.t OWNER TO next;",
    );

    assert!(!can_select(&database, Some("app"), "t", "previous"));
    assert!(can_select(&database, Some("app"), "u", "previous"));
    assert!(can_select(&database, Some("app"), "t", "next"));
    assert!(can_select(&database, Some("app"), "t", "reader"));
    assert!(can_select(&database, Some("app"), "u", "reader"));
}

/// `CURRENT_USER` names a role the schema never spells, so the old owner
/// loses the table and no declared role gains it.
#[test]
fn an_owner_change_to_the_session_takes_the_table_from_the_old_owner() {
    let database = parse(
        "CREATE ROLE previous;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO previous;
         GRANT SELECT ON t TO previous;
         ALTER TABLE t OWNER TO CURRENT_USER;",
    );

    assert!(!can_select(&database, None, "t", "previous"));
}

#[test]
fn the_entry_survives_table_rewrites_role_renames_and_rollbacks() {
    let database = parse(
        "CREATE ROLE owners;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         ALTER TABLE t OWNER TO owners;
         REVOKE SELECT ON t FROM owners;
         ALTER TABLE t ADD COLUMN label TEXT;
         ALTER ROLE owners RENAME TO keepers;
         BEGIN;
         REVOKE INSERT ON t FROM keepers;
         ROLLBACK;",
    );
    let t = table(&database, None, "t");
    let keepers = database.role("keepers").expect("the role exists");

    assert_eq!(t.can_select(keepers, &database), Ok(false));
    assert_eq!(t.can_insert(keepers, &database), Ok(true));
}
