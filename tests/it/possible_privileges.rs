//! Tests that `TableLike::possible_privileges` on a parsed schema leaves out
//! only what a table-level `REVOKE` naming the role itself proves gone and no
//! recorded grant gives back.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::{
    privilege_listing::{EVERYTHING, privilege_names},
    table_lookup::table,
};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

const PROFILES: &str = "CREATE ROLE app;
     CREATE TABLE profiles (id TEXT, display_name TEXT, is_admin BOOL);";

/// The privileges `role` may hold on `profiles`, by name.
fn possible(db: &ParserDB, role: &str) -> Vec<String> {
    privilege_names(
        &table(db, None, "profiles")
            .possible_privileges(db.role(role).expect("the role exists"), db)
            .expect("the table is in the database"),
    )
}

/// The `UPDATE` entry among the privileges `app` may hold on `profiles`.
fn possible_update(sql: &str) -> Vec<String> {
    let db = parse(&format!("{PROFILES} {sql}"));
    possible(&db, "app").into_iter().filter(|name| name.starts_with("UPDATE")).collect()
}

#[test]
fn a_privilege_never_granted_stays_possible() {
    let db = parse(PROFILES);
    let profiles = table(&db, None, "profiles");
    let app = db.role("app").expect("the role exists");

    assert_eq!(
        privilege_names(&profiles.privileges(app, &db).expect("held")),
        Vec::<String>::new()
    );
    assert_eq!(possible(&db, "app"), EVERYTHING);
}

/// Whether `table` proves `role` lacks `UPDATE` on every column, read by a
/// consumer generic over the table type.
fn update_withheld<T: TableLike<DB = ParserDB>>(table: T, role: &str, db: &ParserDB) -> bool {
    let role = db.role(role).expect("the role exists");
    !table
        .possible_privileges(role, db)
        .expect("the table is in the database")
        .update
        .on_any_column()
}

#[test]
fn a_borrowed_table_answers_as_the_table_does() {
    let db = parse(&format!("{PROFILES} CREATE ROLE other; REVOKE UPDATE ON profiles FROM app;"));
    let profiles: &<ParserDB as DatabaseLike>::Table = table(&db, None, "profiles");

    assert!(update_withheld(profiles, "app", &db));
    assert!(!update_withheld(profiles, "other", &db));
}

#[test]
fn a_revoke_naming_the_role_leaves_what_it_still_holds() {
    let db = parse(&format!(
        "{PROFILES}
         REVOKE UPDATE ON profiles FROM app;
         GRANT UPDATE (display_name) ON profiles TO app;"
    ));
    let is_admin = table(&db, None, "profiles")
        .columns(&db)
        .expect("the table is in the database")
        .find(|column| column.column_name() == "is_admin")
        .expect("the column exists");
    let update = table(&db, None, "profiles")
        .possible_privileges(db.role("app").expect("the role exists"), &db)
        .expect("the table is in the database")
        .update;

    assert!(!update.on_table());
    assert!(!update.on_column(is_admin));
    assert_eq!(
        possible(&db, "app"),
        [
            "SELECT",
            "INSERT",
            "UPDATE (display_name)",
            "REFERENCES",
            "DELETE",
            "TRUNCATE",
            "TRIGGER",
            "MAINTAIN"
        ]
    );

    let db = parse(&format!("{PROFILES} REVOKE ALL ON profiles FROM app;"));
    assert_eq!(possible(&db, "app"), Vec::<String>::new());
}

#[test]
fn a_revoke_from_public_or_an_inherited_role_proves_nothing_for_the_role() {
    let db = parse(&format!(
        "{PROFILES}
         CREATE ROLE editors;
         GRANT editors TO app;
         REVOKE UPDATE ON profiles FROM PUBLIC;
         REVOKE DELETE ON profiles FROM editors;"
    ));

    assert_eq!(possible(&db, "app"), EVERYTHING);
    assert_eq!(
        possible(&db, "editors"),
        ["SELECT", "INSERT", "UPDATE", "REFERENCES", "TRUNCATE", "TRIGGER", "MAINTAIN"]
    );
    let public = table(&db, None, "profiles")
        .possible_privileges(ActingRole::Public, &db)
        .expect("the table is in the database");
    assert_eq!(
        privilege_names(&public),
        ["SELECT", "INSERT", "REFERENCES", "DELETE", "TRUNCATE", "TRIGGER", "MAINTAIN"]
    );
}

#[test]
fn only_a_revoke_by_no_grantor_or_by_the_owner_proves_the_loss() {
    assert_eq!(
        possible_update(
            "CREATE ROLE editors;
             ALTER TABLE profiles OWNER TO editors;
             REVOKE UPDATE ON profiles FROM app GRANTED BY editors;"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        possible_update(
            "CREATE ROLE other;
             GRANT UPDATE ON profiles TO other WITH GRANT OPTION;
             REVOKE UPDATE ON profiles FROM app GRANTED BY other;"
        ),
        ["UPDATE"]
    );
}

#[test]
fn a_privilege_reaching_the_role_again_stays_possible() {
    for (sql, update) in [
        ("REVOKE UPDATE ON profiles FROM app; GRANT UPDATE ON profiles TO PUBLIC;", "UPDATE"),
        (
            "REVOKE UPDATE ON profiles FROM app; CREATE ROLE editors;
             GRANT UPDATE ON profiles TO editors; GRANT editors TO app;",
            "UPDATE",
        ),
        (
            "REVOKE UPDATE ON profiles FROM app; GRANT UPDATE (is_admin) ON profiles TO PUBLIC;",
            "UPDATE (is_admin)",
        ),
        (
            "CREATE ROLE owners; ALTER TABLE profiles OWNER TO owners; GRANT owners TO app;
             REVOKE UPDATE ON profiles FROM app;",
            "UPDATE",
        ),
        // A column `REVOKE` leaves a table-level privilege in place.
        ("REVOKE UPDATE (is_admin) ON profiles FROM app;", "UPDATE"),
    ] {
        assert_eq!(possible_update(sql), [update], "{sql}");
    }
}

#[test]
fn the_owner_loses_what_it_revokes_from_itself_and_a_superuser_nothing() {
    let db = parse(&format!(
        "{PROFILES}
         CREATE ROLE owners;
         CREATE ROLE boss SUPERUSER;
         ALTER TABLE profiles OWNER TO owners;
         REVOKE UPDATE, MAINTAIN ON profiles FROM owners;
         REVOKE ALL ON profiles FROM boss;"
    ));

    assert_eq!(
        possible(&db, "owners"),
        ["SELECT", "INSERT", "REFERENCES", "DELETE", "TRUNCATE", "TRIGGER"]
    );
    assert_eq!(possible(&db, "boss"), EVERYTHING);
}
