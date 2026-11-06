//! Tests that `CREATE TYPE ... AS RANGE` records the constructor functions
//! PostgreSQL creates for the range and its multirange, and that they go with
//! the range and follow its schema.
//!
//! The expected outcomes are the ones PostgreSQL 14 and 18 give for the same
//! statements.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{
    ast::ArgMode,
    dialect::{GenericDialect, PostgreSqlDialect},
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// A function as `schema.name(modes and types) -> result`, followed by its
/// language, body, null-input behavior and volatility.
fn describe(database: &ParserDB, function: &<ParserDB as DatabaseLike>::Function) -> String {
    let arguments: Vec<String> = function
        .argument_modes()
        .zip(function.argument_type_names(database))
        .map(|(mode, data_type)| {
            match mode {
                ArgMode::In => data_type.into_owned(),
                mode => format!("{mode} {data_type}"),
            }
        })
        .collect();
    format!(
        "{}({}) -> {} {} '{}' {} {}",
        function.target_name(),
        arguments.join(", "),
        function.return_type_name(database).unwrap_or_default(),
        function.language().unwrap_or_default(),
        function.body().unwrap_or_default(),
        function.null_input_behavior(),
        function.volatility(),
    )
}

/// Every function outside `pg_catalog`, as [`describe`] spells it.
fn user_functions(database: &ParserDB) -> Vec<String> {
    database
        .functions()
        .filter(|function| function.target_name().schema() != Some("pg_catalog"))
        .map(|function| describe(database, function))
        .collect()
}

/// The five constructors of the range `name` over `subtype` in `schema`, with
/// the multirange `multirange`, as [`describe`] spells them.
fn constructors(schema: &str, name: &str, multirange: &str, subtype: &str) -> Vec<String> {
    let range = format!("{schema}.{name}");
    let multirange_type = format!("{schema}.{multirange}");
    vec![
        format!(
            "{range}({subtype}, {subtype}) -> {range} internal 'range_constructor2' \
             CALLED ON NULL INPUT IMMUTABLE"
        ),
        format!(
            "{range}({subtype}, {subtype}, TEXT) -> {range} internal 'range_constructor3' \
             CALLED ON NULL INPUT IMMUTABLE"
        ),
        format!(
            "{schema}.{multirange}() -> {multirange_type} internal 'multirange_constructor0' \
             STRICT IMMUTABLE"
        ),
        format!(
            "{schema}.{multirange}({range}) -> {multirange_type} internal \
             'multirange_constructor1' STRICT IMMUTABLE"
        ),
        format!(
            "{schema}.{multirange}(VARIADIC {range}[]) -> {multirange_type} internal \
             'multirange_constructor2' STRICT IMMUTABLE"
        ),
    ]
}

#[test]
fn a_range_brings_its_five_constructors() {
    let database = parse("CREATE SCHEMA app; CREATE TYPE app.span AS RANGE (subtype = int4);")
        .expect("the range is created");
    assert_eq!(user_functions(&database), constructors("app", "span", "span_multirange", "INT4"));
    for function in
        database.functions().filter(|function| function.target_name().schema() == Some("app"))
    {
        assert_eq!(function.kind(&database), Ok(FunctionKind::Normal));
        assert!(!function.returns_set());
        assert_eq!(function.argument_default_count(), 0);
        assert!(function.argument_names(&database).all(|name| name.is_none()));
        assert_eq!(function.security_mode(), sqlparser::ast::FunctionSecurity::Invoker);
        assert_eq!(function.owner(&database), Ok(None));
        assert_eq!(function.is_executable_by_public(&database), Ok(true));
    }
}

/// The multirange constructors take the multirange's name, and land in the
/// range's schema even when the multirange lives in another.
#[test]
fn the_multirange_constructors_take_the_multirange_name_in_the_range_schema() {
    let database = parse(
        "CREATE SCHEMA app; CREATE SCHEMA other;
         CREATE TYPE app.bar AS RANGE (subtype = int8, multirange_type_name = other.bar_mr);",
    )
    .expect("the range is created");
    let names: Vec<String> = database
        .functions()
        .filter(|function| function.target_name().schema() == Some("app"))
        .map(|function| function.target_name().to_string())
        .collect();
    assert_eq!(names, ["app.bar", "app.bar", "app.bar_mr", "app.bar_mr", "app.bar_mr"]);
}

/// A shell holds no constructor, and the range filling it brings them.
#[test]
fn a_shell_brings_its_constructors_once_filled() {
    let shell = parse("CREATE SCHEMA app; CREATE TYPE app.span;").expect("the shell is created");
    assert_eq!(user_functions(&shell), Vec::<String>::new());
    let filled = parse(
        "CREATE SCHEMA app; CREATE TYPE app.span; CREATE TYPE app.span AS RANGE (subtype = int4);",
    )
    .expect("the shell is filled");
    assert_eq!(user_functions(&filled), constructors("app", "span", "span_multirange", "INT4"));
}

/// Only PostgreSQL creates them, as only PostgreSQL creates a serial column's
/// sequence.
#[test]
fn other_dialects_record_no_constructor() {
    let database = ParserDB::parse::<GenericDialect>("CREATE TYPE span AS RANGE (subtype = int4);")
        .expect("the range is created");
    assert_eq!(user_functions(&database), Vec::<String>::new());
}

/// A call reaches the constructor of its argument count, the variadic one
/// included, and holds it in place.
#[test]
fn calls_reach_the_constructors() {
    let database = parse(
        "CREATE SCHEMA app; CREATE TYPE app.span AS RANGE (subtype = int4);
         CREATE TABLE t (
             x int4 CHECK (app.span(x, x + 1) IS NOT NULL),
             y int4 CHECK (app.span(y, y, '[]') IS NOT NULL),
             z int4 CHECK (
                 app.span_multirange(app.span(z, z + 1), app.span(z + 2, z + 3)) IS NOT NULL
             )
         );",
    )
    .expect("the table is created");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous")
        .expect("the table exists");
    let mut called: Vec<String> = table
        .check_constraints(&database)
        .expect("the table is held")
        .flat_map(|check| check.functions(&database).expect("the check is held"))
        .map(|function| describe(&database, function))
        .collect();
    called.sort();
    called.dedup();
    let mut expected = constructors("app", "span", "span_multirange", "INT4");
    expected.retain(|function| {
        !function.contains("constructor0") && !function.contains("constructor1")
    });
    assert_eq!(called, expected);

    assert!(matches!(
        parse(
            "CREATE TYPE span AS RANGE (subtype = int4);
             CREATE TABLE t (x int4 CHECK (span(x, x + 1) IS NOT NULL));
             DROP TYPE span CASCADE;",
        ),
        Err(Error::FunctionReferenced { function_name }) if function_name == "span"
    ));
}

#[test]
fn a_constructor_is_dropped_only_with_its_range() {
    for (drop, required_by) in [
        ("DROP FUNCTION span(int4, int4);", "span"),
        ("DROP FUNCTION IF EXISTS span(int4, int4, text);", "span"),
        ("DROP FUNCTION span_multirange();", "span_multirange"),
        ("DROP FUNCTION span_multirange(VARIADIC span[]);", "span_multirange"),
        (
            "ALTER TYPE span_multirange RENAME TO spans; DROP FUNCTION span_multirange(span);",
            "spans",
        ),
    ] {
        let result = parse(&format!("CREATE TYPE span AS RANGE (subtype = int4); {drop}"));
        assert!(
            matches!(
                &result,
                Err(Error::FunctionRequiredByType { type_name, .. }) if type_name == required_by
            ),
            "{drop}: {result:?}"
        );
    }
}

#[test]
fn the_constructors_go_with_their_range() {
    for sql in [
        "CREATE TYPE span AS RANGE (subtype = int4); DROP TYPE span;",
        "CREATE TYPE span AS RANGE (subtype = int4); DROP TYPE span CASCADE;",
        "CREATE TYPE e AS ENUM ('a', 'b'); CREATE TYPE span AS RANGE (subtype = e);
         DROP TYPE e CASCADE;",
        "CREATE SCHEMA app; CREATE TYPE app.span AS RANGE (subtype = int4);
         DROP SCHEMA app CASCADE;",
        "CREATE TYPE span AS RANGE (subtype = int4); ALTER TYPE span RENAME TO q; DROP TYPE q;",
        "CREATE SCHEMA app; CREATE TYPE app.span AS RANGE (subtype = int4);
         ALTER SCHEMA app RENAME TO moved; DROP TYPE moved.span;",
    ] {
        let database = parse(sql).expect("the range is dropped");
        assert_eq!(user_functions(&database), Vec::<String>::new(), "{sql}");
    }
}

/// A rename keeps the constructors' names, and a schema rename moves them.
#[test]
fn renames_keep_the_constructor_names_and_schema_renames_move_them() {
    let names = |sql: &str| -> Vec<String> {
        let database = parse(sql).expect("the statements apply");
        database
            .functions()
            .filter(|function| function.target_name().schema() != Some("pg_catalog"))
            .map(|function| function.target_name().to_string())
            .collect()
    };
    assert_eq!(
        names(
            "CREATE SCHEMA app; CREATE TYPE app.span AS RANGE (subtype = int4);
             ALTER TYPE app.span RENAME TO q; ALTER TYPE app.span_multirange RENAME TO qs;"
        ),
        [
            "app.span",
            "app.span",
            "app.span_multirange",
            "app.span_multirange",
            "app.span_multirange"
        ]
    );
    assert_eq!(
        names(
            "CREATE SCHEMA app; CREATE TYPE app.span AS RANGE (subtype = int4);
             ALTER SCHEMA app RENAME TO moved;"
        ),
        [
            "moved.span",
            "moved.span",
            "moved.span_multirange",
            "moved.span_multirange",
            "moved.span_multirange"
        ]
    );
}

#[test]
fn a_function_of_a_constructor_signature_is_refused() {
    for sql in [
        "CREATE FUNCTION span(int4, int4) RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE TYPE span AS RANGE (subtype = int4);",
        "CREATE FUNCTION span_multirange() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE TYPE span AS RANGE (subtype = int4);",
        "CREATE TYPE span AS RANGE (subtype = int4);
         CREATE FUNCTION span(int4, int4, text) RETURNS int LANGUAGE sql AS 'SELECT 1';",
    ] {
        assert!(matches!(parse(sql), Err(Error::FunctionAlreadyExists { .. })), "{sql} is refused");
    }
    parse(
        "CREATE FUNCTION span(int8, int8) RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE TYPE span AS RANGE (subtype = int4);",
    )
    .expect("another signature leaves the constructor free");
}

/// `CREATE OR REPLACE` turns a constructor into an ordinary function, which
/// `DROP FUNCTION` then drops and which holds the range in place as any
/// function naming it does.
#[test]
fn a_replaced_constructor_is_an_ordinary_function() {
    let replace = "CREATE TYPE span AS RANGE (subtype = int4);
         CREATE OR REPLACE FUNCTION span(int4, int4) RETURNS span
             LANGUAGE sql AS 'SELECT NULL::span';";
    parse(&format!("{replace} DROP FUNCTION span(int4, int4);"))
        .expect("the replaced constructor is dropped on its own");
    assert!(matches!(
        parse(&format!("{replace} DROP TYPE span;")),
        Err(Error::TypeHasDependents { dependent_kind: ObjectKind::Function, .. })
    ));
    let database =
        parse(&format!("{replace} DROP TYPE span CASCADE;")).expect("the range is dropped");
    assert_eq!(user_functions(&database), Vec::<String>::new());
}

/// An alteration keeps a constructor bound to its range.
#[test]
fn an_altered_constructor_still_goes_with_its_range() {
    let altered = "CREATE TYPE span AS RANGE (subtype = int4);
         ALTER FUNCTION span(int4, int4) STABLE;";
    assert!(matches!(
        parse(&format!("{altered} DROP FUNCTION span(int4, int4);")),
        Err(Error::FunctionRequiredByType { .. })
    ));
    let database = parse(&format!("{altered} DROP TYPE span;")).expect("the range is dropped");
    assert_eq!(user_functions(&database), Vec::<String>::new());
}
