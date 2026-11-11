//! Tests for the stored-name accessors on the schema traits.
//!
//! The constraint and sequence names below were read back from `pg_constraint`
//! and `pg_class` on PostgreSQL 18.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The names of the checks of table `table` after `sql`.
fn check_names(sql: &str, table: &str, quoted: bool) -> Vec<String> {
    let db = parse(sql).expect("schema parses");
    let table = db
        .table_by_target(TargetName::new(table, quoted), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    table
        .check_constraints(&db)
        .expect("checks are held")
        .map(|check| check.constraint_name(&db).expect("name reads").expect("named").into_owned())
        .collect()
}

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

#[test]
fn a_quoted_constraint_name_keeps_its_surrounding_spaces() {
    let sql = "CREATE TABLE t (a INT CONSTRAINT \" c \" CHECK (a > 0));";
    assert_eq!(check_names(sql, "t", false), [" c "]);
    assert!(parse(&format!("{sql} ALTER TABLE t DROP CONSTRAINT c;")).is_err());
    assert!(parse(&format!("{sql} ALTER TABLE t DROP CONSTRAINT \" c \";")).is_ok());
    assert!(matches!(
        parse(
            "CREATE TABLE x (a INT CONSTRAINT \" n \" NOT NULL); ALTER TABLE x ADD CONSTRAINT n NOT NULL a;"
        ),
        Err(Error::NotNullConstraintAlreadyExists { .. })
    ));
}

#[test]
fn constraint_names_differing_only_in_unicode_composition_are_distinct() {
    let sql = "CREATE TABLE u (a INT, CONSTRAINT \"\u{e9}\" CHECK (a > 0), CONSTRAINT \"e\u{301}\" CHECK (a < 9));";
    assert_eq!(check_names(sql, "u", false), ["\u{e9}", "e\u{301}"]);
}

#[test]
fn a_long_constraint_name_is_cut_to_63_bytes() {
    let sql = format!("CREATE TABLE v (a INT CONSTRAINT {} CHECK (a > 0));", "r".repeat(70));
    assert_eq!(check_names(&sql, "v", false), ["r".repeat(63)]);
    assert!(parse(&format!("{sql} ALTER TABLE v DROP CONSTRAINT {};", "r".repeat(63))).is_ok());
}

#[test]
fn generated_names_keep_the_spaces_of_a_quoted_table_name() {
    let sql = "CREATE TABLE \" w \" (a INT CHECK (a > 0), id SERIAL);";
    assert_eq!(check_names(sql, " w ", true), [" w _a_check"]);
    let db = parse(sql).expect("schema parses");
    let sequences: Vec<_> = db
        .sequences()
        .map(|sequence| {
            (sequence.stored_sequence_name().into_owned(), sequence.sequence_name_is_quoted())
        })
        .collect();
    assert_eq!(sequences, [(" w _id_seq".to_owned(), true)]);
}
