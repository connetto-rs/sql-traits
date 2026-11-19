//! Tests that a row trigger on a partitioned table is copied onto every
//! partition below it, as PostgreSQL records in `pg_trigger.tgparentid`, and
//! that the copies come and go with the trigger and the partitions as they do
//! there.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 18.6.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{Ident, TriggerEvent},
    dialect::PostgreSqlDialect,
};

use crate::support::table_lookup::{table, trigger_table};

const SCHEMA: &str =
    "CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql;
     CREATE TABLE p (id INT, v INT) PARTITION BY RANGE (id);
     CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
     CREATE TABLE p1a PARTITION OF p1 FOR VALUES FROM (0) TO (5);";

fn parse(statements: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} {statements}"))
}

fn db(statements: &str) -> ParserDB {
    parse(statements).unwrap_or_else(|error| panic!("{statements} was refused: {error:?}"))
}

/// The table or foreign table `name`.
fn relation<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    let lookup = |found: Result<Option<_>, _>| found.expect("unambiguous lookup");
    lookup(database.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten))
        .or_else(|| {
            lookup(
                database.foreign_table_by_target(
                    TargetName::new(name, false),
                    IdentifierCase::AsWritten,
                ),
            )
        })
        .expect("the relation exists")
}

/// Each trigger on `table_name` with the table of the trigger it copies.
fn triggers_on(database: &ParserDB, table_name: &str) -> Vec<(String, Option<String>)> {
    let mut triggers: Vec<(String, Option<String>)> = relation(database, table_name)
        .triggers(database)
        .expect("table is in this database")
        .map(|trigger| {
            let parent = trigger.parent_trigger(database).expect("trigger is in this database");
            (
                trigger.name().to_owned(),
                parent.map(|parent| trigger_table(parent, database).table_name().to_owned()),
            )
        })
        .collect();
    triggers.sort();
    triggers
}

fn owned(expected: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    expected.iter().map(|(name, parent)| ((*name).to_owned(), parent.map(str::to_owned))).collect()
}

#[test]
fn row_triggers_are_copied_onto_every_partition_level() {
    let database = db("CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER btrg BEFORE UPDATE OF v ON p FOR EACH ROW WHEN (NEW.v > 0)
             EXECUTE FUNCTION f();
         CREATE TRIGGER stmttrg AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();");

    assert_eq!(
        triggers_on(&database, "p"),
        owned(&[("btrg", None), ("rowtrg", None), ("stmttrg", None)])
    );
    assert_eq!(triggers_on(&database, "p1"), owned(&[("btrg", Some("p")), ("rowtrg", Some("p"))]));
    assert_eq!(
        triggers_on(&database, "p1a"),
        owned(&[("btrg", Some("p1")), ("rowtrg", Some("p1"))])
    );
    let copy = database.trigger("btrg", table(&database, None, "p1a")).expect("copy exists");
    assert_eq!(copy.events(), &[TriggerEvent::Update(vec![Ident::new("v")])]);
    assert_eq!(copy.function(&database).map(FunctionLike::name), Some("f"));
}

#[test]
fn a_later_partition_receives_copies_of_its_parents_row_triggers() {
    let database = db("CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER stmttrg AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TABLE p1b PARTITION OF p1 FOR VALUES FROM (5) TO (10);
         CREATE TABLE q (id INT, v INT) PARTITION BY RANGE (id);
         CREATE TABLE q1 PARTITION OF q FOR VALUES FROM (10) TO (15);
         CREATE TRIGGER own AFTER INSERT ON q FOR EACH ROW EXECUTE FUNCTION f();
         ALTER TABLE ONLY p ATTACH PARTITION q FOR VALUES FROM (10) TO (20);");

    assert_eq!(triggers_on(&database, "p1b"), owned(&[("rowtrg", Some("p1"))]));
    assert_eq!(triggers_on(&database, "q"), owned(&[("own", None), ("rowtrg", Some("p"))]));
    assert_eq!(triggers_on(&database, "q1"), owned(&[("own", Some("q")), ("rowtrg", Some("q"))]));
}

#[test]
fn an_inheritance_child_receives_no_copy() {
    let database = db("CREATE TABLE parent (id INT);
         CREATE TABLE child () INHERITS (parent);
         CREATE TRIGGER rowtrg AFTER INSERT ON parent FOR EACH ROW EXECUTE FUNCTION f();");

    assert_eq!(triggers_on(&database, "child"), owned(&[]));
}

#[test]
fn a_copy_is_neither_dropped_nor_replaced_on_its_own() {
    let schema = "CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();";

    assert!(matches!(
        parse(&format!("{schema} DROP TRIGGER rowtrg ON p1a;")),
        Err(Error::ClonedTriggerDropped { trigger_name, table_name, parent_table_name })
            if trigger_name == "rowtrg" && table_name == "p1a" && parent_table_name == "p1"
    ));
    assert!(matches!(
        parse(&format!("{schema} DROP TRIGGER IF EXISTS rowtrg ON p1a;")),
        Err(Error::ClonedTriggerDropped { .. })
    ));
    assert!(matches!(
        parse(&format!(
            "{schema} CREATE OR REPLACE TRIGGER rowtrg AFTER INSERT ON p1a
                 FOR EACH ROW EXECUTE FUNCTION f();"
        )),
        Err(Error::ClonedTriggerReplaced { trigger_name, table_name })
            if trigger_name == "rowtrg" && table_name == "p1a"
    ));
    assert!(matches!(
        parse(&format!(
            "{schema} CREATE TRIGGER rowtrg AFTER INSERT ON p1a FOR EACH ROW EXECUTE FUNCTION f();"
        )),
        Err(Error::TriggerAlreadyExists { trigger_name, relation_name })
            if trigger_name == "rowtrg" && relation_name == "p1a"
    ));
}

#[test]
fn dropping_a_trigger_drops_its_copies() {
    let database = db("CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER own AFTER INSERT ON p1a FOR EACH ROW EXECUTE FUNCTION f();
         DROP TRIGGER rowtrg ON p;");

    assert_eq!(triggers_on(&database, "p"), owned(&[]));
    assert_eq!(triggers_on(&database, "p1"), owned(&[]));
    assert_eq!(triggers_on(&database, "p1a"), owned(&[("own", None)]));

    let unnamed_table = db("CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW
             EXECUTE FUNCTION f();
         DROP TRIGGER rowtrg;");
    assert_eq!(unnamed_table.triggers().count(), 0);

    let path_moved = db("CREATE SCHEMA s;
         CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         SET search_path = s;
         DROP TRIGGER rowtrg ON public.p;");
    assert_eq!(path_moved.triggers().count(), 0);
}

#[test]
fn a_partition_holding_the_name_refuses_the_copy() {
    assert!(matches!(
        parse(
            "CREATE TRIGGER n AFTER INSERT ON p1a FOR EACH ROW EXECUTE FUNCTION f();
             CREATE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();"
        ),
        Err(Error::TriggerAlreadyExists { trigger_name, relation_name })
            if trigger_name == "n" && relation_name == "p1a"
    ));
    assert!(matches!(
        parse(
            "CREATE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
             CREATE TABLE q (id INT, v INT) PARTITION BY RANGE (id);
             CREATE TABLE q1 PARTITION OF q FOR VALUES FROM (10) TO (15);
             CREATE TRIGGER n AFTER INSERT ON q1 FOR EACH ROW EXECUTE FUNCTION f();
             ALTER TABLE p ATTACH PARTITION q FOR VALUES FROM (10) TO (20);"
        ),
        Err(Error::TriggerAlreadyExists { trigger_name, relation_name })
            if trigger_name == "n" && relation_name == "q1"
    ));
    assert!(matches!(
        parse(
            "CREATE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
             CREATE TABLE p2 (id INT, v INT);
             CREATE TRIGGER n AFTER INSERT ON p2 FOR EACH ROW EXECUTE FUNCTION f();
             ALTER TABLE p ATTACH PARTITION p2 FOR VALUES FROM (10) TO (20);"
        ),
        Err(Error::TriggerAlreadyExists { trigger_name, relation_name })
            if trigger_name == "n" && relation_name == "p2"
    ));
}

#[test]
fn replacing_with_a_row_trigger_takes_over_the_partitions_triggers_of_the_name() {
    let database = db("CREATE TRIGGER n AFTER INSERT ON p1a FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER n AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE OR REPLACE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();");

    assert_eq!(triggers_on(&database, "p1"), owned(&[("n", Some("p"))]));
    assert_eq!(triggers_on(&database, "p1a"), owned(&[("n", Some("p1"))]));
}

#[test]
fn replacing_with_a_statement_trigger_leaves_the_copies() {
    let database = db("CREATE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE OR REPLACE TRIGGER n AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20);");

    assert_eq!(triggers_on(&database, "p1"), owned(&[("n", Some("p"))]));
    assert_eq!(triggers_on(&database, "p1a"), owned(&[("n", Some("p1"))]));
    assert_eq!(triggers_on(&database, "p2"), owned(&[]));
    assert!(matches!(
        parse(
            "CREATE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
             CREATE OR REPLACE TRIGGER n AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();
             DROP TRIGGER n ON p1;"
        ),
        Err(Error::ClonedTriggerDropped { .. })
    ));
    let dropped = db("CREATE TRIGGER n AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE OR REPLACE TRIGGER n AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20);
         CREATE TRIGGER n AFTER INSERT ON p2 FOR EACH ROW EXECUTE FUNCTION f();
         DROP TRIGGER n ON p;");
    assert_eq!(triggers_on(&dropped, "p1a"), owned(&[]));
    assert_eq!(triggers_on(&dropped, "p2"), owned(&[("n", None)]));
}

#[test]
fn detaching_drops_the_copies_of_the_former_parents_triggers() {
    let database = db("CREATE TRIGGER y AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER x AFTER INSERT ON p1 FOR EACH ROW EXECUTE FUNCTION f();
         ALTER TABLE p DETACH PARTITION p1;");

    assert_eq!(triggers_on(&database, "p"), owned(&[("y", None)]));
    assert_eq!(triggers_on(&database, "p1"), owned(&[("x", None)]));
    assert_eq!(triggers_on(&database, "p1a"), owned(&[("x", Some("p1"))]));
}

#[test]
fn a_foreign_partition_takes_row_triggers_but_refuses_constraint_triggers() {
    let foreign = "CREATE FOREIGN DATA WRAPPER dummy;
         CREATE SERVER s FOREIGN DATA WRAPPER dummy;
         CREATE TABLE q (id INT) PARTITION BY LIST (id);";
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} {foreign}
         CREATE TRIGGER qa AFTER INSERT ON q FOR EACH ROW EXECUTE FUNCTION f();
         CREATE FOREIGN TABLE q1 PARTITION OF q FOR VALUES IN (1) SERVER s;"
    ))
    .expect("schema parses");
    assert_eq!(triggers_on(&database, "q1"), owned(&[("qa", Some("q"))]));

    for refused in [
        "CREATE FOREIGN TABLE ft (id INT) SERVER s;
         CREATE CONSTRAINT TRIGGER qc AFTER INSERT ON ft FOR EACH ROW EXECUTE FUNCTION f();",
        "CREATE CONSTRAINT TRIGGER qc AFTER INSERT ON q FOR EACH ROW EXECUTE FUNCTION f();
         CREATE FOREIGN TABLE q1 PARTITION OF q FOR VALUES IN (1) SERVER s;",
        "CREATE CONSTRAINT TRIGGER qc AFTER INSERT ON q FOR EACH ROW EXECUTE FUNCTION f();
         CREATE FOREIGN TABLE q2 (id INT) SERVER s;
         ALTER TABLE q ATTACH PARTITION q2 FOR VALUES IN (2);",
    ] {
        assert!(
            matches!(
                ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} {foreign} {refused}")),
                Err(Error::ConstraintTriggerOnForeignTable { trigger_name, .. })
                    if trigger_name == "qc"
            ),
            "{refused} was not refused"
        );
    }
}

#[test]
fn copies_follow_their_tables_and_columns() {
    let database = db("CREATE TRIGGER btrg BEFORE UPDATE OF v ON p FOR EACH ROW
             EXECUTE FUNCTION f();
         ALTER TABLE p1a RENAME TO p1x;
         ALTER TABLE p RENAME COLUMN v TO w;");

    assert_eq!(triggers_on(&database, "p1x"), owned(&[("btrg", Some("p1"))]));
    let copy = database.trigger("btrg", table(&database, None, "p1x")).expect("copy exists");
    assert_eq!(copy.events(), &[TriggerEvent::Update(vec![Ident::new("w")])]);

    let dropped = db("CREATE TRIGGER btrg BEFORE UPDATE OF v ON p FOR EACH ROW
             EXECUTE FUNCTION f();
         ALTER TABLE p DROP COLUMN v CASCADE;");
    assert_eq!(dropped.triggers().count(), 0);

    for (schema, table_name) in [
        ("CREATE TRIGGER btrg BEFORE UPDATE OF v ON p1 FOR EACH ROW EXECUTE FUNCTION f();", "p"),
        (
            "CREATE TABLE ip (id INT, v INT);
             CREATE TABLE ic () INHERITS (ip);
             CREATE TRIGGER it BEFORE UPDATE ON ic FOR EACH ROW WHEN (NEW.v > 0)
                 EXECUTE FUNCTION f();",
            "ip",
        ),
    ] {
        assert!(
            matches!(
                parse(&format!("{schema} ALTER TABLE {table_name} DROP COLUMN v;")),
                Err(Error::ColumnReferenced { column_name, .. }) if column_name == "v"
            ),
            "{schema} let {table_name} drop v"
        );
        let cascaded = db(&format!("{schema} ALTER TABLE {table_name} DROP COLUMN v CASCADE;"));
        assert_eq!(cascaded.triggers().count(), 0, "{schema}");
    }

    let partition_dropped = db("CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW
             EXECUTE FUNCTION f();
         DROP TABLE p1;");
    assert_eq!(triggers_on(&partition_dropped, "p"), owned(&[("rowtrg", None)]));
    assert_eq!(partition_dropped.triggers().count(), 1);
}
