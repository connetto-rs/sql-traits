//! Which roles a row-level security policy applies to: everyone for `PUBLIC`,
//! otherwise the roles its `TO` clause names and every role that inherits
//! their privileges, as PostgreSQL decides for the session's role.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn applies(database: &ParserDB, policy: &str, role: &str) -> bool {
    database
        .policies()
        .find(|candidate| candidate.name() == policy)
        .expect("the policy exists")
        .applies_to_role(database.role(role).expect("the role exists"), database)
}

#[test]
fn a_public_policy_applies_to_every_role() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE POLICY spelled ON t TO PUBLIC USING (true);
         CREATE POLICY implied ON t USING (true);",
    );

    assert!(applies(&database, "spelled", "reader"));
    assert!(applies(&database, "implied", "reader"));
}

#[test]
fn a_named_policy_applies_to_its_role_and_not_to_others() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE ROLE outsider;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE POLICY p ON t TO reader USING (true);",
    );

    assert!(applies(&database, "p", "reader"));
    assert!(!applies(&database, "p", "outsider"));
}

#[test]
fn a_policy_applies_to_members_in_either_spelling() {
    for membership in
        ["CREATE ROLE app_user IN ROLE reader;", "CREATE ROLE app_user; GRANT reader TO app_user;"]
    {
        let database = parse(&format!(
            "CREATE ROLE reader;
             {membership}
             CREATE TABLE t (id INTEGER PRIMARY KEY);
             CREATE POLICY p ON t TO reader USING (true);"
        ));

        assert!(applies(&database, "p", "app_user"), "{membership}");
    }
}

#[test]
fn a_policy_follows_inheriting_chains_only() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE ROLE staff;
         CREATE ROLE app_user;
         CREATE ROLE auditor;
         GRANT reader TO staff;
         GRANT staff TO app_user;
         GRANT reader TO auditor WITH INHERIT FALSE;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE POLICY to_reader ON t TO reader USING (true);
         CREATE POLICY to_app_user ON t TO app_user USING (true);",
    );

    assert!(applies(&database, "to_reader", "app_user"));
    assert!(!applies(&database, "to_reader", "auditor"));
    assert!(!applies(&database, "to_app_user", "reader"));
}

/// A `TO` entry is an identifier like any other, so quoting decides which
/// role it names, and a quoted `"PUBLIC"` is a role and not everyone.
#[test]
fn a_policy_names_roles_under_identifier_rules() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE ROLE \"Reader\";
         CREATE ROLE \"PUBLIC\";
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE POLICY folded ON t TO READER USING (true);
         CREATE POLICY exact ON t TO \"Reader\" USING (true);
         CREATE POLICY quoted_public ON t TO \"PUBLIC\" USING (true);",
    );

    assert!(applies(&database, "folded", "reader"));
    assert!(!applies(&database, "folded", "Reader"));
    assert!(applies(&database, "exact", "Reader"));
    assert!(!applies(&database, "exact", "reader"));
    assert!(applies(&database, "quoted_public", "PUBLIC"));
    assert!(!applies(&database, "quoted_public", "reader"));
}

/// `CURRENT_USER` names whichever role runs the statement, which the schema
/// never says, so it matches no declared role.
#[test]
fn a_session_principal_names_no_declared_role() {
    let database = parse(
        "CREATE ROLE reader;
         CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE POLICY p ON t TO CURRENT_USER USING (true);",
    );

    assert!(!applies(&database, "p", "reader"));
}
