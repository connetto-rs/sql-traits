//! A `REVOKE` on a table or its columns is kept as a revocation of each
//! privilege from each grantee, whether or not it matched a recorded grant,
//! until a `GRANT` of the same privilege at the same level hands it back.
#![allow(clippy::expect_used)]

use sql_traits::{prelude::*, structs::RevokedPrivilege};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

/// Every table-level revocation on `name`, in the order revoked.
fn revocations<'db>(
    database: &'db ParserDB,
    name: &str,
) -> impl Iterator<Item = &'db RevokedPrivilege> {
    database
        .table_metadata(table(database, None, name))
        .expect("the table is in the database")
        .revoked_privileges()
}

/// The table-level privileges revoked from `role` on `name`, as written.
fn revoked_on_table(database: &ParserDB, name: &str, role: &str) -> Vec<String> {
    let role = database.role(role).expect("the role exists");
    revocations(database, name)
        .filter(|revoked| revoked.applies_to_role(role))
        .map(|revoked| revoked.privilege().to_string())
        .collect()
}

/// The privileges revoked from `role` on the column `column` of `name`.
fn revoked_on_column(database: &ParserDB, name: &str, column: &str, role: &str) -> Vec<String> {
    let role = database.role(role).expect("the role exists");
    let table = table(database, None, name);
    let column = table
        .column(column, database, IdentifierCase::AsWritten)
        .expect("the table is in the database")
        .expect("the column exists");
    database
        .table_metadata(table)
        .expect("the table is in the database")
        .column_revoked_privileges(column)
        .filter(|revoked| revoked.applies_to_role(role))
        .map(|revoked| revoked.privilege().to_string())
        .collect()
}

/// No privilege, for comparing against a list that should be empty.
const NONE: [&str; 0] = [];

const PROFILES: &str = "CREATE ROLE app_user;
     CREATE TABLE profiles (id TEXT, display_name TEXT, is_admin BOOL);";

#[test]
fn a_revoke_matching_no_grant_survives_a_later_column_grant() {
    let granted_only =
        parse(&format!("{PROFILES} GRANT UPDATE (display_name) ON profiles TO app_user;"));
    let revoked_then_granted = parse(&format!(
        "{PROFILES}
         REVOKE UPDATE ON profiles FROM app_user;
         GRANT UPDATE (display_name) ON profiles TO app_user;"
    ));

    assert_eq!(revoked_on_table(&granted_only, "profiles", "app_user"), NONE);
    assert_eq!(revoked_on_table(&revoked_then_granted, "profiles", "app_user"), ["UPDATE"]);
    assert_eq!(revoked_on_column(&revoked_then_granted, "profiles", "is_admin", "app_user"), NONE);
}

#[test]
fn a_revoke_matching_a_recorded_grant_takes_it_and_is_kept() {
    let database = parse(&format!(
        "{PROFILES}
         GRANT SELECT, UPDATE ON profiles TO app_user;
         REVOKE UPDATE ON profiles FROM app_user;"
    ));
    let profiles = table(&database, None, "profiles");
    let app_user = database.role("app_user").expect("the role exists");

    let held = profiles.privileges(app_user, &database).expect("the table is in the database");
    assert!(!held.update.on_table());
    assert!(held.select.on_table());
    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), ["UPDATE"]);
}

#[test]
fn only_a_grant_at_the_same_level_hands_the_privilege_back() {
    let database = parse(&format!(
        "{PROFILES}
         REVOKE INSERT, UPDATE ON profiles FROM app_user;
         REVOKE UPDATE (is_admin), SELECT (is_admin) ON profiles FROM app_user;
         GRANT UPDATE ON profiles TO app_user;
         GRANT SELECT (is_admin) ON profiles TO app_user;"
    ));

    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), ["INSERT"]);
    assert_eq!(revoked_on_column(&database, "profiles", "is_admin", "app_user"), ["UPDATE"]);
    assert_eq!(revoked_on_column(&database, "profiles", "display_name", "app_user"), NONE);
}

#[test]
fn revoke_all_revokes_every_table_privilege_one_by_one() {
    let database = parse(&format!(
        "{PROFILES}
         REVOKE ALL ON profiles FROM app_user;
         GRANT SELECT ON profiles TO app_user;"
    ));

    assert_eq!(
        revoked_on_table(&database, "profiles", "app_user"),
        ["INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "MAINTAIN"]
    );
}

#[test]
fn revoking_only_the_grant_option_revokes_no_privilege() {
    let database =
        parse(&format!("{PROFILES} REVOKE GRANT OPTION FOR UPDATE ON profiles FROM app_user;"));

    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), NONE);
}

#[test]
fn a_revoke_from_public_reaches_every_role() {
    let database = parse(&format!(
        "{PROFILES}
         CREATE ROLE other;
         REVOKE SELECT ON profiles FROM PUBLIC;
         REVOKE DELETE ON profiles FROM other;"
    ));
    let revoked: Vec<_> = revocations(&database, "profiles").collect();

    assert_eq!(revoked.iter().filter(|revoked| revoked.applies_to_public()).count(), 1);
    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), ["SELECT"]);
    assert_eq!(revoked_on_table(&database, "profiles", "other"), ["SELECT", "DELETE"]);

    let regranted = parse(&format!(
        "{PROFILES}
         REVOKE SELECT ON profiles FROM PUBLIC;
         GRANT SELECT ON profiles TO PUBLIC;"
    ));
    assert_eq!(revoked_on_table(&regranted, "profiles", "app_user"), NONE);
}

#[test]
fn the_grantor_a_revoke_names_is_kept_by_its_stored_name() {
    let database = parse(&format!(
        "{PROFILES}
         CREATE ROLE \"Admins\";
         CREATE ROLE owners;
         REVOKE UPDATE ON profiles FROM app_user GRANTED BY \"Admins\";
         REVOKE INSERT ON profiles FROM app_user GRANTED BY OWNERS;
         REVOKE DELETE ON profiles FROM app_user;"
    ));
    let grantors: Vec<(String, Option<&str>)> = revocations(&database, "profiles")
        .map(|revoked| (revoked.privilege().to_string(), revoked.granted_by()))
        .collect();

    assert_eq!(
        grantors,
        [
            ("UPDATE".to_string(), Some("Admins")),
            ("INSERT".to_string(), Some("owners")),
            ("DELETE".to_string(), None),
        ]
    );
}

#[test]
fn a_schema_wide_revoke_reaches_the_tables_the_schema_then_holds() {
    let database = parse(
        "CREATE ROLE app_user;
         CREATE TABLE before_revoke (id INT);
         REVOKE UPDATE ON ALL TABLES IN SCHEMA public FROM app_user;
         CREATE TABLE after_revoke (id INT);",
    );

    assert_eq!(revoked_on_table(&database, "before_revoke", "app_user"), ["UPDATE"]);
    assert_eq!(revoked_on_table(&database, "after_revoke", "app_user"), NONE);
}

#[test]
fn revocations_follow_renames_of_the_role_the_table_and_the_column() {
    let database = parse(&format!(
        "{PROFILES}
         CREATE ROLE admins;
         REVOKE UPDATE ON profiles FROM app_user GRANTED BY admins;
         REVOKE UPDATE (is_admin) ON profiles FROM app_user;
         GRANT SELECT (is_admin) ON profiles TO app_user;
         ALTER ROLE app_user RENAME TO members;
         ALTER ROLE admins RENAME TO keepers;
         ALTER TABLE profiles RENAME TO accounts;
         ALTER TABLE accounts RENAME COLUMN is_admin TO is_staff;
         ALTER TABLE accounts ADD COLUMN bio TEXT;"
    ));
    let granted_by: Vec<_> =
        revocations(&database, "accounts").map(RevokedPrivilege::granted_by).collect();

    assert_eq!(revoked_on_table(&database, "accounts", "members"), ["UPDATE"]);
    assert_eq!(granted_by, [Some("keepers")]);
    assert_eq!(revoked_on_column(&database, "accounts", "is_staff", "members"), ["UPDATE"]);
    assert_eq!(
        database.column_grants().map(ToString::to_string).collect::<Vec<_>>(),
        ["GRANT SELECT (is_staff) ON accounts TO members"]
    );
}

#[test]
fn revocations_go_with_the_column_the_table_and_the_role() {
    let database = parse(&format!(
        "{PROFILES}
         CREATE ROLE leaving;
         REVOKE UPDATE (is_admin) ON profiles FROM app_user;
         REVOKE UPDATE ON profiles FROM leaving;
         REVOKE DELETE ON profiles FROM app_user GRANTED BY leaving;
         REVOKE SELECT ON profiles FROM app_user;
         DROP ROLE leaving;
         ALTER TABLE profiles DROP COLUMN is_admin;
         ALTER TABLE profiles ADD COLUMN is_admin BOOL;"
    ));
    assert_eq!(revocations(&database, "profiles").count(), 1);
    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), ["SELECT"]);
    assert_eq!(revoked_on_column(&database, "profiles", "is_admin", "app_user"), NONE);

    let recreated = parse(&format!(
        "{PROFILES}
         REVOKE UPDATE ON profiles FROM app_user;
         DROP TABLE profiles;
         CREATE TABLE profiles (id TEXT);"
    ));
    assert_eq!(revoked_on_table(&recreated, "profiles", "app_user"), NONE);
}

#[test]
fn a_revocation_is_kept_once_and_handed_back_only_to_the_grantee() {
    let database = parse(&format!(
        "{PROFILES}
         CREATE ROLE other;
         REVOKE UPDATE ON profiles FROM app_user, other;
         REVOKE UPDATE ON profiles FROM app_user;
         GRANT UPDATE ON profiles TO other;"
    ));

    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), ["UPDATE"]);
    assert_eq!(revoked_on_table(&database, "profiles", "other"), NONE);
}

#[test]
fn grant_all_hands_back_every_table_revocation_and_no_column_one() {
    let database = parse(&format!(
        "{PROFILES}
         REVOKE INSERT, UPDATE ON profiles FROM app_user;
         REVOKE UPDATE (is_admin) ON profiles FROM app_user;
         GRANT ALL ON profiles TO app_user;"
    ));

    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), NONE);
    assert_eq!(revoked_on_column(&database, "profiles", "is_admin", "app_user"), ["UPDATE"]);
}

/// PostgreSQL's `aclnewowner` renames the old owner in the table's privilege
/// entries, so a revocation naming it, as grantee or as grantor, moves to the
/// new owner on that table, folding into one the new owner already had.
#[test]
fn an_owner_change_hands_the_revocations_naming_the_old_owner_over() {
    let database = parse(
        "CREATE ROLE previous;
         CREATE ROLE next;
         CREATE ROLE app_user;
         CREATE TABLE t (id INT);
         CREATE TABLE u (id INT);
         ALTER TABLE t OWNER TO previous;
         ALTER TABLE u OWNER TO previous;
         REVOKE TRUNCATE ON t, u FROM previous;
         REVOKE UPDATE ON t, u FROM app_user GRANTED BY previous;
         REVOKE TRUNCATE ON t FROM next;
         ALTER TABLE t OWNER TO next;",
    );
    let grantors = |name: &str| -> Vec<Option<String>> {
        revocations(&database, name)
            .map(|revoked| revoked.granted_by().map(str::to_string))
            .collect()
    };

    assert_eq!(revoked_on_table(&database, "t", "next"), ["TRUNCATE"]);
    assert_eq!(revoked_on_table(&database, "t", "previous"), NONE);
    assert_eq!(revoked_on_table(&database, "u", "previous"), ["TRUNCATE"]);
    assert_eq!(grantors("t"), [None, Some("next".to_string())]);
    assert_eq!(grantors("u"), [None, Some("previous".to_string())]);
}

#[test]
fn a_rolled_back_revoke_leaves_no_revocation() {
    let database = parse(&format!(
        "{PROFILES}
         BEGIN;
         REVOKE UPDATE ON profiles FROM app_user;
         ROLLBACK;"
    ));

    assert_eq!(revoked_on_table(&database, "profiles", "app_user"), NONE);
}
