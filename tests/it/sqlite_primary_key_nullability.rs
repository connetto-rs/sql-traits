//! Tests that a SQLite primary key keeps `NULL` out of its columns only where
//! SQLite does, and that the stored column records `NOT NULL` only where
//! SQLite's catalog does.
//!
//! Every expectation was measured against SQLite 3.51.1 by inserting `NULL`
//! and reading `notnull` from `pragma_table_xinfo`. A key column of an
//! ordinary rowid table accepts `NULL` unless it is the rowid alias. The rowid
//! alias never holds `NULL`, because SQLite turns one into a fresh rowid, yet
//! its catalog records no `NOT NULL` for it.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{ast::ColumnOption, dialect::SQLiteDialect};

/// A statement, the column it declares, whether SQLite lets that column hold
/// `NULL`, and whether SQLite's catalog records `NOT NULL` for it.
struct Measured {
    statement: &'static str,
    table: &'static str,
    column: &'static str,
    nullable: bool,
    stated_not_null: bool,
}

const MEASURED: [Measured; 17] = [
    Measured {
        statement: "CREATE TABLE a (k TEXT PRIMARY KEY)",
        table: "a",
        column: "k",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE c (a TEXT, b INT, PRIMARY KEY (a, b))",
        table: "c",
        column: "a",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE c (a TEXT, b INT, PRIMARY KEY (a, b))",
        table: "c",
        column: "b",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE m (k INTEGER, j INT, UNIQUE (j), PRIMARY KEY (k, j))",
        table: "m",
        column: "k",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE x (k INT PRIMARY KEY)",
        table: "x",
        column: "k",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE d (k INTEGER PRIMARY KEY DESC)",
        table: "d",
        column: "k",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE p (k INTEGER(5) PRIMARY KEY)",
        table: "p",
        column: "k",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE u (k UNSIGNED INTEGER PRIMARY KEY)",
        table: "u",
        column: "k",
        nullable: true,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE y (k INTEGER PRIMARY KEY)",
        table: "y",
        column: "k",
        nullable: false,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE l (k integer PRIMARY KEY ASC)",
        table: "l",
        column: "k",
        nullable: false,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE t (k INTEGER, PRIMARY KEY (k DESC))",
        table: "t",
        column: "k",
        nullable: false,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE q (k \"INTEGER\" PRIMARY KEY)",
        table: "q",
        column: "k",
        nullable: false,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE w (k TEXT PRIMARY KEY) WITHOUT ROWID",
        table: "w",
        column: "k",
        nullable: false,
        stated_not_null: true,
    },
    Measured {
        statement: "CREATE TABLE v (k INTEGER PRIMARY KEY) WITHOUT ROWID",
        table: "v",
        column: "k",
        nullable: false,
        stated_not_null: true,
    },
    Measured {
        statement: "CREATE TABLE s (k TEXT PRIMARY KEY) STRICT",
        table: "s",
        column: "k",
        nullable: false,
        stated_not_null: true,
    },
    Measured {
        statement: "CREATE TABLE r (k INTEGER PRIMARY KEY) STRICT",
        table: "r",
        column: "k",
        nullable: false,
        stated_not_null: false,
    },
    Measured {
        statement: "CREATE TABLE n (k TEXT PRIMARY KEY NOT NULL)",
        table: "n",
        column: "k",
        nullable: false,
        stated_not_null: true,
    },
];

/// Answers whether `column_name` of `table_name` is nullable, and whether its
/// stored definition states `NOT NULL`.
fn read_column(database: &ParserDB, table_name: &str, column_name: &str) -> (bool, bool) {
    let column = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists");
    let stated_not_null =
        column.attribute().options.iter().any(|option| option.option == ColumnOption::NotNull);
    (column.is_nullable(database).expect("table is in this database"), stated_not_null)
}

#[test]
fn a_key_column_answers_what_sqlite_enforces_and_records() {
    for measured in &MEASURED {
        let database =
            ParserDB::parse::<SQLiteDialect>(measured.statement).expect("statement parses");
        assert_eq!(
            read_column(&database, measured.table, measured.column),
            (measured.nullable, measured.stated_not_null),
            "{} column {}",
            measured.statement,
            measured.column,
        );
    }
}

#[test]
fn a_rebuilt_table_keeps_its_key_nullable() {
    let database = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE a (k TEXT PRIMARY KEY, v INT);
         ALTER TABLE a RENAME COLUMN k TO j;
         ALTER TABLE a ADD COLUMN w INT;",
    )
    .expect("schema parses");

    assert_eq!(read_column(&database, "a", "j"), (true, false));
}
