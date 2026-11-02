//! Tests that an `ALTER TABLE` naming a table the input never created is
//! refused rather than skipped in silence, and that `IF EXISTS` is the only
//! thing that excuses it.
//!
//! The row level security operations used to resolve the table and `continue`
//! when it was absent, so a schema whose protection statement named an
//! unreachable table parsed clean and produced a table with no protection. The
//! neighbouring operations on the same statement kind already refused, which
//! made the outcome depend on which operation followed the table name.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const OPERATIONS: [&str; 4] = [
    "ENABLE ROW LEVEL SECURITY",
    "DISABLE ROW LEVEL SECURITY",
    "FORCE ROW LEVEL SECURITY",
    "NO FORCE ROW LEVEL SECURITY",
];

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

#[test]
fn a_row_security_operation_naming_an_absent_table_is_refused() {
    for operation in OPERATIONS {
        let sql = format!(
            "CREATE TABLE present (id INT PRIMARY KEY);
             ALTER TABLE absent {operation};"
        );
        let error = parse(&sql).expect_err("the statement names a table nothing creates");
        assert!(
            matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "absent"),
            "{operation} reported {error:?} instead of naming the absent table"
        );
    }
}

/// `IF EXISTS` is the caller saying the table may be gone, which is the one
/// reason to accept the statement and change nothing.
#[test]
fn if_exists_excuses_an_absent_table() {
    for operation in OPERATIONS {
        let sql = format!(
            "CREATE TABLE present (id INT PRIMARY KEY);
             ALTER TABLE IF EXISTS absent {operation};"
        );
        let database = parse(&sql).expect("IF EXISTS asks for the statement to be tolerated");
        assert_eq!(database.tables().count(), 1);
    }
}

/// The refusal must not come from over-eager matching: an unqualified name
/// still resolves, and the flag lands on the table that was named rather than
/// on whichever table happens to be first.
#[test]
fn the_named_table_still_receives_the_setting() {
    let database = parse(
        "CREATE TABLE untouched (id INT PRIMARY KEY);
         CREATE TABLE guarded (id INT PRIMARY KEY);
         ALTER TABLE guarded ENABLE ROW LEVEL SECURITY;
         ALTER TABLE guarded FORCE ROW LEVEL SECURITY;",
    )
    .expect("both tables exist");

    let guarded = database
        .table_by_target(TargetName::new("guarded", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("guarded was created");
    assert!(guarded.has_row_level_security(&database).expect("guarded is in this database"));
    assert!(guarded.has_forced_row_level_security(&database).expect("guarded is in this database"));

    let untouched = database
        .table_by_target(TargetName::new("untouched", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("untouched was created");
    assert!(!untouched.has_row_level_security(&database).expect("untouched is in this database"));
    assert!(
        !untouched.has_forced_row_level_security(&database).expect("untouched is in this database")
    );
}

/// Disabling has to be reachable after enabling, since the setting is a flag
/// rather than a one-way door.
#[test]
fn a_later_operation_overrides_an_earlier_one() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         ALTER TABLE t FORCE ROW LEVEL SECURITY;
         ALTER TABLE t NO FORCE ROW LEVEL SECURITY;
         ALTER TABLE t DISABLE ROW LEVEL SECURITY;",
    )
    .expect("t exists throughout");

    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t was created");
    assert!(!table.has_row_level_security(&database).expect("t is in this database"));
    assert!(!table.has_forced_row_level_security(&database).expect("t is in this database"));
}

/// One `ALTER TABLE` may carry several operations, and the table is resolved
/// once per operation, so an absent table has to be refused from whichever
/// position the row security operation occupies.
#[test]
fn an_absent_table_is_refused_from_any_position_in_a_multi_operation_statement() {
    let leading = parse(
        "CREATE TABLE present (id INT PRIMARY KEY, a INT);
         ALTER TABLE absent ENABLE ROW LEVEL SECURITY, ADD CONSTRAINT c UNIQUE (a);",
    )
    .expect_err("the statement names a table nothing creates");
    assert!(matches!(&leading, Error::AlterTableNotFound { table_name } if table_name == "absent"));

    let trailing = parse(
        "CREATE TABLE present (id INT PRIMARY KEY, a INT);
         ALTER TABLE absent ADD CONSTRAINT c UNIQUE (a), ENABLE ROW LEVEL SECURITY;",
    )
    .expect_err("the statement names a table nothing creates");
    assert!(
        matches!(&trailing, Error::AlterTableNotFound { table_name } if table_name == "absent")
    );
}

/// A schema-qualified name has to resolve the same way, so the refusal
/// reports the table rather than swallowing a qualified miss.
#[test]
fn a_qualified_absent_table_is_refused() {
    let error = parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.present (id INT PRIMARY KEY);
         ALTER TABLE s.absent ENABLE ROW LEVEL SECURITY;",
    )
    .expect_err("s.absent is not created");
    assert!(matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "absent"));

    let database = parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.guarded (id INT PRIMARY KEY);
         ALTER TABLE s.guarded ENABLE ROW LEVEL SECURITY;",
    )
    .expect("s.guarded exists");
    let guarded = database
        .table_by_target(
            TargetName::new("guarded", false).with_schema("s", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("s.guarded was created");
    assert!(guarded.has_row_level_security(&database).expect("s.guarded is in this database"));
}
