//! Tests that a unique key keeps the SQLite conflict clause its constraint
//! wrote, whether the constraint is written on the column or on the table.
//!
//! SQLite's grammar binds a conflict clause to the constraint right before
//! it, so on a column it belongs to that constraint and to no other.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{dialect::SQLiteDialect, keywords::Keyword};

/// The conflict clause of each unique key, in declaration order.
fn unique_key_conflict_clauses(sql: &str) -> Vec<Option<Keyword>> {
    let database = ParserDB::parse::<SQLiteDialect>(sql).expect("schema parses");
    database
        .tables()
        .flat_map(|table| table.unique_indices(&database).expect("the table is in this database"))
        .map(|index| index.attribute().on_conflict)
        .collect()
}

#[test]
fn a_table_primary_key_lends_its_conflict_clause_to_its_unique_key() {
    assert_eq!(
        unique_key_conflict_clauses(
            "CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b) ON CONFLICT REPLACE);"
        ),
        [Some(Keyword::REPLACE)]
    );
}

#[test]
fn a_column_primary_key_keeps_its_conflict_clause_like_the_table_spelling() {
    let column =
        unique_key_conflict_clauses("CREATE TABLE t (a INTEGER PRIMARY KEY ON CONFLICT REPLACE);");
    let table = unique_key_conflict_clauses(
        "CREATE TABLE t (a INTEGER, PRIMARY KEY (a) ON CONFLICT REPLACE);",
    );
    assert_eq!(column, [Some(Keyword::REPLACE)]);
    assert_eq!(column, table);
}

#[test]
fn a_column_unique_keeps_its_conflict_clause_like_the_table_spelling() {
    let column =
        unique_key_conflict_clauses("CREATE TABLE t (a INTEGER UNIQUE ON CONFLICT IGNORE);");
    let table =
        unique_key_conflict_clauses("CREATE TABLE t (a INTEGER, UNIQUE (a) ON CONFLICT IGNORE);");
    assert_eq!(column, [Some(Keyword::IGNORE)]);
    assert_eq!(column, table);
}

#[test]
fn a_named_column_unique_keeps_its_conflict_clause() {
    assert_eq!(
        unique_key_conflict_clauses(
            "CREATE TABLE t (a INTEGER CONSTRAINT u UNIQUE ON CONFLICT FAIL);"
        ),
        [Some(Keyword::FAIL)]
    );
}

#[test]
fn a_conflict_clause_of_another_column_constraint_stays_off_the_key() {
    assert_eq!(
        unique_key_conflict_clauses(
            "CREATE TABLE t (a INTEGER NOT NULL ON CONFLICT IGNORE PRIMARY KEY UNIQUE);"
        ),
        [None, None]
    );
}

#[test]
fn each_column_key_keeps_only_its_own_conflict_clause() {
    assert_eq!(
        unique_key_conflict_clauses(
            "CREATE TABLE t (a INTEGER PRIMARY KEY ON CONFLICT ROLLBACK UNIQUE ON CONFLICT ABORT);"
        ),
        [Some(Keyword::ROLLBACK), Some(Keyword::ABORT)]
    );
}
