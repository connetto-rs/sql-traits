//! Privileges a role uses through the roles it is a member of, which
//! PostgreSQL passes on along every membership carrying `INHERIT`.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn can_select(database: &ParserDB, table: &str, role: &str) -> bool {
    database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
        .can_select(database.role(role).expect("the role exists"), database)
        .expect("the table is in the database")
}

fn inherited_roles(database: &ParserDB, role: &str) -> Vec<String> {
    let mut names: Vec<_> = database
        .role(role)
        .expect("the role exists")
        .inherited_roles(database)
        .map(|role| role.stored_name().into_owned())
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn member_reads_what_its_group_reads_in_either_spelling() {
    for membership in
        ["CREATE ROLE app_user IN ROLE reader;", "CREATE ROLE app_user; GRANT reader TO app_user;"]
    {
        let database = parse(&format!(
            "CREATE ROLE reader;
             {membership}
             CREATE TABLE t (id INTEGER PRIMARY KEY);
             GRANT SELECT ON t TO reader;"
        ));

        assert!(can_select(&database, "t", "app_user"), "{membership}");
        assert!(can_select(&database, "t", "reader"), "{membership}");
    }
}

#[test]
fn group_does_not_read_what_its_member_reads() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE ROLE app_user IN ROLE reader;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         GRANT SELECT ON t TO app_user;",
    );

    assert!(can_select(&database, "t", "app_user"));
    assert!(!can_select(&database, "t", "reader"));
}

#[test]
fn inheritance_follows_a_chain_of_inheriting_memberships() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE ROLE staff;
         CREATE ROLE app_user;
         GRANT reader TO staff;
         GRANT staff TO app_user;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         GRANT SELECT ON t TO reader;",
    );

    assert_eq!(inherited_roles(&database, "app_user"), ["reader", "staff"]);
    assert!(can_select(&database, "t", "app_user"));
}

/// A link without `INHERIT` passes on nothing, neither its own granted role
/// nor the roles that role inherits from, whether the option is written on
/// the grant or taken from the member's `NOINHERIT`.
#[test]
fn a_link_without_inherit_stops_the_chain() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE ROLE staff;
         CREATE ROLE app_user;
         CREATE ROLE loner NOINHERIT;
         GRANT reader TO staff WITH INHERIT FALSE;
         GRANT staff TO app_user;
         GRANT reader TO loner;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         GRANT SELECT ON t TO reader;",
    );

    assert_eq!(inherited_roles(&database, "staff"), Vec::<String>::new());
    assert_eq!(inherited_roles(&database, "app_user"), ["staff"]);
    assert!(!can_select(&database, "t", "staff"));
    assert!(!can_select(&database, "t", "app_user"));
    assert!(!can_select(&database, "t", "loner"));
}

#[test]
fn a_membership_cycle_ends_the_walk() {
    let database = parse(
        "CREATE ROLE a;
         CREATE ROLE b IN ROLE a;
         CREATE ROLE c IN ROLE b;
         GRANT c TO a;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         GRANT SELECT ON t TO b;",
    );

    assert_eq!(inherited_roles(&database, "a"), ["b", "c"]);
    assert_eq!(inherited_roles(&database, "c"), ["a", "b"]);
    assert!(can_select(&database, "t", "a"));
}

#[test]
fn every_table_privilege_is_inherited() {
    let database = parse(
        "CREATE ROLE owner_group;
         CREATE ROLE app_user IN ROLE owner_group;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         GRANT INSERT, UPDATE, DELETE, TRUNCATE ON t TO owner_group;",
    );
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let app_user = database.role("app_user").expect("the role exists");

    assert_eq!(table.can_insert(app_user, &database), Ok(true));
    assert_eq!(table.can_update(app_user, &database), Ok(true));
    assert_eq!(table.can_delete(app_user, &database), Ok(true));
    assert_eq!(table.can_truncate(app_user, &database), Ok(true));
    assert_eq!(table.can_write(app_user, &database), Ok(true));
    assert_eq!(table.can_select(app_user, &database), Ok(false));
}

#[test]
fn member_executes_and_grants_through_its_group() {
    let database = parse(
        "CREATE ROLE runner;
         CREATE ROLE granter;
         CREATE ROLE owners;
         CREATE ROLE app_user IN ROLE runner, granter;
         CREATE ROLE maintainer IN ROLE owners;
         CREATE ROLE outsider;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO owners;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION f() TO runner;
         GRANT EXECUTE ON FUNCTION f() TO granter WITH GRANT OPTION;",
    );
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the function exists");
    let role = |name| database.role(name).expect("the role exists");

    assert_eq!(function.can_execute(role("app_user"), &database), Ok(true));
    assert_eq!(function.can_grant_execute(role("app_user"), &database), Ok(true));
    assert_eq!(function.can_execute(role("maintainer"), &database), Ok(true));
    assert_eq!(function.can_grant_execute(role("maintainer"), &database), Ok(true));
    assert_eq!(function.can_execute(role("outsider"), &database), Ok(false));
}
