//! Tests that `TableLike::privileges` answers what PostgreSQL's
//! `has_table_privilege`, `has_column_privilege` and
//! `has_any_column_privilege` answer, per command and per column.
//!
//! Every expectation was measured on PostgreSQL 18.6, with role `r`
//! inheriting from `grp`, `noinh` a `WITH INHERIT FALSE` member of `grp`, and
//! `own` owning a table. The comment beside each case gives the measured
//! answers in the order the case asks them.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::{
    privilege_listing::{EVERYTHING, privilege_names as held},
    table_lookup::table,
};

type Column = <ParserDB as DatabaseLike>::Column;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn open_world(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
}

const ROLES: &str = "CREATE ROLE grp; CREATE ROLE r; CREATE ROLE noinh; CREATE ROLE own;
     GRANT grp TO r; GRANT grp TO noinh WITH INHERIT FALSE;";

fn privileges<'db>(db: &'db ParserDB, relation: &str, role: &str) -> TablePrivileges<'db, Column> {
    let (schema, name) = match relation.split_once('.') {
        Some((schema, name)) => (Some(schema), name),
        None => (None, relation),
    };
    table(db, schema, name)
        .privileges(db.role(role).expect("the role exists"), db)
        .expect("the table is in the database")
}

fn column<'db>(db: &'db ParserDB, relation: &str, name: &str) -> &'db Column {
    table(db, None, relation)
        .columns(db)
        .expect("the table is in the database")
        .find(|column| column.column_name() == name)
        .expect("the column exists")
}

#[test]
fn a_column_grant_holds_on_its_columns_and_never_on_the_table() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE cols (id INT, owner TEXT, secret TEXT);
         GRANT SELECT (id, owner) ON cols TO r;"
    ));
    let select = privileges(&db, "cols", "r").select;

    // f, t, f, t
    assert!(!select.on_table());
    assert!(select.on_column(column(&db, "cols", "id")));
    assert!(!select.on_column(column(&db, "cols", "secret")));
    assert!(select.on_any_column());
    assert_eq!(held(&privileges(&db, "cols", "r")), ["SELECT (id, owner)"]);
}

#[test]
fn a_column_revoke_leaves_a_table_grant_whole() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE tab (id INT, x INT);
         GRANT SELECT, UPDATE ON tab TO r;
         REVOKE UPDATE (x) ON tab FROM r;"
    ));
    let update = privileges(&db, "tab", "r").update;

    // t, t
    assert!(update.on_table());
    assert!(update.on_column(column(&db, "tab", "x")));
}

#[test]
fn a_table_grant_holds_on_a_column_added_later() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE tab (id INT, x INT);
         GRANT SELECT, UPDATE ON tab TO r;
         REVOKE UPDATE (x) ON tab FROM r;
         ALTER TABLE tab ADD COLUMN later INT;"
    ));

    // t
    assert!(privileges(&db, "tab", "r").select.on_column(column(&db, "tab", "later")));
}

#[test]
fn a_column_grant_does_not_reach_a_column_added_later() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE cols (id INT, owner TEXT, secret TEXT);
         GRANT SELECT (id, owner) ON cols TO r;
         ALTER TABLE cols ADD COLUMN later INT;"
    ));

    // f
    assert!(!privileges(&db, "cols", "r").select.on_column(column(&db, "cols", "later")));
}

#[test]
fn revoking_one_privilege_from_all_privileges_keeps_the_rest() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE allp (id INT, x INT);
         GRANT ALL PRIVILEGES ON allp TO r;
         REVOKE UPDATE ON allp FROM r;"
    ));
    let held_by_r = privileges(&db, "allp", "r");

    // f, f, t, t
    assert!(!held_by_r.update.on_table());
    assert!(!held_by_r.update.on_column(column(&db, "allp", "x")));
    assert!(held_by_r.delete);
    assert!(held_by_r.truncate);
    assert_eq!(
        held(&held_by_r),
        ["SELECT", "INSERT", "REFERENCES", "DELETE", "TRUNCATE", "TRIGGER", "MAINTAIN"]
    );
}

#[test]
fn maintain_is_granted_and_revoked_like_the_other_table_privileges() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE g (id INT);
         ALTER TABLE g OWNER TO own;
         GRANT MAINTAIN ON g TO r;
         REVOKE MAINTAIN ON g FROM own;"
    ));

    // {own=arwdDxt/own,r=m/own}
    assert_eq!(held(&privileges(&db, "g", "r")), ["MAINTAIN"]);
    assert_eq!(held(&privileges(&db, "g", "own")), EVERYTHING[..7]);
}

#[test]
fn a_grant_to_a_group_reaches_inheriting_members_only() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE viagrp (id INT);
         GRANT INSERT ON viagrp TO grp;"
    ));

    // t, f
    assert!(privileges(&db, "viagrp", "r").insert.on_table());
    assert!(!privileges(&db, "viagrp", "noinh").insert.on_table());
}

const OWNED: &str = "CREATE TABLE owned (id INT, x INT);
     ALTER TABLE owned OWNER TO own;
     REVOKE SELECT ON owned FROM own;";

#[test]
fn an_owner_loses_a_privilege_revoked_from_its_own_entry() {
    let db = parse(&format!("{ROLES} {OWNED}"));
    let held_by_own = privileges(&db, "owned", "own");

    // f, f, t
    assert!(!held_by_own.select.on_table());
    assert!(!held_by_own.select.on_column(column(&db, "owned", "x")));
    assert!(held_by_own.update.on_table());
}

#[test]
fn an_owner_regains_a_column_through_a_column_grant() {
    let db = parse(&format!("{ROLES} {OWNED} GRANT SELECT (x) ON owned TO own;"));
    let select = privileges(&db, "owned", "own").select;

    // f, t, f
    assert!(!select.on_table());
    assert!(select.on_column(column(&db, "owned", "x")));
    assert!(!select.on_column(column(&db, "owned", "id")));
}

#[test]
fn column_grants_to_a_role_and_to_public_both_count() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE ins (id INT, x INT);
         GRANT INSERT (x) ON ins TO r;
         GRANT UPDATE (x) ON ins TO PUBLIC;"
    ));
    let held_by_r = privileges(&db, "ins", "r");

    // f, t, f, t
    assert!(!held_by_r.insert.on_table());
    assert!(held_by_r.insert.on_column(column(&db, "ins", "x")));
    assert!(!held_by_r.insert.on_column(column(&db, "ins", "id")));
    assert!(held_by_r.update.on_column(column(&db, "ins", "x")));
}

#[test]
fn references_can_be_granted_per_column() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE refs (id INT);
         GRANT REFERENCES (id) ON refs TO r;"
    ));
    let references = privileges(&db, "refs", "r").references;

    // f, t
    assert!(!references.on_table());
    assert!(references.on_column(column(&db, "refs", "id")));
}

#[test]
fn a_schema_wide_grant_reaches_the_tables_it_found() {
    let db = parse(&format!(
        "{ROLES}
         CREATE SCHEMA sc;
         CREATE TABLE sc.before (id INT);
         GRANT SELECT ON ALL TABLES IN SCHEMA sc TO r;
         CREATE TABLE sc.after (id INT);"
    ));

    // t, f
    assert!(privileges(&db, "sc.before", "r").select.on_table());
    assert!(!privileges(&db, "sc.after", "r").select.on_table());
}

#[test]
fn each_command_is_answered_on_its_own() {
    let db = parse(
        "CREATE ROLE reader_writer; CREATE ROLE inserter;
         CREATE TABLE docs (id INT);
         GRANT SELECT, INSERT ON docs TO reader_writer;
         GRANT INSERT ON docs TO inserter;",
    );

    assert_eq!(held(&privileges(&db, "docs", "reader_writer")), ["SELECT", "INSERT"]);
    assert_eq!(held(&privileges(&db, "docs", "inserter")), ["INSERT"]);
    assert!(privileges(&db, "docs", "inserter").insert.on_any_column());
    assert!(!privileges(&db, "docs", "inserter").update.on_any_column());
}

#[test]
fn a_grant_to_a_session_principal_reaches_no_declared_role() {
    let db = parse(
        "CREATE ROLE reader; CREATE ROLE other;
         CREATE TABLE docs (id TEXT PRIMARY KEY);
         GRANT SELECT ON docs TO CURRENT_USER, reader;",
    );

    assert_eq!(held(&privileges(&db, "docs", "reader")), ["SELECT"]);
    assert_eq!(held(&privileges(&db, "docs", "other")), Vec::<String>::new());
}

/// `GROUP public` and `"public"` are `PUBLIC` in PostgreSQL 18.6, while
/// `"Reader"` and `Reader` name two different roles.
#[test]
fn public_spelled_as_a_group_or_quoted_reaches_every_role() {
    let db = parse(
        r#"CREATE ROLE "Reader"; CREATE ROLE reader; CREATE ROLE other;
           CREATE TABLE docs (id TEXT PRIMARY KEY);
           GRANT SELECT ON docs TO GROUP public;
           GRANT INSERT ON docs TO "public";
           GRANT UPDATE ON docs TO "Reader";
           GRANT DELETE ON docs TO Reader;"#,
    );

    assert_eq!(held(&privileges(&db, "docs", "other")), ["SELECT", "INSERT"]);
    assert_eq!(held(&privileges(&db, "docs", "Reader")), ["SELECT", "INSERT", "UPDATE"]);
    assert_eq!(held(&privileges(&db, "docs", "reader")), ["SELECT", "INSERT", "DELETE"]);
}

#[test]
fn a_default_privilege_applies_to_a_later_table() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r;
         ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT SELECT, UPDATE ON TABLES TO anon;
         CREATE TABLE before (id INT);
         SET ROLE r;
         CREATE TABLE after (id INT);",
    );

    assert_eq!(held(&privileges(&db, "before", "anon")), Vec::<String>::new());
    assert_eq!(held(&privileges(&db, "after", "anon")), ["SELECT", "UPDATE"]);
}

/// A superuser passes every privilege check, and its members do not inherit
/// that. With `su_m` a member of `su_x`, PostgreSQL 18.6 answers `t` for
/// `su_x` and `f` for `su_m`, on table `SELECT` and on column `UPDATE`.
#[test]
fn a_superuser_holds_everything_and_passes_none_of_it_on() {
    let db = parse(
        "CREATE ROLE su_x SUPERUSER; CREATE ROLE su_m; GRANT su_x TO su_m;
         CREATE TABLE t (id INT);",
    );
    let superuser = privileges(&db, "t", "su_x");

    assert_eq!(held(&superuser), EVERYTHING);
    assert!(superuser.update.on_column(column(&db, "t", "id")));
    assert_eq!(held(&privileges(&db, "t", "su_m")), Vec::<String>::new());
}

#[test]
fn a_caller_holding_no_role_holds_what_public_holds() {
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE docs (id INT, body TEXT);
         GRANT SELECT ON docs TO PUBLIC;
         GRANT UPDATE (body) ON docs TO PUBLIC;
         GRANT DELETE ON docs TO r;",
    );
    let public = table(&db, None, "docs")
        .privileges(ActingRole::Public, &db)
        .expect("the table is in the database");

    assert_eq!(held(&public), ["SELECT", "UPDATE (body)"]);
}

#[test]
fn a_table_outside_the_database_is_refused() {
    let held_here = parse("CREATE ROLE r; CREATE ROLE su SUPERUSER; CREATE TABLE t (id INT);");
    let other = parse("CREATE TABLE elsewhere (id INT);");

    for role in ["r", "su"] {
        assert!(
            table(&other, None, "elsewhere")
                .privileges(held_here.role(role).expect("the role exists"), &held_here)
                .is_err(),
            "{role}"
        );
    }
}

// The cases below are the privilege answers rls2fga's table privilege tests
// pinned, without the OpenFGA modelling built on them.

#[test]
fn public_all_privileges_and_a_revoke_are_each_read() {
    let db = parse(
        "CREATE ROLE admin; CREATE ROLE editor; CREATE ROLE other;
         CREATE TABLE docs (id TEXT PRIMARY KEY);
         GRANT SELECT ON docs TO PUBLIC;
         GRANT ALL PRIVILEGES ON docs TO admin;
         GRANT UPDATE, DELETE ON docs TO editor;
         REVOKE DELETE ON docs FROM editor;",
    );

    assert_eq!(held(&privileges(&db, "docs", "other")), ["SELECT"]);
    assert_eq!(held(&privileges(&db, "docs", "admin")), EVERYTHING);
    assert_eq!(held(&privileges(&db, "docs", "editor")), ["SELECT", "UPDATE"]);
}

#[test]
fn a_column_grant_on_a_command_narrows_it_to_its_columns() {
    let db = parse(
        "CREATE ROLE writer;
         CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT);
         GRANT SELECT ON docs TO writer;
         GRANT UPDATE (body) ON docs TO writer;
         GRANT INSERT (body) ON docs TO writer;",
    );

    assert_eq!(
        held(&privileges(&db, "docs", "writer")),
        ["SELECT", "INSERT (body)", "UPDATE (body)"]
    );
}

#[test]
fn a_named_owner_holds_every_privilege_beside_its_grantees() {
    let db = parse(
        "CREATE ROLE app_owner; CREATE ROLE reader;
         CREATE TABLE docs (id TEXT PRIMARY KEY);
         ALTER TABLE docs OWNER TO app_owner;
         GRANT SELECT ON docs TO reader;",
    );

    assert_eq!(held(&privileges(&db, "docs", "app_owner")), EVERYTHING);
    assert_eq!(held(&privileges(&db, "docs", "reader")), ["SELECT"]);
}

#[test]
fn a_column_grant_to_public_narrows_every_role() {
    let db = parse(
        "CREATE ROLE other;
         CREATE TABLE docs (id TEXT PRIMARY KEY, body TEXT);
         GRANT SELECT ON docs TO PUBLIC;
         GRANT UPDATE (body) ON docs TO PUBLIC;",
    );

    assert_eq!(held(&privileges(&db, "docs", "other")), ["SELECT", "UPDATE (body)"]);
}

#[test]
fn an_owner_loses_only_the_privilege_revoked_from_it() {
    let db = parse(
        "CREATE ROLE app_owner; CREATE ROLE reader;
         CREATE TABLE docs (id TEXT PRIMARY KEY);
         ALTER TABLE docs OWNER TO app_owner;
         GRANT SELECT, UPDATE ON docs TO reader;
         REVOKE UPDATE ON docs FROM app_owner;",
    );

    assert_eq!(
        held(&privileges(&db, "docs", "app_owner")),
        ["SELECT", "INSERT", "REFERENCES", "DELETE", "TRUNCATE", "TRIGGER", "MAINTAIN"]
    );
    assert_eq!(held(&privileges(&db, "docs", "reader")), ["SELECT", "UPDATE"]);
}

#[test]
fn caller_roles_hold_what_their_memberships_carry() {
    let db = open_world(
        "CREATE ROLE anon NOLOGIN;
         CREATE ROLE authenticated NOLOGIN;
         CREATE ROLE writers NOLOGIN;
         GRANT writers TO authenticated;
         CREATE TABLE docs (id TEXT PRIMARY KEY);
         GRANT SELECT ON docs TO anon, authenticated;
         GRANT INSERT ON docs TO writers;
         GRANT UPDATE ON docs TO ghost, authenticated;",
    )
    .expect("the open world lets the undeclared role through");

    assert_eq!(held(&privileges(&db, "docs", "anon")), ["SELECT"]);
    assert_eq!(held(&privileges(&db, "docs", "authenticated")), ["SELECT", "INSERT", "UPDATE"]);
}

/// A dialect that checks no privilege against the object it names lets a
/// grant carry one tables do not have, which holds none of the eight.
#[test]
fn a_privilege_tables_lack_holds_nothing() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE ROLE r;
         CREATE TABLE docs (id INT);
         GRANT SELECT, USAGE ON docs TO r;",
    )
    .expect("schema builds");

    assert_eq!(held(&privileges(&db, "docs", "r")), ["SELECT"]);
}
