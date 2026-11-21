//! Tests that a foreign key never reads a deferrable key.
//!
//! A deferrable key may hold duplicates until the transaction commits, so
//! PostgreSQL passes over it when it looks for the key a foreign key checks
//! its rows against, and refuses the foreign key when no other key fits.
//! Every expectation was measured against PostgreSQL 18.7.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn refused_as_primary_key(sql: &str) {
    let outcome = parse(sql);
    assert!(
        matches!(
            &outcome,
            Err(Error::DeferrablePrimaryKeyForForeignKey { referenced_table, host_table })
                if referenced_table == "r" && host_table == "c"
        ),
        "expected a deferrable primary key refusal, got {outcome:?}"
    );
}

fn refused_as_unique_key(sql: &str, host: &str) {
    let outcome = parse(sql);
    assert!(
        matches!(
            &outcome,
            Err(Error::DeferrableUniqueKeyForForeignKey { referenced_table, host_table })
                if referenced_table == "r" && host_table == host
        ),
        "expected a deferrable unique key refusal, got {outcome:?}"
    );
}

#[test]
fn a_key_naming_no_columns_refuses_a_deferrable_primary_key() {
    refused_as_primary_key(
        "CREATE TABLE r (id INT PRIMARY KEY DEFERRABLE);
         CREATE TABLE c (x INT REFERENCES r);",
    );
    refused_as_primary_key(
        "CREATE TABLE r (id INT, CONSTRAINT pk PRIMARY KEY (id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TABLE c (x INT, FOREIGN KEY (x) REFERENCES r);",
    );
}

#[test]
fn a_unique_key_over_the_same_column_does_not_stand_in_for_the_primary_key() {
    refused_as_primary_key(
        "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY DEFERRABLE, CONSTRAINT u UNIQUE (id));
         CREATE TABLE c (x INT REFERENCES r);",
    );
}

#[test]
fn the_deferrable_primary_key_is_refused_before_the_column_count() {
    refused_as_primary_key(
        "CREATE TABLE r (id INT PRIMARY KEY DEFERRABLE);
         CREATE TABLE c (x INT, y INT, FOREIGN KEY (x, y) REFERENCES r);",
    );
}

#[test]
fn a_missing_host_column_is_refused_first() {
    assert!(matches!(
        parse(
            "CREATE TABLE r (id INT PRIMARY KEY DEFERRABLE);
             CREATE TABLE c (x INT, FOREIGN KEY (nope) REFERENCES r);",
        ),
        Err(Error::HostColumnNotFoundForForeignKey { .. })
    ));
}

#[test]
fn named_columns_refuse_a_deferrable_key() {
    refused_as_unique_key(
        "CREATE TABLE r (id INT PRIMARY KEY DEFERRABLE);
         CREATE TABLE c (x INT REFERENCES r (id));",
        "c",
    );
    refused_as_unique_key(
        "CREATE TABLE r (id INT UNIQUE DEFERRABLE INITIALLY DEFERRED);
         CREATE TABLE c (x INT REFERENCES r (id));",
        "c",
    );
    refused_as_unique_key(
        "CREATE TABLE r (id INT CONSTRAINT u UNIQUE DEFERRABLE INITIALLY IMMEDIATE);
         CREATE TABLE c (x INT REFERENCES r (id));",
        "c",
    );
    refused_as_unique_key(
        "CREATE TABLE r (id INT, CONSTRAINT u UNIQUE (id) DEFERRABLE);
         CREATE TABLE c (x INT, y INT, FOREIGN KEY (x, y) REFERENCES r (id));",
        "c",
    );
}

#[test]
fn a_key_added_later_is_refused_too() {
    refused_as_unique_key(
        "CREATE TABLE r (id INT);
         ALTER TABLE r ADD CONSTRAINT u UNIQUE (id) DEFERRABLE;
         ALTER TABLE r ADD FOREIGN KEY (id) REFERENCES r (id);",
        "r",
    );
}

#[test]
fn columns_no_key_covers_stay_a_missing_key() {
    assert!(matches!(
        parse(
            "CREATE TABLE r (id INT, v INT, CONSTRAINT u UNIQUE (id) DEFERRABLE);
             CREATE TABLE c (x INT REFERENCES r (v));",
        ),
        Err(Error::ReferencedColumnsNotUniqueForForeignKey { .. })
    ));
}

#[test]
fn a_key_that_is_not_deferrable_serves() {
    for sql in [
        "CREATE TABLE r (id INT UNIQUE NOT DEFERRABLE);
         CREATE TABLE c (x INT REFERENCES r (id));",
        "CREATE TABLE r (id INT CONSTRAINT u UNIQUE NOT DEFERRABLE INITIALLY IMMEDIATE);
         CREATE TABLE c (x INT REFERENCES r (id));",
        "CREATE TABLE r (id INT UNIQUE DEFERRABLE, CONSTRAINT u2 UNIQUE (id));
         CREATE TABLE c (x INT REFERENCES r (id));",
        "CREATE TABLE r (id INT CONSTRAINT u UNIQUE DEFERRABLE);
         CREATE UNIQUE INDEX ix ON r (id);
         CREATE TABLE c (x INT REFERENCES r (id));",
    ] {
        parse(sql).expect("a key that is not deferrable covers the columns");
    }
}
