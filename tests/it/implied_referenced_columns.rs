//! Tests that a foreign key written without a referenced column list
//! references the primary key its target holds when the key is declared, the
//! columns PostgreSQL stores in `pg_constraint.confkey`.
//!
//! The expectations below were read from PostgreSQL 18.6.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn names<'db>(
    columns: impl Iterator<Item = &'db <ParserDB as DatabaseLike>::Column>,
) -> Vec<String> {
    columns.map(|column| column.column_name().to_owned()).collect()
}

/// The host and referenced column names of every foreign key `table` holds.
fn key_columns(database: &ParserDB, table: &str) -> Vec<(Vec<String>, Vec<String>)> {
    let table = database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    table
        .foreign_keys(database)
        .expect("the table is in this database")
        .map(|key| {
            (
                names(key.host_columns(database).expect("the host columns resolve")),
                names(key.referenced_columns(database).expect("the target resolves")),
            )
        })
        .collect()
}

/// The primary key is read in key order, which need not be the order its
/// columns are declared in, and every spelling of the key reaches it.
#[test]
fn a_key_naming_no_columns_references_the_primary_key() {
    let host = "CREATE TABLE host (a INT, b INT, PRIMARY KEY (b, a));";
    let pair =
        || vec![(vec!["x".to_owned(), "y".to_owned()], vec!["b".to_owned(), "a".to_owned()])];
    for spelling in [
        "CREATE TABLE child (x INT, y INT, FOREIGN KEY (x, y) REFERENCES host);",
        "CREATE TABLE child (x INT, y INT);
         ALTER TABLE child ADD FOREIGN KEY (x, y) REFERENCES host;",
    ] {
        let database = parse(&format!("{host} {spelling}")).expect("host has a primary key");
        assert_eq!(key_columns(&database, "child"), pair(), "{spelling}");
    }

    let host = "CREATE TABLE host (id INT PRIMARY KEY);";
    let single = || vec![(vec!["h".to_owned()], vec!["id".to_owned()])];
    for spelling in [
        "CREATE TABLE child (h INT REFERENCES host);",
        "CREATE TABLE child (n INT); ALTER TABLE child ADD COLUMN h INT REFERENCES host;",
    ] {
        let database = parse(&format!("{host} {spelling}")).expect("host has a primary key");
        assert_eq!(key_columns(&database, "child"), single(), "{spelling}");
    }
}

#[test]
fn a_self_reference_naming_no_columns_references_its_own_key() {
    let database = parse("CREATE TABLE s (id INT PRIMARY KEY, p INT REFERENCES s);")
        .expect("s has a primary key");
    assert_eq!(key_columns(&database, "s"), [(vec!["p".to_owned()], vec!["id".to_owned()])]);
}

/// The key holds the columns themselves, so a rename on the target reaches
/// it the way it reaches a key that lists them.
#[test]
fn the_referenced_columns_follow_a_rename() {
    let database = parse(
        "CREATE TABLE host (id INT PRIMARY KEY);
         CREATE TABLE child (h INT REFERENCES host);
         ALTER TABLE host RENAME COLUMN id TO key;",
    )
    .expect("id is declared");
    assert_eq!(key_columns(&database, "child"), [(vec!["h".to_owned()], vec!["key".to_owned()])]);
}

/// The key depends on the columns it references, so dropping one asks for
/// `CASCADE`, which takes the key along.
#[test]
fn dropping_a_referenced_column_needs_cascade() {
    let schema = "CREATE TABLE host (id INT PRIMARY KEY);
                  CREATE TABLE child (h INT REFERENCES host);";
    let error = parse(&format!("{schema} ALTER TABLE host DROP COLUMN id;"))
        .expect_err("child's key depends on host.id");
    assert!(
        matches!(&error, Error::ColumnReferenced { table_name, column_name }
            if table_name == "host" && column_name == "id"),
        "got {error:?}"
    );

    let database = parse(&format!("{schema} ALTER TABLE host DROP COLUMN id CASCADE;"))
        .expect("CASCADE takes child's key");
    assert_eq!(key_columns(&database, "child"), Vec::<(Vec<String>, Vec<String>)>::new());
}
