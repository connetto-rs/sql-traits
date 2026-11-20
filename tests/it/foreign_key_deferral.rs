//! Tests that a foreign key reads back when the database checks it, from every
//! spelling that declares one.
//!
//! Every PostgreSQL expectation here was measured against PostgreSQL 18 by
//! reading `pg_constraint.condeferrable` and `pg_constraint.condeferred`, and
//! every SQLite expectation against SQLite 3.51.1 by inserting an orphan row
//! inside a transaction.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The deferral of each foreign key of the table, keyed by name.
fn deferrals(database: &ParserDB, table_name: &str) -> Vec<(String, ConstraintDeferral)> {
    let table = database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists");
    let mut deferrals: Vec<_> = table
        .foreign_keys(database)
        .expect("table is in this database")
        .map(|key| {
            let name = key
                .constraint_name(database)
                .expect("key is in this database")
                .expect("key has a name")
                .into_owned();
            (name, key.deferral(database))
        })
        .collect();
    deferrals.sort_unstable();
    deferrals
}

const PARENT: &str = "CREATE TABLE p (id int PRIMARY KEY, a int, UNIQUE (id, a));";

/// The table-constraint, inline and `ALTER TABLE ... ADD CONSTRAINT`
/// spellings all carry what they declare, `INITIALLY DEFERRED` implies
/// `DEFERRABLE`, and nothing written means `NOT DEFERRABLE`.
#[test]
fn every_spelling_reads_back_its_deferral() {
    use ConstraintDeferral::{InitiallyDeferred, InitiallyImmediate, NotDeferrable};

    let database = parse(&format!(
        "{PARENT}
        CREATE TABLE c (
            plain int REFERENCES p (id),
            inline_def int REFERENCES p (id) DEFERRABLE,
            inline_initdef int REFERENCES p (id) INITIALLY DEFERRED,
            inline_notdef_imm int REFERENCES p (id) NOT DEFERRABLE INITIALLY IMMEDIATE,
            t1 int, t2 int,
            CONSTRAINT tdef FOREIGN KEY (t1, t2) REFERENCES p (id, a) DEFERRABLE INITIALLY DEFERRED,
            CONSTRAINT timm FOREIGN KEY (t1) REFERENCES p (id) INITIALLY IMMEDIATE DEFERRABLE
        );
        ALTER TABLE c ADD CONSTRAINT added FOREIGN KEY (t1) REFERENCES p (id) INITIALLY DEFERRED NOT VALID;"
    ))
    .expect("schema parses");

    let expected = [
        ("added", InitiallyDeferred),
        ("c_inline_def_fkey", InitiallyImmediate),
        ("c_inline_initdef_fkey", InitiallyDeferred),
        ("c_inline_notdef_imm_fkey", NotDeferrable),
        ("c_plain_fkey", NotDeferrable),
        ("tdef", InitiallyDeferred),
        ("timm", InitiallyImmediate),
    ]
    .map(|(name, deferral)| (name.to_owned(), deferral));
    assert_eq!(deferrals(&database, "c"), expected);
}

/// A partition receives its parent's foreign key checked as late as the
/// parent's.
#[test]
fn a_partition_inherits_the_deferral_of_its_parent() {
    let database = parse(&format!(
        "{PARENT}
        CREATE TABLE parted (x int REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED) PARTITION BY RANGE (x);
        CREATE TABLE part1 PARTITION OF parted FOR VALUES FROM (0) TO (10);"
    ))
    .expect("schema parses");

    assert_eq!(
        deferrals(&database, "part1"),
        [("parted_x_fkey".to_owned(), ConstraintDeferral::InitiallyDeferred)]
    );
}

/// PostgreSQL refuses a foreign key declared `NOT DEFERRABLE INITIALLY
/// DEFERRED` with "constraint declared INITIALLY DEFERRED must be
/// DEFERRABLE", whichever spelling declares it.
#[test]
fn initially_deferred_without_deferrable_is_refused() {
    for declaration in [
        "CREATE TABLE bad (x int REFERENCES p (id) NOT DEFERRABLE INITIALLY DEFERRED);",
        "CREATE TABLE bad (x int, FOREIGN KEY (x) REFERENCES p (id) NOT DEFERRABLE INITIALLY DEFERRED);",
        "CREATE TABLE bad (x int);
        ALTER TABLE bad ADD FOREIGN KEY (x) REFERENCES p (id) NOT DEFERRABLE INITIALLY DEFERRED;",
    ] {
        let result = parse(&format!("{PARENT} {declaration}"));
        assert!(
            matches!(
                result,
                Err(Error::InitiallyDeferredNotDeferrable { ref table_name }) if table_name == "bad"
            ),
            "{declaration}: {result:?}"
        );
    }
}

/// SQLite stores a foreign key declared `NOT DEFERRABLE INITIALLY DEFERRED`
/// and checks it immediately, whichever spelling declares it.
#[test]
fn sqlite_checks_not_deferrable_initially_deferred_immediately() {
    for declaration in [
        "CREATE TABLE c (pid REFERENCES p (id) NOT DEFERRABLE INITIALLY DEFERRED);",
        "CREATE TABLE c (pid INT, FOREIGN KEY (pid) REFERENCES p (id) NOT DEFERRABLE INITIALLY DEFERRED);",
    ] {
        let result = ParserDB::parse::<SQLiteDialect>(&format!(
            "CREATE TABLE p (id INTEGER PRIMARY KEY); {declaration}"
        ));
        assert!(result.is_ok(), "{declaration}: {result:?}");
        let database = result.expect("schema parses");
        let table = database
            .table_by_target(TargetName::new("c", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists");
        let deferrals: Vec<_> = table
            .foreign_keys(&database)
            .expect("table is in this database")
            .map(|key| key.deferral(&database))
            .collect();
        assert_eq!(deferrals, [ConstraintDeferral::NotDeferrable], "{declaration}");
    }
}
