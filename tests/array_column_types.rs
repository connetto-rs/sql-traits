//! Tests that the declared type of an array column is answered rather than
//! aborting the process.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{
    dialect::{DatabricksDialect, PostgreSqlDialect},
    parser::Parser,
};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_type_probes.rs"));

#[test]
fn test_array_column_data_type() {
    assert_eq!(
        column_types("CREATE TABLE t (id INTEGER PRIMARY KEY, tags TEXT[]);"),
        vec!["INT", "TEXT[]"]
    );
}

/// Every array spelling PostgreSQL accepts answers a canonical token: the
/// declared length is dropped exactly as a `VARCHAR(255)` length is, and
/// nesting is preserved.
#[test]
fn test_array_column_spellings() {
    for (declared, expected) in [
        ("TEXT[]", "TEXT[]"),
        ("INTEGER[]", "INT[]"),
        ("TEXT[3]", "TEXT[]"),
        ("INT[][]", "INT[][]"),
        ("TIMESTAMPTZ[]", "TIMESTAMPTZ[]"),
    ] {
        assert_eq!(
            column_types(&format!("CREATE TABLE t (value {declared});")),
            vec![expected],
            "failed for {declared}"
        );
    }
}

/// The normalized type of an array column is the array token itself, since no
/// PostgreSQL alias folds into it.
#[test]
fn test_array_column_normalized_data_type() {
    let statements =
        Parser::parse_sql(&PostgreSqlDialect {}, "CREATE TABLE t (tags TEXT[]);").expect("parses");
    let database =
        ParserDB::from_statements(statements, "test".to_string()).expect("schema builds");
    let table = database.tables().next().expect("input declares a table");
    let column = table
        .column("tags", &database, IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("column exists");

    assert_eq!(column.data_type(&database).normalized_name(&database), "TEXT[]");
    assert!(!column.data_type(&database).is_textual(&database));
}

/// A Databricks element collation stays with the element, never the column.
#[test]
fn test_array_column_collated_element() {
    let database = ParserDB::parse::<DatabricksDialect>(
        "CREATE TABLE t (tags ARRAY<STRING COLLATE UTF8_LCASE>, name STRING COLLATE UTF8_LCASE);",
    )
    .expect("schema builds");
    let table = database.tables().next().expect("input declares a table");
    let column = |name| {
        table
            .column(name, &database, IdentifierCase::AsWritten)
            .expect("lookup runs")
            .expect("column exists")
    };

    let tags = column("tags");
    assert_eq!(tags.data_type(&database).name(&database), "STRING[]");
    assert_eq!(
        tags.collation(&database).expect("collation resolves"),
        ColumnCollation::DatabaseDefault
    );

    let name = column("name");
    assert_eq!(name.data_type(&database).name(&database), "STRING");
    assert!(matches!(
        name.collation(&database).expect("collation resolves"),
        ColumnCollation::Named(collation) if collation.name().name() == "UTF8_LCASE"
    ));
}

/// An array column takes part in a schema fingerprint like any other column,
/// and a differing element type is a differing fingerprint.
#[test]
fn test_array_column_fingerprint() {
    assert_eq!(
        fingerprint("CREATE TABLE t (tags TEXT[]);"),
        fingerprint("CREATE TABLE t (tags TEXT[]);")
    );
    assert_ne!(
        fingerprint("CREATE TABLE t (tags TEXT[]);"),
        fingerprint("CREATE TABLE t (tags INT[]);")
    );
}

/// A function taking and returning arrays reports both without aborting.
#[test]
fn test_array_function_types() {
    let ddl = "CREATE FUNCTION first_tag(tags TEXT[]) RETURNS TEXT[] AS $$ SELECT tags $$ \
               LANGUAGE SQL;";
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, ddl).expect("parses");
    let database =
        ParserDB::from_statements(statements, "test".to_string()).expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("first_tag", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");

    assert_eq!(function.argument_type_names(&database).collect::<Vec<_>>(), vec!["TEXT[]"]);
    assert_eq!(function.return_type_name(&database).as_deref(), Some("TEXT[]"));
}
