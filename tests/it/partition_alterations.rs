//! Tests that a partition keeps the shape of the table it partitions, and that
//! `ONLY` on a partitioned table withholds nothing its partitions must share.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18. Where the two differ, the model follows 18.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, InheritedChange},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

const SCHEMA: &str = "CREATE TABLE orders (id INT NOT NULL, note TEXT) PARTITION BY RANGE (id);
     CREATE TABLE p0 PARTITION OF orders FOR VALUES FROM (0) TO (10);
     CREATE TABLE sub PARTITION OF orders FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (id);
     CREATE TABLE subp PARTITION OF sub FOR VALUES FROM (10) TO (15);
     CREATE TABLE attached (id INT NOT NULL, note TEXT);
     ALTER TABLE orders ATTACH PARTITION attached FOR VALUES FROM (20) TO (30);
     CREATE TABLE empty (id INT NOT NULL, note TEXT) PARTITION BY RANGE (id);
     CREATE TABLE parent (id INT, note TEXT);
     CREATE TABLE child () INHERITS (parent);";

fn parse(statement: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} {statement}"))
}

fn refusal(statement: &str) -> Error {
    match parse(statement) {
        Ok(_) => panic!("{statement} was accepted"),
        Err(error) => error,
    }
}

#[test]
fn a_partition_takes_no_column_of_its_own() {
    // Measured: "cannot add column to a partition", for a leaf, a partition
    // that is itself partitioned, one of its partitions and an attached
    // table, and before `IF NOT EXISTS` is looked at.
    for (statement, table) in [
        ("ALTER TABLE p0 ADD COLUMN extra INT;", "p0"),
        ("ALTER TABLE p0 ADD COLUMN IF NOT EXISTS note TEXT;", "p0"),
        ("ALTER TABLE sub ADD COLUMN extra INT;", "sub"),
        ("ALTER TABLE subp ADD COLUMN extra INT;", "subp"),
        ("ALTER TABLE attached ADD COLUMN extra INT;", "attached"),
    ] {
        let error = refusal(statement);
        assert!(
            matches!(&error, Error::ColumnAddedToPartition { table_name, .. } if table_name == table),
            "{statement}: {error:?}"
        );
    }
    parse("ALTER TABLE child ADD COLUMN extra INT;").expect("an inheriting child adds columns");
}

#[test]
fn a_partition_column_takes_no_identity_of_its_own() {
    // Measured: "cannot add identity to a column of a partition". An identity
    // added on the root reaches its partitions.
    for (statement, table) in [
        ("ALTER TABLE p0 ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;", "p0"),
        ("ALTER TABLE sub ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;", "sub"),
    ] {
        let error = refusal(statement);
        assert!(
            matches!(
                &error,
                Error::IdentityAddedToPartitionColumn { table_name, column_name }
                    if table_name == table && column_name == "id"
            ),
            "{statement}: {error:?}"
        );
    }
    parse("ALTER TABLE orders ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;")
        .expect("the root takes an identity");
    // Measured: "cannot add identity to a column of only the partitioned
    // table", with or without partitions, while an inheritance parent takes
    // one alone.
    for (statement, table) in [
        ("ALTER TABLE ONLY orders ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;", "orders"),
        ("ALTER TABLE ONLY empty ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;", "empty"),
    ] {
        let error = refusal(statement);
        assert!(
            matches!(
                &error,
                Error::IdentityAddedToOnlyPartitionedTable { table_name, column_name }
                    if table_name == table && column_name == "id"
            ),
            "{statement}: {error:?}"
        );
    }
    parse(
        "ALTER TABLE parent ALTER COLUMN id SET NOT NULL;
         ALTER TABLE ONLY parent ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
    )
    .expect("an inheritance parent takes an identity alone");
}

#[test]
fn only_withholds_no_column_or_requirement_from_partitions() {
    // Measured: "cannot drop column from only the partitioned table when
    // partitions exist" and, on PostgreSQL 18 whatever the partitions
    // declare, "constraint must be added to child tables too".
    for (statement, change) in [
        ("ALTER TABLE ONLY orders DROP COLUMN note;", InheritedChange::DropColumn),
        ("ALTER TABLE ONLY orders ALTER COLUMN note SET NOT NULL;", InheritedChange::SetNotNull),
    ] {
        let error = refusal(statement);
        assert!(
            matches!(
                &error,
                Error::OnlyRefusedWithChildren { table_name, change: refused }
                    if table_name == "orders" && *refused == change
            ),
            "{statement}: {error:?}"
        );
    }
    for statement in [
        // The column already requires a value, so nothing changes.
        "ALTER TABLE ONLY orders ALTER COLUMN id SET NOT NULL;",
        // A partitioned table without partitions has nothing to withhold from.
        "ALTER TABLE ONLY empty DROP COLUMN note;",
        "ALTER TABLE ONLY empty ALTER COLUMN note SET NOT NULL;",
        // Plain inheritance lets the parent alone drop a column or require a
        // value, and the child keeps its copy.
        "ALTER TABLE ONLY parent DROP COLUMN note;",
        "ALTER TABLE ONLY parent ALTER COLUMN id SET NOT NULL;",
    ] {
        parse(statement).unwrap_or_else(|error| panic!("{statement} was refused: {error:?}"));
    }
}
