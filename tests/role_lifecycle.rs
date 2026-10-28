//! Role lifecycle references follow PostgreSQL identity changes.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

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

/// `CREATE GROUP` is PostgreSQL's older spelling of `CREATE ROLE`, so the name
/// it takes is a role name like any other.
#[test]
fn a_group_takes_a_role_name() {
    let duplicate = parse("CREATE GROUP readers; CREATE ROLE readers;");
    assert!(
        matches!(&duplicate, Err(Error::RoleAlreadyExists { role_name }) if role_name == "readers"),
        "got {duplicate:?}"
    );
}

fn memberships(database: &ParserDB, role: &str) -> Vec<String> {
    database
        .role(role)
        .expect("role resolves")
        .member_of(database)
        .map(|parent| parent.stored_name().into_owned())
        .collect()
}

#[test]
fn a_granted_membership_is_recorded_once_and_revoked() {
    let granted = parse(
        "CREATE ROLE parent_role;
         CREATE ROLE child_role;
         GRANT parent_role TO child_role;
         GRANT parent_role TO child_role WITH ADMIN OPTION;",
    )
    .expect("schema builds");
    assert_eq!(memberships(&granted, "child_role"), ["parent_role"]);

    let revoked = parse(
        "CREATE ROLE parent_role;
         CREATE ROLE child_role IN ROLE parent_role;
         REVOKE parent_role FROM child_role;",
    )
    .expect("schema builds");
    assert_eq!(memberships(&revoked, "child_role"), Vec::<String>::new());

    // Revoking an option leaves the membership itself in place.
    let option_revoked = parse(
        "CREATE ROLE parent_role;
         CREATE ROLE child_role;
         GRANT parent_role TO child_role WITH ADMIN OPTION;
         REVOKE ADMIN OPTION FOR parent_role FROM child_role;",
    )
    .expect("schema builds");
    assert_eq!(memberships(&option_revoked, "child_role"), ["parent_role"]);
}

#[test]
fn a_granted_membership_follows_renames_and_drops() {
    let renamed = parse(
        "CREATE ROLE parent_role;
         CREATE ROLE child_role;
         GRANT parent_role TO child_role;
         ALTER ROLE parent_role RENAME TO elder_role;",
    )
    .expect("schema builds");
    assert_eq!(memberships(&renamed, "child_role"), ["elder_role"]);

    let dropped = parse(
        "CREATE ROLE parent_role;
         CREATE ROLE child_role;
         GRANT parent_role TO child_role;
         DROP ROLE parent_role;
         CREATE ROLE parent_role;",
    )
    .expect("schema builds");
    assert_eq!(memberships(&dropped, "child_role"), Vec::<String>::new());
}

/// PostgreSQL refuses a membership naming a role it does not hold, `PUBLIC`
/// included, and one that would make a role a member of itself.
#[test]
fn a_membership_the_server_refuses_is_refused() {
    for (statement, absent) in [
        ("GRANT ghost TO child_role;", "ghost"),
        ("GRANT parent_role TO ghost;", "ghost"),
        ("REVOKE ghost FROM child_role;", "ghost"),
        ("GRANT parent_role TO PUBLIC;", "public"),
    ] {
        let result = parse(&format!(
            "CREATE ROLE parent_role;
             CREATE ROLE child_role;
             {statement}"
        ));
        assert!(
            matches!(&result, Err(Error::RoleNotFoundForGrant { role_name }) if role_name == absent),
            "{statement} gave {result:?}"
        );
    }

    for statement in [
        "GRANT child_role TO child_role;",
        "GRANT child_role TO parent_role;",
        "GRANT grandchild_role TO parent_role;",
    ] {
        let result = parse(&format!(
            "CREATE ROLE parent_role;
             CREATE ROLE child_role IN ROLE parent_role;
             CREATE ROLE grandchild_role IN ROLE child_role;
             {statement}"
        ));
        assert!(
            matches!(&result, Err(Error::CircularRoleMembership { .. })),
            "{statement} gave {result:?}"
        );
    }
}

/// A dump omits role creation, so the open world records a membership whose
/// roles the input never creates and reports none of them as members.
#[test]
fn the_open_world_accepts_a_membership_naming_absent_roles() {
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<sqlparser::dialect::PostgreSqlDialect>(
            "CREATE ROLE child_role;
             GRANT ghost TO child_role;
             GRANT child_role TO phantom;
             REVOKE child_role FROM phantom;",
        )
        .expect("schema builds");
    assert_eq!(memberships(&database, "child_role"), Vec::<String>::new());
}

/// Two paths up to one role are no cycle, so a grant below them goes through.
#[test]
fn a_diamond_of_memberships_is_not_a_cycle() {
    let database = parse(
        "CREATE ROLE top_role;
         CREATE ROLE left_role IN ROLE top_role;
         CREATE ROLE right_role IN ROLE top_role;
         CREATE ROLE bottom_role IN ROLE left_role, right_role;
         CREATE ROLE other_role;
         GRANT bottom_role TO other_role;",
    )
    .expect("schema builds");
    assert_eq!(memberships(&database, "other_role"), ["bottom_role"]);
}
