//! Tests that a stored expression is refused where PostgreSQL 18 refuses a
//! call for the kind of function it selects, or for a clause no function
//! may combine with another, with the message PostgreSQL gives.
#![expect(clippy::panic, reason = "a test fails loudly on an unexpected outcome")]

use sql_traits::{
    errors::{Error, Unresolved},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// A view selecting `expression` over a table of an `int4`, a `text`, a
/// `float8` and a key and a value.
fn view(expression: &str) -> String {
    format!(
        "CREATE TABLE t (a int, b text, d float8, k text, v int);
         CREATE VIEW w AS SELECT {expression} AS x FROM t;"
    )
}

/// Asserts that the view selecting `expression` is refused with `message`.
#[track_caller]
fn assert_misapplied(expression: &str, message: &str) {
    match parse(&view(expression)) {
        Err(error @ Error::MisappliedCall { .. }) => {
            assert_eq!(error.to_string(), message, "{expression}");
        }
        other => panic!("{expression}\ngot {other:?}"),
    }
}

/// A call naming a plain function, or reading as a cast, refuses each
/// clause only an aggregate or a window function takes, the clauses checked
/// in the order PostgreSQL checks them.
#[test]
fn a_plain_function_refuses_aggregate_and_window_clauses() {
    for (expression, message) in [
        ("now(*)", "now(*) specified, but now is not an aggregate function"),
        ("now(*) OVER ()", "now(*) specified, but now is not an aggregate function"),
        (
            "pg_catalog.now(*)",
            "pg_catalog.now(*) specified, but pg_catalog.now is not an aggregate function",
        ),
        ("lower(DISTINCT b)", "DISTINCT specified, but lower is not an aggregate function"),
        ("lower(b ORDER BY b)", "ORDER BY specified, but lower is not an aggregate function"),
        (
            "lower(b) FILTER (WHERE a > 0)",
            "FILTER specified, but lower is not an aggregate function",
        ),
        (
            "lower(b) FILTER (WHERE a > 0) OVER ()",
            "FILTER specified, but lower is not an aggregate function",
        ),
        (
            "lower(b) OVER ()",
            "OVER specified, but lower is not a window function nor an aggregate function",
        ),
        (
            "left(b) WITHIN GROUP (ORDER BY 1)",
            "WITHIN GROUP specified, but left is not an aggregate function",
        ),
        (
            "int4(d) OVER ()",
            "OVER specified, but int4 is not a window function nor an aggregate function",
        ),
        ("text(a ORDER BY a)", "ORDER BY specified, but text is not an aggregate function"),
    ] {
        assert_misapplied(expression, message);
    }
}

/// An ordered-set aggregate needs `WITHIN GROUP` and refuses `OVER`, and a
/// normal aggregate refuses `WITHIN GROUP`.
#[test]
fn an_aggregate_refuses_the_clauses_of_another_aggregate_kind() {
    for (expression, message) in [
        (
            "percentile_cont(0.5, d)",
            "WITHIN GROUP is required for ordered-set aggregate percentile_cont",
        ),
        ("rank(1)", "WITHIN GROUP is required for ordered-set aggregate rank"),
        ("\"rank\"(a) OVER ()", "WITHIN GROUP is required for ordered-set aggregate rank"),
        (
            "percentile_cont(0.5) WITHIN GROUP (ORDER BY d) OVER ()",
            "OVER is not supported for ordered-set aggregate percentile_cont",
        ),
        (
            "percentile_disc(0.5) WITHIN GROUP (ORDER BY 'x') OVER ()",
            "OVER is not supported for ordered-set aggregate percentile_disc",
        ),
        (
            "string_agg(b) WITHIN GROUP (ORDER BY b)",
            "string_agg is not an ordered-set aggregate, so it cannot have WITHIN GROUP",
        ),
    ] {
        assert_misapplied(expression, message);
    }
}

/// A window function needs `OVER` and refuses `WITHIN GROUP`.
#[test]
fn a_window_function_needs_over() {
    for (expression, message) in [
        ("row_number()", "window function row_number requires an OVER clause"),
        ("row_number() FILTER (WHERE a > 0)", "window function row_number requires an OVER clause"),
        ("ntile(a)", "window function ntile requires an OVER clause"),
        ("lag(a) WITHIN GROUP (ORDER BY 1)", "window function lag requires an OVER clause"),
        (
            "lag(a) WITHIN GROUP (ORDER BY 1) OVER ()",
            "window function lag cannot have WITHIN GROUP",
        ),
    ] {
        assert_misapplied(expression, message);
    }
}

/// A function called over a window refuses `DISTINCT` and an argument
/// `ORDER BY`, a window function refuses `FILTER`, and an aggregate needs
/// `*` for no arguments there as without a window.
#[test]
fn a_call_over_a_window_refuses_what_windows_lack() {
    for (expression, message) in [
        ("count(DISTINCT a) OVER ()", "DISTINCT is not implemented for window functions"),
        ("ntile(DISTINCT a) OVER ()", "DISTINCT is not implemented for window functions"),
        ("count() OVER ()", "count(*) must be used to call a parameterless aggregate function"),
        ("sum(a ORDER BY a) OVER ()", "aggregate ORDER BY is not implemented for window functions"),
        (
            "row_number() FILTER (WHERE a > 0) OVER ()",
            "FILTER is not implemented for non-aggregate window functions",
        ),
    ] {
        assert_misapplied(expression, message);
    }
}

/// An aggregate called without a window needs `*` for no arguments and
/// takes no argument by name.
#[test]
fn an_aggregate_needs_a_star_and_positional_arguments() {
    for (expression, message) in [
        ("count()", "count(*) must be used to call a parameterless aggregate function"),
        ("json_object_agg(key => k, value => v)", "aggregates cannot use named arguments"),
    ] {
        assert_misapplied(expression, message);
    }
}

/// `WITHIN GROUP` combines with no `DISTINCT` or argument `ORDER BY`,
/// whatever function the call names.
#[test]
fn within_group_refuses_distinct_and_order_by() {
    for (expression, message) in [
        ("sum(DISTINCT a) WITHIN GROUP (ORDER BY a)", "cannot use DISTINCT with WITHIN GROUP"),
        ("no_such(DISTINCT a) WITHIN GROUP (ORDER BY a)", "cannot use DISTINCT with WITHIN GROUP"),
        (
            "string_agg(b, ',' ORDER BY b) WITHIN GROUP (ORDER BY b)",
            "cannot use multiple ORDER BY clauses with WITHIN GROUP",
        ),
    ] {
        assert_misapplied(expression, message);
    }
}

/// The grammar refuses `DISTINCT` beside `WITHIN GROUP` before any argument
/// is bound, so an argument naming a column of two tables changes nothing.
#[test]
fn within_group_refuses_distinct_before_binding_arguments() {
    let result = parse(
        "CREATE TABLE t (a int);
         CREATE TABLE u (a int);
         CREATE VIEW w AS SELECT sum(DISTINCT a) WITHIN GROUP (ORDER BY t.a) AS x FROM t, u;",
    );
    match result {
        Err(error @ Error::MisappliedCall { .. }) => {
            assert_eq!(error.to_string(), "cannot use DISTINCT with WITHIN GROUP");
        }
        other => panic!("got {other:?}"),
    }
}

/// A schema on the search path the model does not hold may supply an
/// aggregate or a window function by the name a call writes, so only the
/// clauses no function combines are refused, when written and when a type
/// change parses the call again.
#[test]
fn a_call_reaching_an_unheld_schema_is_refused_only_for_clauses_no_function_takes() {
    let path = "SET search_path = \"$user\", public, extensions;";
    for expression in ["lower(DISTINCT b)", "lower(b) OVER ()", "text(a ORDER BY a)"] {
        let sql = format!("{path} {}", view(expression));
        let result = parse(&sql);
        assert!(result.is_ok(), "{sql}\ngot {result:?}");
    }
    let retyped = format!(
        "{path} CREATE TABLE t (a int4, CONSTRAINT ck CHECK (text(a ORDER BY a) <> ''));
         ALTER TABLE t ALTER COLUMN a TYPE int8;"
    );
    let result = parse(&retyped);
    assert!(result.is_ok(), "{retyped}\ngot {result:?}");
    match parse(&format!("{path} {}", view("sum(DISTINCT a) WITHIN GROUP (ORDER BY a)"))) {
        Err(error @ Error::MisappliedCall { .. }) => {
            assert_eq!(error.to_string(), "cannot use DISTINCT with WITHIN GROUP");
        }
        other => panic!("got {other:?}"),
    }
}

/// A function in `FROM` writes no clause, so a window function, an
/// ordered-set aggregate and a parameterless aggregate called there are
/// refused as without one.
#[test]
fn a_function_in_from_is_refused_for_its_kind() {
    for (call, message) in [
        ("row_number()", "window function row_number requires an OVER clause"),
        ("count()", "count(*) must be used to call a parameterless aggregate function"),
        (
            "percentile_cont(0.5, 1.0)",
            "WITHIN GROUP is required for ordered-set aggregate percentile_cont",
        ),
        ("now(*)", "now(*) specified, but now is not an aggregate function"),
    ] {
        match parse(&format!("CREATE VIEW w AS SELECT * FROM {call};")) {
            Err(error @ Error::MisappliedCall { .. }) => {
                assert_eq!(error.to_string(), message, "{call}");
            }
            other => panic!("{call}\ngot {other:?}"),
        }
    }
}

/// An ordered-set aggregate whose arguments split between the direct ones
/// and those `WITHIN GROUP` orders by otherwise than it declares is no
/// function PostgreSQL has.
#[test]
fn an_ordered_set_aggregate_counts_its_direct_arguments() {
    for (expression, function, types) in [
        (
            "percentile_cont() WITHIN GROUP (ORDER BY 0.5, d)",
            "percentile_cont",
            &["numeric", "float8"][..],
        ),
        ("rank(1, 2) WITHIN GROUP (ORDER BY a)", "rank", &["int4", "int4", "int4"][..]),
    ] {
        match parse(&view(expression)) {
            Err(Error::UnresolvedCall { function_name, argument_types, reason }) => {
                assert_eq!(function_name, function, "{expression}");
                assert!(
                    argument_types.iter().map(AsRef::as_ref).eq(types.iter().copied()),
                    "{expression}: {argument_types:?}"
                );
                assert_eq!(reason, Unresolved::DoesNotExist, "{expression}");
            }
            other => panic!("{expression}\ngot {other:?}"),
        }
    }
}

/// Each call form PostgreSQL takes for the kind of function it selects
/// stores.
#[test]
fn calls_of_the_right_kind_are_stored() {
    for expression in [
        "count(*)",
        "count(*) OVER ()",
        "row_number(*) OVER ()",
        "row_number() OVER ()",
        "sum(a) FILTER (WHERE a > 0) OVER ()",
        "sum(a ORDER BY a)",
        "count(DISTINCT a) FILTER (WHERE a > 0)",
        "json_object_agg(key => k, value => v) OVER ()",
        "percentile_cont(0.5) WITHIN GROUP (ORDER BY d) FILTER (WHERE a > 0)",
        "mode() WITHIN GROUP (ORDER BY a)",
        "rank(1) WITHIN GROUP (ORDER BY a)",
        "rank(1, 2) WITHIN GROUP (ORDER BY a, a)",
    ] {
        if let Err(error) = parse(&view(expression)) {
            panic!("{expression}\ngot {error:?}");
        }
    }
}

/// A column default is refused as a view is.
#[test]
fn a_default_is_refused() {
    let result = parse("CREATE TABLE d (a timestamptz DEFAULT now(*));");
    match result {
        Err(error @ Error::MisappliedCall { .. }) => {
            assert_eq!(error.to_string(), "now(*) specified, but now is not an aggregate function");
        }
        other => panic!("got {other:?}"),
    }
}

/// Each aggregate answers its `pg_aggregate.aggkind` with the number of
/// direct arguments an ordered-set one takes, and any other function none.
#[test]
fn an_aggregate_answers_its_aggregate_kind() {
    let db = parse("").unwrap_or_else(|error| panic!("{error:?}"));
    let kind_of = |name: &str, arguments: usize| {
        db.functions()
            .find(|function| {
                function.name() == name && function.argument_type_names(&db).count() == arguments
            })
            .map(|function| {
                function.aggregate_kind(&db).unwrap_or_else(|error| panic!("{error:?}"))
            })
    };
    assert_eq!(kind_of("count", 1), Some(Some(AggregateKind::Normal)));
    assert_eq!(
        kind_of("percentile_cont", 2),
        Some(Some(AggregateKind::OrderedSet { direct_arguments: 1 }))
    );
    assert_eq!(kind_of("mode", 1), Some(Some(AggregateKind::OrderedSet { direct_arguments: 0 })));
    assert_eq!(
        kind_of("rank", 1),
        Some(Some(AggregateKind::HypotheticalSet { direct_arguments: 1 }))
    );
    assert_eq!(kind_of("rank", 0), Some(None));
    assert_eq!(kind_of("lower", 1), Some(None));
}
