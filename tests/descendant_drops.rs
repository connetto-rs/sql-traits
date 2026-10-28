//! Tests that `DROP TABLE` reaches the tables below the one it names the way
//! PostgreSQL 18 does.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn table_names(database: &ParserDB) -> Vec<String> {
    database.tables().map(|table| table.table_name().to_owned()).collect()
}

fn view_names(database: &ParserDB) -> Vec<String> {
    database.views().map(|view| view.view_name().to_owned()).collect()
}

fn foreign_key_count(database: &ParserDB, name: &str) -> usize {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .foreign_keys(database)
        .expect("table is in this database")
        .count()
}

#[test]
fn a_partitioned_table_takes_its_partitions_along_without_cascade() {
    let database = parse(
        "CREATE TABLE p (id INT) PARTITION BY LIST (id);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1) PARTITION BY LIST (id);
         CREATE TABLE cc PARTITION OF c FOR VALUES IN (1);
         CREATE TABLE attached (id INT);
         ALTER TABLE p ATTACH PARTITION attached FOR VALUES IN (2);
         CREATE TABLE untouched (id INT);
         DROP TABLE p;",
    )
    .expect("the partitions go with their root");

    assert_eq!(table_names(&database), ["untouched"]);
}

#[test]
fn a_partition_leaves_its_root_behind() {
    let schema = "CREATE TABLE p (id INT) PARTITION BY LIST (id);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1) PARTITION BY LIST (id);
         CREATE TABLE cc PARTITION OF c FOR VALUES IN (1);";

    let database = parse(&format!("{schema} DROP TABLE cc;")).expect("a leaf drops alone");
    assert_eq!(table_names(&database), ["c", "p"]);
    let database = parse(&format!("{schema} DROP TABLE c;")).expect("its partition goes with it");
    assert_eq!(table_names(&database), ["p"]);
}

#[test]
fn a_root_named_beside_its_partition_drops_both() {
    let schema = "CREATE TABLE p (id INT) PARTITION BY LIST (id);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1);";
    for drop in ["DROP TABLE p, c;", "DROP TABLE c, p;"] {
        let database = parse(&format!("{schema} {drop}")).expect("both names resolve first");
        assert_eq!(database.tables().count(), 0, "{drop}");
    }
    assert!(matches!(
        parse(&format!("{schema} DROP TABLE p, absent;")),
        Err(Error::DropTableNotFound { table_name }) if table_name == "absent"
    ));
    // An absent name is reported before anything is dropped.
    assert!(matches!(
        parse(&format!("{schema} CREATE VIEW v AS SELECT * FROM c; DROP TABLE p, absent;")),
        Err(Error::DropTableNotFound { .. })
    ));
}

#[test]
fn a_view_reading_a_partition_holds_the_root_back_until_cascade() {
    let schema = "CREATE TABLE p (id INT) PARTITION BY LIST (id);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1);
         CREATE VIEW v AS SELECT * FROM c;";

    assert!(matches!(
        parse(&format!("{schema} DROP TABLE p;")),
        Err(Error::RelationHasDependents {
            object_kind: ObjectKind::Table,
            object_name,
            dependent_kind: ObjectKind::View,
            dependent_name,
        }) if object_name == "p" && dependent_name == "v"
    ));
    let database = parse(&format!("{schema} DROP TABLE p CASCADE;")).expect("cascade takes v");
    assert_eq!(database.views().count(), 0);
}

#[test]
fn a_view_reading_an_inheritor_goes_with_a_cascaded_parent() {
    let database = parse(
        "CREATE TABLE p (id INT);
         CREATE TABLE k () INHERITS (p);
         CREATE VIEW v AS SELECT * FROM k;
         CREATE VIEW w AS SELECT 1 AS one;
         DROP TABLE p CASCADE;",
    )
    .expect("cascade takes the inheritor and its view");

    assert_eq!(database.tables().count(), 0);
    assert_eq!(view_names(&database), ["w"]);
}

#[test]
fn a_foreign_key_to_a_partition_holds_the_root_back_until_cascade() {
    let schema = "CREATE TABLE p (id INT PRIMARY KEY) PARTITION BY LIST (id);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1);
         CREATE TABLE x (cid INT REFERENCES c (id));";

    assert!(matches!(
        parse(&format!("{schema} DROP TABLE p;")),
        Err(Error::TableReferenced { table_name }) if table_name == "p"
    ));
    let database = parse(&format!("{schema} DROP TABLE p CASCADE;")).expect("cascade cuts the key");
    assert_eq!(table_names(&database), ["x"]);
    assert_eq!(foreign_key_count(&database, "x"), 0);
}

#[test]
fn a_partitions_own_foreign_keys_do_not_hold_it_back() {
    let database = parse(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (id INT, rid INT REFERENCES r (id)) PARTITION BY LIST (id);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1);
         DROP TABLE p;",
    )
    .expect("keys leaving with the dropped tables refuse nothing");

    assert_eq!(table_names(&database), ["r"]);
}
