//! Tests for who may execute a function.
//!
//! PostgreSQL grants `EXECUTE` on a new function to `PUBLIC` and to its owner,
//! and `GRANT`/`REVOKE ... ON FUNCTION` edit that list per function. A
//! `SECURITY DEFINER` body runs with its owner's rights, so whether `PUBLIC`
//! still holds `EXECUTE` decides whether any caller can borrow them.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::CreateFunction, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn open_world(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
}

/// The one function the schema declares under `name` with these argument types.
fn function<'db>(db: &'db ParserDB, name: &str, arguments: &[&str]) -> &'db CreateFunction {
    let matching: Vec<_> = db
        .functions()
        .filter(|function| {
            function.name() == name && function.normalized_argument_type_names(db) == arguments
        })
        .collect();
    assert_eq!(matching.len(), 1, "{name}({arguments:?}) is declared once");
    matching[0]
}

fn role<'db>(db: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateRole {
    db.role(name).expect("the role is declared")
}

#[test]
fn a_new_function_is_executable_by_everyone() {
    let db = parse(
        "CREATE ROLE anon;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);

    assert_eq!(f.is_executable_by_public(&db), Ok(true));
    assert_eq!(f.can_execute(role(&db, "anon"), &db), Ok(true));
    assert_eq!(f.can_grant_execute(role(&db, "anon"), &db), Ok(false));
}

#[test]
fn revoking_from_public_takes_execute_from_every_role_without_its_own_grant() {
    for revoke in [
        "REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;",
        // The spelling pg_dump writes.
        "REVOKE ALL ON FUNCTION public.f() FROM PUBLIC;",
        "REVOKE ALL PRIVILEGES ON FUNCTION f FROM PUBLIC;",
    ] {
        let db = parse(&format!(
            "CREATE ROLE anon;
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
             {revoke}"
        ))
        .expect("schema parses");
        let f = function(&db, "f", &[]);

        assert_eq!(f.is_executable_by_public(&db), Ok(false), "{revoke}");
        assert_eq!(f.can_execute(role(&db, "anon"), &db), Ok(false), "{revoke}");
    }
}

/// A grant to a role reaches that role alone, and a grant back to `PUBLIC`
/// restores the default.
#[test]
fn grants_after_the_revoke_reach_exactly_their_grantees() {
    let db = parse(
        "CREATE ROLE anon;
         CREATE ROLE authenticated;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION g() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f(), g() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION f() TO authenticated;
         GRANT EXECUTE ON FUNCTION g() TO PUBLIC;",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);
    let g = function(&db, "g", &[]);

    assert_eq!(f.is_executable_by_public(&db), Ok(false));
    assert_eq!(f.can_execute(role(&db, "authenticated"), &db), Ok(true));
    assert_eq!(f.can_execute(role(&db, "anon"), &db), Ok(false));
    assert_eq!(g.is_executable_by_public(&db), Ok(true));
    assert_eq!(g.can_execute(role(&db, "anon"), &db), Ok(true));
}

/// Supabase grants `anon` its own entry through default privileges, which a
/// revoke from `PUBLIC` leaves in place.
#[test]
fn a_role_keeps_its_own_grant_after_public_loses_execute() {
    let db = parse(
        "CREATE ROLE anon;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION f() TO anon;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);

    assert_eq!(f.is_executable_by_public(&db), Ok(false));
    assert_eq!(f.can_execute(role(&db, "anon"), &db), Ok(true));

    let revoked = parse(
        "CREATE ROLE anon;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION f() TO anon;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC, anon;",
    )
    .expect("schema parses");
    assert_eq!(
        function(&revoked, "f", &[]).can_execute(role(&revoked, "anon"), &revoked),
        Ok(false)
    );
}

#[test]
fn a_signature_reaches_only_its_own_overload() {
    let db = parse(
        "CREATE FUNCTION f(int) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(text) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f(integer) FROM PUBLIC;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;",
    )
    .expect("schema parses");

    assert_eq!(function(&db, "f", &["INT"]).is_executable_by_public(&db), Ok(false));
    assert_eq!(function(&db, "f", &[]).is_executable_by_public(&db), Ok(false));
    assert_eq!(function(&db, "f", &["TEXT"]).is_executable_by_public(&db), Ok(true));
}

/// pg_dump writes the identity arguments with their names and modes, and an
/// `OUT` argument is no part of the identity.
#[test]
fn a_dumped_signature_with_argument_names_resolves() {
    let db = parse(
        "CREATE FUNCTION public.f(p_id uuid, OUT total int) LANGUAGE sql AS 'SELECT 1';
         REVOKE ALL ON FUNCTION public.f(p_id uuid) FROM PUBLIC;",
    )
    .expect("schema parses");

    let f = db
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");
    assert_eq!(f.is_executable_by_public(&db), Ok(false));
}

/// An unquoted name folds to lowercase and a quoted one keeps its case, so `F`
/// and `f` name one function and `"F"` names another.
#[test]
fn a_function_target_folds_like_any_other_name() {
    let db = parse(
        "CREATE FUNCTION F() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;",
    )
    .expect("schema parses");
    assert_eq!(function(&db, "F", &[]).is_executable_by_public(&db), Ok(false));

    let quoted = parse(
        r#"CREATE FUNCTION F() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION "F"() FROM PUBLIC;"#,
    );
    assert!(
        matches!(&quoted, Err(Error::FunctionNotFoundForGrant { function_name }) if function_name == r#""F"()"#),
        "got {quoted:?}"
    );
}

/// Without an argument list the name has to be unique, and PostgreSQL refuses
/// it otherwise.
#[test]
fn a_bare_name_reaches_the_only_function_of_that_name() {
    let db = parse(
        "CREATE FUNCTION f(uuid) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f FROM PUBLIC;",
    )
    .expect("schema parses");
    assert_eq!(function(&db, "f", &["UUID"]).is_executable_by_public(&db), Ok(false));

    let overloaded = parse(
        "CREATE FUNCTION f(uuid) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(int) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f FROM PUBLIC;",
    );
    assert!(
        matches!(&overloaded, Err(Error::AmbiguousFunctionForGrant { function_name }) if function_name == "f"),
        "got {overloaded:?}"
    );

    // An empty list names the zero-argument overload rather than the name.
    let empty = parse(
        "CREATE FUNCTION f(uuid) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;",
    );
    assert!(
        matches!(&empty, Err(Error::FunctionNotFoundForGrant { function_name }) if function_name == "f()"),
        "got {empty:?}"
    );
}

#[test]
fn a_qualifier_keeps_functions_of_other_schemas_apart() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION app.f FROM PUBLIC;",
    )
    .expect("schema parses");
    let in_app = db
        .functions()
        .find(|function| function.target_name().schema() == Some("app"))
        .expect("app.f is declared");
    let in_public = db
        .functions()
        .find(|function| function.target_name().schema().is_none())
        .expect("f is declared");

    assert_eq!(in_app.is_executable_by_public(&db), Ok(false));
    assert_eq!(in_public.is_executable_by_public(&db), Ok(true));
}

/// The schema-wide form covers the functions the schema holds when it runs,
/// and none created afterwards.
#[test]
fn all_functions_in_schema_covers_the_functions_already_there() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.before() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION outside() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA app FROM PUBLIC;
         CREATE FUNCTION app.after() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert_eq!(function(&db, "before", &[]).is_executable_by_public(&db), Ok(false));
    assert_eq!(function(&db, "outside", &[]).is_executable_by_public(&db), Ok(true));
    assert_eq!(function(&db, "after", &[]).is_executable_by_public(&db), Ok(true));

    let public = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC;",
    )
    .expect("schema parses");
    assert_eq!(
        function(&public, "f", &[]).is_executable_by_public(&public),
        Ok(false),
        "an unqualified function lives in public"
    );
}

/// `ROUTINE` names a function or a procedure, and this model holds only
/// functions, so it reaches exactly what `FUNCTION` reaches.
#[test]
fn a_routine_target_reaches_its_function() {
    let db = parse(
        "CREATE ROLE bob;
         CREATE FUNCTION f(INT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(TEXT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON ROUTINE f(INT) FROM PUBLIC;
         GRANT EXECUTE ON ROUTINE f(INT) TO bob WITH GRANT OPTION;",
    )
    .expect("schema parses");
    let bob = role(&db, "bob");
    let reached = function(&db, "f", &["INT"]);
    let other = function(&db, "f", &["TEXT"]);

    assert_eq!(reached.is_executable_by_public(&db), Ok(false));
    assert_eq!(reached.can_execute(bob, &db), Ok(true));
    assert_eq!(reached.can_grant_execute(bob, &db), Ok(true));
    assert_eq!(other.is_executable_by_public(&db), Ok(true));
    assert_eq!(other.can_grant_execute(bob, &db), Ok(false));
}

/// `ALL ROUTINES` covers the functions the schema holds when it runs, like
/// `ALL FUNCTIONS`.
#[test]
fn all_routines_in_schema_covers_the_functions_already_there() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.before() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION outside() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON ALL ROUTINES IN SCHEMA app FROM PUBLIC;
         CREATE FUNCTION app.after() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");

    assert_eq!(function(&db, "before", &[]).is_executable_by_public(&db), Ok(false));
    assert_eq!(function(&db, "outside", &[]).is_executable_by_public(&db), Ok(true));
    assert_eq!(function(&db, "after", &[]).is_executable_by_public(&db), Ok(true));
}

/// `ALL PROCEDURES` leaves every function alone, as no function is a
/// procedure.
#[test]
fn all_procedures_in_schema_reaches_no_function() {
    let db = parse(
        "CREATE ROLE bob;
         CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON ALL PROCEDURES IN SCHEMA app FROM PUBLIC;
         GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA app TO bob WITH GRANT OPTION;",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);

    assert_eq!(f.is_executable_by_public(&db), Ok(true));
    assert_eq!(f.can_grant_execute(role(&db, "bob"), &db), Ok(false));
}

#[test]
fn function_grants_are_not_table_or_column_grants() {
    let db = parse(
        "CREATE ROLE bob;
         CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION app.f() TO bob;
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA app TO bob;
         GRANT EXECUTE ON ROUTINE app.f() TO bob;
         GRANT EXECUTE ON ALL ROUTINES IN SCHEMA app TO bob;
         GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA app TO bob;",
    )
    .expect("schema parses");

    assert_eq!(db.table_grants().count(), 0);
    assert_eq!(db.column_grants().count(), 0);
}

#[test]
fn the_closed_world_refuses_a_function_the_schema_does_not_declare() {
    for sql in [
        "REVOKE EXECUTE ON FUNCTION nope() FROM PUBLIC;",
        "CREATE ROLE bob; GRANT EXECUTE ON FUNCTION nope() TO bob;",
        // No procedure can be declared in this model.
        "CREATE ROLE bob; GRANT EXECUTE ON PROCEDURE nope() TO bob;",
        "CREATE ROLE bob; GRANT EXECUTE ON ROUTINE nope() TO bob;",
        "REVOKE EXECUTE ON ROUTINE nope() FROM PUBLIC;",
    ] {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::FunctionNotFoundForGrant { function_name }) if function_name == "nope()"),
            "{sql}: got {result:?}"
        );
    }

    // The database reports the absent function before the absent role.
    let result = parse("GRANT EXECUTE ON FUNCTION nope() TO bob;");
    assert!(matches!(result, Err(Error::FunctionNotFoundForGrant { .. })), "got {result:?}");
    let result = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION f() TO bob;",
    );
    assert!(
        matches!(&result, Err(Error::RoleNotFoundForGrant { role_name }) if role_name == "bob"),
        "got {result:?}"
    );

    // The schema-wide forms still refuse a grantee naming no role.
    for sql in [
        "CREATE SCHEMA app; GRANT EXECUTE ON ALL ROUTINES IN SCHEMA app TO bob;",
        "CREATE SCHEMA app; GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA app TO bob;",
    ] {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::RoleNotFoundForGrant { role_name }) if role_name == "bob"),
            "{sql}: got {result:?}"
        );
    }
}

#[test]
fn the_open_world_reports_a_grant_on_a_function_the_schema_does_not_declare() {
    for sql in [
        "GRANT EXECUTE ON FUNCTION auth.uid() TO app;",
        "GRANT EXECUTE ON ROUTINE auth.uid() TO app;",
    ] {
        let db = open_world(sql).expect("schema parses");

        let unresolved: Vec<_> =
            db.unresolved_access_references().expect("targets are well formed").collect();
        assert!(
            matches!(unresolved[..], [UnresolvedAccessReference::GrantFunction(function)]
                if function.to_string() == "auth.uid()"),
            "{sql}: the absent function is reported, and its grantee goes with it: {unresolved:?}"
        );
        assert!(matches!(
            db.validate_access_targets(),
            Err(Error::FunctionNotFoundForGrant { function_name }) if function_name == "auth.uid()"
        ));
    }

    // A revoke naming nothing changes nothing, as for tables.
    let db =
        open_world("REVOKE EXECUTE ON FUNCTION auth.uid() FROM PUBLIC;").expect("schema parses");
    assert!(db.validate_access_targets().is_ok());
}

#[test]
fn the_open_world_reports_a_grantee_the_schema_does_not_declare() {
    let db = open_world(
        "CREATE ROLE known;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION f() TO known, app;",
    )
    .expect("schema parses");

    let unresolved: Vec<_> =
        db.unresolved_access_references().expect("targets are well formed").collect();
    assert!(
        matches!(unresolved[..], [UnresolvedAccessReference::GranteeRole(role)] if role.value == "app"),
        "{unresolved:?}"
    );
}

/// The owner's `EXECUTE` is an ordinary entry the owner can revoke, and the
/// right to grant it again comes with ownership.
#[test]
fn the_owner_holds_execute_until_it_is_revoked_from_the_owner() {
    let db = parse(
        "CREATE ROLE owner_role;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO owner_role;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);
    assert_eq!(f.can_execute(role(&db, "owner_role"), &db), Ok(true));
    assert_eq!(f.can_grant_execute(role(&db, "owner_role"), &db), Ok(true));

    let revoked = parse(
        "CREATE ROLE owner_role;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO owner_role;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC, owner_role;",
    )
    .expect("schema parses");
    let f = function(&revoked, "f", &[]);
    assert_eq!(f.can_execute(role(&revoked, "owner_role"), &revoked), Ok(false));
    assert_eq!(f.can_grant_execute(role(&revoked, "owner_role"), &revoked), Ok(true));
}

/// PostgreSQL hands the old owner's entry to the new owner.
#[test]
fn a_change_of_owner_carries_the_owner_entry() {
    let db = parse(
        "CREATE ROLE first;
         CREATE ROLE second;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO first;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         ALTER FUNCTION f() OWNER TO second;",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);

    assert_eq!(f.can_execute(role(&db, "second"), &db), Ok(true));
    assert_eq!(f.can_execute(role(&db, "first"), &db), Ok(false));
}

#[test]
fn a_grant_option_is_kept_and_can_be_revoked_alone() {
    let db = parse(
        "CREATE ROLE client;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION f() TO client WITH GRANT OPTION;",
    )
    .expect("schema parses");
    let f = function(&db, "f", &[]);
    assert_eq!(f.can_grant_execute(role(&db, "client"), &db), Ok(true));

    let revoked = parse(
        "CREATE ROLE client;
         CREATE ROLE partner;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION f() TO client, partner WITH GRANT OPTION;
         REVOKE GRANT OPTION FOR EXECUTE ON FUNCTION f() FROM client;",
    )
    .expect("schema parses");
    let f = function(&revoked, "f", &[]);
    assert_eq!(f.can_grant_execute(role(&revoked, "client"), &revoked), Ok(false));
    assert_eq!(f.can_execute(role(&revoked, "client"), &revoked), Ok(true));
    assert_eq!(f.can_grant_execute(role(&revoked, "partner"), &revoked), Ok(true));
}

/// A replacement keeps the catalog entry and so its privileges, while a drop
/// and a new creation start again from the default.
#[test]
fn privileges_survive_a_replacement_and_not_a_drop() {
    let replaced = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    )
    .expect("schema parses");
    assert_eq!(function(&replaced, "f", &[]).is_executable_by_public(&replaced), Ok(false));

    let recreated = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         DROP FUNCTION f();
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    )
    .expect("schema parses");
    assert_eq!(function(&recreated, "f", &[]).is_executable_by_public(&recreated), Ok(true));
}

#[test]
fn a_function_grant_follows_its_role_through_a_rename_and_blocks_its_drop() {
    let db = parse(
        "CREATE ROLE before;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION f() TO before;
         ALTER ROLE before RENAME TO after;",
    )
    .expect("schema parses");
    assert_eq!(function(&db, "f", &[]).can_execute(role(&db, "after"), &db), Ok(true));

    let dropped = parse(
        "CREATE ROLE bob;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT EXECUTE ON FUNCTION f() TO bob;
         DROP ROLE bob;",
    );
    assert!(
        matches!(&dropped, Err(Error::RoleReferenced { role_name }) if role_name == "bob"),
        "got {dropped:?}"
    );
}

#[test]
fn a_privilege_functions_do_not_have_is_refused() {
    let result = parse(
        "CREATE ROLE bob;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT SELECT ON FUNCTION f() TO bob;",
    );
    assert!(
        matches!(&result, Err(Error::InvalidFunctionPrivilege { privilege }) if privilege == "SELECT"),
        "got {result:?}"
    );

    for sql in [
        "CREATE ROLE bob;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         GRANT SELECT ON ROUTINE f() TO bob;",
        "CREATE ROLE bob; CREATE SCHEMA app; GRANT SELECT ON ALL ROUTINES IN SCHEMA app TO bob;",
        "CREATE ROLE bob; CREATE SCHEMA app; GRANT SELECT ON ALL PROCEDURES IN SCHEMA app TO bob;",
    ] {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::InvalidFunctionPrivilege { privilege }) if privilege == "SELECT"),
            "{sql}: got {result:?}"
        );
    }
}

/// The keyword names whoever runs the statement, which is no declared role.
#[test]
fn a_grant_to_the_session_role_reaches_no_declared_role() {
    let db = parse(
        "CREATE ROLE bob;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         GRANT EXECUTE ON FUNCTION f() TO CURRENT_USER;",
    )
    .expect("schema parses");

    assert_eq!(function(&db, "f", &[]).can_execute(role(&db, "bob"), &db), Ok(false));
}

#[test]
fn a_rolled_back_revoke_leaves_the_default() {
    let db = parse(
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         BEGIN;
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC;
         ROLLBACK;",
    )
    .expect("schema parses");

    assert_eq!(function(&db, "f", &[]).is_executable_by_public(&db), Ok(true));
}
