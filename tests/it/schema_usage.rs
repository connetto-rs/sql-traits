//! Who holds `USAGE` on a schema, as PostgreSQL's `has_schema_privilege`
//! answers it.
//!
//! Every expectation was measured on PostgreSQL 17.11.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

const ROLES: &str =
    "CREATE ROLE o; CREATE ROLE o2; CREATE ROLE x; CREATE ROLE r; CREATE ROLE boss SUPERUSER;";

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{ROLES} {sql}")).expect("schema parses")
}

/// Whether `role` holds `USAGE` on `schema`.
fn usage(database: &ParserDB, role: &str, schema: &str) -> bool {
    let role = database.role(role).expect("the role exists");
    database.schema(schema).expect("the schema exists").has_usage(role, database)
}

#[test]
fn the_owner_holds_usage_and_nobody_else_does_unless_granted() {
    let database = parse("CREATE SCHEMA s AUTHORIZATION o;");
    assert!(usage(&database, "o", "s"));
    assert!(!usage(&database, "x", "s"));
    assert!(usage(&database, "boss", "s"), "a superuser passes every check");
    let schema = database.schema("s").expect("the schema exists");
    assert!(!schema.has_usage(ActingRole::Public, &database));
}

#[test]
fn a_grant_reaches_its_grantee_inheritors_and_public() {
    let database = parse(
        "CREATE SCHEMA s; CREATE SCHEMA t; CREATE SCHEMA u;
         GRANT USAGE ON SCHEMA s TO r; GRANT r TO o;
         GRANT r TO o2 WITH INHERIT FALSE;
         GRANT USAGE ON SCHEMA t TO PUBLIC;",
    );
    assert!(usage(&database, "r", "s"));
    assert!(usage(&database, "o", "s"), "through an inherited membership");
    assert!(!usage(&database, "o2", "s"), "a membership that does not inherit");
    assert!(!usage(&database, "x", "s"));
    assert!(usage(&database, "x", "t"));
    assert!(database.schema("t").expect("exists").has_usage(ActingRole::Public, &database));
    assert!(!usage(&database, "x", "u"), "a created schema gives PUBLIC nothing");
}

#[test]
fn all_privileges_carries_usage_and_create_alone_does_not() {
    let database = parse(
        "CREATE SCHEMA s; CREATE SCHEMA t; CREATE SCHEMA \"Quoted\";
         GRANT ALL ON SCHEMA s TO x; GRANT CREATE ON SCHEMA t TO x;
         GRANT USAGE ON SCHEMA \"Quoted\" TO x;",
    );
    assert!(usage(&database, "x", "s"));
    assert!(!usage(&database, "x", "t"));
    assert!(usage(&database, "x", "\"Quoted\""));
}

#[test]
fn a_revoke_takes_a_granted_usage_and_the_owners_own() {
    let database = parse(
        "CREATE SCHEMA s AUTHORIZATION o; CREATE SCHEMA t AUTHORIZATION o; CREATE SCHEMA u AUTHORIZATION o;
         GRANT USAGE ON SCHEMA s TO x; REVOKE USAGE ON SCHEMA s FROM x;
         REVOKE USAGE ON SCHEMA t FROM o;
         REVOKE USAGE ON SCHEMA u FROM o; GRANT USAGE ON SCHEMA u TO o;
         GRANT o TO r; REVOKE USAGE ON SCHEMA s FROM r;",
    );
    assert!(!usage(&database, "x", "s"));
    assert!(!usage(&database, "o", "t"), "the owner revoked its own entry");
    assert!(usage(&database, "o", "u"), "and granted it back");
    assert!(usage(&database, "r", "s"), "a revoke from a member leaves the owner's entry");
    assert!(usage(&database, "o", "s"));
}

#[test]
fn an_owner_change_hands_the_owners_entry_to_the_new_owner() {
    let database = parse(
        "CREATE SCHEMA s AUTHORIZATION o; GRANT USAGE ON SCHEMA s TO x; ALTER SCHEMA s OWNER TO o2;
         CREATE SCHEMA t AUTHORIZATION o; REVOKE USAGE ON SCHEMA t FROM o; ALTER SCHEMA t OWNER TO o2;
         CREATE SCHEMA u AUTHORIZATION o; REVOKE USAGE ON SCHEMA u FROM o; GRANT USAGE ON SCHEMA u TO o;
         ALTER SCHEMA u OWNER TO o2;",
    );
    assert!(usage(&database, "x", "s"));
    assert!(!usage(&database, "o", "s"));
    assert!(usage(&database, "o2", "s"));
    assert!(!usage(&database, "o2", "t"), "a revoked owner entry stays revoked");
    assert!(!usage(&database, "o", "u"), "the old owner's grant to itself goes with the schema");
    assert!(usage(&database, "o2", "u"));
}

#[test]
fn public_gives_every_role_usage_until_a_revoke_takes_it() {
    let database = parse("");
    assert!(usage(&database, "x", "public"));
    assert!(database.schema("public").expect("exists").has_usage(ActingRole::Public, &database));

    let held = |statements: &str| usage(&parse(statements), "x", "public");
    assert!(!held("REVOKE USAGE ON SCHEMA public FROM PUBLIC;"));
    assert!(!held("REVOKE ALL ON SCHEMA public FROM PUBLIC;"));
    assert!(held("REVOKE GRANT OPTION FOR USAGE ON SCHEMA public FROM PUBLIC;"));
    assert!(held("REVOKE USAGE ON SCHEMA public FROM x;"), "PUBLIC's entry is not x's");
    assert!(held(
        "REVOKE USAGE ON SCHEMA public FROM PUBLIC; GRANT USAGE ON SCHEMA public TO PUBLIC;"
    ));
    assert!(
        !held("DROP SCHEMA public; CREATE SCHEMA public;"),
        "a recreated public is an ordinary schema"
    );
}

#[test]
fn public_keeps_its_entry_through_a_rename_and_an_owner_change() {
    let database = parse(
        "ALTER SCHEMA public RENAME TO p2; ALTER SCHEMA p2 OWNER TO o; ALTER ROLE o RENAME TO o3;",
    );
    assert!(usage(&database, "x", "p2"));
    assert!(usage(&database, "o3", "p2"));
    let database =
        parse("REVOKE USAGE ON SCHEMA public FROM PUBLIC; ALTER SCHEMA public RENAME TO p2;");
    assert!(!usage(&database, "x", "p2"));
}

#[test]
fn public_hands_pg_database_owners_entry_to_a_new_owner() {
    let revoked = "REVOKE USAGE ON SCHEMA public FROM PUBLIC;";
    let database = parse(&format!("{revoked} ALTER SCHEMA public OWNER TO o;"));
    assert!(usage(&database, "o", "public"));
    let database = parse(&format!(
        "{revoked} REVOKE USAGE ON SCHEMA public FROM pg_database_owner;
         ALTER SCHEMA public OWNER TO o;"
    ));
    assert!(!usage(&database, "o", "public"), "a revoked owner entry stays revoked");
}

#[test]
fn an_owner_change_merges_the_old_owners_grants_into_the_new_owners() {
    let database = parse(
        "CREATE SCHEMA v AUTHORIZATION o;
         SET ROLE o; GRANT USAGE ON SCHEMA v TO o, o2, x GRANTED BY o; RESET ROLE;
         ALTER SCHEMA v OWNER TO o2;",
    );
    assert!(usage(&database, "x", "v"));
    assert!(!usage(&database, "o", "v"));
    assert!(usage(&database, "o2", "v"));
    let grant = database.table_grants().next().expect("the grant on v remains");
    let grantees: Vec<String> = grant.grantees(&database).map(ToString::to_string).collect();
    assert_eq!(grantees, ["o2", "x"], "o's entry is now o2's, once");
    assert_eq!(grant.granted_by(&database).map(RoleLike::name), Some("o2"));
}
