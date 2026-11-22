//! Tests that a check or foreign key written `NOT ENFORCED` reads back
//! unenforced and unvalidated, and cannot be validated.
//!
//! Every expectation here was measured against PostgreSQL 18 by reading
//! `pg_constraint.conenforced` and `pg_constraint.convalidated` after each
//! statement.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn table<'db>(database: &'db ParserDB, table_name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
}

/// Whether each check of the table is enforced and validated, keyed by name.
fn checks(database: &ParserDB, table_name: &str) -> Vec<(String, bool, bool)> {
    table(database, table_name)
        .check_constraints(database)
        .expect("table is in this database")
        .map(|check| {
            (
                check
                    .constraint_name(database)
                    .expect("check is in this database")
                    .expect("PostgreSQL names every check")
                    .into_owned(),
                check.is_enforced(),
                check.is_validated(database).expect("check is in this database"),
            )
        })
        .collect()
}

/// Whether each foreign key of the table is enforced and validated, keyed by
/// name.
fn foreign_keys(database: &ParserDB, table_name: &str) -> Vec<(String, bool, bool)> {
    table(database, table_name)
        .foreign_keys(database)
        .expect("table is in this database")
        .map(|key| {
            (
                key.constraint_name(database)
                    .expect("key is in this database")
                    .expect("PostgreSQL names every foreign key")
                    .into_owned(),
                key.is_enforced(database),
                key.is_validated(database).expect("key is in this database"),
            )
        })
        .collect()
}

fn states(expected: &[(&str, bool, bool)]) -> Vec<(String, bool, bool)> {
    expected
        .iter()
        .map(|(name, enforced, validated)| ((*name).to_owned(), *enforced, *validated))
        .collect()
}

const SCHEMA: &str = "CREATE TABLE r (id INT PRIMARY KEY);
    CREATE TABLE t (
        a INT CONSTRAINT colck CHECK (a > 0) NOT ENFORCED CONSTRAINT colfk REFERENCES r (id) NOT ENFORCED,
        b INT,
        CONSTRAINT tck CHECK (b > 0) NOT ENFORCED,
        CONSTRAINT tfk FOREIGN KEY (b) REFERENCES r (id) NOT ENFORCED,
        CONSTRAINT eck CHECK (b < 100) ENFORCED,
        CONSTRAINT efk FOREIGN KEY (b) REFERENCES r (id) ENFORCED
    );";

#[test]
fn not_enforced_constraints_read_back_unenforced_and_unvalidated() {
    let database = parse(SCHEMA).expect("schema parses");
    let mut checks = checks(&database, "t");
    checks.sort();
    assert_eq!(
        checks,
        states(&[("colck", false, false), ("eck", true, true), ("tck", false, false)])
    );
    let mut keys = foreign_keys(&database, "t");
    keys.sort();
    assert_eq!(
        keys,
        states(&[("colfk", false, false), ("efk", true, true), ("tfk", false, false)])
    );
}

#[test]
fn not_enforced_constraints_added_later_read_back_unenforced_and_unvalidated() {
    let database = parse(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE t (b INT);
         ALTER TABLE t ADD CONSTRAINT ck CHECK (b > 0) NOT ENFORCED NOT VALID;
         ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (b) REFERENCES r (id) NOT ENFORCED;",
    )
    .expect("schema parses");
    assert_eq!(checks(&database, "t"), states(&[("ck", false, false)]));
    assert_eq!(foreign_keys(&database, "t"), states(&[("fk", false, false)]));
}

#[test]
fn a_partition_holds_a_not_enforced_foreign_key_unenforced_and_unvalidated() {
    let database = parse(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (b INT, CONSTRAINT pfk FOREIGN KEY (b) REFERENCES r (id) NOT ENFORCED)
             PARTITION BY RANGE (b);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);",
    )
    .expect("schema parses");
    for table_name in ["p", "p1"] {
        assert_eq!(foreign_keys(&database, table_name), states(&[("pfk", false, false)]));
    }
}

#[test]
fn a_not_enforced_foreign_key_is_never_always_enforced() {
    let database = parse(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE t (b INT NOT NULL, CONSTRAINT fk FOREIGN KEY (b) REFERENCES r (id) NOT ENFORCED);",
    )
    .expect("schema parses");
    let key = table(&database, "t")
        .foreign_keys(&database)
        .expect("in database")
        .next()
        .expect("one key");
    assert!(!key.is_always_enforced(&database).expect("in database"));
}

#[test]
fn validating_a_not_enforced_constraint_is_refused() {
    for name in ["colck", "colfk", "tck", "tfk"] {
        let sql = format!("{SCHEMA} ALTER TABLE t VALIDATE CONSTRAINT {name};");
        assert!(
            matches!(
                parse(&sql),
                Err(Error::NotEnforcedNotValidatable { ref table_name, ref constraint_name })
                    if table_name == "t" && constraint_name == name
            ),
            "should have been refused:\n{sql}"
        );
    }
    for name in ["eck", "efk"] {
        let sql = format!("{SCHEMA} ALTER TABLE t VALIDATE CONSTRAINT {name};");
        assert!(parse(&sql).is_ok(), "should have been accepted:\n{sql}");
    }

    let partitioned = "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (b INT, CONSTRAINT pfk FOREIGN KEY (b) REFERENCES r (id) NOT ENFORCED)
             PARTITION BY RANGE (b);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
         ALTER TABLE p VALIDATE CONSTRAINT pfk;";
    assert!(matches!(parse(partitioned), Err(Error::NotEnforcedNotValidatable { .. })));
}
