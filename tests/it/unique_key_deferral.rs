//! Tests that a unique or primary key reads back when the database checks it,
//! from every spelling that declares one.
//!
//! Every expectation here was measured against PostgreSQL 18.7 by reading
//! `pg_constraint.condeferrable`, `pg_constraint.condeferred` and
//! `pg_index.indimmediate`.
#![expect(clippy::expect_used, reason = "Test schemas and catalog lookups must succeed")]

use ConstraintDeferral::{InitiallyDeferred, InitiallyImmediate, NotDeferrable};
use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn table<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
}

/// The deferral of each unique and primary key of the table, keyed by
/// constraint name.
fn key_deferrals(database: &ParserDB, table_name: &str) -> Vec<(String, ConstraintDeferral)> {
    let mut deferrals: Vec<_> = table(database, table_name)
        .unique_indices(database)
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

/// The inline, table-constraint, `ALTER TABLE ... ADD CONSTRAINT` and
/// `USING INDEX` spellings all carry what they declare, `INITIALLY DEFERRED`
/// implies `DEFERRABLE`, and nothing written means `NOT DEFERRABLE`.
#[test]
fn every_key_spelling_reads_back_its_deferral() {
    let database = parse(
        "CREATE TABLE a (
            plain int UNIQUE,
            inl_def int UNIQUE DEFERRABLE,
            inl_initdef int UNIQUE INITIALLY DEFERRED,
            pk int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED,
            t1 int, t2 int,
            CONSTRAINT tdef UNIQUE (t1) DEFERRABLE INITIALLY DEFERRED,
            CONSTRAINT timm UNIQUE (t2) DEFERRABLE INITIALLY IMMEDIATE
        );
        ALTER TABLE a ADD CONSTRAINT added UNIQUE (t1, t2) DEFERRABLE;
        CREATE UNIQUE INDEX ui2 ON a (t1, t2, plain);
        ALTER TABLE a ADD CONSTRAINT adopted UNIQUE USING INDEX ui2 DEFERRABLE INITIALLY DEFERRED;",
    );

    let expected = [
        ("a_inl_def_key", InitiallyImmediate),
        ("a_inl_initdef_key", InitiallyDeferred),
        ("a_pkey", InitiallyDeferred),
        ("a_plain_key", NotDeferrable),
        ("added", InitiallyImmediate),
        ("adopted", InitiallyDeferred),
        ("tdef", InitiallyDeferred),
        ("timm", InitiallyImmediate),
    ]
    .map(|(name, deferral)| (name.to_owned(), deferral));
    assert_eq!(key_deferrals(&database, "a"), expected);
}

/// A primary key reads back its deferral in both spellings, and one declaring
/// no clause is checked at the end of every statement.
#[test]
fn primary_keys_read_back_their_deferral() {
    for (declaration, deferral) in [
        ("id int PRIMARY KEY", NotDeferrable),
        ("id int, PRIMARY KEY (id)", NotDeferrable),
        ("id int PRIMARY KEY DEFERRABLE", InitiallyImmediate),
        ("id int, PRIMARY KEY (id) DEFERRABLE INITIALLY IMMEDIATE", InitiallyImmediate),
        ("id int, PRIMARY KEY (id) INITIALLY DEFERRED", InitiallyDeferred),
    ] {
        let database = parse(&format!("CREATE TABLE t ({declaration});"));
        assert_eq!(
            key_deferrals(&database, "t"),
            [("t_pkey".to_owned(), deferral)],
            "{declaration}"
        );
    }
}

/// `CREATE UNIQUE INDEX` builds an index backing no constraint, which
/// PostgreSQL checks at the end of every statement.
#[test]
fn a_created_unique_index_is_not_deferrable() {
    let database = parse("CREATE TABLE t (id int); CREATE UNIQUE INDEX ui ON t (id);");
    let deferrals: Vec<_> = table(&database, "t")
        .indices(&database)
        .expect("table is in this database")
        .map(|index| index.deferral(&database))
        .collect();
    assert_eq!(deferrals, [NotDeferrable]);
}

/// A partition, created as one or attached, receives its parent's keys checked
/// as late as the parent's.
#[test]
fn a_partition_inherits_the_deferral_of_its_parent_keys() {
    let database = parse(
        "CREATE TABLE parted (
            id int PRIMARY KEY DEFERRABLE INITIALLY DEFERRED,
            u int,
            UNIQUE (id, u) DEFERRABLE
        ) PARTITION BY RANGE (id);
        CREATE TABLE part1 PARTITION OF parted FOR VALUES FROM (0) TO (10);
        CREATE TABLE part2 (id int NOT NULL, u int);
        ALTER TABLE parted ATTACH PARTITION part2 FOR VALUES FROM (10) TO (20);",
    );

    for partition in ["part1", "part2"] {
        assert_eq!(
            key_deferrals(&database, partition),
            [
                (format!("{partition}_id_u_key"), InitiallyImmediate),
                (format!("{partition}_pkey"), InitiallyDeferred),
            ],
            "{partition}"
        );
    }
}
