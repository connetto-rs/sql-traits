//! Tests that a schema whose access control statements name objects it does not
//! create parses under [`AccessResolution::OpenWorld`], and that the references
//! they left unresolved are reported rather than silently dropped.
//!
//! Both a `GRANT` and a `CREATE POLICY` can name a role the input never
//! creates, and the setting governs both, so one call answers for both.
#![allow(clippy::expect_used)]

use std::path::Path;

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::{
    dialect::{MySqlDialect, PostgreSqlDialect},
    parser::Parser,
};

/// Roles are cluster objects a schema dump never carries, so this is the
/// shape every `pg_dump` of a schema with one grant produces.
const GRANT_TO_ABSENT_ROLE: &str = "CREATE TABLE docs (id uuid PRIMARY KEY);
     GRANT SELECT ON docs TO app;";

fn open_world() -> ParseOptions {
    ParseOptions::default().with_access_resolution(AccessResolution::OpenWorld)
}

#[test]
fn closed_world_is_the_default() {
    assert_eq!(ParseOptions::default().access_resolution(), AccessResolution::ClosedWorld);
    assert_eq!(open_world().access_resolution(), AccessResolution::OpenWorld);
}

/// A grantee that names no role of its own is never an absent role. Neither
/// spelling ever demanded a `CREATE ROLE`, and neither may start.
#[test]
fn a_grantee_that_names_no_role_is_never_reported() {
    let pseudo_role = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         GRANT SELECT ON docs TO GROUP public;",
    )
    .expect("the PUBLIC pseudo-role needs no CREATE ROLE");
    assert_eq!(
        pseudo_role.unresolved_access_references().expect("targets are well formed").count(),
        0
    );

    let user_host = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE docs (id INT PRIMARY KEY);
         GRANT SELECT ON docs TO 'app'@'localhost';",
    )
    .expect("a host-qualified user is not resolved as a role");
    assert_eq!(
        user_host.unresolved_access_references().expect("targets are well formed").count(),
        0
    );
}

#[test]
fn closed_world_still_refuses_every_unresolved_grant_shape() {
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(GRANT_TO_ABSENT_ROLE),
        Err(Error::RoleNotFoundForGrant { role_name }) if role_name == "app"
    ));
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             GRANT SELECT ON other TO PUBLIC;"
        ),
        Err(Error::TableNotFoundForGrant { table_name }) if table_name == "other"
    ));
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             REVOKE SELECT ON docs FROM app;"
        ),
        Err(Error::RoleNotFoundForGrant { role_name }) if role_name == "app"
    ));
}

#[test]
fn open_world_records_a_grant_to_an_absent_role() {
    let db = open_world().parse::<PostgreSqlDialect>(GRANT_TO_ABSENT_ROLE).expect("schema parses");

    assert_eq!(db.table_grants().count(), 1);
    let grant = db.table_grants().next().expect("the grant is recorded");
    let granted_tables: Vec<_> = grant.tables(&db).collect();
    assert_eq!(granted_tables.len(), 1, "the table the grant names is still resolved");

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert!(
        matches!(unresolved[..], [UnresolvedAccessReference::GranteeRole(role)] if role.value == "app"),
        "the absent grantee is reported once: {unresolved:?}"
    );
    assert!(matches!(
        db.validate_access_targets(),
        Err(Error::RoleNotFoundForGrant { role_name }) if role_name == "app"
    ));
}

#[test]
fn open_world_records_a_grant_on_an_absent_table() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             GRANT SELECT ON other TO PUBLIC;",
        )
        .expect("schema parses");

    let grant = db.table_grants().next().expect("the grant is recorded");
    assert_eq!(grant.tables(&db).count(), 0, "an absent table resolves to no table");

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert!(
        matches!(unresolved[..], [UnresolvedAccessReference::GrantTable(table)]
            if table.to_string() == "other"),
        "the absent table is reported once: {unresolved:?}"
    );
    assert!(matches!(
        db.validate_access_targets(),
        Err(Error::TableNotFoundForGrant { table_name }) if table_name == "other"
    ));
}

/// A grant whose unqualified target the closed world resolves through the
/// search path is not a dangling reference.
#[test]
fn a_path_resolved_grant_target_is_not_reported() {
    let sql = "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE docs (id INT);
         CREATE ROLE r;
         GRANT SELECT ON docs TO r;";
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the closed world accepts it");

    let db = open_world().parse::<PostgreSqlDialect>(sql).expect("schema parses");
    assert_eq!(db.unresolved_access_references().expect("targets are well formed").count(), 0);
    db.validate_access_targets().expect("nothing is dangling");
}

/// A grant keeps the table it reached when it was made, as PostgreSQL keeps
/// it, so a later path change neither strands it for the accessors nor for
/// the validator.
#[test]
fn a_grant_keeps_its_table_across_a_later_path_change() {
    let sql = "CREATE SCHEMA app;
         CREATE SCHEMA zzz;
         SET search_path TO app;
         CREATE TABLE docs (id INT);
         CREATE ROLE r;
         GRANT SELECT ON docs TO r;
         SET search_path TO zzz;";
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the closed world resolved it in the moment");

    let db = open_world().parse::<PostgreSqlDialect>(sql).expect("schema parses");
    let grant = db.table_grants().next().expect("the grant is recorded");
    let tables: Vec<_> = grant.tables(&db).collect();
    assert!(matches!(tables[..], [table] if table.table_schema() == Some("app")));
    assert_eq!(db.unresolved_access_references().expect("targets are well formed").count(), 0);
}

#[test]
fn open_world_ignores_a_revoke_matching_no_recorded_grant() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             REVOKE SELECT ON docs FROM app;",
        )
        .expect("schema parses");

    assert_eq!(db.table_grants().count(), 0);
    assert!(db.validate_access_targets().is_ok(), "a dropped revoke leaves nothing unresolved");
}

/// `pg_dump` emits this for every function whose default execute privilege
/// was revoked, so an ordinary dump reaches the revoke path without ever
/// carrying a `GRANT`.
#[test]
fn open_world_accepts_the_revoke_pg_dump_emits_for_functions() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE FUNCTION f() RETURNS integer AS $$ SELECT 1 $$ LANGUAGE sql;
             REVOKE ALL ON FUNCTION f() FROM PUBLIC;",
        )
        .expect("schema parses");

    assert!(
        db.function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
}

/// Under the closed world every access control reference resolves when it is
/// recorded, and each statement that could move the ground under it either
/// carries the reference along or refuses. The accessor therefore reports for
/// the open world, which is the only way to record a reference that does not
/// resolve.
#[test]
fn the_closed_world_leaves_no_access_reference_unresolved() {
    for sql in [
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE ROLE app;
         GRANT SELECT ON docs TO app;
         ALTER TABLE docs RENAME TO papers;",
        "CREATE TABLE docs (id uuid PRIMARY KEY, title text);
         CREATE ROLE app;
         GRANT SELECT (title) ON docs TO app;
         ALTER TABLE docs RENAME TO papers;",
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE ROLE app;
         GRANT SELECT ON docs TO app;
         DROP TABLE docs;",
    ] {
        let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses");
        assert_eq!(
            db.unresolved_access_references().expect("targets are well formed").count(),
            0,
            "{sql}"
        );
    }

    let refused = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE ROLE app;
         GRANT SELECT ON docs TO app;
         DROP ROLE app;",
    );
    assert!(matches!(refused, Err(Error::RoleReferenced { .. })), "got {refused:?}");
}

/// The setting governs a policy as well as a grant, so the report has to cover
/// both. A policy naming a role the input never creates was accepted and
/// reported by nothing, which handed a caller the acceptance without the means
/// to ask about it.
#[test]
fn a_policy_role_the_input_does_not_create_is_reported() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             CREATE POLICY docs_owner ON docs TO auditor USING (true);",
        )
        .expect("the open world records the policy as written");

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert!(
        matches!(unresolved[..], [UnresolvedAccessReference::PolicyRole { policy, role }]
            if policy.value == "docs_owner" && role.value == "auditor"),
        "the policy and the role it applies to are both named: {unresolved:?}"
    );

    assert!(
        matches!(
            db.validate_access_targets(),
            Err(Error::RoleNotFoundForPolicy { ref role_name, ref policy_name })
                if role_name == "auditor" && policy_name == "docs_owner"
        ),
        "enforcing closure afterwards refuses the policy too"
    );
}

/// A grant and a policy naming the same absent role are two references, since
/// each names the statement it came from and a caller has to be able to tell
/// which needs fixing.
#[test]
fn a_grant_and_a_policy_naming_the_same_absent_role_are_reported_apart() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             GRANT SELECT ON docs TO app;
             CREATE POLICY docs_app ON docs TO app USING (true);",
        )
        .expect("the open world records both as written");

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert_eq!(unresolved.len(), 2, "{unresolved:?}");
    assert!(
        unresolved.iter().any(|r| {
            matches!(r, UnresolvedAccessReference::GranteeRole(role)
            if role.value == "app")
        }),
        "the grantee is reported: {unresolved:?}"
    );
    assert!(
        unresolved.iter().any(|r| {
            matches!(r, UnresolvedAccessReference::PolicyRole { role, .. }
            if role.value == "app")
        }),
        "the policy role is reported: {unresolved:?}"
    );
}

/// The pseudo-roles never demanded a declaration, for a policy exactly as for a
/// grant, so neither shows up as unresolved.
#[test]
fn a_policy_naming_no_role_of_its_own_is_never_reported() {
    for target in ["PUBLIC", "CURRENT_USER", "SESSION_USER"] {
        let db = open_world()
            .parse::<PostgreSqlDialect>(&format!(
                "CREATE TABLE docs (id uuid PRIMARY KEY);
                 CREATE POLICY docs_all ON docs TO {target} USING (true);"
            ))
            .expect("a pseudo-role needs no CREATE ROLE");
        assert_eq!(
            db.unresolved_access_references().expect("targets are well formed").count(),
            0,
            "{target}"
        );
    }
}

#[test]
fn open_world_still_refuses_an_unrepresentable_revoke() {
    let unsupported = open_world().parse::<PostgreSqlDialect>(
        "CREATE TABLE docs (id uuid PRIMARY KEY, title text);
         CREATE ROLE app;
         GRANT SELECT ON docs TO app;
         REVOKE SELECT (title) ON docs FROM app;",
    );

    assert!(
        matches!(unsupported, Err(Error::UnsupportedRevoke { .. })),
        "the open world widens which references resolve, not what the model can represent"
    );
}

/// The parse-time check reads a half-built schema, so it can only see roles
/// declared above the grant. The post-parse check runs against the finished
/// database and is therefore insensitive to statement order.
#[test]
fn validation_after_parsing_resolves_a_role_created_below_the_grant() {
    let sql = "CREATE TABLE docs (id uuid PRIMARY KEY);
               GRANT SELECT ON docs TO app;
               CREATE ROLE app;";

    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(sql),
        Err(Error::RoleNotFoundForGrant { .. })
    ));

    let db = open_world().parse::<PostgreSqlDialect>(sql).expect("schema parses");
    assert_eq!(db.unresolved_access_references().expect("targets are well formed").count(), 0);
    assert!(db.validate_access_targets().is_ok());
}

#[test]
fn open_world_records_a_column_grant_to_an_absent_role() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY, title text);
             GRANT SELECT (id, title) ON docs TO app;",
        )
        .expect("schema parses");

    let grant = db.column_grants().next().expect("the column grant is recorded");
    let table = grant.table(&db).expect("the table the grant names is resolved");
    assert_eq!(grant.columns(table, &db).expect("columns resolve").count(), 2);

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert!(
        matches!(unresolved[..], [UnresolvedAccessReference::GranteeRole(role)] if role.value == "app"),
        "the grant stored in both views is reported once: {unresolved:?}"
    );
}

#[test]
fn an_absent_reference_named_twice_is_reported_once() {
    let db = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             GRANT SELECT ON docs TO app;
             GRANT INSERT ON docs TO app, auditor;",
        )
        .expect("schema parses");

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert_eq!(unresolved.len(), 2, "one entry per distinct role: {unresolved:?}");
}

#[test]
fn a_grant_target_that_cannot_denote_a_table_is_reported_as_a_lookup_error() {
    let db = open_world()
        .parse::<PostgreSqlDialect>("GRANT SELECT ON catalog.public.docs TO PUBLIC;")
        .expect("schema parses");

    assert!(matches!(
        db.unresolved_access_references().err(),
        Some(LookupError::InvalidObjectName { .. })
    ));
    assert!(matches!(db.validate_access_targets(), Err(Error::IdentifierLookupError(_))));
}

#[test]
fn options_apply_to_statements_parsed_by_the_caller() {
    let statements =
        Parser::parse_sql(&PostgreSqlDialect {}, GRANT_TO_ABSENT_ROLE).expect("SQL parses");

    assert!(matches!(
        ParserDB::from_statements(statements.clone(), "docs".to_string()),
        Err(Error::RoleNotFoundForGrant { .. })
    ));

    let db = open_world().from_statements(statements, "docs".to_string()).expect("schema parses");
    assert_eq!(db.catalog_name(), "docs");
    assert_eq!(db.table_grants().count(), 1);
}

#[test]
fn options_apply_to_a_directory_of_migrations() {
    let directory =
        std::env::temp_dir().join(format!("sql-traits-open-world-grants-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("the temporary directory is writable");
    let migration = directory.join("up.sql");
    std::fs::write(&migration, GRANT_TO_ABSENT_ROLE).expect("the migration is writable");

    assert!(matches!(
        ParserDB::from_path::<PostgreSqlDialect>(&directory),
        Err(Error::RoleNotFoundForGrant { .. })
    ));

    let from_directory =
        open_world().from_path::<PostgreSqlDialect>(&directory).expect("migrations parse");
    assert_eq!(from_directory.table_grants().count(), 1);

    let from_file = open_world()
        .from_paths::<PostgreSqlDialect>(&[Path::new(&migration)])
        .expect("migration parses");
    assert_eq!(from_file.table_grants().count(), 1);

    std::fs::remove_dir_all(&directory).expect("the temporary directory is removable");
}
