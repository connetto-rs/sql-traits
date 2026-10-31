//! Tests for `ALTER DEFAULT PRIVILEGES` and the role that creates each object.
//!
//! PostgreSQL keys a default by the role that will create the object, the
//! current role as `SET ROLE` and `SET SESSION AUTHORIZATION` leave it, and
//! applies it when that role creates an object of the kind it names, in the
//! schemas it names. The creating role also becomes the object's owner. The
//! expectations below were read from PostgreSQL 17 and 18.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*, traits::grant::GrantTarget};
use sqlparser::{
    ast::{Action, CreateFunction, CreateRole},
    dialect::{MySqlDialect, PostgreSqlDialect},
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn open_world(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
}

/// The function `name` resolves to, `schema.function` naming one outside the
/// search path.
fn function<'db>(db: &'db ParserDB, name: &str) -> &'db CreateFunction {
    let target = match name.split_once('.') {
        Some((schema, function)) => TargetName::new(function, false).with_schema(schema, false),
        None => TargetName::new(name, false),
    };
    db.function_by_target(target, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the function is declared")
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table is declared")
}

fn role<'db>(db: &'db ParserDB, name: &str) -> &'db CreateRole {
    db.role(name).expect("the role is declared")
}

fn can_execute(db: &ParserDB, function_name: &str, role_name: &str) -> bool {
    function(db, function_name).can_execute(role(db, role_name), db).expect("function is held")
}

fn executable_by_public(db: &ParserDB, function_name: &str) -> bool {
    function(db, function_name).is_executable_by_public(db).expect("function is held")
}

fn function_owner<'db>(db: &'db ParserDB, function_name: &str) -> Option<&'db str> {
    function(db, function_name).owner(db).expect("function is held")
}

/// The privileges the grants on the relation called `relation` give `role`,
/// each spelled as PostgreSQL prints it.
fn relation_privileges(db: &ParserDB, relation: &str, role_name: &str) -> Vec<String> {
    let role = role(db, role_name);
    let mut privileges: Vec<String> = db
        .table_grants()
        .filter(|grant| {
            grant.relations(db).any(|reached| {
                let name = match reached {
                    Relation::Table(table) => table.table_name(),
                    Relation::View(view) => view.view_name(),
                    Relation::MaterializedView(view) => view.view_name(),
                    Relation::Sequence(sequence) => sequence.sequence_name(),
                };
                name == relation
            }) && grant.applies_to_role(role)
        })
        .flat_map(|grant| grant.privileges(db).map(ToString::to_string).collect::<Vec<_>>())
        .collect();
    privileges.sort();
    privileges
}

/// The privileges the grants on the schema called `schema` give `role`.
fn schema_privileges(db: &ParserDB, schema: &str, role_name: &str) -> Vec<String> {
    let role = role(db, role_name);
    let mut privileges: Vec<String> = db
        .table_grants()
        .filter(|grant| {
            let on_schema = match grant.target() {
                GrantTarget::Schemas(mut names) => names.any(|name| name.name() == schema),
                _ => false,
            };
            on_schema && grant.applies_to_role(role)
        })
        .flat_map(|grant| grant.privileges(db).map(ToString::to_string).collect::<Vec<_>>())
        .collect();
    privileges.sort();
    privileges
}

/// The schema the request measured, where a default of `owner_role` gives
/// `anon` its own functions in `public` and another takes `PUBLIC`'s away.
const MEASURED: &str = "
CREATE ROLE anon; CREATE ROLE owner_role; CREATE ROLE other_owner;
GRANT CREATE ON SCHEMA public TO owner_role, other_owner;
CREATE FUNCTION public.before() RETURNS INT LANGUAGE sql AS 'SELECT 1';
ALTER DEFAULT PRIVILEGES FOR ROLE owner_role IN SCHEMA public GRANT EXECUTE ON FUNCTIONS TO anon;
ALTER DEFAULT PRIVILEGES FOR ROLE owner_role REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
SET SESSION AUTHORIZATION owner_role;
CREATE FUNCTION public.after_owned() RETURNS INT LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION public.revoked() RETURNS INT LANGUAGE sql AS 'SELECT 1';
REVOKE EXECUTE ON FUNCTION public.revoked() FROM anon;
RESET SESSION AUTHORIZATION;
SET SESSION AUTHORIZATION other_owner;
CREATE FUNCTION public.after_other() RETURNS INT LANGUAGE sql AS 'SELECT 1';
RESET SESSION AUTHORIZATION;
";

#[test]
fn a_default_reaches_the_functions_its_role_creates_afterwards() {
    let db = parse(MEASURED).expect("schema parses");

    for (name, anon, public) in [
        ("before", true, true),
        ("after_owned", true, false),
        ("revoked", false, false),
        ("after_other", true, true),
    ] {
        assert_eq!(can_execute(&db, name, "anon"), anon, "{name}: anon");
        assert_eq!(executable_by_public(&db, name), public, "{name}: PUBLIC");
    }
    // The owner keeps the entry the default left it.
    assert!(can_execute(&db, "revoked", "owner_role"));
    assert!(!can_execute(&db, "revoked", "other_owner"));
}

#[test]
fn the_creating_role_owns_what_it_creates() {
    let db = parse(MEASURED).expect("schema parses");

    assert_eq!(function_owner(&db, "before"), None);
    assert_eq!(function_owner(&db, "after_owned"), Some("owner_role"));
    assert_eq!(function_owner(&db, "after_other"), Some("other_owner"));
}

/// Without `FOR ROLE` a default belongs to the role running it, which in a
/// schema that never sets one is the role creating every later function.
#[test]
fn a_default_without_for_role_reaches_every_later_function_of_an_unnamed_session() {
    let db = parse(
        "CREATE ROLE anon;
         CREATE FUNCTION early() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         CREATE FUNCTION late() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(executable_by_public(&db, "early"));
    assert!(!executable_by_public(&db, "late"));
    assert!(!can_execute(&db, "late", "anon"));
}

#[test]
fn a_default_for_a_named_role_skips_functions_of_an_unnamed_session() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r;
         ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         CREATE FUNCTION unnamed() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         SET ROLE r;
         CREATE FUNCTION named() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(executable_by_public(&db, "unnamed"));
    assert!(!executable_by_public(&db, "named"));
}

/// A default run while a role is set belongs to that role.
#[test]
fn a_default_without_for_role_belongs_to_the_role_running_it() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r;
         SET ROLE r;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         RESET ROLE;
         CREATE FUNCTION unnamed() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         SET ROLE r;
         CREATE FUNCTION named() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(executable_by_public(&db, "unnamed"));
    assert!(!executable_by_public(&db, "named"));
}

/// A per-schema default only adds to the global one. Its revoke takes nothing
/// the global or built-in default gives, and takes back what a per-schema
/// grant gave.
#[test]
fn a_per_schema_default_only_adds() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r;
         CREATE SCHEMA s; CREATE SCHEMA t;
         ALTER DEFAULT PRIVILEGES FOR ROLE r IN SCHEMA s REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE r IN SCHEMA s, t
             GRANT EXECUTE ON FUNCTIONS TO anon WITH GRANT OPTION;
         ALTER DEFAULT PRIVILEGES FOR ROLE r IN SCHEMA t REVOKE EXECUTE ON FUNCTIONS FROM anon;
         SET ROLE r;
         CREATE FUNCTION public.in_public() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION s.in_s() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION t.in_t() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");
    let anon = role(&db, "anon");

    for name in ["in_public", "s.in_s", "t.in_t"] {
        assert!(executable_by_public(&db, name), "{name}");
    }
    assert_eq!(function(&db, "s.in_s").can_grant_execute(anon, &db), Ok(true));
    assert_eq!(function(&db, "t.in_t").can_grant_execute(anon, &db), Ok(false));
    assert_eq!(function(&db, "in_public").can_grant_execute(anon, &db), Ok(false));
}

#[test]
fn a_global_default_can_take_the_owner_entry() {
    let db = parse(
        "CREATE ROLE r;
         ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON ROUTINES FROM r;
         SET ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");
    let r = role(&db, "r");

    assert_eq!(function(&db, "f").can_execute(r, &db), Ok(false));
    // Ownership still lets the owner grant the privilege back to itself.
    assert_eq!(function(&db, "f").can_grant_execute(r, &db), Ok(true));
}

/// `CURRENT_USER` names the role running the default, not the role it is for.
#[test]
fn a_session_principal_grantee_names_the_role_running_the_default() {
    let db = parse(
        "CREATE ROLE r; CREATE ROLE other; CREATE ROLE third;
         SET ROLE other;
         ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT EXECUTE ON FUNCTIONS TO CURRENT_USER;
         ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         SET ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(can_execute(&db, "f", "other"));
    assert!(!can_execute(&db, "f", "third"));
}

#[test]
fn a_replaced_function_keeps_its_privileges() {
    let db = parse(
        "CREATE ROLE anon;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    )
    .expect("schema parses");

    assert!(executable_by_public(&db, "f"));
}

#[test]
fn extension_functions_take_the_defaults() {
    let db = parse(
        r#"ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
           CREATE EXTENSION "uuid-ossp";"#,
    )
    .expect("schema parses");

    assert!(!executable_by_public(&db, "uuid_nil"));
}

#[test]
fn a_rolled_back_default_reaches_nothing() {
    let db = parse(
        "BEGIN;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ROLLBACK;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(executable_by_public(&db, "f"));
}

#[test]
fn a_default_follows_a_renamed_grantee_and_a_renamed_schema() {
    let db = parse(
        "CREATE ROLE anon; CREATE SCHEMA ds;
         ALTER DEFAULT PRIVILEGES IN SCHEMA ds GRANT EXECUTE ON FUNCTIONS TO anon;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER ROLE anon RENAME TO anon2;
         ALTER SCHEMA ds RENAME TO ds2;
         CREATE FUNCTION ds2.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(can_execute(&db, "ds2.f", "anon2"));
}

#[test]
fn a_dropped_schema_takes_its_defaults() {
    let db = parse(
        "CREATE ROLE anon; CREATE SCHEMA ds;
         ALTER DEFAULT PRIVILEGES IN SCHEMA ds GRANT EXECUTE ON FUNCTIONS TO anon;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         DROP SCHEMA ds;
         CREATE SCHEMA ds;
         CREATE FUNCTION ds.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(!can_execute(&db, "ds.f", "anon"));
}

/// The role a function is created under, after each way of moving between
/// roles, read back as the owner the function records.
#[test]
fn session_statements_move_the_creating_role_as_postgresql_does() {
    for (session, owner) in [
        ("SET ROLE r;", Some("r")),
        ("SET SESSION ROLE r;", Some("r")),
        ("SET role = r;", Some("r")),
        ("SET ROLE r; SET ROLE NONE;", None),
        ("SET ROLE r; RESET ROLE;", None),
        ("SET ROLE r; SET role = none;", None),
        ("SET ROLE r; SET role = DEFAULT;", None),
        ("SET ROLE r; RESET ALL;", Some("r")),
        ("SET SESSION AUTHORIZATION other;", Some("other")),
        ("SET session_authorization = 'other';", Some("other")),
        ("SET ROLE r; SET SESSION AUTHORIZATION other;", Some("other")),
        ("SET SESSION AUTHORIZATION other; SET ROLE r; RESET ROLE;", Some("other")),
        ("SET SESSION AUTHORIZATION other; SET SESSION AUTHORIZATION DEFAULT;", None),
        ("SET SESSION AUTHORIZATION other; RESET SESSION AUTHORIZATION;", None),
        ("SET SESSION AUTHORIZATION other; RESET session_authorization;", None),
        ("SET LOCAL ROLE r;", None),
        ("BEGIN; SET LOCAL ROLE r; COMMIT;", None),
        ("BEGIN; SET ROLE r; ROLLBACK;", None),
        ("BEGIN; SET ROLE r; COMMIT;", Some("r")),
        ("BEGIN; SET LOCAL ROLE r; SET ROLE other; COMMIT;", Some("other")),
        ("BEGIN; SET ROLE other; SET LOCAL ROLE r; COMMIT;", Some("other")),
        ("BEGIN; SAVEPOINT a; SET ROLE r; ROLLBACK TO a; COMMIT;", None),
        ("BEGIN; SET LOCAL session_authorization = 'other'; COMMIT;", None),
        ("BEGIN; SET ROLE r; SET LOCAL session_authorization = 'other'; COMMIT;", Some("r")),
        ("BEGIN; SET LOCAL ROLE r; SET SESSION AUTHORIZATION other; COMMIT;", Some("other")),
        ("BEGIN; SET LOCAL session_authorization = 'other'; SET ROLE r; COMMIT;", Some("r")),
    ] {
        let db = parse(&format!(
            "CREATE ROLE r; CREATE ROLE other;
             {session}
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';"
        ))
        .expect(session);

        assert_eq!(function_owner(&db, "f"), owner, "{session}");
    }
}

#[test]
fn a_function_created_inside_a_local_role_belongs_to_it() {
    let db = parse(
        "CREATE ROLE r;
         BEGIN; SET LOCAL ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         COMMIT;",
    )
    .expect("schema parses");

    assert_eq!(function_owner(&db, "f"), Some("r"));
}

#[test]
fn a_quoted_role_keeps_its_case() {
    let db = parse(
        r#"CREATE ROLE "Admin";
           SET ROLE "Admin";
           CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
           SET ROLE 'Admin';
           CREATE FUNCTION g() RETURNS INT LANGUAGE sql AS 'SELECT 1';"#,
    )
    .expect("schema parses");

    assert_eq!(function_owner(&db, "f"), Some("Admin"));
    assert_eq!(function_owner(&db, "g"), Some("Admin"));
}

#[test]
fn an_owner_change_still_wins() {
    let db = parse(
        "CREATE ROLE r; CREATE ROLE other;
         SET ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE t (id INT);
         RESET ROLE;
         ALTER FUNCTION f() OWNER TO other;
         ALTER TABLE t OWNER TO other;",
    )
    .expect("schema parses");

    assert_eq!(function_owner(&db, "f"), Some("other"));
    assert_eq!(table(&db, "t").owner(&db), Ok(Some("other")));
}

#[test]
fn every_owned_kind_takes_the_creating_role() {
    let db = parse(
        "CREATE ROLE r;
         SET ROLE r;
         CREATE SCHEMA rs;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT 1 AS x;
         CREATE MATERIALIZED VIEW mv AS SELECT 1 AS x;",
    )
    .expect("schema parses");

    assert_eq!(db.schema("rs").and_then(SchemaLike::authorization), Some("r"));
    assert_eq!(table(&db, "t").owner(&db), Ok(Some("r")));
    let view = db.views().find(|view| view.view_name() == "v").expect("view is declared");
    assert_eq!(db.view_metadata(view).and_then(|metadata| metadata.owner()), Some("r"));
    let materialized = db
        .materialized_views()
        .find(|view| view.view_name() == "mv")
        .expect("materialized view is declared");
    assert_eq!(
        db.materialized_view_metadata(materialized).and_then(|metadata| metadata.owner()),
        Some("r")
    );
}

#[test]
fn other_engines_leave_owners_unnamed() {
    let db = ParserDB::parse::<MySqlDialect>("SET ROLE r; CREATE TABLE t (id INT);")
        .expect("schema parses");

    assert_eq!(table(&db, "t").owner(&db), Ok(None));
}

#[test]
fn table_defaults_reach_tables_views_and_their_owner() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r; CREATE ROLE svc;
         ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT SELECT, INSERT ON TABLES TO anon;
         ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT ALL ON TABLES TO svc WITH GRANT OPTION;
         ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE TRUNCATE, DELETE ON TABLES FROM r;
         CREATE TABLE before (id INT);
         SET ROLE r;
         CREATE TABLE t (id INT);
         CREATE TABLE revoked (id INT);
         REVOKE INSERT ON revoked FROM anon;
         CREATE VIEW v AS SELECT 1 AS x;
         CREATE MATERIALIZED VIEW mv AS SELECT 1 AS x;",
    )
    .expect("schema parses");
    let anon = role(&db, "anon");
    let r = role(&db, "r");
    let t = table(&db, "t");

    assert_eq!(t.can_select(anon, &db), Ok(true));
    assert_eq!(t.can_insert(anon, &db), Ok(true));
    assert_eq!(t.can_delete(anon, &db), Ok(false));
    assert_eq!(t.can_delete(r, &db), Ok(false));
    assert_eq!(t.can_update(r, &db), Ok(true));
    assert!(
        !t.owner_privileges(&db)
            .expect("table is held")
            .any(|held| matches!(held, Action::Truncate))
    );
    assert_eq!(table(&db, "before").can_select(anon, &db), Ok(false));
    assert_eq!(table(&db, "revoked").can_select(anon, &db), Ok(true));
    assert_eq!(table(&db, "revoked").can_insert(anon, &db), Ok(false));
    assert_eq!(relation_privileges(&db, "v", "anon"), ["INSERT", "SELECT"]);
    assert_eq!(relation_privileges(&db, "mv", "anon"), ["INSERT", "SELECT"]);
    assert!(db.table_grants().any(|grant| {
        grant.with_grant_option()
            && grant.applies_to_role(role(&db, "svc"))
            && grant.tables(&db).any(|granted| granted.table_name() == "t")
    }));
}

#[test]
fn sequence_defaults_reach_declared_and_serial_sequences() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r;
         ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT USAGE ON SEQUENCES TO anon;
         SET ROLE r;
         CREATE SEQUENCE s;
         CREATE TABLE t (id SERIAL);",
    )
    .expect("schema parses");

    assert_eq!(relation_privileges(&db, "s", "anon"), ["USAGE"]);
    assert_eq!(relation_privileges(&db, "t_id_seq", "anon"), ["USAGE"]);
    assert_eq!(relation_privileges(&db, "t", "anon"), Vec::<String>::new());
}

#[test]
fn schema_defaults_reach_schemas_of_their_owner() {
    let db = parse(
        "CREATE ROLE anon; CREATE ROLE r;
         ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT USAGE ON SCHEMAS TO anon;
         CREATE SCHEMA unnamed;
         CREATE SCHEMA authorized AUTHORIZATION r;
         SET ROLE r;
         CREATE SCHEMA created;",
    )
    .expect("schema parses");

    assert_eq!(schema_privileges(&db, "unnamed", "anon"), Vec::<String>::new());
    assert_eq!(schema_privileges(&db, "authorized", "anon"), ["USAGE"]);
    assert_eq!(schema_privileges(&db, "created", "anon"), ["USAGE"]);
}

#[test]
fn postgresql_refusals_are_kept() {
    for (sql, expected) in [
        (
            "ALTER DEFAULT PRIVILEGES IN SCHEMA nosuch GRANT SELECT ON TABLES TO PUBLIC;",
            "SchemaNotFoundForDefaultPrivileges",
        ),
        (
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE ON SCHEMAS TO PUBLIC;",
            "DefaultPrivilegesInSchema",
        ),
        ("ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON TABLES TO PUBLIC;", "InvalidDefaultPrivilege"),
        (
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON FUNCTIONS TO PUBLIC;",
            "InvalidDefaultPrivilege",
        ),
        (
            "ALTER DEFAULT PRIVILEGES FOR ROLE nosuch GRANT SELECT ON TABLES TO PUBLIC;",
            "RoleNotFoundForGrant",
        ),
        ("ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO nosuch;", "RoleNotFoundForGrant"),
        ("SET ROLE nosuch;", "RoleNotFoundForSession"),
        ("SET SESSION AUTHORIZATION nosuch;", "RoleNotFoundForSession"),
        (
            "CREATE ROLE r; CREATE ROLE anon;
             ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT SELECT ON TABLES TO anon;
             DROP ROLE anon;",
            "RoleReferenced",
        ),
        (
            "CREATE ROLE r;
             ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT SELECT ON TABLES TO PUBLIC;
             DROP ROLE r;",
            "RoleReferenced",
        ),
        ("CREATE ROLE r; SET ROLE r; DROP ROLE r;", "RoleInUseBySession"),
        (
            "CREATE ROLE r; SET SESSION AUTHORIZATION r; ALTER ROLE r RENAME TO s;",
            "RoleInUseBySession",
        ),
    ] {
        let error = parse(sql).expect_err(sql);
        assert!(format!("{error:?}").starts_with(expected), "{sql}: {error:?}");
    }
}

#[test]
fn the_open_world_lets_undeclared_roles_through() {
    let db = open_world(
        "ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         SET ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(!executable_by_public(&db, "f"));
    assert_eq!(function_owner(&db, "f"), Some("r"));
}

#[test]
fn a_default_restored_to_the_builtin_one_changes_nothing() {
    let db = parse(
        "CREATE ROLE anon;
         ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO PUBLIC;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert!(executable_by_public(&db, "f"));
}
