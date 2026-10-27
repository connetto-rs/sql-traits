//! Role lifecycle references follow PostgreSQL identity changes.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::Password, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

#[test]
fn rename_repoints_every_modeled_role_reference() {
    let database = parse(
        "CREATE ROLE App_Owner BYPASSRLS;
         CREATE ROLE Child_Role IN ROLE App_Owner;
         CREATE TABLE docs (id INT);
         ALTER TABLE docs OWNER TO App_Owner;
         CREATE SCHEMA created_schema AUTHORIZATION App_Owner;
         CREATE SCHEMA altered_schema;
         ALTER SCHEMA altered_schema OWNER TO App_Owner;
         CREATE POLICY docs_read ON docs TO App_Owner USING (true);
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO App_Owner;
         GRANT SELECT ON docs TO App_Owner;
         ALTER ROLE App_Owner RENAME TO \"Renamed_Owner\";",
    )
    .expect("schema builds");

    let renamed = database.role("Renamed_Owner").expect("renamed role resolves");
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");
    let created_schema = database.schema("created_schema").expect("created_schema exists");
    let altered_schema = database.schema("altered_schema").expect("altered_schema exists");
    let child = database.role("child_role").expect("child resolves");
    let memberships: Vec<_> = child.member_of(&database).map(RoleLike::stored_name).collect();
    let policies: Vec<_> = renamed.policies(&database).map(PolicyLike::name).collect();

    assert!(database.role("app_owner").is_none());
    assert_eq!(docs.owner(&database), Ok(Some("Renamed_Owner")));
    assert_eq!(function.owner(&database), Ok(Some("Renamed_Owner")));
    assert_eq!(created_schema.authorization(), Some("Renamed_Owner"));
    assert_eq!(altered_schema.authorization(), Some("Renamed_Owner"));
    assert_eq!(memberships, ["Renamed_Owner"]);
    assert_eq!(policies, ["docs_read"]);
    assert_eq!(docs.can_select(renamed, &database), Ok(true));
}

/// A rename repoints only the grantees naming the renamed role.
#[test]
fn rename_leaves_the_grants_of_other_roles_alone() {
    let database = parse(
        "CREATE ROLE before;
         CREATE ROLE other;
         CREATE ROLE bystander;
         CREATE TABLE docs (id INT);
         CREATE TABLE notes (id INT);
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         GRANT SELECT ON docs TO before, other;
         GRANT SELECT ON notes TO bystander;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION f() TO before, other;
         ALTER ROLE before RENAME TO after;",
    )
    .expect("schema builds");

    let role = |name: &str| database.role(name).expect("role resolves");
    let table = |name: &str| {
        database
            .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
    };
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(table("docs").can_select(role("after"), &database), Ok(true));
    assert_eq!(table("docs").can_select(role("other"), &database), Ok(true));
    assert_eq!(table("notes").can_select(role("bystander"), &database), Ok(true));
    assert_eq!(table("notes").can_select(role("after"), &database), Ok(false));
    assert_eq!(function.can_execute(role("after"), &database), Ok(true));
    assert_eq!(function.can_execute(role("other"), &database), Ok(true));
    assert_eq!(function.can_execute(role("bystander"), &database), Ok(false));
}

#[test]
fn drop_refuses_every_blocking_role_dependency() {
    let cases = [
        (
            "table owner",
            "CREATE ROLE app_owner;
             CREATE TABLE docs (id INT);
             ALTER TABLE docs OWNER TO app_owner;
             DROP ROLE app_owner;",
        ),
        (
            "function owner",
            "CREATE ROLE app_owner;
             CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
             ALTER FUNCTION f() OWNER TO app_owner;
             DROP ROLE app_owner;",
        ),
        (
            "schema owner",
            "CREATE ROLE app_owner;
             CREATE SCHEMA app AUTHORIZATION app_owner;
             DROP ROLE app_owner;",
        ),
        (
            "policy target",
            "CREATE ROLE app_owner;
             CREATE TABLE docs (id INT);
             CREATE POLICY docs_read ON docs TO app_owner USING (true);
             DROP ROLE app_owner;",
        ),
        (
            "grant target",
            "CREATE ROLE app_owner;
             CREATE TABLE docs (id INT);
             GRANT SELECT ON docs TO app_owner;
             DROP ROLE app_owner;",
        ),
    ];

    for (dependency, sql) in cases {
        let error = parse(sql).expect_err("referenced role is not dropped");
        assert!(
            matches!(&error, Error::RoleReferenced { role_name } if role_name == "app_owner"),
            "{dependency} produced {error:?}"
        );
    }
}

#[test]
fn drop_removes_membership_before_role_recreation() {
    let database = parse(
        "CREATE ROLE parent_role;
         CREATE ROLE child_role IN ROLE parent_role;
         DROP ROLE parent_role;
         CREATE ROLE parent_role;",
    )
    .expect("schema builds");
    let child = database.role("child_role").expect("child resolves");

    assert_eq!(child.member_of(&database).count(), 0);
}

/// Every attribute `RoleLike` reports, in trait order, for the role `a`.
fn attributes(sql: &str) -> (bool, bool, bool, bool, bool, bool, bool, Option<i32>) {
    let database = parse(sql).expect("schema builds");
    let role = database.role("a").expect("a resolves");
    (
        role.is_superuser(),
        role.can_create_db(),
        role.can_create_role(),
        role.inherits(),
        role.can_login(),
        role.can_bypass_rls(),
        role.is_replication(),
        role.connection_limit(),
    )
}

#[test]
fn alter_role_grants_every_attribute() {
    let granted = (true, true, true, true, true, true, true, Some(3));
    let options =
        "SUPERUSER CREATEDB CREATEROLE INHERIT LOGIN BYPASSRLS REPLICATION CONNECTION LIMIT 3";

    assert_eq!(attributes(&format!("CREATE ROLE a NOINHERIT; ALTER ROLE a {options};")), granted);
    assert_eq!(
        attributes(&format!("CREATE ROLE a NOINHERIT; ALTER ROLE a WITH {options};")),
        granted
    );
}

#[test]
fn alter_role_revokes_every_attribute() {
    assert_eq!(
        attributes(
            "CREATE ROLE a SUPERUSER CREATEDB CREATEROLE LOGIN BYPASSRLS REPLICATION CONNECTION LIMIT 3;
             ALTER ROLE a NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOLOGIN NOBYPASSRLS NOREPLICATION CONNECTION LIMIT 0;",
        ),
        (false, false, false, false, false, false, false, Some(0)),
    );
}

#[test]
fn alter_role_keeps_the_attributes_it_does_not_name() {
    assert_eq!(
        attributes("CREATE ROLE a SUPERUSER LOGIN CONNECTION LIMIT 3; ALTER ROLE a BYPASSRLS;"),
        (true, false, false, true, true, true, false, Some(3)),
    );
}

#[test]
fn later_alter_role_statements_override_earlier_ones() {
    assert_eq!(
        attributes("CREATE ROLE a; ALTER ROLE a BYPASSRLS; ALTER ROLE a NOBYPASSRLS SUPERUSER;"),
        (true, false, false, true, false, false, false, None),
    );
}

/// `CREATE ROLE a, b` stores both roles in one node, and altering one of them
/// leaves the other as it was created.
#[test]
fn alter_role_changes_only_the_role_it_names() {
    let database = parse("CREATE ROLE a, b LOGIN; ALTER ROLE b BYPASSRLS;").expect("schema builds");
    let a = database.role("a").expect("a resolves");
    let b = database.role("b").expect("b resolves");

    assert!(!a.can_bypass_rls());
    assert!(a.can_login());
    assert!(b.can_bypass_rls());
    assert!(b.can_login());
}

#[test]
fn a_rolled_back_alter_role_leaves_the_role_as_created() {
    assert_eq!(
        attributes("CREATE ROLE a; BEGIN; ALTER ROLE a SUPERUSER BYPASSRLS; ROLLBACK;"),
        (false, false, false, true, false, false, false, None),
    );
}

/// `RoleLike` reports no credentials, so a consumer reads them off the stored
/// `CREATE ROLE` node.
#[test]
fn alter_role_replaces_the_stored_credentials() {
    let database = parse(
        "CREATE ROLE a PASSWORD 'old' VALID UNTIL '2020-01-01';
         ALTER ROLE a PASSWORD NULL VALID UNTIL '2030-01-01';",
    )
    .expect("schema builds");
    let role = database.role("a").expect("a resolves");

    assert_eq!(role.password, Some(Password::NullPassword));
    assert_eq!(role.valid_until.as_ref().map(ToString::to_string).as_deref(), Some("'2030-01-01'"));
}
