//! Tests that a foreign key reads its columns under the comparison the parse
//! took its names under.
//!
//! Measured on SQLite 3.51.1 with `PRAGMA foreign_keys = ON`: `CREATE TABLE
//! c(pid REFERENCES p(ID))` against `p(id INTEGER PRIMARY KEY)` enforces the
//! key on `id`, and `CREATE TABLE c("Pid" INTEGER, FOREIGN KEY (pid)
//! REFERENCES p(id))` against `p("ID" INTEGER PRIMARY KEY)` enforces it on
//! `Pid` and `ID`. PostgreSQL 18.7 stores `REFERENCES p2(ID)` against
//! `p2(id int PRIMARY KEY)` as `REFERENCES p2(id)`.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

/// The host and referenced column names of the one foreign key of `c`.
fn key_columns(database: &ParserDB) -> (Vec<&str>, Vec<&str>) {
    let child = database
        .table_by_target(TargetName::new("c", false), IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .expect("table c exists");
    let key = child
        .foreign_keys(database)
        .expect("table is in this database")
        .next()
        .expect("c declares one foreign key");
    (
        key.host_columns(database)
            .expect("host columns resolve")
            .map(ColumnLike::column_name)
            .collect(),
        key.referenced_columns(database)
            .expect("referenced columns resolve")
            .map(ColumnLike::column_name)
            .collect(),
    )
}

/// An unquoted name reaches the column under PostgreSQL's rule, whatever
/// case each side spells it in.
#[test]
fn an_unquoted_name_reaches_the_column_in_another_case() -> Result<(), Error> {
    let sqlite = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE p(id INTEGER PRIMARY KEY); CREATE TABLE c(pid REFERENCES p(ID));",
    )?;
    assert_eq!(key_columns(&sqlite), (vec!["pid"], vec!["id"]));

    let postgres = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE p(id INT PRIMARY KEY);
         CREATE TABLE c(pid INT, FOREIGN KEY (PID) REFERENCES p(ID));",
    )?;
    assert_eq!(key_columns(&postgres), (vec!["pid"], vec!["id"]));

    // A quoted spelling keeps its case under PostgreSQL's rule.
    assert!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE p(id INT PRIMARY KEY); CREATE TABLE c(pid INT REFERENCES p(\"ID\"));",
        )
        .is_err()
    );
    Ok(())
}

/// A folding parse takes the key SQLite takes and reads its columns back.
#[test]
fn a_folding_parse_reaches_quoted_columns_in_another_case() -> Result<(), Error> {
    let sql = "CREATE TABLE p(\"ID\" INTEGER PRIMARY KEY);
               CREATE TABLE c(\"Pid\" INTEGER, FOREIGN KEY (pid) REFERENCES p(id));";
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(sql)?;
    assert_eq!(key_columns(&database), (vec!["Pid"], vec!["ID"]));
    Ok(())
}
