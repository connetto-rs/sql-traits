//! Tests that a trigger answers whether it is a constraint trigger and when
//! it fires, as PostgreSQL records it in `pg_trigger.tgconstraint`,
//! `tgdeferrable` and `tginitdeferred`, on the copies of a partitioned
//! table's row triggers too.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

const FUNCTION: &str =
    "CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;";

fn db(statements: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{FUNCTION} {statements}"))
        .unwrap_or_else(|error| panic!("{statements} was refused: {error:?}"))
}

/// The deferral of the trigger `trigger_name` on the table `table_name`.
fn deferral(
    database: &ParserDB,
    table_name: &str,
    trigger_name: &str,
) -> Option<ConstraintDeferral> {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    database
        .trigger(trigger_name, table)
        .unwrap_or_else(|| panic!("{table_name} carries {trigger_name}"))
        .deferral()
}

#[test]
fn constraint_triggers_answer_their_deferral_and_ordinary_triggers_none() {
    let database = db("CREATE TABLE parents (id INT PRIMARY KEY);
         CREATE CONSTRAINT TRIGGER deferred AFTER INSERT ON parents
           DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION f();
         CREATE CONSTRAINT TRIGGER immediate AFTER INSERT ON parents
           DEFERRABLE FOR EACH ROW EXECUTE FUNCTION f();
         CREATE CONSTRAINT TRIGGER spelled AFTER INSERT ON parents
           DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION f();
         CREATE CONSTRAINT TRIGGER implied AFTER INSERT ON parents
           INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION f();
         CREATE CONSTRAINT TRIGGER fixed AFTER INSERT ON parents
           FOR EACH ROW EXECUTE FUNCTION f();
         CREATE CONSTRAINT TRIGGER said AFTER INSERT ON parents
           NOT DEFERRABLE FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER plain AFTER INSERT ON parents
           FOR EACH ROW EXECUTE FUNCTION f();");
    for (trigger_name, expected) in [
        ("deferred", Some(ConstraintDeferral::InitiallyDeferred)),
        ("immediate", Some(ConstraintDeferral::InitiallyImmediate)),
        ("spelled", Some(ConstraintDeferral::InitiallyImmediate)),
        ("implied", Some(ConstraintDeferral::InitiallyDeferred)),
        ("fixed", Some(ConstraintDeferral::NotDeferrable)),
        ("said", Some(ConstraintDeferral::NotDeferrable)),
        ("plain", None),
    ] {
        assert_eq!(deferral(&database, "parents", trigger_name), expected, "{trigger_name}");
    }
}

#[test]
fn partition_copies_keep_the_deferral_of_their_parent_trigger() {
    let database = db("CREATE TABLE p (id INT) PARTITION BY RANGE (id);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
         CREATE TABLE p1a PARTITION OF p1 FOR VALUES FROM (0) TO (5);
         CREATE CONSTRAINT TRIGGER checked AFTER INSERT ON p
           DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER plain AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TABLE p2 (id INT);
         ALTER TABLE p ATTACH PARTITION p2 FOR VALUES FROM (10) TO (20);");
    for table_name in ["p", "p1", "p1a", "p2"] {
        assert_eq!(
            deferral(&database, table_name, "checked"),
            Some(ConstraintDeferral::InitiallyDeferred),
            "{table_name}"
        );
        assert_eq!(deferral(&database, table_name, "plain"), None, "{table_name}");
    }
}
