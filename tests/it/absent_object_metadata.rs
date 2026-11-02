//! Tests that metadata accessors report an object the database does not hold
//! instead of aborting the process.
#![allow(clippy::expect_used)]
// The accessors under test return an opaque iterator in the `Ok` case, which is
// not `Debug`, so `unwrap_err` cannot be used and the expectations are compared
// against `Result::err`.
#![allow(clippy::unnecessary_wraps)]

use sql_traits::{
    errors::{LookupError, ObjectKind},
    prelude::*,
    utils::maintenance_trigger_parser::{MaintenanceBodyError, parse_maintenance_body},
};
use sqlparser::{
    ast::{CreateTable, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

/// Builds a database from `sql` and hands back the `CREATE TABLE` node the
/// caller's own input carries, which is the normal situation for a translator
/// walking the statements it was given.
///
/// `sql` must declare exactly one table, so that the node handed back is not a
/// silent choice among several.
fn parse_with_create_table(sql: &str) -> (CreateTable, ParserDB) {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("SQL parses");
    let database =
        ParserDB::from_statements(statements.clone(), "test".to_string()).expect("schema builds");
    let mut create_tables = statements.iter().filter_map(|statement| {
        match statement {
            Statement::CreateTable(create_table) => Some(create_table.clone()),
            _ => None,
        }
    });
    let create_table = create_tables.next().expect("input carries a CREATE TABLE");
    assert!(create_tables.next().is_none(), "input must declare exactly one table");

    (create_table, database)
}

/// A rename makes the pre-rename node a table the database no longer holds,
/// which is the cheapest way to construct the mismatch.
const RENAMED: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, owner TEXT);
    ALTER TABLE t ENABLE ROW LEVEL SECURITY;
    RENAME TABLE t TO t2;";

fn absent_table(table_name: &str) -> Option<LookupError> {
    Some(ObjectKind::Table.not_in_database(table_name))
}

#[test]
fn renamed_table_is_reported_rather_than_aborting() {
    let (stale, database) = parse_with_create_table(RENAMED);

    assert_eq!(
        database.tables().map(TableLike::table_name).collect::<Vec<&str>>(),
        vec!["t2"],
        "the rename is applied, so the database holds t2 and nothing named t"
    );
    assert_eq!(stale.has_row_level_security(&database).err(), absent_table("t"));
}

#[test]
fn every_table_metadata_accessor_reports_the_absent_table() {
    let (stale, database) = parse_with_create_table(RENAMED);

    assert_eq!(stale.table_doc(&database).err(), absent_table("t"));
    assert_eq!(stale.columns(&database).err(), absent_table("t"));
    assert_eq!(stale.primary_key_columns(&database).err(), absent_table("t"));
    assert_eq!(stale.unique_indices(&database).err(), absent_table("t"));
    assert_eq!(stale.indices(&database).err(), absent_table("t"));
    assert_eq!(stale.check_constraints(&database).err(), absent_table("t"));
    assert_eq!(stale.foreign_keys(&database).err(), absent_table("t"));
    assert_eq!(stale.has_row_level_security(&database).err(), absent_table("t"));
    assert_eq!(stale.has_forced_row_level_security(&database).err(), absent_table("t"));
    assert_eq!(stale.owner(&database).err(), absent_table("t"));
}

#[test]
fn the_renamed_table_itself_still_answers() {
    let (_, database) = parse_with_create_table(RENAMED);
    let table = database
        .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t2 exists after the rename");

    assert_eq!(
        table.columns(&database).expect("t2 is in this database").count(),
        2,
        "the surviving table keeps its columns, so the rename did not orphan its metadata"
    );
    assert!(table.has_row_level_security(&database).expect("t2 is in this database"));
}

#[test]
fn a_node_from_a_different_database_is_reported() {
    // Renaming is only one way to construct the mismatch: querying a node
    // against a database built from different input does it too.
    let (foreign_node, _) = parse_with_create_table("CREATE TABLE elsewhere (id INTEGER);");
    let database = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INTEGER);")
        .expect("schema builds");

    assert_eq!(foreign_node.columns(&database).err(), absent_table("elsewhere"));
}

/// A foreign key whose target the database does not hold can no longer be
/// built from SQL, since a dangling reference is refused as it is read. It is
/// still reachable the way every other mismatch in this file is: a node from
/// one database queried against another. The accessor must report it rather
/// than abort.
#[test]
fn a_foreign_key_reporting_an_absent_target_does_not_abort() {
    let (_, with_parent) = parse_with_create_table(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, child_id INTEGER REFERENCES parent(id));",
    );
    let elsewhere = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE unrelated (id INTEGER);")
        .expect("schema builds");

    let parent = with_parent
        .table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("parent exists");
    let foreign_key = parent
        .foreign_keys(&with_parent)
        .expect("parent is in its own database")
        .next()
        .expect("parent declares a foreign key");

    assert_eq!(
        foreign_key.referenced_table(&elsewhere).err(),
        Some(LookupError::TableNotFound { object_name: "parent".to_string() })
    );
}

/// Same construction for the column side: the target table exists in the other
/// database but carries no column of that name.
#[test]
fn a_foreign_key_naming_an_undeclared_column_does_not_abort() {
    let (_, declared) = parse_with_create_table(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, other INTEGER REFERENCES parent(id));",
    );
    let narrowed = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE parent (absent INTEGER);")
        .expect("schema builds");

    let parent = declared
        .table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("parent exists");
    let foreign_key = parent
        .foreign_keys(&declared)
        .expect("parent is in its own database")
        .next()
        .expect("parent declares a foreign key");

    assert_eq!(
        foreign_key.referenced_columns(&narrowed).err(),
        Some(LookupError::ColumnNotFound {
            table_name: "parent".to_string(),
            column_name: "id".to_string(),
        })
    );
}

/// A cycle cannot be written as two `CREATE TABLE` statements, because the
/// first would name a table that does not exist yet and the database refuses
/// that. It is written the way a real schema writes it, with the constraints
/// added once both tables are there.
#[test]
fn a_foreign_key_cycle_is_reported_rather_than_aborting() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, b_id INTEGER);
         CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER);
         ALTER TABLE a ADD CONSTRAINT fa FOREIGN KEY (b_id) REFERENCES b(id);
         ALTER TABLE b ADD CONSTRAINT fb FOREIGN KEY (a_id) REFERENCES a(id);",
    )
    .expect("schema builds");

    let error = database.table_dag().expect_err("a cycle has no topological order");
    assert!(
        matches!(error, sql_traits::errors::Error::CyclicTableDependencies { .. }),
        "expected a cycle report, got {error}"
    );
}

/// A maintenance trigger body resolves its assigned columns against the host
/// table, so an absent host table means the question cannot be decided. It must
/// not collapse into "this is not a maintenance body".
#[test]
fn a_maintenance_body_over_an_absent_table_reports_the_lookup() {
    let (stale, database) = parse_with_create_table(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, edited_at TIMESTAMP);
         RENAME TABLE t TO t2;",
    );
    let body = "BEGIN NEW.edited_at = CURRENT_TIMESTAMP; RETURN NEW; END;";

    assert_eq!(
        parse_maintenance_body(body, &stale, &database),
        Err(MaintenanceBodyError::Lookup(ObjectKind::Table.not_in_database("t")))
    );
}

#[test]
fn a_body_that_is_not_a_maintenance_body_stays_distinguishable() {
    let (_, database) =
        parse_with_create_table("CREATE TABLE t (id INTEGER PRIMARY KEY, edited_at TIMESTAMP);");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");

    assert_eq!(
        parse_maintenance_body("BEGIN RAISE NOTICE 'hello'; END;", table, &database),
        Err(MaintenanceBodyError::NotMaintenanceBody)
    );
    assert_eq!(
        parse_maintenance_body(
            "BEGIN NEW.edited_at = CURRENT_TIMESTAMP; RETURN NEW; END;",
            table,
            &database,
        )
        .map(|assignments| assignments.len()),
        Ok(1)
    );
}

/// These four answer from the tables `database` holds rather than from the
/// receiver's own metadata, so they would report an absent receiver as merely
/// unrelated to everything. `require_in_database` is what stops that, and this
/// pins it. The accessors that filter the database's own triggers, policies and
/// grants have the same shape and are pinned by
/// `the_database_filtering_accessors_report_an_absent_receiver` below.
#[test]
fn the_identity_scanning_accessors_report_an_absent_receiver() {
    let (stale, database) = parse_with_create_table(RENAMED);

    assert_eq!(stale.extending_tables(&database).err(), absent_table("t"));
    assert_eq!(stale.is_extended(&database).err(), absent_table("t"));
    assert_eq!(stale.dependent_tables(&database).err(), absent_table("t"));
    assert_eq!(stale.has_dependent_tables(&database).err(), absent_table("t"));
}

/// The check must not change the answer for a table the database does hold.
#[test]
fn the_identity_scanning_accessors_still_answer_a_live_receiver() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (id INTEGER PRIMARY KEY REFERENCES parent(id));",
    )
    .expect("schema builds");
    let parent = database
        .table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("parent exists");
    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("child exists");

    assert_eq!(
        parent
            .extending_tables(&database)
            .expect("parent is in this database")
            .map(TableLike::table_name)
            .collect::<Vec<&str>>(),
        vec!["child"]
    );
    assert!(parent.is_extended(&database).expect("parent is in this database"));
    assert!(parent.has_dependent_tables(&database).expect("parent is in this database"));
    assert!(!child.is_extended(&database).expect("child is in this database"));
}

/// These filter the database's own triggers, policies and grants by comparing
/// each one's table against the receiver, the same shape as the four above, so
/// they resolve the receiver first for the same reason. The `can_*` methods
/// inherit it through `grants`.
#[test]
fn the_database_filtering_accessors_report_an_absent_receiver() {
    let (stale, database) = parse_with_create_table(
        "CREATE TABLE t (id INTEGER PRIMARY KEY);
         CREATE ROLE reader;
         GRANT SELECT ON t TO reader;
         RENAME TABLE t TO t2;",
    );
    let reader = database.role("reader").expect("reader exists");

    assert_eq!(stale.triggers(&database).err(), absent_table("t"));
    assert_eq!(stale.policies(&database).err(), absent_table("t"));
    assert_eq!(stale.grants(&database).err(), absent_table("t"));
    assert_eq!(stale.can_select(reader, &database).err(), absent_table("t"));
    assert_eq!(stale.can_insert(reader, &database).err(), absent_table("t"));
    assert_eq!(stale.can_update(reader, &database).err(), absent_table("t"));
    assert_eq!(stale.can_delete(reader, &database).err(), absent_table("t"));
    assert_eq!(stale.can_write(reader, &database).err(), absent_table("t"));
    assert_eq!(stale.can_truncate(reader, &database).err(), absent_table("t"));
}
