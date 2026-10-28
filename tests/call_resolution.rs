//! Tests that each call in a policy or check expression answers the
//! functions it may reach, wherever it is nested.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use core::{convert::Infallible, ops::ControlFlow};

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{Expr, visit_expressions},
    dialect::PostgreSqlDialect,
};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn policy<'db>(db: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreatePolicy {
    db.policies().find(|policy| policy.name() == name).expect("policy exists")
}

/// Every call of `expression`, in walk order, with the qualified names of the
/// functions `reached` answers for it.
fn reached_per_call(
    expression: &Expr,
    mut reached: impl FnMut(&sqlparser::ast::Function) -> Vec<String>,
) -> Vec<(String, Vec<String>)> {
    let mut calls = Vec::new();
    let ControlFlow::<Infallible>::Continue(()) = visit_expressions(expression, |expression| {
        if let Expr::Function(call) = expression {
            calls.push((call.name.to_string(), reached(call)));
        }
        ControlFlow::Continue(())
    });
    calls
}

fn names<'db>(functions: impl Iterator<Item = &'db sqlparser::ast::CreateFunction>) -> Vec<String> {
    functions.map(|function| function.target_name().to_string()).collect()
}

/// The calls of a policy resolve wherever they are nested, an unqualified
/// built-in through `pg_catalog`, each to the overloads taking its arguments.
#[test]
fn nested_policy_calls_reach_their_functions() {
    let db = parse(
        "CREATE SCHEMA auth;
         CREATE FUNCTION auth.uid() RETURNS TEXT LANGUAGE sql AS 'SELECT NULL';
         CREATE TABLE members (member TEXT);
         CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (
             owner = ANY(string_to_array(current_setting('app.subjects', true), ','))
             OR EXISTS (SELECT 1 FROM members WHERE lower(member) = (SELECT auth.uid()))
         );",
    );
    let policy = policy(&db, "p");
    let using = policy.using_expression(&db).expect("USING clause");
    let calls = reached_per_call(using, |call| {
        names(policy.functions_reached_by(call, &db).expect("call belongs to the policy"))
    });
    assert_eq!(
        calls,
        [
            ("string_to_array".to_owned(), vec!["pg_catalog.string_to_array".to_owned()]),
            ("current_setting".to_owned(), vec!["pg_catalog.current_setting".to_owned()]),
            ("lower".to_owned(), vec!["pg_catalog.lower".to_owned()]),
            ("auth.uid".to_owned(), vec!["auth.uid".to_owned()]),
        ]
    );
    let current_setting = policy
        .using_functions(&db)
        .expect("policy is held")
        .find(|function| function.name() == "current_setting")
        .expect("current_setting is called");
    assert_eq!(current_setting.argument_type_names(&db).count(), 2, "the overload taking two");
    assert_eq!(
        names(policy.using_functions(&db).expect("policy is held")),
        [
            "pg_catalog.string_to_array",
            "pg_catalog.current_setting",
            "pg_catalog.lower",
            "auth.uid"
        ]
    );
}

/// Each call answers only for itself, and every overload accepting its
/// argument count counts.
#[test]
fn each_call_reaches_the_overloads_accepting_its_arguments() {
    let db = parse(
        "CREATE FUNCTION f(x INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION f(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION f(x TEXT, y TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (f(owner)) WITH CHECK (f(owner, owner));",
    );
    let policy = policy(&db, "p");
    let arities = |expression: &Expr| {
        reached_per_call(expression, |call| {
            policy
                .functions_reached_by(call, &db)
                .expect("call belongs to the policy")
                .map(|function| function.argument_type_names(&db).collect::<Vec<_>>().join(", "))
                .collect()
        })
    };
    assert_eq!(
        arities(policy.using_expression(&db).expect("USING clause")),
        [("f".to_owned(), vec!["INT".to_owned(), "TEXT".to_owned()])]
    );
    assert_eq!(
        arities(policy.check_expression(&db).expect("WITH CHECK clause")),
        [("f".to_owned(), vec!["TEXT, TEXT".to_owned()])]
    );
}

/// A call inside a subquery resolves under the path in force at the policy's
/// statement, whatever path the script ends on.
#[test]
fn a_subquery_call_resolves_under_the_statement_search_path() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION f(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE TABLE members (member TEXT);
         CREATE TABLE items (owner TEXT);
         SET search_path TO app;
         CREATE POLICY p ON public.items
             USING (EXISTS (SELECT 1 FROM public.members WHERE f(member)));
         SET search_path TO public;",
    );
    let policy = policy(&db, "p");
    let using = policy.using_expression(&db).expect("USING clause");
    let calls = reached_per_call(using, |call| {
        names(policy.functions_reached_by(call, &db).expect("call belongs to the policy"))
    });
    assert_eq!(calls, [("f".to_owned(), vec!["app.f".to_owned()])]);
}

/// A call is matched by address, so one from a copy of the expression or
/// from another policy is refused.
#[test]
fn a_call_from_elsewhere_is_refused() {
    let db = parse(
        "CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (lower(owner) = 'a');
         CREATE POLICY q ON items USING (lower(owner) = 'a');",
    );
    let p = policy(&db, "p");
    let q = policy(&db, "q");
    let copy = q.using_expression(&db).expect("USING clause").clone();
    for expression in [&copy, q.using_expression(&db).expect("USING clause")] {
        let refusals = reached_per_call(expression, |call| {
            match p.functions_reached_by(call, &db) {
                Err(LookupError::CallNotInExpression { call }) => vec![call],
                Err(error) => panic!("unexpected error {error}"),
                Ok(_) => panic!("a call of another expression was answered"),
            }
        });
        assert_eq!(refusals, [("lower".to_owned(), vec!["lower(owner)".to_owned()])]);
    }
}

/// `CREATE OR REPLACE FUNCTION` and `ALTER POLICY` reach the per-call answers.
#[test]
fn per_call_answers_follow_replacements_and_altered_clauses() {
    let db = parse(
        "CREATE FUNCTION f(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION g(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (f(owner)) WITH CHECK (true);
         CREATE OR REPLACE FUNCTION f(x TEXT) RETURNS BOOL LANGUAGE sql STRICT AS 'SELECT true';
         ALTER POLICY p ON items WITH CHECK (EXISTS (SELECT g(owner)));",
    );
    let policy = policy(&db, "p");
    let behaviour = |expression: &Expr| {
        reached_per_call(expression, |call| {
            policy
                .functions_reached_by(call, &db)
                .expect("call belongs to the policy")
                .map(|function| format!("{} {}", function.name(), function.null_input_behavior()))
                .collect()
        })
    };
    assert_eq!(
        behaviour(policy.using_expression(&db).expect("USING clause")),
        [("f".to_owned(), vec!["f STRICT".to_owned()])]
    );
    assert_eq!(
        behaviour(policy.check_expression(&db).expect("WITH CHECK clause")),
        [("g".to_owned(), vec!["g CALLED ON NULL INPUT".to_owned()])]
    );
}

/// A check constraint answers per call the same way, so a rule can name the
/// call that runs a volatile function.
#[test]
fn check_constraint_calls_reach_their_functions() {
    let db = parse(
        "CREATE FUNCTION stamp(x TEXT) RETURNS BOOL LANGUAGE sql VOLATILE AS 'SELECT true';
         CREATE TABLE users (name TEXT, CHECK (lower(name) = name AND stamp(name)));",
    );
    let table = db.tables().next().expect("table exists");
    let check = table.check_constraints(&db).expect("table is held").next().expect("check");
    let calls = reached_per_call(check.expression(&db), |call| {
        check
            .functions_reached_by(call, &db)
            .expect("call belongs to the check")
            .map(|function| format!("{} {:?}", function.target_name(), function.volatility()))
            .collect()
    });
    assert_eq!(
        calls,
        [
            ("lower".to_owned(), vec!["pg_catalog.lower Immutable".to_owned()]),
            ("stamp".to_owned(), vec!["stamp Volatile".to_owned()]),
        ]
    );
}
