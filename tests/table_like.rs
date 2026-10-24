//! Tests that a table built as a copy of another carries that table's
//! columns, and that the copy is only a copy.
//!
//! `CREATE TABLE copy (LIKE original)` parsed and was then discarded, so the
//! model held `copy` with no columns whatsoever: nothing to project, no
//! nullability to honour, nothing for a policy to name.
//!
//! Copying is not inheritance and shares none of its machinery. It duplicates
//! the columns once, at the point the statement runs, and records no link, so
//! a later change to the original must not reach the copy.
//!
//! Every expectation was measured against PostgreSQL 18.4. The parts that are
//! easy to guess wrong: a copy receives `NOT NULL` but not the primary key,
//! unique constraint, check, identity or stored generated expression that
//! may have implied it, and it receives a default only when the statement
//! says `INCLUDING DEFAULTS`.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_names.rs"));

/// A source table exercising every column feature a copy might carry.
const SOURCE: &str = "CREATE TABLE src (
     keyed INT PRIMARY KEY,
     uniq INT UNIQUE,
     defaulted INT NOT NULL DEFAULT 5,
     checked INT CHECK (checked > 0),
     counted INT GENERATED ALWAYS AS IDENTITY,
     base INT NOT NULL,
     doubled INT GENERATED ALWAYS AS (base * 2) STORED,
     labelled TEXT COLLATE \"C\",
     plain TEXT
 );";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

fn nullability(database: &ParserDB, table_name: &str) -> Vec<(String, bool)> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| {
            (
                column.column_name().to_owned(),
                column.is_nullable(database).expect("column is in this database"),
            )
        })
        .collect()
}

fn defaults(database: &ParserDB, table_name: &str) -> Vec<(String, Option<String>)> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| (column.column_name().to_owned(), column.default_value()))
        .collect()
}

#[test]
fn a_copy_receives_the_columns_of_the_table_it_copies() {
    let database = database(&format!("{SOURCE} CREATE TABLE bare (LIKE src);"));

    assert_eq!(
        column_names(&database, "bare"),
        [
            "keyed",
            "uniq",
            "defaulted",
            "checked",
            "counted",
            "base",
            "doubled",
            "labelled",
            "plain"
        ]
    );
}

#[test]
fn a_copy_receives_not_null_but_none_of_what_implied_it() {
    let database = database(&format!("{SOURCE} CREATE TABLE bare (LIKE src);"));
    let copy = database
        .table_by_target(TargetName::new("bare", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");

    // `keyed` and `counted` are not nullable because a primary key and an
    // identity each imply it, even though neither is copied.
    assert_eq!(
        nullability(&database, "bare"),
        [
            ("keyed".to_owned(), false),
            ("uniq".to_owned(), true),
            ("defaulted".to_owned(), false),
            ("checked".to_owned(), true),
            ("counted".to_owned(), false),
            ("base".to_owned(), false),
            ("doubled".to_owned(), true),
            ("labelled".to_owned(), true),
            ("plain".to_owned(), true),
        ]
    );

    assert_eq!(copy.primary_key_columns(&database).expect("in database").count(), 0);
    assert_eq!(copy.unique_indices(&database).expect("in database").count(), 0);
    assert_eq!(copy.check_constraints(&database).expect("in database").count(), 0);
    assert_eq!(copy.indices(&database).expect("in database").count(), 0);

    // The original keeps everything it declared.
    let source = database
        .table_by_target(TargetName::new("src", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(source.primary_key_columns(&database).expect("in database").count(), 1);
    assert_eq!(source.check_constraints(&database).expect("in database").count(), 1);
}

#[test]
fn a_default_arrives_only_when_the_statement_asks_for_it() {
    let with = database(&format!("{SOURCE} CREATE TABLE c (LIKE src INCLUDING DEFAULTS);"));
    let defaulted = defaults(&with, "c")
        .into_iter()
        .find(|(name, _)| name == "defaulted")
        .expect("column exists");
    assert_eq!(defaulted.1, Some("5".to_owned()));

    // Saying nothing and saying `EXCLUDING DEFAULTS` both leave it behind.
    for clause in ["", " EXCLUDING DEFAULTS"] {
        let without = database(&format!("{SOURCE} CREATE TABLE c (LIKE src{clause});"));
        assert!(
            defaults(&without, "c").into_iter().all(|(_, default)| default.is_none()),
            "`LIKE src{clause}` copies no default"
        );
    }
}

#[test]
fn a_copy_is_not_a_child() {
    let database = database(&format!("{SOURCE} CREATE TABLE bare (LIKE src);"));

    let copy = database
        .table_by_target(TargetName::new("bare", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let source = database
        .table_by_target(TargetName::new("src", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");

    // Copying records no edge in either direction, and every column the copy
    // holds is its own.
    assert_eq!(copy.inherits_from(&database).expect("in database").count(), 0);
    assert_eq!(source.inheritors(&database).expect("in database").count(), 0);
    assert_eq!(copy.local_columns(&database).expect("in database").count(), 9);
}

#[test]
fn a_copy_does_not_follow_the_table_it_copied() {
    // The distinguishing property: a copy is taken once. An inheriting child
    // would have followed both of these changes.
    let database = database(
        "CREATE TABLE src (a INT);
         CREATE TABLE copy (LIKE src);
         ALTER TABLE src ADD COLUMN added TEXT;
         ALTER TABLE src DROP COLUMN a;",
    );

    assert_eq!(column_names(&database, "copy"), ["a"]);
    assert_eq!(column_names(&database, "src"), ["added"]);
}

#[test]
fn a_copy_can_name_a_table_in_another_schema() {
    let database = database(
        "CREATE SCHEMA s1;
         CREATE TABLE s1.other (z INT NOT NULL);
         CREATE TABLE fromschema (LIKE s1.other);",
    );

    assert_eq!(column_names(&database, "fromschema"), ["z"]);
    assert_eq!(nullability(&database, "fromschema"), [("z".to_owned(), false)]);
}

#[test]
fn copying_a_table_the_input_never_created_is_refused() {
    assert!(matches!(
        parse("CREATE TABLE c (LIKE absent_table);"),
        Err(Error::CopiedTableNotFound { ref copied_table, ref table_name })
            if copied_table == "absent_table" && table_name == "c"
    ));
}

#[test]
fn a_copy_and_an_inheritance_in_one_statement_each_contribute() {
    // PostgreSQL puts the parent's columns first and counts the copied ones
    // as the table's own, because a copy is a declaration.
    let database = database(
        "CREATE TABLE lsrc (l1 INT, l2 TEXT);
         CREATE TABLE par (p1 INT);
         CREATE TABLE combo (LIKE lsrc) INHERITS (par);",
    );

    assert_eq!(column_names(&database, "combo"), ["p1", "l1", "l2"]);

    let combo = database
        .table_by_target(TargetName::new("combo", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(
        combo
            .local_columns(&database)
            .expect("in database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<_>>(),
        ["l1", "l2"]
    );
    assert_eq!(
        combo
            .inherits_from(&database)
            .expect("in database")
            .map(|parent| parent.table_name().to_owned())
            .collect::<Vec<_>>(),
        ["par"]
    );
}

#[test]
fn the_spelling_without_parentheses_copies_alike() {
    // Snowflake and BigQuery write the clause outside the column list. The
    // parser accepts it for any dialect, and it names the same copy.
    let database = database("CREATE TABLE src (a INT NOT NULL, b TEXT); CREATE TABLE c LIKE src;");

    assert_eq!(column_names(&database, "c"), ["a", "b"]);
    assert_eq!(nullability(&database, "c"), [("a".to_owned(), false), ("b".to_owned(), true)]);
    assert_eq!(
        database
            .table_by_target(TargetName::new("c", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .inherits_from(&database)
            .expect("in database")
            .count(),
        0
    );
}

#[test]
fn a_table_that_copies_nothing_is_left_alone() {
    let database = database("CREATE TABLE plain (a INT PRIMARY KEY, b TEXT);");

    assert_eq!(column_names(&database, "plain"), ["a", "b"]);
    assert_eq!(
        database
            .table_by_target(TargetName::new("plain", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .primary_key_columns(&database)
            .expect("in database")
            .count(),
        1
    );
}
