//! Tests that `SequenceLike::privileges` answers what PostgreSQL's
//! `has_sequence_privilege` answers.
//!
//! Every expectation was measured on PostgreSQL 18.7, with role `r`
//! inheriting from `grp`, `noinh` a `WITH INHERIT FALSE` member of `grp`, and
//! `su` a superuser.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

const ROLES: &str = "CREATE ROLE grp; CREATE ROLE r; CREATE ROLE noinh; CREATE ROLE own;
     CREATE ROLE own2; CREATE ROLE su SUPERUSER;
     GRANT grp TO r; GRANT grp TO noinh WITH INHERIT FALSE;
     GRANT CREATE ON SCHEMA public TO own;";

type Sequence = <ParserDB as DatabaseLike>::Sequence;

fn sequence<'db>(db: &'db ParserDB, name: &str) -> &'db Sequence {
    db.sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the sequence exists")
}

/// The names of the privileges `held` marks, in `USAGE`, `SELECT`, `UPDATE`
/// order.
fn names(held: SequencePrivileges) -> Vec<&'static str> {
    [(held.usage, "USAGE"), (held.select, "SELECT"), (held.update, "UPDATE")]
        .into_iter()
        .filter_map(|(is_held, name)| is_held.then_some(name))
        .collect()
}

/// What `role` holds on sequence `name`.
fn held(db: &ParserDB, name: &str, role: &str) -> Vec<&'static str> {
    names(
        sequence(db, name)
            .privileges(db.role(role).expect("the role exists"), db)
            .expect("the sequence is in the database"),
    )
}

const EVERYTHING: [&str; 3] = ["USAGE", "SELECT", "UPDATE"];

const NOTHING: [&str; 0] = [];

#[test]
fn a_serial_sequence_starts_with_its_owner_alone() {
    let db = parse(&format!(
        "{ROLES}
         SET ROLE own;
         CREATE TABLE notes (id SERIAL PRIMARY KEY, body TEXT);
         RESET ROLE;
         GRANT INSERT ON notes TO r;"
    ));

    assert_eq!(held(&db, "notes_id_seq", "own"), EVERYTHING);
    assert_eq!(held(&db, "notes_id_seq", "r"), NOTHING);
    assert_eq!(held(&db, "notes_id_seq", "su"), EVERYTHING);
}

#[test]
fn sequence_grants_reach_inheriting_members_and_public() {
    let db = parse(&format!(
        "{ROLES}
         CREATE SEQUENCE s;
         GRANT USAGE ON SEQUENCE s TO grp;
         GRANT SELECT ON SEQUENCE s TO PUBLIC;"
    ));

    assert_eq!(held(&db, "s", "r"), ["USAGE", "SELECT"]);
    assert_eq!(held(&db, "s", "noinh"), ["SELECT"]);
    let public =
        sequence(&db, "s").privileges(ActingRole::Public, &db).expect("the sequence is in db");
    assert_eq!(names(public), ["SELECT"]);
}

#[test]
fn all_privileges_reach_a_sequence_through_every_spelling() {
    let db = parse(&format!(
        "{ROLES}
         CREATE SEQUENCE a;
         CREATE SEQUENCE b;
         CREATE SEQUENCE c;
         GRANT ALL ON SEQUENCE a TO r;
         GRANT ALL PRIVILEGES ON b TO own;
         GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO own2;"
    ));

    assert_eq!(held(&db, "a", "r"), EVERYTHING);
    assert_eq!(held(&db, "b", "r"), NOTHING);
    assert_eq!(held(&db, "b", "own"), EVERYTHING);
    assert_eq!(held(&db, "c", "own"), NOTHING);
    for name in ["a", "b", "c"] {
        assert_eq!(held(&db, name, "own2"), EVERYTHING);
    }
}

#[test]
fn a_grant_on_a_sequence_keeps_only_sequence_privileges() {
    let db = parse(&format!(
        "{ROLES}
         CREATE SEQUENCE s;
         GRANT UPDATE ON s TO r;
         GRANT SELECT ON SEQUENCE s TO own;
         GRANT INSERT, USAGE ON s TO own2;"
    ));

    assert_eq!(held(&db, "s", "r"), ["UPDATE"]);
    assert_eq!(held(&db, "s", "own"), ["SELECT"]);
    assert_eq!(held(&db, "s", "own2"), ["USAGE"]);
}

#[test]
fn a_revoke_naming_the_sequence_owner_takes_from_its_entry() {
    let db = parse(&format!(
        "{ROLES}
         SET ROLE own;
         CREATE SEQUENCE s;
         CREATE SEQUENCE s2;
         CREATE TABLE notes (id SERIAL);
         RESET ROLE;
         REVOKE USAGE ON ALL SEQUENCES IN SCHEMA public FROM own;
         REVOKE ALL ON SEQUENCE s FROM own;
         REVOKE UPDATE ON s2 FROM own;
         GRANT USAGE ON SEQUENCE s TO own;"
    ));

    assert_eq!(held(&db, "s", "own"), ["USAGE"]);
    assert_eq!(held(&db, "s2", "own"), ["SELECT"]);
    assert_eq!(held(&db, "notes_id_seq", "own"), ["SELECT", "UPDATE"]);
}

#[test]
fn a_table_owner_change_hands_the_sequence_entry_over() {
    let db = parse(&format!(
        "{ROLES}
         SET ROLE own;
         CREATE TABLE notes (id SERIAL);
         RESET ROLE;
         REVOKE UPDATE ON SEQUENCE notes_id_seq FROM own;
         GRANT SELECT ON SEQUENCE notes_id_seq TO r;
         ALTER TABLE notes OWNER TO own2;"
    ));

    assert_eq!(held(&db, "notes_id_seq", "own"), NOTHING);
    assert_eq!(held(&db, "notes_id_seq", "own2"), ["USAGE", "SELECT"]);
    assert_eq!(held(&db, "notes_id_seq", "r"), ["SELECT"]);
}

#[test]
fn default_privileges_shape_a_new_sequence() {
    let db = parse(&format!(
        "{ROLES}
         ALTER DEFAULT PRIVILEGES FOR ROLE own REVOKE UPDATE ON SEQUENCES FROM own;
         ALTER DEFAULT PRIVILEGES FOR ROLE own GRANT USAGE ON SEQUENCES TO r;
         SET ROLE own;
         CREATE TABLE notes (id SERIAL);
         CREATE SEQUENCE s;
         RESET ROLE;"
    ));

    for name in ["notes_id_seq", "s"] {
        assert_eq!(held(&db, name, "own"), ["USAGE", "SELECT"]);
        assert_eq!(held(&db, name, "r"), ["USAGE"]);
    }
}

#[test]
fn a_sequence_another_database_holds_is_refused() {
    let elsewhere = parse("CREATE SEQUENCE s;");
    let db = parse("CREATE ROLE r;");

    let refused = sequence(&elsewhere, "s")
        .privileges(db.role("r").expect("the role exists"), &db)
        .expect_err("the database holds no such sequence");
    assert!(matches!(
        refused,
        LookupError::ObjectNotInDatabase { object_kind: ObjectKind::Sequence, .. }
    ));
}
