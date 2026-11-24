//! Tests that a trigger answers the transition tables its `REFERENCING`
//! clause declares, as PostgreSQL records them in `pg_trigger.tgoldtable` and
//! `tgnewtable`, and that every placement PostgreSQL accepts is ingested.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

const SETUP: &str = "CREATE TABLE t (id INT);
    CREATE TABLE p (id INT) PARTITION BY RANGE (id);
    CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
    CREATE TABLE parent (id INT);
    CREATE TABLE child () INHERITS (parent);
    CREATE TABLE lone (id INT);
    CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;";

fn db(statements: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SETUP} {statements}"))
        .unwrap_or_else(|error| panic!("{statements} was refused: {error:?}"))
}

/// The transition tables of the trigger `trigger_name` on `table_name`, each
/// as its rows with its stored name.
fn transition_tables(
    database: &ParserDB,
    table_name: &str,
    trigger_name: &str,
) -> Vec<(TriggerRow, String)> {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    database
        .trigger(trigger_name, table)
        .unwrap_or_else(|| panic!("{table_name} carries {trigger_name}"))
        .transition_tables()
        .map(|table| (table.rows(), table.stored_name().into_owned()))
        .collect()
}

#[test]
fn every_placement_postgres_accepts_answers_its_transition_tables() {
    let database = db("CREATE TRIGGER audit AFTER UPDATE ON t
           REFERENCING OLD TABLE AS Old_Rows NEW TABLE AS \"New_Rows\"
           FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER bare AFTER INSERT ON t REFERENCING NEW TABLE t
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TRIGGER partitioned AFTER DELETE ON p REFERENCING OLD TABLE AS gone
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TRIGGER partition AFTER INSERT ON p1 REFERENCING NEW TABLE AS added
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TRIGGER inherited AFTER INSERT ON child REFERENCING NEW TABLE AS added
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TRIGGER ancestor AFTER INSERT ON parent REFERENCING NEW TABLE AS added
           FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER plain AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();");
    for (table_name, trigger_name, expected) in [
        (
            "t",
            "audit",
            vec![
                (TriggerRow::Old, "old_rows".to_owned()),
                (TriggerRow::New, "New_Rows".to_owned()),
            ],
        ),
        ("t", "bare", vec![(TriggerRow::New, "t".to_owned())]),
        ("p", "partitioned", vec![(TriggerRow::Old, "gone".to_owned())]),
        ("p1", "partition", vec![(TriggerRow::New, "added".to_owned())]),
        ("child", "inherited", vec![(TriggerRow::New, "added".to_owned())]),
        ("parent", "ancestor", vec![(TriggerRow::New, "added".to_owned())]),
        ("t", "plain", vec![]),
    ] {
        assert_eq!(
            transition_tables(&database, table_name, trigger_name),
            expected,
            "{trigger_name}"
        );
    }
}

#[test]
fn a_replacing_trigger_answers_its_own_transition_tables() {
    let database = db("CREATE TRIGGER audit AFTER INSERT ON t REFERENCING NEW TABLE AS added
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE OR REPLACE TRIGGER audit AFTER DELETE ON t REFERENCING OLD TABLE AS removed
           FOR EACH STATEMENT EXECUTE FUNCTION f();");
    assert_eq!(
        transition_tables(&database, "t", "audit"),
        [(TriggerRow::Old, "removed".to_owned())]
    );
}

#[test]
fn statement_triggers_with_transition_tables_leave_a_table_free_to_join_a_hierarchy() {
    let database = db("CREATE TRIGGER audit AFTER INSERT ON lone REFERENCING NEW TABLE AS added
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         ALTER TABLE p ATTACH PARTITION lone FOR VALUES FROM (10) TO (20);
         CREATE TABLE heir (id INT);
         CREATE TRIGGER audit AFTER INSERT ON heir REFERENCING NEW TABLE AS added
           FOR EACH STATEMENT EXECUTE FUNCTION f();
         ALTER TABLE heir INHERIT parent;");
    assert_eq!(
        transition_tables(&database, "lone", "audit"),
        [(TriggerRow::New, "added".to_owned())]
    );
    assert_eq!(
        transition_tables(&database, "heir", "audit"),
        [(TriggerRow::New, "added".to_owned())]
    );
}
