//! Tests that a call in the key of a unique constraint is bound when the
//! constraint is written, as the calls of an index are, and that PostgreSQL
//! refuses a unique constraint keyed by an expression.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]
#![expect(
    clippy::disallowed_methods,
    reason = "test code ships in no binary, so walking the tree directly costs nothing"
)]

use core::{convert::Infallible, ops::ControlFlow};

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{
    ast::{Expr, visit_expressions},
    dialect::{GenericDialect, PostgreSqlDialect},
};

/// A table whose unique key calls `norm` under a path the script then
/// leaves.
const BOUND: &str = "CREATE SCHEMA earlier;
     CREATE SCHEMA later;
     CREATE FUNCTION earlier.norm(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x';
     CREATE FUNCTION later.norm(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT -x';
     CREATE TABLE public.t (x INT, y INT);
     SET search_path = earlier, public;
     ALTER TABLE public.t ADD CONSTRAINT u UNIQUE (y, (norm(x)));
     SET search_path = later, public;";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<GenericDialect>(sql)
}

/// The unique key `u` of table `t`.
fn key(db: &ParserDB) -> &<ParserDB as DatabaseLike>::UniqueIndex {
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("t is held");
    table
        .unique_indices(db)
        .expect("t is held")
        .find(|key| key.name() == Some("u"))
        .expect("u is held")
}

/// Each call of the key of `u`, with the functions it reaches.
fn key_calls(db: &ParserDB) -> Vec<(String, Vec<String>)> {
    let key = key(db);
    let mut calls = Vec::new();
    let expression = key.expression(db).expect("u is held");
    let ControlFlow::<Infallible>::Continue(()) = visit_expressions(expression, |expression| {
        if let Expr::Function(call) = expression {
            let reached = key
                .functions_reached_by(call, db)
                .expect("the call belongs to u")
                .map(|function| function.target_name().to_string())
                .collect();
            calls.push((call.name.to_string(), reached));
        }
        ControlFlow::Continue(())
    });
    calls
}

fn reaches(schema: &str) -> Vec<(String, Vec<String>)> {
    vec![("norm".to_owned(), vec![format!("{schema}.norm")])]
}

/// A call of the key reaches the function the path in force when the
/// constraint was written selects, whatever path the script ends on.
#[test]
fn a_unique_key_call_resolves_under_the_statement_search_path() {
    let db = parse(BOUND).expect("the schema parses");
    assert_eq!(key_calls(&db), reaches("earlier"));
}

/// A rename rebuilds the table, and the key keeps the function it was bound
/// to whatever path the rename runs under.
#[test]
fn a_unique_key_call_survives_a_rebuild_under_another_path() {
    let db = parse(&format!("{BOUND} ALTER TABLE public.t RENAME COLUMN y TO z;"))
        .expect("the rename applies");
    assert_eq!(key_calls(&db), reaches("earlier"));
}

/// The key follows the function it calls into another schema, its call
/// spelled qualified as PostgreSQL deparses it.
#[test]
fn a_unique_key_call_follows_its_function_moving() {
    let db = parse(&format!(
        "{BOUND} CREATE SCHEMA moved; ALTER FUNCTION earlier.norm(INT) SET SCHEMA moved;"
    ))
    .expect("the move applies");
    assert_eq!(key_calls(&db), [("moved.norm".to_owned(), vec!["moved.norm".to_owned()])]);
}

/// The function a key calls cannot be dropped alone, and `CASCADE` takes the
/// constraint while the table stays.
#[test]
fn a_function_a_unique_key_calls_goes_only_with_the_key() {
    let refused = parse(&format!("{BOUND} DROP FUNCTION earlier.norm(INT);"));
    assert!(
        matches!(
            &refused,
            Err(Error::FunctionReferenced { dependent_kind: ObjectKind::UniqueIndex, dependent_name, .. })
                if dependent_name == "u"
        ),
        "{refused:?}"
    );
    let db = parse(&format!("{BOUND} DROP FUNCTION earlier.norm(INT) CASCADE;"))
        .expect("the cascade takes u");
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("t stays");
    assert_eq!(table.unique_indices(&db).expect("t is held").count(), 0);
}

/// PostgreSQL lists only columns in a unique constraint, however it is
/// written, while a plain column list stays accepted.
#[test]
fn postgres_refuses_a_unique_constraint_keyed_by_an_expression() {
    for statement in [
        "CREATE TABLE t (x INT, UNIQUE (abs(x)));",
        "CREATE TABLE t (x INT, CONSTRAINT u UNIQUE (x, (x + 1)));",
        "CREATE TABLE t (x INT); ALTER TABLE t ADD CONSTRAINT u UNIQUE (abs(x));",
    ] {
        let refused = ParserDB::parse::<PostgreSqlDialect>(statement);
        assert!(matches!(refused, Err(Error::InvalidIndex { .. })), "{statement}: {refused:?}");
    }
    ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (x INT, y INT, UNIQUE (x, y));")
        .expect("a column list is a unique key");
}
