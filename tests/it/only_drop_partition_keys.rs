//! Tests that `ALTER TABLE ONLY` drops a partitioned table's keys from its
//! partitions too.
//!
//! A partition cannot keep a copy of its parent's primary key, unique key,
//! exclusion constraint or foreign key without the parent's, so PostgreSQL
//! drops those copies at every level even under `ONLY`. A `CHECK` or
//! `NOT NULL` copy stays and becomes the partition's own. Every expectation
//! was measured against PostgreSQL 18.7.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

const PARTITIONED_KEY: &str =
    "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY) PARTITION BY RANGE (id);
     CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);";

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("PostgreSQL accepts the script")
}

fn table<'db>(database: &'db ParserDB, table_name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
}

fn foreign_keys(database: &ParserDB, table_name: &str) -> usize {
    table(database, table_name).foreign_keys(database).expect("table is in this database").count()
}

#[test]
fn a_primary_key_leaves_every_partition() {
    let database = parse(&format!(
        "{PARTITIONED_KEY}
         ALTER TABLE ONLY r DROP CONSTRAINT pk;
         CREATE INDEX r1_pkey ON r1 (id);"
    ));
    assert!(!table(&database, "r1").has_primary_key(&database).expect("table is in this database"));
}

#[test]
fn if_exists_drops_the_copies_too() {
    let database =
        parse(&format!("{PARTITIONED_KEY} ALTER TABLE ONLY r DROP CONSTRAINT IF EXISTS pk;"));
    assert!(!table(&database, "r1").has_primary_key(&database).expect("table is in this database"));
}

#[test]
fn a_unique_key_leaves_every_level() {
    let database = parse(
        "CREATE TABLE r (id INT CONSTRAINT u UNIQUE) PARTITION BY RANGE (id);
         CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
         CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);
         ALTER TABLE ONLY r DROP CONSTRAINT u;
         CREATE INDEX r1_id_key ON r1 (id);
         CREATE INDEX r11_id_key ON r11 (id);",
    );
    for partition in ["r1", "r11"] {
        assert_eq!(
            table(&database, partition)
                .unique_indices(&database)
                .expect("table is in this database")
                .count(),
            0,
            "{partition}"
        );
    }
}

#[test]
fn a_table_level_key_leaves_the_partition() {
    let database = parse(
        "CREATE TABLE r (id INT, CONSTRAINT u UNIQUE (id)) PARTITION BY RANGE (id);
         CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
         ALTER TABLE ONLY r DROP CONSTRAINT u;
         CREATE INDEX r1_id_key ON r1 (id);",
    );
    assert_eq!(
        table(&database, "r1")
            .unique_indices(&database)
            .expect("table is in this database")
            .count(),
        0
    );
}

#[test]
fn a_foreign_key_leaves_every_level() {
    let database = parse(
        "CREATE TABLE p (id INT PRIMARY KEY);
         CREATE TABLE r (id INT CONSTRAINT f REFERENCES p) PARTITION BY RANGE (id);
         CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
         CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);
         ALTER TABLE ONLY r DROP CONSTRAINT f;",
    );
    assert_eq!(foreign_keys(&database, "r1"), 0);
    assert_eq!(foreign_keys(&database, "r11"), 0);
}

#[test]
fn a_table_level_foreign_key_leaves_the_partition() {
    let database = parse(
        "CREATE TABLE p (id INT PRIMARY KEY);
         CREATE TABLE r (id INT, CONSTRAINT f FOREIGN KEY (id) REFERENCES p) PARTITION BY RANGE (id);
         CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
         ALTER TABLE ONLY r DROP CONSTRAINT f;",
    );
    assert_eq!(foreign_keys(&database, "r1"), 0);
}

#[test]
fn a_check_stays_as_the_partitions_own() {
    let database = parse(
        "CREATE TABLE r (id INT, CONSTRAINT c CHECK (id > 0)) PARTITION BY RANGE (id);
         CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
         ALTER TABLE ONLY r DROP CONSTRAINT c;
         ALTER TABLE r1 DROP CONSTRAINT c;",
    );
    assert_eq!(
        table(&database, "r1")
            .check_constraints(&database)
            .expect("table is in this database")
            .count(),
        0
    );
}

#[test]
fn a_key_adopted_at_attach_leaves_with_the_parents_key() {
    let database = parse(
        "CREATE TABLE r (id INT, CONSTRAINT u UNIQUE (id)) PARTITION BY RANGE (id);
         CREATE TABLE r1 (id INT);
         ALTER TABLE r1 ADD CONSTRAINT own UNIQUE (id);
         ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
         ALTER TABLE ONLY r DROP CONSTRAINT u;
         CREATE INDEX own ON r1 (id);",
    );
    assert_eq!(
        table(&database, "r1")
            .unique_indices(&database)
            .expect("table is in this database")
            .count(),
        0
    );
}
