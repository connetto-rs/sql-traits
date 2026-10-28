//! Role memberships, from `GRANT role TO role` and from the membership
//! clauses of `CREATE ROLE`, with the options PostgreSQL 16 gives each one.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    dialect::{MySqlDialect, PostgreSqlDialect},
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn open_world(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
}

/// The memberships `member` holds, as `(role, admin, inherit, set)`.
fn memberships_of(database: &ParserDB, member: &str) -> Vec<(String, bool, bool, bool)> {
    database
        .role(member)
        .expect("the member resolves")
        .memberships(database)
        .map(|membership| {
            (
                membership.stored_role_name().into_owned(),
                membership.admin_option(),
                membership.inherit_option(),
                membership.set_option(),
            )
        })
        .collect()
}

fn membership(role: &str, admin: bool, inherit: bool, set: bool) -> (String, bool, bool, bool) {
    (role.to_owned(), admin, inherit, set)
}

#[test]
fn granted_role_is_a_membership_of_the_grantee() {
    let database = parse(
        "CREATE ROLE bypasser BYPASSRLS;
         CREATE ROLE app_user;
         GRANT bypasser TO app_user;",
    )
    .expect("schema builds");

    let app_user = database.role("app_user").expect("app_user resolves");
    let parents: Vec<_> = app_user.member_of(&database).map(RoleLike::stored_name).collect();

    assert_eq!(parents, ["bypasser"]);
    assert!(
        database
            .role("bypasser")
            .expect("bypasser resolves")
            .memberships(&database)
            .next()
            .is_none()
    );
}

/// Measured on PostgreSQL 17.11 and 18.6 through `pg_auth_members`.
#[test]
fn unwritten_options_take_the_postgres_defaults() {
    let database = parse(
        "CREATE ROLE bypasser BYPASSRLS;
         CREATE ROLE inh INHERIT;
         CREATE ROLE noinh NOINHERIT;
         CREATE ROLE opts;
         CREATE ROLE spelled;
         GRANT bypasser TO inh;
         GRANT bypasser TO noinh;
         GRANT bypasser TO opts WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
         GRANT bypasser TO spelled WITH ADMIN OPTION, INHERIT OPTION, SET OPTION;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "inh"), [membership("bypasser", false, true, true)]);
    assert_eq!(memberships_of(&database, "noinh"), [membership("bypasser", false, false, true)]);
    assert_eq!(memberships_of(&database, "opts"), [membership("bypasser", true, false, false)]);
    assert_eq!(memberships_of(&database, "spelled"), [membership("bypasser", true, true, true)]);
}

/// The default `INHERIT` is the member's attribute when the grant runs, so a
/// later `ALTER ROLE` leaves the membership as it was granted.
#[test]
fn inherit_default_is_taken_when_the_grant_runs() {
    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child NOINHERIT;
         ALTER ROLE child INHERIT;
         GRANT parent TO child;
         ALTER ROLE child NOINHERIT;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, true)]);
}

/// A repeated grant changes only the options it names, as measured on
/// PostgreSQL 18.
#[test]
fn repeated_grant_changes_only_the_options_it_names() {
    let granted = "CREATE ROLE parent;
         CREATE ROLE child;
         GRANT parent TO child WITH ADMIN TRUE, SET FALSE;
         GRANT parent TO child WITH INHERIT FALSE;
         GRANT parent TO child;";
    let database = parse(granted).expect("schema builds");
    assert_eq!(memberships_of(&database, "child"), [membership("parent", true, false, false)]);

    let regranted = format!(
        "{granted}
         GRANT parent TO child WITH ADMIN FALSE, INHERIT TRUE, SET TRUE;"
    );
    let database = parse(&regranted).expect("schema builds");
    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, true)]);
}

/// Each grantor keeps its own row in `pg_auth_members`, and the member reaches
/// `SET ROLE` through any of them, as measured on PostgreSQL 17.11 and 18.6.
#[test]
fn grants_from_two_grantors_answer_their_union() {
    let database = parse(
        "CREATE ROLE bypasser BYPASSRLS;
         CREATE ROLE m;
         CREATE ROLE g1;
         CREATE ROLE g2;
         GRANT bypasser TO g1, g2 WITH ADMIN TRUE;
         GRANT bypasser TO m GRANTED BY g1;
         GRANT bypasser TO m WITH SET FALSE, INHERIT FALSE GRANTED BY g2;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "m"), [membership("bypasser", false, true, true)]);
}

/// The union holds for each option on its own, so one grantor's `ADMIN` and
/// another's `SET` both stand.
#[test]
fn each_option_is_the_union_of_its_grants() {
    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child NOINHERIT;
         CREATE ROLE g1;
         CREATE ROLE g2;
         GRANT parent TO g1, g2 WITH ADMIN TRUE;
         GRANT parent TO child WITH ADMIN TRUE, SET FALSE GRANTED BY g1;
         GRANT parent TO child WITH INHERIT TRUE, SET FALSE GRANTED BY g2;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "child"), [membership("parent", true, true, false)]);
}

/// A grant written without `GRANTED BY` is a grantor of its own, apart from
/// one that names its grantor, and grantors compare as identifiers.
#[test]
fn unwritten_grantor_is_its_own_grant() {
    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         CREATE ROLE g;
         GRANT parent TO g WITH ADMIN TRUE;
         GRANT parent TO child WITH ADMIN TRUE GRANTED BY g;
         GRANT parent TO child WITH SET FALSE;
         GRANT parent TO child WITH ADMIN FALSE GRANTED BY G;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, true)]);
}

#[test]
fn one_grant_records_every_role_for_every_grantee() {
    let database = parse(
        "CREATE ROLE a;
         CREATE ROLE b;
         CREATE ROLE x;
         CREATE ROLE y;
         GRANT a, b TO x, y WITH ADMIN TRUE;",
    )
    .expect("schema builds");

    for member in ["x", "y"] {
        assert_eq!(
            memberships_of(&database, member),
            [membership("a", true, true, true), membership("b", true, true, true)]
        );
    }
}

/// Measured on PostgreSQL 18 through `pg_auth_members`.
#[test]
fn create_role_clauses_record_memberships() {
    let database = parse(
        "CREATE ROLE b;
         CREATE ROLE m1;
         CREATE ROLE m2 NOINHERIT;
         CREATE ROLE viarole ROLE m1, m2;
         CREATE ROLE viaadmin ADMIN m2;
         CREATE ROLE viauser USER m2;
         CREATE ROLE viagroup IN GROUP b;
         CREATE ROLE noinh NOINHERIT IN ROLE b;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "viagroup"), [membership("b", false, true, true)]);
    assert_eq!(memberships_of(&database, "noinh"), [membership("b", false, false, true)]);
    assert_eq!(memberships_of(&database, "m1"), [membership("viarole", false, true, true)]);
    assert_eq!(
        memberships_of(&database, "m2"),
        [
            membership("viarole", false, false, true),
            membership("viaadmin", true, false, true),
            membership("viauser", false, false, true),
        ]
    );
}

/// `ROLE q ADMIN q` grants `q` one membership with `ADMIN`, as measured on
/// PostgreSQL 18.
#[test]
fn role_and_admin_clauses_naming_one_member_merge() {
    let database = parse(
        "CREATE ROLE q;
         CREATE ROLE both_clauses ROLE q ADMIN q;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "q"), [membership("both_clauses", true, true, true)]);
}

#[test]
fn closed_world_refuses_an_absent_granted_role() {
    let error = parse(
        "CREATE ROLE app_user;
         GRANT absent TO app_user;",
    )
    .expect_err("the granted role is absent");

    assert!(
        matches!(error, Error::RoleNotFoundForMembership { role_name } if role_name == "absent")
    );
}

#[test]
fn closed_world_refuses_an_absent_grantee() {
    let error = parse(
        "CREATE ROLE parent;
         GRANT parent TO absent;",
    )
    .expect_err("the grantee is absent");

    assert!(
        matches!(error, Error::RoleNotFoundForMembership { role_name } if role_name == "absent")
    );
}

#[test]
fn closed_world_refuses_an_absent_role_in_a_create_role_clause() {
    let error = parse("CREATE ROLE child IN ROLE absent;").expect_err("the parent is absent");

    assert!(
        matches!(error, Error::RoleNotFoundForMembership { role_name } if role_name == "absent")
    );
}

/// `pg_dumpall` names grantors such as `postgres` that a schema rarely
/// creates, so an absent grantor is not refused.
#[test]
fn closed_world_accepts_an_absent_grantor() {
    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         GRANT parent TO child GRANTED BY postgres;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, true)]);
}

/// PostgreSQL answers `role "public" does not exist` under either setting.
#[test]
fn public_is_refused_as_a_member_or_a_granted_role() {
    for sql in [
        "CREATE ROLE parent; GRANT parent TO PUBLIC;",
        "CREATE ROLE parent; GRANT parent TO GROUP public;",
        "CREATE ROLE child; GRANT public TO child;",
        "CREATE ROLE child IN ROLE public;",
        "CREATE ROLE parent ADMIN public;",
    ] {
        for result in [parse(sql), open_world(sql)] {
            assert!(
                matches!(result, Err(Error::RoleNotFoundForMembership { ref role_name }) if role_name.eq_ignore_ascii_case("public")),
                "{sql} gave {result:?}"
            );
        }
    }
}

#[test]
fn open_world_records_memberships_naming_undeclared_roles() {
    let database = open_world(
        "CREATE ROLE bypasser BYPASSRLS;
         GRANT bypasser TO authenticated;
         CREATE ROLE app_user;
         GRANT anon TO app_user;",
    )
    .expect("schema builds");

    let recorded: Vec<_> = database
        .role_memberships()
        .map(|membership| {
            (
                membership.stored_role_name().into_owned(),
                membership.stored_member_name().into_owned(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            ("bypasser".to_owned(), "authenticated".to_owned()),
            ("anon".to_owned(), "app_user".to_owned()),
        ]
    );

    let app_user = database.role("app_user").expect("app_user resolves");
    assert_eq!(memberships_of(&database, "app_user"), [membership("anon", false, true, true)]);
    assert!(app_user.member_of(&database).next().is_none());

    let unresolved: Vec<_> =
        database.unresolved_access_references().expect("names resolve").collect();
    assert!(matches!(
        unresolved[..],
        [
            UnresolvedAccessReference::MembershipRole(anon),
            UnresolvedAccessReference::MembershipRole(authenticated),
        ] if anon.value == "anon" && authenticated.value == "authenticated"
    ));
    assert!(matches!(
        database.validate_access_targets(),
        Err(Error::RoleNotFoundForMembership { role_name }) if role_name == "anon"
    ));
}

/// The member's `INHERIT` default reads the `INHERIT` an `ALTER ROLE` kept for
/// a role the input never creates, and the PostgreSQL default otherwise.
#[test]
fn open_world_inherit_default_reads_a_kept_alteration() {
    let database = open_world(
        "CREATE ROLE parent;
         ALTER ROLE platform BYPASSRLS NOINHERIT;
         ALTER ROLE other BYPASSRLS;
         GRANT parent TO platform, other;",
    )
    .expect("schema builds");

    let inherit: Vec<_> = database
        .role_memberships()
        .map(|membership| (membership.member_name().to_owned(), membership.inherit_option()))
        .collect();
    assert_eq!(inherit, [("platform".to_owned(), false), ("other".to_owned(), true)]);
}

#[test]
fn open_world_records_a_create_role_clause_naming_an_undeclared_role() {
    let database =
        open_world("CREATE ROLE app_user IN ROLE authenticated;").expect("schema builds");

    assert_eq!(
        memberships_of(&database, "app_user"),
        [membership("authenticated", false, true, true)]
    );
    assert!(matches!(
        database.validate_access_targets(),
        Err(Error::RoleNotFoundForMembership { role_name }) if role_name == "authenticated"
    ));
}

#[test]
fn open_world_grant_resolves_once_the_role_is_created() {
    let database = open_world(
        "GRANT parent TO child;
         CREATE ROLE parent;
         CREATE ROLE child;",
    )
    .expect("schema builds");

    let child = database.role("child").expect("child resolves");
    let parents: Vec<_> = child.member_of(&database).map(RoleLike::stored_name).collect();

    assert_eq!(parents, ["parent"]);
    assert!(database.validate_access_targets().is_ok());
}

/// A grantee naming the session's own role names no declared role.
#[test]
fn session_role_grantee_records_nothing() {
    let database = parse(
        "CREATE ROLE parent;
         GRANT parent TO CURRENT_USER;",
    )
    .expect("schema builds");

    assert_eq!(database.role_memberships().count(), 0);
}

/// Dropping a role removes the memberships it holds and the ones granted in
/// it, as measured on PostgreSQL 18.
#[test]
fn drop_role_removes_its_memberships_on_both_sides() {
    let database = parse(
        "CREATE ROLE top;
         CREATE ROLE middle;
         CREATE ROLE bottom;
         GRANT top TO middle;
         GRANT middle TO bottom;
         CREATE ROLE kept;
         GRANT top TO kept;
         DROP ROLE middle;",
    )
    .expect("schema builds");

    let recorded: Vec<_> = database
        .role_memberships()
        .map(|membership| {
            (
                membership.stored_role_name().into_owned(),
                membership.stored_member_name().into_owned(),
            )
        })
        .collect();
    assert_eq!(recorded, [("top".to_owned(), "kept".to_owned())]);
}

/// A rename repoints the memberships naming the renamed role, on either side,
/// and leaves every other membership as it was.
#[test]
fn rename_carries_memberships_on_both_sides() {
    let database = parse(
        "CREATE ROLE top;
         CREATE ROLE middle;
         CREATE ROLE bottom;
         CREATE ROLE bystander;
         GRANT top TO middle WITH ADMIN TRUE;
         GRANT middle TO bottom WITH SET FALSE;
         GRANT top TO bystander WITH INHERIT FALSE;
         ALTER ROLE middle RENAME TO \"Middle\";",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "Middle"), [membership("top", true, true, true)]);
    assert_eq!(memberships_of(&database, "bottom"), [membership("Middle", false, true, false)]);
    assert_eq!(memberships_of(&database, "bystander"), [membership("top", false, false, true)]);
}

/// A renamed grantor keeps its grant, so a later grant under the new name
/// updates it, and dropping the grantor stays refused, as measured on
/// PostgreSQL 18.
#[test]
fn rename_carries_the_grantor() {
    let renamed = "CREATE ROLE parent;
         CREATE ROLE child;
         CREATE ROLE g;
         GRANT parent TO g WITH ADMIN TRUE;
         GRANT parent TO child GRANTED BY g;
         ALTER ROLE g RENAME TO h;
         GRANT parent TO child WITH SET FALSE GRANTED BY h;";
    let database = parse(renamed).expect("schema builds");
    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, false)]);

    let error = parse(&format!("{renamed} DROP ROLE h;")).expect_err("h granted a membership");
    assert!(matches!(error, Error::RoleReferenced { role_name } if role_name == "h"));
}

/// A resumed ingestion keeps the memberships recorded before it, and a later
/// grant updates them.
#[test]
fn resumed_ingestion_keeps_and_updates_memberships() {
    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         GRANT parent TO child WITH SET FALSE;",
    )
    .expect("schema builds");

    let mut ingestor = database.into_ingestor();
    for statement in Parser::parse_sql(
        &PostgreSqlDialect {},
        "CREATE ROLE other; GRANT parent TO child WITH ADMIN TRUE; GRANT parent TO other;",
    )
    .expect("SQL parses")
    {
        ingestor = ingestor.apply_statement(statement).expect("statement applies");
    }
    let resumed = ingestor.finish();

    assert_eq!(memberships_of(&resumed, "child"), [membership("parent", true, true, false)]);
    assert_eq!(memberships_of(&resumed, "other"), [membership("parent", false, true, true)]);
}

/// MySQL commits the open transaction before a definition, so a rollback
/// leaves the membership standing.
#[test]
fn mysql_role_grant_commits_the_open_transaction() {
    let database = ParserDB::parse::<MySqlDialect>(
        "CREATE ROLE parent;
         CREATE ROLE child;
         START TRANSACTION;
         GRANT parent TO child;
         ROLLBACK;",
    )
    .expect("schema builds");

    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, true)]);
}

#[test]
fn rolled_back_grant_leaves_no_membership() {
    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         BEGIN;
         GRANT parent TO child;
         ROLLBACK;",
    )
    .expect("schema builds");

    assert_eq!(database.role_memberships().count(), 0);
}

#[test]
fn membership_resolves_both_roles() {
    let database = parse(
        "CREATE ROLE \"Parent\";
         CREATE ROLE child;
         GRANT \"Parent\" TO Child;",
    )
    .expect("schema builds");

    let recorded = database.role_memberships().next().expect("one membership");

    assert_eq!(recorded.role_name(), "Parent");
    assert!(recorded.role_name_is_quoted());
    assert_eq!(recorded.member_name(), "Child");
    assert!(!recorded.member_name_is_quoted());
    assert_eq!(recorded.stored_member_name(), "child");
    assert_eq!(recorded.role(&database).map(RoleLike::stored_name).as_deref(), Some("Parent"));
    assert_eq!(recorded.member(&database).map(RoleLike::stored_name).as_deref(), Some("child"));
}

/// PostgreSQL 18 refuses to drop a role that granted a membership, with
/// `role "g" cannot be dropped because some objects depend on it`.
#[test]
fn dropping_a_grantor_is_refused() {
    let error = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         CREATE ROLE g;
         GRANT parent TO child GRANTED BY g;
         DROP ROLE g;",
    )
    .expect_err("g granted a membership");

    assert!(matches!(error, Error::RoleReferenced { role_name } if role_name == "g"));
}

/// PostgreSQL 18 refuses an unknown option with `unrecognized role option
/// "foo"`, and a later option of a kind replaces an earlier one.
#[test]
fn grant_options_are_read_as_postgres_reads_them() {
    let error = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         GRANT parent TO child WITH FOO TRUE;",
    )
    .expect_err("FOO is no role option");
    assert!(matches!(error, Error::UnrecognizedRoleOption { option } if option == "FOO"));

    let database = parse(
        "CREATE ROLE parent;
         CREATE ROLE child;
         GRANT parent TO child WITH admin TRUE, Admin FALSE;",
    )
    .expect("schema builds");
    assert_eq!(memberships_of(&database, "child"), [membership("parent", false, true, true)]);
}

/// A MySQL account grantee names a user the schema does not model as a role,
/// so it records nothing.
#[test]
fn mysql_account_grantee_records_nothing() {
    let database = ParserDB::parse::<MySqlDialect>(
        "CREATE ROLE parent;
         GRANT parent TO 'app'@'localhost';",
    )
    .expect("schema builds");

    assert_eq!(database.role_memberships().count(), 0);
}
