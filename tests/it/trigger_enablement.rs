//! Tests that `ALTER TABLE ... { ENABLE | DISABLE } [ REPLICA | ALWAYS ]
//! TRIGGER` sets which sessions a trigger fires in, as PostgreSQL records it
//! in `pg_trigger.tgenabled`, on the copies of a partitioned table's row
//! triggers too.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 18.6.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const SCHEMA: &str =
    "CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN RETURN NEW; END' LANGUAGE plpgsql;
     CREATE TABLE t (id INT);
     CREATE TRIGGER audit AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
     CREATE TRIGGER other AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
     CREATE TABLE p (id INT) PARTITION BY RANGE (id);
     CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
     CREATE TABLE p1a PARTITION OF p1 FOR VALUES FROM (0) TO (5);
     CREATE TRIGGER rowtrg AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f();
     CREATE TRIGGER stmttrg AFTER INSERT ON p FOR EACH STATEMENT EXECUTE FUNCTION f();";

fn parse(statements: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} {statements}"))
}

fn db(statements: &str) -> ParserDB {
    parse(statements).unwrap_or_else(|error| panic!("{statements} was refused: {error:?}"))
}

/// The enablement of the trigger `trigger_name` on the table `table_name`.
fn enablement(database: &ParserDB, table_name: &str, trigger_name: &str) -> TriggerEnablement {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    database
        .trigger(trigger_name, table)
        .unwrap_or_else(|| panic!("{table_name} carries {trigger_name}"))
        .enablement(database)
        .expect("trigger is in this database")
}

#[test]
fn each_alteration_sets_the_state_it_names_in_statement_order() {
    use TriggerEnablement::{Always, Disabled, Origin, Replica};
    let mut statements = String::new();
    assert_eq!(enablement(&db(""), "t", "audit"), Origin);
    for (alteration, expected) in [
        ("ALTER TABLE t DISABLE TRIGGER audit;", Disabled),
        ("ALTER TABLE t ENABLE TRIGGER audit;", Origin),
        ("ALTER TABLE t ENABLE REPLICA TRIGGER audit;", Replica),
        ("ALTER TABLE t ENABLE ALWAYS TRIGGER audit;", Always),
        ("ALTER TABLE ONLY public.t DISABLE TRIGGER audit;", Disabled),
        ("SET session_replication_role = replica;", Disabled),
    ] {
        statements.push_str(alteration);
        let database = db(&statements);
        assert_eq!(enablement(&database, "t", "audit"), expected, "{statements}");
        assert_eq!(enablement(&database, "t", "other"), Origin, "{statements}");
    }
}

#[test]
fn all_and_user_reach_every_trigger_of_the_table() {
    use TriggerEnablement::{Disabled, Origin};
    for (alteration, expected) in [
        ("ALTER TABLE t DISABLE TRIGGER ALL;", Disabled),
        ("ALTER TABLE t DISABLE TRIGGER all;", Disabled),
        ("ALTER TABLE t DISABLE TRIGGER USER;", Disabled),
        ("ALTER TABLE t DISABLE TRIGGER ALL; ALTER TABLE t ENABLE TRIGGER ALL;", Origin),
        ("ALTER TABLE t DISABLE TRIGGER ALL; ALTER TABLE t ENABLE TRIGGER USER;", Origin),
    ] {
        let database = db(alteration);
        assert_eq!(enablement(&database, "t", "audit"), expected, "{alteration}");
        assert_eq!(enablement(&database, "t", "other"), expected, "{alteration}");
        assert_eq!(enablement(&database, "p", "rowtrg"), Origin, "{alteration}");
    }
    db("CREATE TABLE empty (id INT); ALTER TABLE empty DISABLE TRIGGER ALL;");
}

#[test]
fn a_quoted_name_is_a_trigger_name_and_follows_the_identifier_rules() {
    let database = db("CREATE TRIGGER \"ALL\" AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER \"Mixed\" AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
         ALTER TABLE t DISABLE TRIGGER \"ALL\";
         ALTER TABLE public.t ENABLE REPLICA TRIGGER \"Mixed\";");
    assert_eq!(enablement(&database, "t", "ALL"), TriggerEnablement::Disabled);
    assert_eq!(enablement(&database, "t", "Mixed"), TriggerEnablement::Replica);
    assert_eq!(enablement(&database, "t", "audit"), TriggerEnablement::Origin);

    for (alteration, missing) in [
        ("ALTER TABLE t DISABLE TRIGGER nope;", "nope"),
        (
            "CREATE TRIGGER \"Mixed\" AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
             ALTER TABLE t DISABLE TRIGGER mixed;",
            "mixed",
        ),
        ("ALTER TABLE p1 DISABLE TRIGGER audit;", "audit"),
    ] {
        assert!(
            matches!(
                parse(alteration),
                Err(Error::AlterTableTriggerNotFound { trigger_name, .. }) if trigger_name == missing
            ),
            "{alteration} was not refused"
        );
    }
    db("ALTER TABLE IF EXISTS absent DISABLE TRIGGER audit;");
}

#[test]
fn an_alteration_without_only_reaches_the_copies_below() {
    use TriggerEnablement::{Always, Disabled, Origin, Replica};
    let database = db("ALTER TABLE p1 DISABLE TRIGGER rowtrg;");
    assert_eq!(enablement(&database, "p", "rowtrg"), Origin);
    assert_eq!(enablement(&database, "p1", "rowtrg"), Disabled);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Disabled);

    let database = db("ALTER TABLE p1 DISABLE TRIGGER rowtrg;
         ALTER TABLE ONLY p1 ENABLE REPLICA TRIGGER rowtrg;");
    assert_eq!(enablement(&database, "p1", "rowtrg"), Replica);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Disabled);

    let database = db("ALTER TABLE ONLY p DISABLE TRIGGER rowtrg;
         ALTER TABLE p1a ENABLE ALWAYS TRIGGER rowtrg;");
    assert_eq!(enablement(&database, "p", "rowtrg"), Disabled);
    assert_eq!(enablement(&database, "p1", "rowtrg"), Origin);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Always);

    let database = db("CREATE TRIGGER own AFTER INSERT ON p1a FOR EACH ROW EXECUTE FUNCTION f();
         ALTER TABLE p DISABLE TRIGGER ALL;");
    assert_eq!(enablement(&database, "p", "stmttrg"), Disabled);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Disabled);
    assert_eq!(enablement(&database, "p1a", "own"), Origin);

    let database = db("ALTER TABLE ONLY p DISABLE TRIGGER ALL;");
    assert_eq!(enablement(&database, "p", "rowtrg"), Disabled);
    assert_eq!(enablement(&database, "p1", "rowtrg"), Origin);

    let database = db("CREATE TABLE ip (id INT);
         CREATE TABLE ic () INHERITS (ip);
         CREATE TRIGGER it AFTER INSERT ON ip FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER it AFTER INSERT ON ic FOR EACH ROW EXECUTE FUNCTION f();
         ALTER TABLE ip DISABLE TRIGGER it;");
    assert_eq!(enablement(&database, "ip", "it"), Disabled);
    assert_eq!(enablement(&database, "ic", "it"), Origin);
}

#[test]
fn a_new_copy_starts_from_its_parents_current_state() {
    use TriggerEnablement::{Disabled, Replica};
    let database = db("ALTER TABLE p ENABLE REPLICA TRIGGER rowtrg;
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20);");
    assert_eq!(enablement(&database, "p2", "rowtrg"), Replica);

    let database = db("ALTER TABLE p1 DISABLE TRIGGER rowtrg;
         ALTER TABLE p DETACH PARTITION p1;
         ALTER TABLE p ENABLE REPLICA TRIGGER rowtrg;
         ALTER TABLE p ATTACH PARTITION p1 FOR VALUES FROM (0) TO (10);");
    assert_eq!(enablement(&database, "p1", "rowtrg"), Replica);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Replica);

    let database = db("ALTER TABLE p1a DISABLE TRIGGER rowtrg;
         ALTER TABLE p1a RENAME TO p1x;");
    assert_eq!(enablement(&database, "p1x", "rowtrg"), Disabled);
}

#[test]
fn replacing_a_trigger_starts_it_and_its_new_copies_over() {
    use TriggerEnablement::{Always, Disabled, Origin};
    let database = db("ALTER TABLE p DISABLE TRIGGER rowtrg;
         CREATE OR REPLACE TRIGGER rowtrg AFTER INSERT OR UPDATE ON p
             FOR EACH ROW EXECUTE FUNCTION f();");
    assert_eq!(enablement(&database, "p", "rowtrg"), Origin);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Origin);

    let database = db("ALTER TABLE p DISABLE TRIGGER rowtrg;
         CREATE OR REPLACE TRIGGER rowtrg AFTER INSERT ON p
             FOR EACH STATEMENT EXECUTE FUNCTION f();
         ALTER TABLE p ENABLE ALWAYS TRIGGER rowtrg;");
    assert_eq!(enablement(&database, "p", "rowtrg"), Always);
    assert_eq!(enablement(&database, "p1", "rowtrg"), Disabled);
    assert_eq!(enablement(&database, "p1a", "rowtrg"), Disabled);
}

#[test]
fn a_trigger_answers_only_for_the_database_holding_it() {
    let database = db("");
    let other =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT);").expect("the table parses");
    let trigger = database.triggers().next().expect("a trigger exists");
    assert!(matches!(
        trigger.enablement(&other),
        Err(sql_traits::errors::LookupError::ObjectNotInDatabase { .. })
    ));
}

#[test]
fn a_pg_dump_schema_ingests_with_each_triggers_state() {
    use TriggerEnablement::{Always, Disabled, Origin, Replica};
    let dump = "CREATE FUNCTION public.fired() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RETURN NEW; END $$;
         CREATE TABLE public.t (id integer);
         CREATE TABLE public.p (id integer) PARTITION BY RANGE (id);
         CREATE TABLE public.p1 (id integer);
         ALTER TABLE ONLY public.p ATTACH PARTITION public.p1 FOR VALUES FROM (0) TO (10);
         CREATE TRIGGER audit AFTER INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.fired();
         ALTER TABLE public.t ENABLE ALWAYS TRIGGER audit;
         CREATE TRIGGER disabled AFTER INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.fired();
         ALTER TABLE public.t DISABLE TRIGGER disabled;
         CREATE TRIGGER other AFTER INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.fired();
         CREATE TRIGGER replica AFTER INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.fired();
         ALTER TABLE public.t ENABLE REPLICA TRIGGER replica;
         CREATE TRIGGER rowtrg AFTER INSERT ON public.p FOR EACH ROW EXECUTE FUNCTION public.fired();
         ALTER TABLE public.p DISABLE TRIGGER rowtrg;
         ALTER TABLE public.p1 ENABLE ALWAYS TRIGGER rowtrg;
         ALTER TABLE public.t ENABLE ROW LEVEL SECURITY;";
    let database = ParserDB::parse::<PostgreSqlDialect>(dump).expect("the dump parses");

    assert_eq!(enablement(&database, "t", "audit"), Always);
    assert_eq!(enablement(&database, "t", "disabled"), Disabled);
    assert_eq!(enablement(&database, "t", "other"), Origin);
    assert_eq!(enablement(&database, "t", "replica"), Replica);
    assert_eq!(enablement(&database, "p", "rowtrg"), Disabled);
    assert_eq!(enablement(&database, "p1", "rowtrg"), Always);
}
