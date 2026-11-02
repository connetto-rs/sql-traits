//! Tests for the stored-name accessors on the schema traits.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

const SQL: &str = "CREATE SCHEMA My_Schema;
CREATE SCHEMA \"Other\";
CREATE TABLE My_Schema.Docs (ID uuid, Owner_Id text, \"Mixed_Case\" text);
CREATE TABLE \"Other\".\"Docs\" (id uuid);
CREATE TABLE plain (id uuid);
CREATE FUNCTION Add_One(x INT) RETURNS INT AS 'SELECT x + 1;';";

fn db() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SQL).expect("schema parses")
}

#[test]
fn unquoted_identifiers_fold_to_lowercase() {
    let db = db();
    let table = db
        .table_by_target(
            TargetName::new("docs", false).with_schema("my_schema", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists");

    assert_eq!(table.table_name(), "Docs");
    assert_eq!(table.stored_table_name(), "docs");
    assert_eq!(table.stored_table_schema().as_deref(), Some("my_schema"));

    let stored: Vec<_> = table
        .columns(&db)
        .expect("columns succeeds")
        .map(|column| column.stored_column_name().into_owned())
        .collect();
    assert_eq!(stored, vec!["id", "owner_id", "Mixed_Case"]);
}

#[test]
fn quoted_identifiers_keep_their_case() {
    let db = db();
    let table = db
        .table_by_target(
            TargetName::new("Docs", true).with_schema("Other", true),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists");

    assert_eq!(table.stored_table_name(), "Docs");
    assert_eq!(table.stored_table_schema().as_deref(), Some("Other"));
}

#[test]
fn a_table_without_a_schema_stores_no_schema_name() {
    let db = db();
    let table = db
        .table_by_target(TargetName::new("plain", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");

    assert_eq!(table.table_schema(), None);
    assert_eq!(table.stored_table_schema(), None);
}

#[test]
fn schemas_and_functions_expose_their_stored_name() {
    let db = db();

    assert_eq!(db.schema("my_schema").expect("schema exists").stored_name(), "my_schema");
    assert_eq!(db.schema("\"Other\"").expect("schema exists").stored_name(), "Other");
    assert_eq!(db.functions().next().expect("function exists").stored_name(), "add_one");
}
