//! When an aggregate returns `NULL`, from `pg_aggregate` and the strictness
//! of its transition and final functions.
#![expect(clippy::expect_used, reason = "test fixtures must parse and resolve")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

use crate::support::function_overloads::overload;

/// The `NULL` answers a sample of the builtin aggregates in `pg_catalog` give,
/// one row per aggregate, named as `pg_proc` stores it.
mod expected {
    /// Name, argument types, the answer over no input row, and the answer over
    /// `NULL` inputs only.
    pub(super) type Row = (&'static str, &'static [&'static str], bool, bool);

    pub(super) const EXPECTED: &[Row] = &[
        // A `NULL` initial state, a strict transition function and no final
        // function, so the aggregate returns `NULL` in both cases.
        ("max", &["int"], true, true),
        ("max", &["text"], true, true),
        ("min", &["int"], true, true),
        ("bool_or", &["boolean"], true, true),
        ("bool_and", &["boolean"], true, true),
        ("every", &["boolean"], true, true),
        ("bit_and", &["int"], true, true),
        // A `NULL` initial state and no final function, but a non-strict
        // transition function. The aggregate returns `NULL` over no input
        // row, but the `NULL`-inputs answer is not provable from the catalog.
        ("sum", &["int"], true, false),
        ("sum", &["smallint"], true, false),
        // A non-`NULL` initial state, so the aggregate returns a value over no
        // input row.
        ("count", &[], false, false),
        ("count", &["any"], false, false),
        // A non-strict final function may turn a `NULL` state into a value, so
        // neither answer is provable.
        ("array_agg", &["anynonarray"], false, false),
        ("array_agg", &["anyarray"], false, false),
        ("string_agg", &["text", "text"], false, false),
    ];
}

/// A sample of the builtin aggregates answers the two `NULL` questions the way
/// `pg_aggregate` and the strictness of its transition and final functions
/// allow.
#[test]
fn sampled_builtins_answer_their_null_cases() {
    let database = db("");
    for (name, argument_types, no_input_row, null_inputs) in expected::EXPECTED {
        let function = overload(&database, "pg_catalog", name, argument_types);
        assert_eq!(
            (
                function.returns_null_over_no_input_row(&database),
                function.returns_null_over_null_inputs(&database),
            ),
            (*no_input_row, *null_inputs),
            "{name}({argument_types:?})"
        );
    }
}

/// A function that is no aggregate answers `false` to both questions, whether
/// declared, a window function, or a plain builtin.
#[test]
fn non_aggregates_answer_false() {
    let database = db("
        CREATE FUNCTION total(a INT) RETURNS INT LANGUAGE sql AS 'SELECT a';
    ");
    let total = database
        .functions()
        .find(|function| function.name() == "total")
        .expect("total is registered");
    assert!(!total.returns_null_over_no_input_row(&database));
    assert!(!total.returns_null_over_null_inputs(&database));

    let row_number = database
        .functions()
        .find(|function| function.name() == "row_number")
        .expect("row_number is a builtin");
    assert!(!row_number.returns_null_over_no_input_row(&database));
    assert!(!row_number.returns_null_over_null_inputs(&database));

    let lower = database
        .functions()
        .find(|function| function.name() == "lower")
        .expect("lower is a builtin");
    assert!(!lower.returns_null_over_no_input_row(&database));
    assert!(!lower.returns_null_over_null_inputs(&database));
}
