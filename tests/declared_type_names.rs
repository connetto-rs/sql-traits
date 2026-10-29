//! Tests that the declared type of a column, a function argument and a
//! function return is answered rather than aborting the process.
//!
//! Every declaration here parses, so reaching it means the caller was handed a
//! database and then asked it an ordinary question about a type.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    dialect::{GenericDialect, PostgreSqlDialect, SQLiteDialect, SnowflakeDialect},
    parser::Parser,
};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_type_probes.rs"));

/// Builds a database from `ddl` and returns the declared argument types and
/// return type of its `probe_fn` function.
fn function_types(ddl: &str) -> (Vec<String>, Option<String>) {
    let statements = Parser::parse_sql(&GenericDialect {}, ddl).expect("SQL parses");
    let database =
        ParserDB::from_statements(statements, "test".to_string()).expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("probe_fn", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("input declares probe_fn");

    (
        function.argument_type_names(&database).map(std::borrow::Cow::into_owned).collect(),
        function.return_type_name(&database).map(std::borrow::Cow::into_owned),
    )
}

/// Every column type PostgreSQL accepts answers a name.
///
/// The types grouped here are the ones a schema reaches without trying: a
/// spelled-out `CHARACTER VARYING`, an interval, a bit string, a text search
/// column, a catalog reference and a geometric value.
#[test]
fn test_postgres_column_types_are_answered() {
    for (declared, expected) in [
        ("CHARACTER VARYING(10)", "VARCHAR"),
        ("CHARACTER VARYING", "VARCHAR"),
        ("INTERVAL", "INTERVAL"),
        ("INTERVAL YEAR TO MONTH", "INTERVAL"),
        ("BIT(8)", "BIT"),
        ("BIT VARYING(8)", "VARBIT"),
        ("TSVECTOR", "TSVECTOR"),
        ("TSQUERY", "TSQUERY"),
        ("REGCLASS", "REGCLASS"),
        ("FLOAT4", "FLOAT4"),
        ("FLOAT8", "FLOAT8"),
        ("POINT", "POINT"),
        ("TEXT", "TEXT"),
    ] {
        assert_eq!(
            column_types(&format!("CREATE TABLE t (value {declared});")),
            vec![expected],
            "failed for {declared}"
        );
    }
}

#[test]
fn test_custom_column_types_keep_modifiers() {
    for (declared, expected) in [
        ("GEOMETRY(Point, 4326)", "GEOMETRY(Point, 4326)"),
        ("GEOGRAPHY(Point, 4326)", "GEOGRAPHY(Point, 4326)"),
        ("geometry(Point, 4326)", "geometry(Point, 4326)"),
        ("geometry(Point, 3857)", "geometry(Point, 3857)"),
        ("geography(Polygon)", "geography(Polygon)"),
        ("GEOGRAPHY(Point, 4269)", "GEOGRAPHY(Point, 4269)"),
        ("GeOmEtRy(point, 3857)", "GeOmEtRy(point, 3857)"),
        ("shape(Point, 4326)", "shape(Point, 4326)"),
        ("custom_type(1, two, 3)", "custom_type(1, two, 3)"),
        (r#"custom_type("Point", 'it''s', 3)"#, r#"custom_type("Point", 'it''s', 3)"#),
        (r#""Geometry"(Point, 3857)"#, "Geometry(Point, 3857)"),
        ("geometry(Point, 3857)[]", "geometry(Point, 3857)[]"),
        ("geometry", "geometry"),
        ("geography", "geography"),
    ] {
        assert_eq!(
            column_types(&format!("CREATE TABLE t (value {declared});")),
            vec![expected],
            "failed for {declared}"
        );
    }
}

/// A composite type or a domain declared in a schema keeps the schema, since
/// the schema is what tells two same-named types apart.
#[test]
fn test_schema_qualified_column_type_keeps_its_schema() {
    assert_eq!(column_types("CREATE TABLE t (value app.my_type);"), vec!["app.my_type"]);
    assert_eq!(column_types("CREATE TABLE t (value public.citext);"), vec!["public.citext"]);
    assert_eq!(column_types("CREATE TABLE t (value app.my_type[]);"), vec!["app.my_type[]"]);
}

/// A zero-length quoted schema parses, so the join has to keep its separator
/// rather than silently reporting the type as unqualified.
#[test]
fn test_empty_schema_part_keeps_its_separator() {
    assert_eq!(column_types(r#"CREATE TABLE t (value "".foo);"#), vec![".foo"]);
    assert_eq!(column_types(r#"CREATE TABLE t (value ""."");"#), vec!["."]);
}

/// SQLite lets a column declare no type at all, which the parser records as a
/// type of its own. The answer names the absence rather than being empty,
/// which would collapse into a neighbouring column's fingerprint.
#[test]
fn test_sqlite_typeless_column_is_answered() {
    let database = ParserDB::parse::<SQLiteDialect>("CREATE TABLE t (a);").expect("schema builds");
    let table = database.tables().next().expect("input declares a table");
    let types: Vec<_> = table
        .columns(&database)
        .expect("table belongs to the database")
        .map(|column| column.data_type(&database).name(&database).into_owned())
        .collect();

    assert_eq!(types, vec!["UNSPECIFIED"]);
}

/// Snowflake's structured `OBJECT(...)` and its semi-structured `OBJECT` are
/// one type family, the field list dropped as a declared length is.
#[test]
fn test_snowflake_object_columns_are_one_family() {
    let database = ParserDB::parse::<SnowflakeDialect>(
        "CREATE TABLE t (address OBJECT(city VARCHAR, zip INT), payload OBJECT);",
    )
    .expect("schema builds");
    let table = database.tables().next().expect("input declares a table");
    let types: Vec<_> = table
        .columns(&database)
        .expect("table belongs to the database")
        .map(|column| column.data_type(&database).name(&database).into_owned())
        .collect();

    assert_eq!(types, vec!["OBJECT", "OBJECT"]);
}

/// A column type that used to abort takes part in a schema fingerprint like
/// any other, and two different such types stay different.
#[test]
fn test_formerly_aborting_column_type_fingerprints() {
    assert_eq!(
        fingerprint("CREATE TABLE t (span INTERVAL);"),
        fingerprint("CREATE TABLE t (span INTERVAL);")
    );
    assert_ne!(
        fingerprint("CREATE TABLE t (span INTERVAL);"),
        fingerprint("CREATE TABLE t (span TSVECTOR);")
    );
    assert_ne!(
        fingerprint("CREATE TABLE t (value app.my_type);"),
        fingerprint("CREATE TABLE t (value other.my_type);")
    );
}

/// The declaration table from the upstream report, each row parsed first and
/// then asked for both its argument types and its return type.
#[test]
fn test_function_declarations_are_answered() {
    for (declared, expected_arguments, expected_return) in [
        ("() RETURNS UUID", vec![], Some("UUID")),
        ("() RETURNS UUID[]", vec![], Some("UUID[]")),
        ("() RETURNS SETOF UUID", vec![], Some("SETOF UUID")),
        ("() RETURNS my_type", vec![], Some("my_type")),
        ("() RETURNS app.my_type", vec![], Some("app.my_type")),
        ("() RETURNS SETOF app.my_type", vec![], Some("SETOF app.my_type")),
        ("(a app.my_type) RETURNS UUID", vec!["app.my_type"], Some("UUID")),
        ("() RETURNS TRIGGER", vec![], Some("TRIGGER")),
        ("() RETURNS TABLE(id UUID)", vec![], Some("TABLE")),
        ("() RETURNS RECORD", vec![], Some("RECORD")),
        ("() RETURNS VOID", vec![], Some("VOID")),
        ("() RETURNS INTERVAL", vec![], Some("INTERVAL")),
        ("(a INTERVAL, b TSVECTOR) RETURNS VOID", vec!["INTERVAL", "TSVECTOR"], Some("VOID")),
    ] {
        let ddl = format!("CREATE FUNCTION probe_fn{declared} AS 'SELECT 1';");
        let (arguments, returned) = function_types(&ddl);

        assert_eq!(arguments, expected_arguments, "arguments failed for {declared}");
        assert_eq!(returned.as_deref(), expected_return, "return failed for {declared}");
    }
}

/// A trigger function is the common case the report leads with, taking the
/// same route through the public API that the report's reproduction takes.
#[test]
fn test_trigger_function_reports_its_return_type() {
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE FUNCTION probe_fn() RETURNS TRIGGER AS 'SELECT 1';",
    )
    .expect("schema builds");
    let function =
        database.functions().find(|function| function.name() == "probe_fn").expect("declared");

    assert_eq!(function.return_type_name(&database).as_deref(), Some("TRIGGER"));
    assert_eq!(function.normalized_return_type_name(&database).as_deref(), Some("TRIGGER"));
}

/// Function identity is decided by normalizing each argument type, and that
/// runs inside the parse rather than at an accessor: a second `CREATE
/// FUNCTION` of the same name compares against the first, and a `DROP
/// FUNCTION` carrying an argument list compares against every candidate. Both
/// routes reached the normalizer with types it had no answer for.
#[test]
fn test_function_identity_normalizes_argument_types() {
    let overloaded = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(a app.my_type) RETURNS INT AS 'SELECT 1'; \
         CREATE FUNCTION f(a INT) RETURNS INT AS 'SELECT 1';",
    )
    .expect("two argument types, so two functions");
    assert_eq!(overloaded.functions().filter(|function| function.name() == "f").count(), 2);

    // The long and the short spelling of the same text type are one type, so
    // the second declaration is a redeclaration rather than an overload.
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(a VARCHAR) RETURNS INT AS 'SELECT 1'; \
         CREATE FUNCTION f(a CHARACTER VARYING) RETURNS INT AS 'SELECT 2';",
    )
    .expect_err("one type spelled twice is one function");
    assert!(matches!(&error, Error::FunctionAlreadyExists { .. }), "got {error:?}");

    let dropped = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(a app.my_type) RETURNS INT AS 'SELECT 1'; \
         DROP FUNCTION f(app.my_type);",
    )
    .expect("the drop resolves the same argument type the create declared");
    assert_eq!(dropped.functions().filter(|function| function.name() == "f").count(), 0);
}
