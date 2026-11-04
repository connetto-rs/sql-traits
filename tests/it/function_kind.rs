//! Tests that a function answers its `pg_proc.prokind`, so a caller can tell
//! the aggregate a call reaches from a plain function.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use core::{convert::Infallible, ops::ControlFlow};

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, visit_expressions},
    dialect::PostgreSqlDialect,
};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

/// Every call of the policy's `USING` expression, in walk order, with the
/// argument count and kind of each function it reaches.
fn kinds_per_call(db: &ParserDB, policy: &str) -> Vec<(String, Vec<(usize, FunctionKind)>)> {
    let policy = db.policies().find(|candidate| candidate.name() == policy).expect("policy exists");
    let using = policy.using_expression(db).expect("USING clause");
    let mut calls = Vec::new();
    let ControlFlow::<Infallible>::Continue(()) = visit_expressions(using, |expression| {
        if let Expr::Function(call) = expression {
            let reached = policy
                .functions_reached_by(call, db)
                .expect("call belongs to the policy")
                .map(|function| {
                    (
                        function.argument_type_names(db).count(),
                        function.kind(db).expect("function is held"),
                    )
                })
                .collect();
            calls.push((call.name.to_string(), reached));
        }
        ControlFlow::Continue(())
    });
    calls
}

/// `count(*)` reaches the parameterless aggregate PostgreSQL resolves it to,
/// while `count(member)` reaches the one-argument aggregate, and a window
/// function and a plain function answer their own kinds.
#[test]
fn calls_in_a_subquery_reach_functions_of_their_kind() {
    let db = parse(
        "CREATE TABLE members (member TEXT, active BOOLEAN);
         CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (
             EXISTS (SELECT count(*) FROM members WHERE member = owner)
             OR EXISTS (SELECT count(member) FROM members WHERE member = owner)
             OR EXISTS (SELECT bool_and(active) FROM members WHERE member = owner)
             OR EXISTS (SELECT string_agg(member, ',') FROM members WHERE member = owner)
             OR EXISTS (SELECT row_number() OVER () FROM members WHERE member = owner)
             OR EXISTS (SELECT lag(member, 1) OVER () FROM members WHERE member = owner)
             OR EXISTS (SELECT lower(member) FROM members WHERE member = owner)
         );",
    );
    assert_eq!(
        kinds_per_call(&db, "p"),
        [
            ("count".to_owned(), vec![(0, FunctionKind::Aggregate)]),
            ("count".to_owned(), vec![(1, FunctionKind::Aggregate)]),
            ("bool_and".to_owned(), vec![(1, FunctionKind::Aggregate)]),
            ("string_agg".to_owned(), vec![(2, FunctionKind::Aggregate)]),
            ("row_number".to_owned(), vec![(0, FunctionKind::Window)]),
            ("lag".to_owned(), vec![(2, FunctionKind::Window)]),
            ("lower".to_owned(), vec![(1, FunctionKind::Normal)]),
        ]
    );
}

/// `rank` is both a window function and a hypothetical-set aggregate, and the
/// `WITHIN GROUP` call passing a direct argument reaches only the aggregate.
#[test]
fn a_hypothetical_set_call_reaches_the_aggregate_rank() {
    let db = parse(
        "CREATE TABLE members (member TEXT);
         CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (
             EXISTS (SELECT rank('a') WITHIN GROUP (ORDER BY member) FROM members WHERE member = owner)
         );",
    );
    assert_eq!(kinds_per_call(&db, "p"), [("rank".to_owned(), vec![(1, FunctionKind::Aggregate)])]);
    let mut kinds: Vec<_> = db
        .functions()
        .filter(|function| function.name() == "rank")
        .map(|function| function.kind(&db).expect("function is held"))
        .collect();
    kinds.sort_unstable();
    assert_eq!(kinds, [FunctionKind::Aggregate, FunctionKind::Window]);
}

/// A function the input declares is a normal function, even when it shadows
/// an aggregate's name in another schema.
#[test]
fn a_declared_function_is_normal() {
    let db = parse(
        "CREATE FUNCTION public.count(x TEXT) RETURNS BIGINT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION is_member() RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';",
    );
    let count = db
        .function_by_target(
            TargetName::new("count", false).with_schema("public", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("function exists");
    assert_eq!(count.kind(&db), Ok(FunctionKind::Normal));
    let is_member = db
        .function_by_target(TargetName::new("is_member", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    assert_eq!(is_member.kind(&db), Ok(FunctionKind::Normal));
}

/// A function node the database does not hold has no kind to report.
#[test]
fn a_function_of_another_database_has_no_kind() {
    let other = parse("CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';");
    let function = other
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    let db = parse("CREATE TABLE t (id INT);");
    assert!(function.kind(&db).is_err());
}

/// `CREATE OR REPLACE FUNCTION` keeps the `pg_proc` row, so PostgreSQL
/// refuses it over an aggregate or a window function and accepts it over a
/// normal one.
#[test]
fn a_replacement_cannot_change_a_routine_kind() {
    for (sql, name, kind) in [
        (
            "CREATE OR REPLACE FUNCTION pg_catalog.count() RETURNS BIGINT LANGUAGE sql AS 'SELECT 1::bigint';",
            "count",
            FunctionKind::Aggregate,
        ),
        (
            "CREATE OR REPLACE FUNCTION pg_catalog.row_number() RETURNS BIGINT LANGUAGE sql AS 'SELECT 1::bigint';",
            "row_number",
            FunctionKind::Window,
        ),
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(sql).expect_err("PostgreSQL refuses it");
        assert!(
            matches!(
                &error,
                sql_traits::errors::Error::FunctionKindChange { function_name, kind: refused }
                    if *refused == kind && function_name == name
            ),
            "{error:?}"
        );
    }
    let db = parse(
        "CREATE OR REPLACE FUNCTION pg_catalog.lower(x TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT x';",
    );
    let lower = db
        .function_by_target(
            TargetName::new("lower", false).with_schema("pg_catalog", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("function exists");
    assert_eq!(lower.kind(&db), Ok(FunctionKind::Normal));
    assert_eq!(lower.body(), Some("SELECT x"));
}
