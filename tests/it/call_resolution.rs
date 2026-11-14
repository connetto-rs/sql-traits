//! Tests that each call in a policy, check, column or domain expression
//! answers the functions it may reach, wherever it is nested.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]
#![expect(
    clippy::disallowed_methods,
    reason = "test code ships in no binary, so walking the tree directly costs nothing"
)]

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

/// Every call of `expression`, in walk order, with the qualified names of the
/// functions `lookup` answers for it.
fn names_per_call(
    expression: &Expr,
    mut lookup: impl FnMut(&sqlparser::ast::Function) -> Result<Vec<String>, LookupError>,
) -> Vec<(String, Vec<String>)> {
    reached_per_call(expression, |call| lookup(call).expect("call belongs to the queried object"))
}

/// Every call of `expression`, in walk order, with the call `lookup` names
/// when it refuses the call as no node of the object it queries.
fn refusals_per_call(
    expression: &Expr,
    mut lookup: impl FnMut(&sqlparser::ast::Function) -> Result<Vec<String>, LookupError>,
) -> Vec<(String, Vec<String>)> {
    reached_per_call(expression, |call| {
        match lookup(call) {
            Err(LookupError::CallNotInExpression { call }) => vec![call],
            Err(error) => panic!("unexpected error {error}"),
            Ok(_) => panic!("a call from elsewhere was answered"),
        }
    })
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
    let using = policy.using_expression(&db).expect("literals read").expect("USING clause");
    let calls = names_per_call(using, |call| policy.functions_reached_by(call, &db).map(names));
    assert_eq!(
        calls,
        [
            ("string_to_array".to_owned(), vec!["pg_catalog.string_to_array".to_owned()]),
            ("current_setting".to_owned(), vec!["pg_catalog.current_setting".to_owned()]),
            // `lower` of text, of a range and of a multirange each take one.
            ("lower".to_owned(), vec!["pg_catalog.lower".to_owned(); 3]),
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
            "pg_catalog.lower",
            "pg_catalog.lower",
            "auth.uid"
        ]
    );
}

/// A clause holding a literal whose stored value is unknown still resolves
/// the calls borrowed from it.
#[test]
fn calls_of_a_clause_with_an_unread_literal_reach_their_functions() {
    let db = parse(
        "CREATE SCHEMA auth;
         CREATE FUNCTION auth.uid() RETURNS TEXT LANGUAGE sql AS 'SELECT NULL';
         CREATE TABLE items (owner TEXT, expires TIMESTAMPTZ);
         CREATE POLICY p ON items USING (expires > '2026-01-01 09:00' AND owner = auth.uid());",
    );
    let policy = policy(&db, "p");
    let using = policy.using_clause(&db).expect("policy is held").expect("USING clause");
    assert!(matches!(using, PolicyExpression::Written { .. }), "the literal needs a zone");
    let calls = names_per_call(using.expression(), |call| {
        policy.functions_reached_by(call, &db).map(names)
    });
    assert_eq!(calls, [("auth.uid".to_owned(), vec!["auth.uid".to_owned()])]);
}

/// Each call answers only for itself, the overload its argument types select
/// among those accepting its argument count.
#[test]
fn each_call_reaches_the_overload_its_arguments_select() {
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
        arities(policy.using_expression(&db).expect("literals read").expect("USING clause")),
        [("f".to_owned(), vec!["TEXT".to_owned()])]
    );
    assert_eq!(
        arities(policy.check_expression(&db).expect("literals read").expect("WITH CHECK clause")),
        [("f".to_owned(), vec!["TEXT, TEXT".to_owned()])]
    );
}

/// A `VARIADIC` parameter collects at least one argument unless it has a
/// default, so a call passing none reaches only the overloads that need none,
/// and PostgreSQL refuses one reaching none.
#[test]
fn a_variadic_parameter_without_a_default_needs_an_argument() {
    let functions =
        "CREATE FUNCTION f(VARIADIC xs INT[]) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION g(VARIADIC xs INT[] DEFAULT '{}') RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE TABLE items (owner TEXT);";
    let refused = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{functions} CREATE POLICY p ON items USING (f());"
    ));
    assert!(
        matches!(
            &refused,
            Err(sql_traits::errors::Error::UnresolvedCall { function_name, argument_types, .. })
                if function_name == "f" && argument_types.is_empty()
        ),
        "got {refused:?}"
    );
    let db = parse(&format!(
        "{functions} CREATE POLICY p ON items USING (
             f(1) AND f(1, 2) AND g() AND g(1)
             AND EXISTS (SELECT rank() OVER () FROM items)
         );"
    ));
    let policy = policy(&db, "p");
    let using = policy.using_expression(&db).expect("literals read").expect("USING clause");
    let calls = names_per_call(using, |call| policy.functions_reached_by(call, &db).map(names));
    assert_eq!(
        calls,
        [
            ("f".to_owned(), vec!["f".to_owned()]),
            ("f".to_owned(), vec!["f".to_owned()]),
            ("g".to_owned(), vec!["g".to_owned()]),
            ("g".to_owned(), vec!["g".to_owned()]),
            // The window function, and not the hypothetical-set aggregate.
            ("rank".to_owned(), vec!["pg_catalog.rank".to_owned()]),
        ]
    );
    let rank = policy
        .using_functions(&db)
        .expect("policy is held")
        .find(|function| function.name() == "rank")
        .expect("rank is called");
    assert_eq!(rank.argument_type_names(&db).count(), 0);
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
    let using = policy.using_expression(&db).expect("literals read").expect("USING clause");
    let calls = names_per_call(using, |call| policy.functions_reached_by(call, &db).map(names));
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
    let using = q.using_expression(&db).expect("literals read").expect("USING clause");
    let copy = using.clone();
    for expression in [&copy, using] {
        let refusals =
            refusals_per_call(expression, |call| p.functions_reached_by(call, &db).map(names));
        assert_eq!(refusals, [("lower".to_owned(), vec!["lower(owner)".to_owned()])]);
    }
}

/// `CREATE OR REPLACE FUNCTION` and `ALTER POLICY` reach the per-call answers,
/// and a call to a function left alone keeps its answer.
#[test]
fn per_call_answers_follow_replacements_and_altered_clauses() {
    let db = parse(
        "CREATE FUNCTION f(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION g(x TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE TABLE items (owner TEXT);
         CREATE POLICY p ON items USING (f(owner) AND g(owner)) WITH CHECK (true);
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
        behaviour(policy.using_expression(&db).expect("literals read").expect("USING clause")),
        [
            ("f".to_owned(), vec!["f STRICT".to_owned()]),
            ("g".to_owned(), vec!["g CALLED ON NULL INPUT".to_owned()]),
        ]
    );
    assert_eq!(
        behaviour(policy.check_expression(&db).expect("literals read").expect("WITH CHECK clause")),
        [("g".to_owned(), vec!["g CALLED ON NULL INPUT".to_owned()])]
    );
}

/// A check constraint answers per call the same way, so a rule can name the
/// call that runs a volatile function, and refuses a call from a copy.
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
            ("lower".to_owned(), vec!["pg_catalog.lower Immutable".to_owned(); 3]),
            ("stamp".to_owned(), vec!["stamp Volatile".to_owned()]),
        ]
    );
    let copy = check.expression(&db).clone();
    let refusals =
        refusals_per_call(&copy, |call| check.functions_reached_by(call, &db).map(names));
    assert_eq!(
        refusals,
        [
            ("lower".to_owned(), vec!["lower(name)".to_owned()]),
            ("stamp".to_owned(), vec!["stamp(name)".to_owned()]),
        ]
    );
}

/// `COALESCE`, `CURRENT_USER` and the other names PostgreSQL reads as
/// expressions are nodes of the expression that reach no function, while the
/// calls written inside them answer for themselves. A copy of one is refused.
#[test]
fn an_expression_form_reaches_no_function() {
    let db = parse(
        "CREATE TABLE items (owner TEXT, CHECK (coalesce(lower(owner), current_user) = owner));
         CREATE POLICY p ON items USING (owner = lower(current_user))
             WITH CHECK (owner = session_user);",
    );
    let lower = || vec!["pg_catalog.lower".to_owned(); 3];
    let policy = policy(&db, "p");
    let in_policy = |expression: &Expr| {
        names_per_call(expression, |call| policy.functions_reached_by(call, &db).map(names))
    };
    assert_eq!(
        in_policy(policy.using_expression(&db).expect("literals read").expect("USING clause")),
        [("lower".to_owned(), lower()), ("current_user".to_owned(), Vec::new())]
    );
    assert_eq!(
        in_policy(policy.check_expression(&db).expect("literals read").expect("WITH CHECK clause")),
        [("session_user".to_owned(), Vec::new())]
    );

    let table = db.tables().next().expect("table exists");
    let check = table.check_constraints(&db).expect("table is held").next().expect("check");
    let in_check = names_per_call(check.expression(&db), |call| {
        check.functions_reached_by(call, &db).map(names)
    });
    assert_eq!(
        in_check,
        [
            ("coalesce".to_owned(), Vec::new()),
            ("lower".to_owned(), lower()),
            ("current_user".to_owned(), Vec::new()),
        ]
    );

    let copy = policy.using_expression(&db).expect("literals read").expect("USING clause").clone();
    let refusals =
        refusals_per_call(&copy, |call| policy.functions_reached_by(call, &db).map(names));
    assert_eq!(
        refusals,
        [
            ("lower".to_owned(), vec!["lower(current_user)".to_owned()]),
            ("current_user".to_owned(), vec!["current_user".to_owned()]),
        ]
    );
}

/// The default and the generation expression of a column answer per call
/// under the path in force when the table was written, whatever path the
/// script ends on, and a call from a copy is refused.
#[test]
fn column_expression_calls_resolve_under_the_statement_search_path() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x';
         CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x';
         CREATE FUNCTION app.d() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION d() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         SET search_path TO app;
         CREATE TABLE public.t (
             x INT DEFAULT d(),
             y INT GENERATED ALWAYS AS (f(x) + 1) STORED,
             z INT
         );
         SET search_path TO public;",
    );
    let table = db.tables().next().expect("table exists");
    let column = |name: &str| {
        table.column(name, &db, IdentifierCase::AsWritten).expect("table is held").expect(name)
    };
    let (x, y, z) = (column("x"), column("y"), column("z"));
    let default = x.default_expression().expect("x has a default");
    let generation = y.generation_expression().expect("y is generated");
    assert_eq!(default.to_string(), "d()");
    assert_eq!(generation.to_string(), "f(x) + 1");
    assert!(x.generation_expression().is_none());
    assert!(y.default_expression().is_none());
    assert!(z.default_expression().is_none() && z.generation_expression().is_none());
    assert_eq!(
        names_per_call(default, |call| x.functions_reached_by(call, &db).map(names)),
        [("d".to_owned(), vec!["app.d".to_owned()])]
    );
    assert_eq!(
        names_per_call(generation, |call| y.functions_reached_by(call, &db).map(names)),
        [("f".to_owned(), vec!["app.f".to_owned()])]
    );
    let refusals =
        refusals_per_call(generation, |call| x.functions_reached_by(call, &db).map(names));
    assert_eq!(refusals, [("f".to_owned(), vec!["f(x)".to_owned()])]);
    let copy = generation.clone();
    let refusals = refusals_per_call(&copy, |call| y.functions_reached_by(call, &db).map(names));
    assert_eq!(refusals, [("f".to_owned(), vec!["f(x)".to_owned()])]);
}

/// A default set later reaches the function the path in force at the
/// `ALTER TABLE` selects.
#[test]
fn an_altered_column_default_answers_its_own_calls() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.d() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION d() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT x';
         CREATE TABLE t (x INT, y INT GENERATED ALWAYS AS (f(x)) STORED);
         SET search_path TO app;
         ALTER TABLE public.t ALTER COLUMN x SET DEFAULT d();
         SET search_path TO public;",
    );
    let table = db.tables().next().expect("table exists");
    let column = |name: &str| {
        table.column(name, &db, IdentifierCase::AsWritten).expect("table is held").expect(name)
    };
    let (x, y) = (column("x"), column("y"));
    let default = x.default_expression().expect("x has a default");
    assert_eq!(
        names_per_call(default, |call| x.functions_reached_by(call, &db).map(names)),
        [("d".to_owned(), vec!["app.d".to_owned()])]
    );
    let generation = y.generation_expression().expect("y is generated");
    assert_eq!(
        names_per_call(generation, |call| y.functions_reached_by(call, &db).map(names)),
        [("f".to_owned(), vec!["f".to_owned()])]
    );
}

/// The default and every check of a domain answer per call under the path
/// in force when the domain was created, each check counting its own calls
/// whatever the default and the earlier checks call.
#[test]
fn domain_expression_calls_resolve_under_the_statement_search_path() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.ok(x INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION ok(x INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION app.d() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION d() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         SET search_path TO app;
         CREATE DOMAIN public.checked AS INT DEFAULT d()
             CHECK (ok(VALUE)) CHECK (VALUE > 0 AND abs(VALUE) < 9 AND ok(VALUE));
         SET search_path TO public;",
    );
    let domain = db.domains().next().expect("domain exists");
    let default = domain.default_expression().expect("the domain has a default");
    assert_eq!(
        names_per_call(default, |call| domain.functions_reached_by(call, &db).map(names)),
        [("d".to_owned(), vec!["app.d".to_owned()])]
    );
    let checks: Vec<_> = domain
        .check_expressions()
        .map(|check| {
            (
                check.to_string(),
                names_per_call(check, |call| domain.functions_reached_by(call, &db).map(names)),
            )
        })
        .collect();
    let ok = || ("ok".to_owned(), vec!["app.ok".to_owned()]);
    let abs = vec!["pg_catalog.abs".to_owned(); 6];
    assert_eq!(
        checks,
        [
            ("ok(VALUE)".to_owned(), vec![ok()]),
            (
                "VALUE > 0 AND abs(VALUE) < 9 AND ok(VALUE)".to_owned(),
                vec![("abs".to_owned(), abs), ok()]
            ),
        ]
    );
    let copy = default.clone();
    let refusals =
        refusals_per_call(&copy, |call| domain.functions_reached_by(call, &db).map(names));
    assert_eq!(refusals, [("d".to_owned(), vec!["d()".to_owned()])]);
}

/// A check `DROP FUNCTION … CASCADE` removes from a domain takes its calls
/// along, so the checks left keep answering for their own.
#[test]
fn domain_checks_left_by_a_cascade_answer_their_calls() {
    let db = parse(
        "CREATE FUNCTION gone(x INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE FUNCTION ok(x INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE DOMAIN checked AS INT CHECK (gone(VALUE)) CHECK (ok(VALUE));
         DROP FUNCTION gone(INT) CASCADE;",
    );
    let domain = db.domains().next().expect("domain exists");
    let checks: Vec<_> = domain
        .check_expressions()
        .map(|check| {
            names_per_call(check, |call| domain.functions_reached_by(call, &db).map(names))
        })
        .collect();
    assert_eq!(checks, [[("ok".to_owned(), vec!["ok".to_owned()])]]);
}

/// A domain asked about its calls in a database that does not hold it is
/// refused as absent.
#[test]
fn a_domain_from_another_database_is_refused() {
    let schema = "CREATE FUNCTION ok(x INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
                  CREATE DOMAIN checked AS INT CHECK (ok(VALUE));";
    let held = parse(schema);
    let other = parse("CREATE TABLE t (x INT);");
    let domain = held.domains().next().expect("domain exists");
    let check = domain.check_expressions().next().expect("the domain has a check");
    let refusals = reached_per_call(check, |call| {
        match domain.functions_reached_by(call, &other) {
            Err(LookupError::ObjectNotInDatabase { object, .. }) => vec![object],
            Err(error) => panic!("unexpected error {error}"),
            Ok(_) => panic!("a domain the database lacks was answered"),
        }
    });
    assert_eq!(refusals, [("ok".to_owned(), vec!["`checked`".to_owned()])]);
}
