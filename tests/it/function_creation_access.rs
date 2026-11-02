//! Tests for PostgreSQL refusing a function its creator could not execute.
//!
//! `CREATE FUNCTION` runs the language's validator, which checks that the
//! role running the statement may execute the function it just created. A
//! default withholding `EXECUTE` from that role therefore refuses the
//! statement, unless the role is a superuser or inherits the privilege. The
//! expectations below were read from PostgreSQL 17 and 18.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Roles `r` and `g`, with defaults for `r` giving `EXECUTE` to `g` alone.
const WITHHELD: &str = "
CREATE ROLE r; CREATE ROLE g;
ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT EXECUTE ON FUNCTIONS TO g;
ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC, r;
";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

#[test]
fn a_creator_left_without_execute_is_refused() {
    for function in [
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
        "CREATE FUNCTION f() RETURNS INT LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';",
        "CREATE FUNCTION f() RETURNS INT RETURN 1;",
        "CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    ] {
        let error = parse(&format!("{WITHHELD} SET ROLE r; {function}")).expect_err(function);
        assert!(
            matches!(
                &error,
                Error::FunctionNotExecutableByCreator { function_name, role_name }
                    if function_name == "f" && role_name == "r"
            ),
            "{function}: {error:?}"
        );
    }
}

#[test]
fn replacing_a_function_its_creator_cannot_execute_is_refused() {
    let error = parse(
        "CREATE ROLE r;
         SET ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         REVOKE EXECUTE ON FUNCTION f() FROM PUBLIC, r;
         CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    )
    .expect_err("the replacement runs the validator");

    assert!(matches!(error, Error::FunctionNotExecutableByCreator { .. }), "{error:?}");
}

/// The check holds only for a role the input names and declares as no
/// superuser, since any other role may be one the validator lets through.
#[test]
fn creators_the_check_lets_through() {
    for (setup, creator) in [
        // Superusers pass every privilege check.
        ("ALTER ROLE r SUPERUSER;", "SET ROLE r;"),
        // A role inheriting the grantee's privileges may execute.
        ("GRANT g TO r;", "SET ROLE r;"),
        // Two paths to the grantee reach it once.
        ("CREATE ROLE a; CREATE ROLE b; GRANT g TO a, b; GRANT a, b TO r;", "SET ROLE r;"),
        // The unnamed role may be a superuser.
        ("", ""),
    ] {
        let sql = format!(
            "{WITHHELD} {setup} {creator}
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';"
        );
        assert!(parse(&sql).is_ok(), "{setup} {creator}: {:?}", parse(&sql).err());
    }
}

#[test]
fn a_membership_without_inherit_does_not_lend_execute() {
    let error = parse(&format!(
        "{WITHHELD} GRANT g TO r WITH INHERIT FALSE; SET ROLE r;
         CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';"
    ))
    .expect_err("r does not inherit from g");

    assert!(matches!(error, Error::FunctionNotExecutableByCreator { .. }), "{error:?}");
}

/// A role `r` inherits from may hold the privilege where the input does not
/// show it, when the input never creates that role.
#[test]
fn inheriting_from_an_undeclared_role_is_let_through() {
    let db = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(
        "CREATE ROLE r;
             GRANT ghost TO r;
             ALTER DEFAULT PRIVILEGES FOR ROLE r GRANT EXECUTE ON FUNCTIONS TO ghost;
             ALTER DEFAULT PRIVILEGES FOR ROLE r REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC, r;
             SET ROLE r;
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
    );

    assert!(db.is_ok(), "{:?}", db.err());
}

#[test]
fn an_undeclared_parse_time_role_is_let_through() {
    let db = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .with_session_role("postgres")
        .parse::<PostgreSqlDialect>(
            "ALTER DEFAULT PRIVILEGES FOR ROLE postgres GRANT EXECUTE ON FUNCTIONS TO g;
             ALTER DEFAULT PRIVILEGES FOR ROLE postgres
                 REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC, postgres;
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';",
        );

    assert!(db.is_ok(), "{:?}", db.err());
}

#[test]
fn a_declared_parse_time_role_is_checked() {
    let error = ParseOptions::default()
        .with_session_role("r")
        .parse::<PostgreSqlDialect>(&format!(
            "{WITHHELD} CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';"
        ))
        .expect_err("r is declared and no superuser");

    assert!(matches!(error, Error::FunctionNotExecutableByCreator { .. }), "{error:?}");
}
