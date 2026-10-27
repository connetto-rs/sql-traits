//! Tests that every `ALTER TABLE` operation accounts for itself.
//!
//! The statement match used to end in a catch-all arm that silently discarded
//! thirty-seven operations along with everything not yet implemented, so an
//! operation that changed the schema and one that changed nothing the model
//! describes were indistinguishable. The arm is gone: an operation is applied,
//! or deliberately ignored because the model carries no representation of what
//! it changes, or reported as not yet supported.
//!
//! That the accounting stays complete is enforced by the compiler rather than
//! by a test here, since the operation list is not open for extension and the
//! match has no wildcard, so a new operation in the parser library fails the
//! build until it is placed.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{ClickHouseDialect, GenericDialect, MySqlDialect, PostgreSqlDialect};

const TABLE: &str = "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT);";

fn parse(tail: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{TABLE} {tail};"))
}

/// What an ignored operation must leave exactly as the bare table left it.
fn shape(database: &ParserDB) -> (usize, usize, usize, usize, usize, bool) {
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t survives");
    (
        database.tables().count(),
        table.columns(database).expect("t is in this database").count(),
        table.unique_indices(database).expect("t is in this database").count(),
        table.check_constraints(database).expect("t is in this database").count(),
        database.indexes().count(),
        table.has_row_level_security(database).expect("t is in this database"),
    )
}

/// Each of these changes part of the schema the model represents, so silently
/// discarding it would leave the model wrong rather than merely coarse.
#[test]
fn an_operation_that_would_leave_the_model_wrong_is_reported() {
    let postgres = [
        "ALTER TABLE t DROP CONSTRAINT IF EXISTS nothing, RENAME CONSTRAINT c TO d",
        "ALTER TABLE t SWAP WITH other",
    ];
    for tail in postgres {
        let error = parse(tail).expect_err("the operation is not applied");
        assert!(
            matches!(&error, Error::UnsupportedAlterTableOperation { table_name, .. }
                if table_name == "t"),
            "{tail} reported {error:?}"
        );
    }

    let mysql = [
        "ALTER TABLE t DROP PRIMARY KEY",
        "ALTER TABLE t DROP FOREIGN KEY fk",
        "ALTER TABLE t DROP INDEX i",
    ];
    for tail in mysql {
        let error = ParserDB::parse::<MySqlDialect>(&format!("{TABLE} {tail};"))
            .expect_err("the operation is not applied");
        assert!(
            matches!(&error, Error::UnsupportedAlterTableOperation { table_name, .. }
                if table_name == "t"),
            "{tail} reported {error:?}"
        );
    }
}

/// The report names the operation, so a caller reading it knows which clause of
/// a multi-operation statement stopped the parse.
#[test]
fn the_report_names_the_operation() {
    let error =
        ParserDB::parse::<MySqlDialect>(&format!("{TABLE} ALTER TABLE t DROP PRIMARY KEY;"))
            .expect_err("dropping the primary key is not applied");
    assert!(
        matches!(&error, Error::UnsupportedAlterTableOperation { operation, .. }
            if operation.contains("PRIMARY KEY")),
        "the report names the operation: {error:?}"
    );
}

/// The owner is recorded rather than discarded. The role has to exist, as the
/// database requires, and a dump that names an owner while creating no role is
/// covered by the permissive setting exercised below.
#[test]
fn an_ownership_change_records_the_owner() {
    let database =
        parse("CREATE ROLE someone; ALTER TABLE t OWNER TO someone").expect("ownership recorded");

    assert_eq!(database.tables().count(), 1);
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t survives");
    assert_eq!(table.columns(&database).expect("t is in this database").count(), 3);
    assert_eq!(table.owner(&database), Ok(Some("someone")));
}

/// A Postgres dump emits an ownership change for every table while creating no
/// role at all, so refusing it by default would turn away ordinary input. The
/// setting that already excuses a grantee excuses an owner for the same reason.
#[test]
fn a_dump_naming_an_owner_it_never_creates_needs_the_permissive_setting() {
    let sql = format!("{TABLE} ALTER TABLE t OWNER TO someone;");

    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&sql),
        Err(Error::RoleNotFoundForOwner { ref role_name, ref object_name })
            if role_name == "someone" && object_name == "t"
    ));

    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(&sql)
        .expect("a dump names owners it does not create");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t survives");
    assert_eq!(table.owner(&database), Ok(Some("someone")));
}

/// A representative operation from each ignored family parses and leaves the
/// model exactly as the bare table left it.
#[test]
fn operations_over_things_the_model_does_not_describe_change_nothing() {
    let bare = ParserDB::parse::<PostgreSqlDialect>(TABLE).expect("the table alone parses");

    let ignored = [
        // Physical layout.
        "ALTER TABLE t CLUSTER BY (a)",
        // Rewrite rules, whose enablement has nothing to attach to.
        "ALTER TABLE t DISABLE RULE r",
        "ALTER TABLE t ENABLE ALWAYS RULE r",
        // A trigger is modelled, but whether it is armed is not.
        "ALTER TABLE t DISABLE TRIGGER g",
        "ALTER TABLE t ENABLE REPLICA TRIGGER g",
        // Replication identity.
        "ALTER TABLE t REPLICA IDENTITY FULL",
    ];

    for tail in ignored {
        let parsed = parse(tail);
        assert!(parsed.is_ok(), "{tail} reported {:?}", parsed.as_ref().err());
        let database = parsed.expect("the parse succeeded just above");
        assert_eq!(shape(&database), shape(&bare), "{tail} changed the model");
    }
}

/// Partition and vendor operations arrive under the dialects that spell them,
/// and are ignored just the same.
#[test]
fn vendor_operations_parse_under_their_dialects() {
    let mysql = [
        "ALTER TABLE t ALGORITHM = INPLACE",
        "ALTER TABLE t LOCK = NONE",
        "ALTER TABLE t AUTO_INCREMENT = 100",
    ];
    for tail in mysql {
        let parsed = ParserDB::parse::<MySqlDialect>(&format!("{TABLE} {tail};"));
        assert!(parsed.is_ok(), "{tail} reported {:?}", parsed.err());
    }

    let generic = ["ALTER TABLE t DROP PARTITION (p1)", "ALTER TABLE t ADD PARTITION (p1)"];
    for tail in generic {
        let parsed = ParserDB::parse::<GenericDialect>(&format!("{TABLE} {tail};"));
        assert!(parsed.is_ok(), "{tail} reported {:?}", parsed.err());
    }

    // ClickHouse's sorting key orders stored rows and leaves the primary key
    // alone, so replacing it changes nothing the model describes.
    let bare = ParserDB::parse::<ClickHouseDialect>(TABLE).expect("the table alone parses");
    let clickhouse = ["ALTER TABLE t MODIFY ORDER BY (a, b)", "ALTER TABLE t MODIFY ORDER BY a"];
    for tail in clickhouse {
        let parsed = ParserDB::parse::<ClickHouseDialect>(&format!("{TABLE} {tail};"));
        assert!(parsed.is_ok(), "{tail} reported {:?}", parsed.as_ref().err());
        let database = parsed.expect("the parse succeeded just above");
        assert_eq!(shape(&database), shape(&bare), "{tail} changed the model");
    }
}

/// An ignored operation sitting beside an applied one must not swallow it, and
/// a reported one must stop the statement wherever it sits.
#[test]
fn a_multi_operation_statement_treats_each_operation_on_its_own() {
    let database = parse("CREATE ROLE someone; ALTER TABLE t OWNER TO someone, ADD COLUMN x INT")
        .expect("ownership is recorded, the column is added");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t survives");
    assert_eq!(table.columns(&database).expect("t is in this database").count(), 4);
    assert_eq!(table.owner(&database), Ok(Some("someone")));

    let error = ParserDB::parse::<MySqlDialect>(&format!(
        "{TABLE} ALTER TABLE t ADD COLUMN x INT, DROP PRIMARY KEY;"
    ))
    .expect_err("the second operation is not applied");
    assert!(matches!(&error, Error::UnsupportedAlterTableOperation { .. }), "got {error:?}");
}
