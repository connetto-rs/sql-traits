//! Tests that `ALTER TABLE root DETACH PARTITION p` turns a partition back
//! into a table of its own, as PostgreSQL does, and refuses what PostgreSQL
//! refuses.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect};

const SCHEMA: &str = "CREATE TABLE target (id INT PRIMARY KEY);
     CREATE TABLE orders (
         id INT NOT NULL,
         note TEXT DEFAULT 'x',
         rid INT REFERENCES target (id),
         v INT GENERATED ALWAYS AS IDENTITY,
         CONSTRAINT pos CHECK (id >= 0),
         PRIMARY KEY (id)
     ) PARTITION BY RANGE (id);
     CREATE TABLE p0 PARTITION OF orders (CONSTRAINT own CHECK (id < 100))
         FOR VALUES FROM (0) TO (10);
     CREATE TABLE sub PARTITION OF orders FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (id);
     CREATE TABLE sub1 PARTITION OF sub FOR VALUES FROM (10) TO (15);
     CREATE TABLE other (id INT) PARTITION BY RANGE (id);
     CREATE TABLE elsewhere PARTITION OF other FOR VALUES FROM (0) TO (10);
     CREATE TABLE plain (id INT);
     CREATE VIEW v AS SELECT 1 AS id;";

fn parse(statements: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} {statements}"))
}

fn db(statements: &str) -> ParserDB {
    parse(statements).unwrap_or_else(|error| panic!("{statements} was refused: {error:?}"))
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("table {name} is recorded"))
}

fn local_columns(db: &ParserDB, name: &str) -> Vec<String> {
    table(db, name)
        .local_columns(db)
        .expect("held")
        .map(|column| column.column_name().to_owned())
        .collect()
}

fn identity_columns(db: &ParserDB, name: &str) -> Vec<String> {
    table(db, name)
        .columns(db)
        .expect("held")
        .filter(|column| column.identity_generation(db).is_some())
        .map(|column| column.column_name().to_owned())
        .collect()
}

#[test]
fn a_detached_partition_is_a_table_of_its_own() {
    for mode in ["", " CONCURRENTLY"] {
        let db = db(&format!("ALTER TABLE ONLY orders DETACH PARTITION p0{mode};"));
        let orders = table(&db, "orders");
        let p0 = table(&db, "p0");
        assert_eq!(p0.partition_root(&db).expect("held"), None, "{mode}");
        assert_eq!(p0.partition_bound(), None);
        assert!(!orders.partitions(&db).expect("held").any(|partition| partition == p0));
        // The columns, the key, the foreign key and the checks stay, as the
        // table's own.
        assert_eq!(local_columns(&db, "p0"), ["id", "note", "rid", "v"]);
        assert_eq!(p0.primary_key_columns(&db).expect("held").count(), 1);
        assert_eq!(p0.foreign_keys(&db).expect("held").count(), 1);
        assert_eq!(p0.check_constraints(&db).expect("held").count(), 2);
        let note =
            p0.column("note", &db, IdentifierCase::AsWritten).expect("lookup").expect("kept");
        assert_eq!(note.default_expression().map(ToString::to_string).as_deref(), Some("'x'"));
        // The identity the root gave stays with the root.
        assert_eq!(identity_columns(&db, "p0"), Vec::<String>::new());
        let v = p0.column("v", &db, IdentifierCase::AsWritten).expect("lookup").expect("kept");
        assert!(!v.is_nullable(&db).expect("held"));
        assert_eq!(identity_columns(&db, "orders"), ["v"]);
    }
}

#[test]
fn what_a_partition_received_becomes_its_own_to_change() {
    let detached = "ALTER TABLE orders DETACH PARTITION p0;";
    for change in [
        "ALTER TABLE p0 DROP COLUMN note;",
        "ALTER TABLE p0 RENAME COLUMN note TO body;",
        "ALTER TABLE p0 DROP CONSTRAINT pos;",
        "ALTER TABLE p0 ADD COLUMN extra INT;",
        "ALTER TABLE p0 ALTER COLUMN v ADD GENERATED ALWAYS AS IDENTITY;",
    ] {
        assert!(parse(change).is_err(), "{change} was accepted on the partition");
        db(&format!("{detached} {change}"));
    }
    // The bound it took is free again.
    db(&format!("{detached} CREATE TABLE p2 PARTITION OF orders FOR VALUES FROM (0) TO (10);"));
    // And the table can be attached back.
    db(&format!("{detached} ALTER TABLE orders ATTACH PARTITION p0 FOR VALUES FROM (0) TO (10);"));
}

#[test]
fn a_detached_partitioned_partition_keeps_its_own_partitions() {
    let db = db("ALTER TABLE orders DETACH PARTITION sub;");
    let sub = table(&db, "sub");
    let sub1 = table(&db, "sub1");
    assert_eq!(sub.partition_root(&db).expect("held"), None);
    assert_eq!(sub1.partition_root(&db).expect("held"), Some(sub));
    assert_eq!(local_columns(&db, "sub"), ["id", "note", "rid", "v"]);
    assert_eq!(local_columns(&db, "sub1"), Vec::<String>::new());
    // The identity leaves the whole detached tree.
    assert_eq!(identity_columns(&db, "sub"), Vec::<String>::new());
    assert_eq!(identity_columns(&db, "sub1"), Vec::<String>::new());
    // A check sub1 receives from sub is still not sub1's to drop.
    assert!(
        parse("ALTER TABLE orders DETACH PARTITION sub; ALTER TABLE sub1 DROP CONSTRAINT pos;")
            .is_err()
    );
    parse("ALTER TABLE orders DETACH PARTITION sub; ALTER TABLE sub DROP CONSTRAINT pos;")
        .expect("sub owns the check now");
}

#[test]
fn detach_refuses_what_postgresql_refuses() {
    for (statement, check) in [
        ("ALTER TABLE orders DETACH PARTITION elsewhere;", "a partition of another table"),
        ("ALTER TABLE orders DETACH PARTITION plain;", "a plain table"),
        ("ALTER TABLE orders DETACH PARTITION v;", "a view"),
        ("ALTER TABLE orders DETACH PARTITION orders;", "the root itself"),
        ("ALTER TABLE orders DETACH PARTITION sub1;", "a partition of a partition"),
    ] {
        let error = parse(statement).expect_err(check);
        assert!(
            matches!(&error, Error::NotAPartitionOf { table_name, .. } if table_name == "orders"),
            "{check}: {error:?}"
        );
    }
    assert!(matches!(
        parse("ALTER TABLE orders DETACH PARTITION missing;"),
        Err(Error::RelationNotFound { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE plain DETACH PARTITION p0;"),
        Err(Error::AlterActionUnsupportedOnRelation { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE v DETACH PARTITION p0;"),
        Err(Error::AlterActionUnsupportedOnRelation { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE nowhere DETACH PARTITION p0;"),
        Err(Error::AlterTableNotFound { .. })
    ));
    db("ALTER TABLE IF EXISTS nowhere DETACH PARTITION p0;");
    // A partition already detached is no longer one.
    assert!(matches!(
        parse("ALTER TABLE orders DETACH PARTITION p0; ALTER TABLE orders DETACH PARTITION p0;"),
        Err(Error::NotAPartitionOf { .. })
    ));
}

#[test]
fn detach_concurrently_and_finalize_follow_postgresql() {
    assert!(matches!(
        parse("BEGIN; ALTER TABLE orders DETACH PARTITION p0 CONCURRENTLY; COMMIT;"),
        Err(Error::DetachConcurrentlyInTransaction { .. })
    ));
    let with_default = "CREATE TABLE rest PARTITION OF orders DEFAULT;";
    for partition in ["p0", "rest"] {
        assert!(
            matches!(
                parse(&format!(
                    "{with_default} ALTER TABLE orders DETACH PARTITION {partition} CONCURRENTLY;"
                )),
                Err(Error::DetachConcurrentlyWithDefault { .. })
            ),
            "{partition}"
        );
    }
    db(&format!("{with_default} ALTER TABLE orders DETACH PARTITION rest;"));
    // A detach in an ingested schema always completes, so none is pending.
    assert!(matches!(
        parse("ALTER TABLE orders DETACH PARTITION p0 FINALIZE;"),
        Err(Error::NoDetachPending { .. })
    ));
    assert!(matches!(
        parse("ALTER TABLE orders DETACH PARTITION plain FINALIZE;"),
        Err(Error::NotAPartitionOf { .. })
    ));
}

#[test]
fn a_partition_command_stands_alone_in_its_statement() {
    for statement in [
        "ALTER TABLE orders ADD COLUMN y INT, DETACH PARTITION p0;",
        "ALTER TABLE orders DETACH PARTITION p0, ADD COLUMN y INT;",
        "ALTER TABLE orders ADD COLUMN y INT, ATTACH PARTITION plain FOR VALUES FROM (30) TO (40);",
    ] {
        assert!(
            matches!(parse(statement), Err(Error::PartitionCommandNotAlone { .. })),
            "{statement}"
        );
    }
}

#[test]
fn a_detached_foreign_partition_stays_foreign() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT) PARTITION BY RANGE (id);
         CREATE FOREIGN TABLE f PARTITION OF r FOR VALUES FROM (0) TO (10) SERVER s;
         ALTER TABLE r DETACH PARTITION f;",
    )
    .expect("schema");
    let f = db
        .foreign_table_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("still a foreign table");
    assert_eq!(f.partition_root(&db).expect("held"), None);
}
