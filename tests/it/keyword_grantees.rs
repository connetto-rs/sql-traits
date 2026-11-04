//! Tests that a grant to `CURRENT_USER` and its siblings names whoever runs
//! the statement, not a role of that name.
//!
//! The grammar spells these principals as keywords and the parser hands them
//! back as ordinary identifiers, so reading them as role names refused a
//! statement PostgreSQL accepts and, with role resolution opened up, matched a
//! role somebody had literally called `current_user`.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn build(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

const KEYWORDS: [&str; 3] = ["CURRENT_USER", "CURRENT_ROLE", "SESSION_USER"];

/// A grant to a session principal is accepted with no role declared, since
/// there is no role for a schema to declare.
#[test]
fn a_grant_to_a_session_principal_needs_no_role() {
    for keyword in KEYWORDS {
        let db = build(&format!("CREATE TABLE docs (id INT); GRANT SELECT ON docs TO {keyword};"))
            .unwrap_or_else(|error| panic!("`{keyword}` was refused: {error}"));

        let grant = db.table_grants().next().expect("the grant is recorded");
        assert!(!grant.applies_to_public(), "`{keyword}` is not everyone");
        assert_eq!(db.roles().count(), 0, "`{keyword}` declares no role");
    }
}

/// A session principal is not a declared role, so a role of that spelling
/// does not receive the grant.
#[test]
fn a_session_principal_is_not_a_declared_role() {
    let db = build(
        "CREATE TABLE docs (id INT);
         CREATE ROLE \"current_user\";
         GRANT SELECT ON docs TO CURRENT_USER;",
    )
    .expect("the schema builds");

    let grant = db.table_grants().next().expect("the grant is recorded");
    // `role` asks by stored identity, and a quoted declaration stores its case.
    let role = db.role("current_user").expect("the quoted role exists");
    assert!(!grant.applies_to_role(role), "the keyword named the session, not this role");
}

/// The quoted spelling is an ordinary role name, so it still has to exist and
/// it still receives the grant.
#[test]
fn the_quoted_spelling_stays_a_role() {
    let missing = build("CREATE TABLE docs (id INT); GRANT SELECT ON docs TO \"CURRENT_USER\";");
    assert!(matches!(missing, Err(Error::RoleNotFoundForGrant { .. })), "got {missing:?}");

    let db = build(
        "CREATE TABLE docs (id INT);
         CREATE ROLE \"CURRENT_USER\";
         GRANT SELECT ON docs TO \"CURRENT_USER\";",
    )
    .expect("the declared role builds");

    let grant = db.table_grants().next().expect("the grant is recorded");
    let role = db.role("CURRENT_USER").expect("the role exists");
    assert!(grant.applies_to_role(role));
}

/// An undeclared ordinary role is still refused, so opening the keyword up
/// did not open everything up.
#[test]
fn an_undeclared_role_stays_refused() {
    let refused = build("CREATE TABLE docs (id INT); GRANT SELECT ON docs TO reader;");
    assert!(matches!(refused, Err(Error::RoleNotFoundForGrant { .. })), "got {refused:?}");
}

/// `CURRENT_ROLE` is another spelling of `CURRENT_USER`, so a revoke written
/// with one removes a grant written with the other, which is what PostgreSQL
/// 18 does. `SESSION_USER` is the role that logged in and stays itself.
#[test]
fn the_current_role_spelling_revokes_a_current_user_grant() {
    let revoked = build(
        "CREATE TABLE docs (id INT);
         GRANT SELECT ON docs TO CURRENT_USER;
         REVOKE SELECT ON docs FROM CURRENT_ROLE;",
    )
    .expect("the schema builds");
    assert_eq!(revoked.table_grants().count(), 0, "the same principal was revoked");

    let kept = build(
        "CREATE TABLE docs (id INT);
         GRANT SELECT ON docs TO CURRENT_USER;
         REVOKE SELECT ON docs FROM SESSION_USER;",
    )
    .expect("the schema builds");
    assert_eq!(kept.table_grants().count(), 1, "another principal keeps the grant");
}

/// A revoke naming the quoted literal does not reach a grant made to the
/// keyword, since one names a role and the other names the session.
#[test]
fn a_quoted_revoke_leaves_a_keyword_grant_alone() {
    let kept = build(
        "CREATE TABLE docs (id INT);
         CREATE ROLE \"current_user\";
         GRANT SELECT ON docs TO CURRENT_USER;
         REVOKE SELECT ON docs FROM \"current_user\";",
    )
    .expect("the schema builds");
    assert_eq!(kept.table_grants().count(), 1, "the keyword grant survives");

    let revoked = build(
        "CREATE TABLE docs (id INT);
         CREATE ROLE \"current_user\";
         GRANT SELECT ON docs TO \"current_user\";
         REVOKE SELECT ON docs FROM \"current_user\";",
    )
    .expect("the schema builds");
    assert_eq!(revoked.table_grants().count(), 0, "the role's own grant is revoked");
}
