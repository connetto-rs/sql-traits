//! Tests that a stored reference stays on the relation it named when created.
//!
//! PostgreSQL binds a foreign key, a trigger, a policy, a grant, an index and a
//! view definition to the object the name reached at creation, so a later `SET
//! search_path` changes nothing about what they refer to. Every rule asserted
//! here was measured against PostgreSQL 18.4 in Docker.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{CreateTable, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the schema builds")
}

fn table<'db>(db: &'db ParserDB, schema: Option<&str>, name: &str) -> &'db CreateTable {
    let target = TargetName::new(name, false);
    let target = match schema {
        Some(schema) => target.with_schema(schema, false),
        None => target,
    };
    db.table_by_target(target, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table is stored")
}

fn row_source<'db>(db: &'db ParserDB, sql: &str) -> &'db CreateTable {
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("parses");
    let Some(Statement::Query(query)) = statements.pop() else {
        panic!("expected a query");
    };
    query
        .projection_source_table(db)
        .expect("the row-identity question answers")
        .expect("the rows come from one table")
}

const FUNCTION: &str =
    "CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;";

#[test]
fn a_foreign_key_keeps_its_target_across_a_path_change() {
    let db = db("CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE p (id INT PRIMARY KEY);
         CREATE TABLE c (pid INT REFERENCES p (id));
         SET search_path TO public;
         CREATE TABLE p (id INT PRIMARY KEY, other INT);");
    let child = table(&db, Some("app"), "c");
    let key = child.foreign_keys(&db).expect("in this database").next().expect("one key");
    assert_eq!(key.referenced_table(&db).expect("resolves").table_schema(), Some("app"));
}

#[test]
fn every_reference_keeps_its_table_across_a_path_change() {
    let db = db(&format!(
        "CREATE SCHEMA app;
         {FUNCTION}
         CREATE ROLE reader;
         SET search_path TO app;
         CREATE TABLE t (id INT PRIMARY KEY);
         CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION public.touch();
         CREATE POLICY pol ON t USING (true);
         GRANT SELECT ON t TO reader;
         CREATE INDEX i ON t (id);
         CREATE VIEW v AS SELECT id FROM t;
         SET search_path TO public;
         CREATE TABLE t (id INT PRIMARY KEY, note TEXT);"
    ));

    let trigger = db.triggers().next().expect("the trigger exists");
    assert_eq!(trigger.table(&db).expect("resolves").table_schema(), Some("app"));

    let policy = db.policies().next().expect("the policy exists");
    assert_eq!(policy.table(&db).expect("resolves").table_schema(), Some("app"));

    let grant = db.table_grants().next().expect("the grant exists");
    let granted: Vec<_> = grant.tables(&db).collect();
    assert!(matches!(granted[..], [table] if table.table_schema() == Some("app")));

    let index = db.indexes().next().expect("the index exists");
    assert_eq!(IndexLike::table(index, &db).table_schema(), Some("app"));

    assert_eq!(row_source(&db, "SELECT id FROM app.v").table_schema(), Some("app"));
}

#[test]
fn a_view_definition_is_stored_bound() {
    let db = db("CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;");
    let stored = db
        .view_by_target(
            TargetName::new("v", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the view is stored");
    assert_eq!(stored.definition().to_string(), "SELECT id FROM app.t");
}

#[test]
fn a_view_depends_on_the_table_it_bound_rather_than_one_beside_it() {
    let sql = "CREATE SCHEMA app;
         SET search_path TO app, public;
         CREATE TABLE public.t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         CREATE TABLE app.t (id INT);";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&format!("{sql} DROP TABLE public.t;")),
        Err(Error::RelationHasDependents { .. })
    ));
    db(&format!("{sql} DROP TABLE app.t;"));
}

#[test]
fn a_dropped_trigger_is_the_one_the_name_reaches() {
    let db = db(&format!(
        "CREATE SCHEMA app;
         {FUNCTION}
         CREATE TABLE t (id INT);
         CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION touch();
         SET search_path TO app, public;
         CREATE TABLE t (id INT);
         CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION touch();
         DROP TRIGGER trg ON t;"
    ));
    let remaining: Vec<_> = db.triggers().collect();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].table(&db).expect("resolves").table_schema(), None);
}

#[test]
fn a_dropped_policy_is_the_one_the_name_reaches() {
    let db = db("CREATE SCHEMA app;
         CREATE TABLE t (id INT);
         CREATE POLICY pol ON t USING (true);
         SET search_path TO app, public;
         CREATE TABLE t (id INT);
         CREATE POLICY pol ON t USING (true);
         DROP POLICY pol ON t;");
    let remaining: Vec<_> = db.policies().collect();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].table(&db).expect("resolves").table_schema(), None);
}

#[test]
fn a_with_body_reads_the_outer_relation_its_own_name_shadows() {
    let db = db("CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS WITH t AS (SELECT id FROM t) SELECT id FROM t;");
    let stored = db
        .view_by_target(
            TargetName::new("v", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the view is stored");
    assert_eq!(
        stored.definition().to_string(),
        "WITH t AS (SELECT id FROM app.t) SELECT id FROM t"
    );
}

#[test]
fn a_recursive_with_body_reads_its_own_name() {
    let db = db("CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE t (id INT);
         CREATE VIEW v AS WITH RECURSIVE t (id) AS (SELECT 1 UNION ALL SELECT id + 1 FROM t WHERE id < 3) SELECT id FROM t;");
    let stored = db
        .view_by_target(
            TargetName::new("v", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the view is stored");
    assert!(!stored.definition().to_string().contains("app.t"));
}
