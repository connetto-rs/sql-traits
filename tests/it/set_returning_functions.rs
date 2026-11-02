//! Tests that a set-returning declaration stays distinguishable from the
//! scalar declaration of the same element type.
//!
//! The upstream report showed `RETURNS UUID` and `RETURNS SETOF UUID`
//! answering identically through `return_type_name`, with no accessor
//! reporting the difference. Now the name keeps its `SETOF` marker, the way
//! an array keeps its `[]`, and `returns_set` mirrors `pg_proc.proretset`.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::MsSqlDialect;

/// Builds a database declaring `probe_fn` with the given signature suffix.
fn probe(declared: &str) -> ParserDB {
    let ddl = format!("CREATE FUNCTION probe_fn{declared} AS 'SELECT 1';");
    ParserDB::parse::<GenericDialect>(&ddl).expect("schema builds")
}

/// The declaration table from the upstream report: the set-returning row is
/// the only one whose spelling changes, and set-ness is answered for every
/// row.
#[test]
fn test_set_returning_declaration_is_distinguishable() {
    for (declared, expected_return, expected_set) in [
        ("() RETURNS UUID", Some("UUID"), false),
        ("() RETURNS UUID[]", Some("UUID[]"), false),
        ("() RETURNS SETOF UUID", Some("SETOF UUID"), true),
        ("() RETURNS SETOF app.my_type", Some("SETOF app.my_type"), true),
        ("() RETURNS TABLE(id UUID)", Some("TABLE"), true),
        ("() RETURNS TRIGGER", Some("TRIGGER"), false),
        ("()", None, false),
    ] {
        let database = probe(declared);
        let function = database
            .function_by_target(TargetName::new("probe_fn", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("input declares probe_fn");

        assert_eq!(
            function.return_type_name(&database).as_deref(),
            expected_return,
            "return type failed for {declared}"
        );
        assert_eq!(function.returns_set(), expected_set, "set-ness failed for {declared}");
    }
}

/// Normalization rewrites the element type and keeps the marker, so a caller
/// reading the normalized name still sees the set-ness it saw in the SQL.
#[test]
fn test_normalization_keeps_the_setof_marker() {
    let database = probe("() RETURNS SETOF UUID");
    let function = database
        .function_by_target(TargetName::new("probe_fn", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("input declares probe_fn");

    assert_eq!(function.normalized_return_type_name(&database).as_deref(), Some("SETOF UUID"));
}

/// MSSQL declares a table-valued function through a named table variable
/// rather than `SETOF`, and it is a set all the same.
#[test]
fn test_mssql_named_table_return_is_a_set() {
    let database = ParserDB::parse::<MsSqlDialect>(
        "CREATE FUNCTION probe_fn(@x INT) RETURNS @result TABLE (id INT) AS BEGIN INSERT INTO @result SELECT 1; RETURN; END",
    )
    .expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("probe_fn", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("input declares probe_fn");

    assert_eq!(function.return_type_name(&database).as_deref(), Some("TABLE"));
    assert!(function.returns_set());
}
