//! The role a policy's `TO CURRENT_USER`, `TO CURRENT_ROLE` or
//! `TO SESSION_USER` names, which PostgreSQL settles when it runs the
//! statement and stores as that role.
#![allow(clippy::expect_used)]

use sql_traits::{errors::RowSecurityError, prelude::*};
use sqlparser::{ast::Owner, dialect::PostgreSqlDialect};

const ROLES: &str = "CREATE ROLE creator; CREATE ROLE r; CREATE ROLE other;
                     CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
                     ALTER TABLE t ENABLE ROW LEVEL SECURITY;";

fn parse(login: Option<&str>, sql: &str) -> ParserDB {
    let options = ParseOptions::default();
    let options = match login {
        Some(login) => options.with_session_role(login),
        None => options,
    };
    options.parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

/// The stored `TO` list of `policy`, a role by its stored name and a keyword
/// left unsettled by its spelling.
fn targets(database: &ParserDB, policy: &str) -> Vec<String> {
    database
        .policies()
        .find(|candidate| candidate.name() == policy)
        .expect("the policy exists")
        .roles(database)
        .map(|owner| {
            match owner {
                Owner::Ident(ident) => ident.value.clone(),
                keyword => keyword.to_string(),
            }
        })
        .collect()
}

fn row_security<'db>(
    database: &'db ParserDB,
    role: &str,
) -> Result<RowSecurity<'db, <ParserDB as DatabaseLike>::Policy>, RowSecurityError> {
    let role = database.role(role).expect("the role exists");
    database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
        .row_security(RowStatement::Select, role, database)
}

/// The names of the permissive and restrictive policies gating what `role`
/// reads.
fn select_gate(database: &ParserDB, role: &str) -> (Vec<String>, Vec<String>) {
    let security = row_security(database, role).expect("settled");
    let gates = security.gates().expect("the role is not exempt");
    assert_eq!(gates.len(), 1, "a select reads through one gate");
    let gate = gates.first().expect("the select gate");
    let names = |terms: &[GateTerm<'_, <ParserDB as DatabaseLike>::Policy>]| {
        terms.iter().map(|term| term.policy().name().to_string()).collect()
    };
    (names(gate.permissive()), names(gate.restrictive()))
}

/// Each way the session moves between roles, before and after the policy is
/// created, against the role each keyword stores. `CURRENT_ROLE` is another
/// spelling of `CURRENT_USER`, and `SESSION_USER` follows only
/// `SET SESSION AUTHORIZATION`.
#[test]
fn each_keyword_stores_the_role_the_session_runs_as_at_creation() {
    for (login, before, after, current, session) in [
        (Some("creator"), "", "", "creator", "creator"),
        (Some("creator"), "SET ROLE r;", "", "r", "creator"),
        (Some("creator"), "SET role = r;", "", "r", "creator"),
        (Some("creator"), "SET ROLE r; RESET ROLE;", "", "creator", "creator"),
        (Some("creator"), "SET ROLE r; SET ROLE NONE;", "", "creator", "creator"),
        (Some("creator"), "SET SESSION AUTHORIZATION other;", "", "other", "other"),
        (Some("creator"), "SET session_authorization = 'other';", "", "other", "other"),
        (Some("creator"), "SET SESSION AUTHORIZATION other; SET ROLE r;", "", "r", "other"),
        (Some("creator"), "SET ROLE r; SET SESSION AUTHORIZATION other;", "", "other", "other"),
        (
            Some("creator"),
            "SET SESSION AUTHORIZATION other; SET ROLE r; RESET ROLE;",
            "",
            "other",
            "other",
        ),
        (
            Some("creator"),
            "SET SESSION AUTHORIZATION other; RESET SESSION AUTHORIZATION;",
            "",
            "creator",
            "creator",
        ),
        (
            Some("creator"),
            "SET SESSION AUTHORIZATION other; SET SESSION AUTHORIZATION DEFAULT;",
            "",
            "creator",
            "creator",
        ),
        (Some("creator"), "BEGIN; SET LOCAL ROLE r;", "COMMIT;", "r", "creator"),
        (Some("creator"), "BEGIN; SET ROLE r; ROLLBACK;", "", "creator", "creator"),
        // A later move leaves the stored role where it was.
        (Some("creator"), "", "SET ROLE r;", "creator", "creator"),
        (Some("creator"), "", "SET SESSION AUTHORIZATION other;", "creator", "creator"),
        // An unnamed session role leaves its keywords unsettled.
        (None, "", "", "CURRENT_USER", "SESSION_USER"),
        (None, "SET ROLE r;", "", "r", "SESSION_USER"),
        (None, "SET SESSION AUTHORIZATION other;", "", "other", "other"),
        (
            None,
            "SET SESSION AUTHORIZATION other; RESET SESSION AUTHORIZATION;",
            "",
            "CURRENT_USER",
            "SESSION_USER",
        ),
    ] {
        let database = parse(
            login,
            &format!(
                "{ROLES}
                 {before}
                 CREATE POLICY cu ON t TO CURRENT_USER USING (true);
                 CREATE POLICY cr ON t TO CURRENT_ROLE USING (true);
                 CREATE POLICY su ON t TO SESSION_USER USING (true);
                 {after}"
            ),
        );
        let context = format!("{login:?} {before} | {after}");
        let current_role = if current == "CURRENT_USER" { "CURRENT_ROLE" } else { current };
        assert_eq!(targets(&database, "cu"), [current], "{context}");
        assert_eq!(targets(&database, "cr"), [current_role], "{context}");
        assert_eq!(targets(&database, "su"), [session], "{context}");
    }
}

/// `ALTER POLICY ... TO` settles its keywords when it runs, as `CREATE POLICY`
/// does, and an `ALTER POLICY` naming no roles keeps the stored ones.
#[test]
fn alter_policy_stores_the_role_the_session_runs_as_then() {
    let database = parse(
        Some("creator"),
        &format!(
            "{ROLES}
             CREATE POLICY p ON t TO CURRENT_USER USING (true);
             CREATE POLICY q ON t TO CURRENT_USER USING (true);
             SET ROLE r;
             ALTER POLICY p ON t TO CURRENT_USER, SESSION_USER;
             ALTER POLICY q ON t USING (v > 0);
             RESET ROLE;"
        ),
    );
    assert_eq!(targets(&database, "p"), ["r", "creator"]);
    assert_eq!(targets(&database, "q"), ["creator"]);
}

/// The stored role keeps the case of the name the session runs as, and a
/// role spelled `PUBLIC` in quotes stays a role rather than everyone.
#[test]
fn a_stored_role_keeps_its_exact_name() {
    let database = parse(
        Some("Admin"),
        r#"CREATE ROLE "Admin"; CREATE ROLE admin; CREATE ROLE "PUBLIC";
           CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
           ALTER TABLE t ENABLE ROW LEVEL SECURITY;
           CREATE POLICY p ON t TO CURRENT_USER USING (true);
           SET ROLE "PUBLIC";
           CREATE POLICY q ON t TO CURRENT_USER USING (true);"#,
    );
    let policy = |name: &str| {
        database.policies().find(|candidate| candidate.name() == name).expect("the policy exists")
    };
    let role = |name: &str| database.role(name).expect("the role exists");
    assert!(policy("p").applies_to_role(role("Admin"), &database));
    assert!(!policy("p").applies_to_role(role("admin"), &database));
    assert!(!policy("q").applies_to_public());
    assert!(policy("q").applies_to_role(role("PUBLIC"), &database));
    assert!(!policy("q").applies_to_role(role("admin"), &database));
}

/// A policy scoped to its creator neither admits nor restricts a role that is
/// not one, and a member of the creator is both admitted and restricted.
#[test]
fn a_policy_scoped_to_its_creator_gates_only_its_members() {
    for keyword in ["CURRENT_USER", "CURRENT_ROLE", "SESSION_USER"] {
        let database = parse(
            Some("creator"),
            &format!(
                "CREATE ROLE creator; CREATE ROLE app_user; CREATE ROLE heir IN ROLE creator;
                 CREATE ROLE keeper;
                 CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
                 ALTER TABLE t OWNER TO keeper;
                 ALTER TABLE t ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY own ON t TO {keyword} USING (true);
                 CREATE POLICY everyone ON t USING (true);
                 CREATE POLICY barrier ON t AS RESTRICTIVE TO {keyword} USING (v > 0);"
            ),
        );
        assert_eq!(
            select_gate(&database, "app_user"),
            (vec!["everyone".to_string()], Vec::new()),
            "TO {keyword}"
        );
        assert_eq!(
            select_gate(&database, "heir"),
            (vec!["everyone".to_string(), "own".to_string()], vec!["barrier".to_string()]),
            "TO {keyword}"
        );
    }
}

/// A creator the input never declares may hold members the input does not
/// show, so a policy scoped to it stays unsettled, naming that role.
#[test]
fn an_undeclared_creator_stays_unsettled() {
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .with_session_role("creator")
        .parse::<PostgreSqlDialect>(
            "CREATE ROLE app_user;
             CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
             ALTER TABLE t ENABLE ROW LEVEL SECURITY;
             CREATE POLICY p ON t TO CURRENT_USER USING (true);",
        )
        .expect("schema builds");
    assert_eq!(
        row_security(&database, "app_user").map(|_| ()),
        Err(RowSecurityError::UnsettledRole { policy: "p".into(), role: "creator".into() })
    );
}
