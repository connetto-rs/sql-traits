//! Tests for grants reaching exactly the objects they reached when they ran.
//!
//! PostgreSQL keeps no grant on a schema's contents or on a list of names. It
//! writes the privilege into each object the statement reaches when it runs,
//! and a `REVOKE` takes it from each object it reaches then, whatever form
//! granted it there. The expectations below were read from PostgreSQL 17 and
//! 18.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// A role `reader`, a schema `s` holding tables `a` and `b`, then `sql`.
fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!(
        "CREATE ROLE reader; CREATE SCHEMA s;
         CREATE TABLE s.a (id INT); CREATE TABLE s.b (id INT);
         {sql}"
    ))
}

fn can_select(db: &ParserDB, schema: &str, table: &str) -> bool {
    db.table_by_target(
        TargetName::new(table, false).with_schema(schema, false),
        IdentifierCase::AsWritten,
    )
    .expect("unambiguous lookup")
    .expect("the table is declared")
    .can_select(db.role("reader").expect("the role is declared"), db)
    .expect("the table is held")
}

/// The relations whose grants give `reader` something, each spelled
/// `schema.name`.
fn reached(db: &ParserDB) -> Vec<String> {
    let reader = db.role("reader").expect("the role is declared");
    let mut names: Vec<String> = db
        .table_grants()
        .filter(|grant| grant.applies_to_role(reader))
        .flat_map(|grant| grant.relations(db).collect::<Vec<_>>())
        .map(|relation| {
            let (schema, name) = match relation {
                Relation::Table(table) | Relation::ForeignTable(table) => {
                    (table.table_schema(), table.table_name())
                }
                Relation::View(view) => (view.view_schema(), view.view_name()),
                Relation::MaterializedView(view) => (view.view_schema(), view.view_name()),
                Relation::Sequence(sequence) => {
                    (sequence.sequence_schema(), sequence.sequence_name())
                }
            };
            format!("{}.{name}", schema.unwrap_or("public"))
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_schema_wide_grant_reaches_only_what_the_schema_holds_when_it_runs() {
    let db = parse(
        "CREATE VIEW s.v AS SELECT 1 AS x;
         CREATE MATERIALIZED VIEW s.mv AS SELECT 1 AS x;
         CREATE SEQUENCE s.q;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO reader;
         GRANT USAGE ON ALL SEQUENCES IN SCHEMA s TO reader;
         CREATE TABLE s.later (id SERIAL);
         CREATE VIEW s.later_v AS SELECT 1 AS x;
         CREATE SEQUENCE s.later_q;",
    )
    .expect("schema parses");

    assert_eq!(reached(&db), ["s.a", "s.b", "s.mv", "s.q", "s.v"]);
    assert!(!can_select(&db, "s", "later"));
}

#[test]
fn a_schema_wide_grant_on_an_empty_schema_records_nothing() {
    let db = parse("CREATE SCHEMA empty; GRANT SELECT ON ALL TABLES IN SCHEMA empty TO reader;")
        .expect("schema parses");

    assert_eq!(db.table_grants().count(), 0);
}

#[test]
fn a_revoke_reaches_each_object_whatever_form_granted_it() {
    for (sql, a, b) in [
        ("GRANT SELECT ON s.a, s.b TO reader; REVOKE SELECT ON s.a FROM reader;", false, true),
        (
            "GRANT SELECT ON s.a TO reader; REVOKE SELECT ON ALL TABLES IN SCHEMA s FROM reader;",
            false,
            false,
        ),
        (
            "GRANT SELECT ON ALL TABLES IN SCHEMA s TO reader; REVOKE SELECT ON s.b FROM reader;",
            true,
            false,
        ),
        (
            "GRANT SELECT ON ALL TABLES IN SCHEMA s TO reader;
             REVOKE SELECT ON ALL TABLES IN SCHEMA s FROM reader;",
            false,
            false,
        ),
    ] {
        let db = parse(sql).expect(sql);

        assert_eq!(can_select(&db, "s", "a"), a, "{sql}");
        assert_eq!(can_select(&db, "s", "b"), b, "{sql}");
    }
}

#[test]
fn a_dropped_and_recreated_table_carries_no_grant() {
    let db = parse(
        "GRANT SELECT ON ALL TABLES IN SCHEMA s TO reader;
         DROP TABLE s.a;
         CREATE TABLE s.a (id INT);",
    )
    .expect("schema parses");

    assert!(!can_select(&db, "s", "a"));
    assert!(can_select(&db, "s", "b"));
}

#[test]
fn a_grant_follows_its_renamed_table() {
    let db = parse(
        "GRANT SELECT ON ALL TABLES IN SCHEMA s TO reader;
         ALTER TABLE s.a RENAME TO renamed;",
    )
    .expect("schema parses");

    assert!(can_select(&db, "s", "renamed"));
    assert!(can_select(&db, "s", "b"));
}

/// A column list on a schema-wide grant has to name a column of every
/// relation the schema holds.
#[test]
fn a_schema_wide_column_grant_checks_every_relation() {
    let refused = parse(
        "CREATE TABLE s.c (other INT); GRANT SELECT (id) ON ALL TABLES IN SCHEMA s TO reader;",
    )
    .expect_err("s.c has no column id");
    assert!(matches!(refused, Error::ColumnNotFoundForGrant { .. }), "{refused:?}");

    parse("GRANT SELECT (id) ON ALL TABLES IN SCHEMA s TO reader;").expect("every table holds id");
}

#[test]
fn a_revoke_takes_one_schema_of_a_list() {
    let db = parse(
        "CREATE SCHEMA t;
         GRANT USAGE ON SCHEMA s, t TO reader;
         REVOKE USAGE ON SCHEMA s FROM reader;",
    )
    .expect("schema parses");
    let reader = db.role("reader").expect("the role is declared");

    let schemas: Vec<String> = db
        .table_grants()
        .filter(|grant| grant.applies_to_role(reader))
        .flat_map(|grant| {
            match grant.target() {
                sql_traits::traits::grant::GrantTarget::Schemas(names) => {
                    names.map(|name| name.name().to_string()).collect()
                }
                _ => Vec::new(),
            }
        })
        .collect();
    assert_eq!(schemas, ["t"]);
}

/// `ALL PRIVILEGES` means a different set on a table and on a sequence, and a
/// revoke takes its subset from each one's own set.
#[test]
fn a_subset_revoke_takes_from_each_kind_of_all_privileges() {
    let db = parse(
        "CREATE SEQUENCE s.q;
         GRANT ALL PRIVILEGES ON s.a, s.q TO reader;
         REVOKE UPDATE ON s.a, s.q FROM reader;",
    )
    .expect("schema parses");

    let held: Vec<Vec<String>> = db
        .table_grants()
        .map(|grant| grant.privileges(&db).map(ToString::to_string).collect())
        .collect();
    assert_eq!(held.len(), 2);
    for privileges in &held {
        assert!(privileges.iter().any(|privilege| privilege == "SELECT"), "{privileges:?}");
        assert!(!privileges.iter().any(|privilege| privilege == "UPDATE"), "{privileges:?}");
    }
}

/// A view's grants go with it when it is dropped, a cascade included, and
/// stay on it when it is renamed.
#[test]
fn view_grants_follow_the_view() {
    let db = parse(
        "CREATE VIEW s.v AS SELECT 1 AS x;
         CREATE MATERIALIZED VIEW s.m AS SELECT 1 AS x;
         CREATE VIEW s.r AS SELECT 1 AS x;
         CREATE VIEW s.over_a AS SELECT id FROM s.a;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO reader;
         GRANT USAGE ON SCHEMA s TO reader;
         DROP VIEW s.v;
         CREATE VIEW s.v AS SELECT 1 AS x;
         DROP MATERIALIZED VIEW s.m;
         CREATE MATERIALIZED VIEW s.m AS SELECT 1 AS x;
         ALTER VIEW s.r RENAME TO renamed;
         DROP TABLE s.a CASCADE;
         CREATE VIEW s.over_a AS SELECT 1 AS x;",
    )
    .expect("schema parses");

    assert_eq!(reached(&db), ["s.b", "s.renamed"]);
}

/// An object written twice in one statement, under any spelling that reaches
/// it, holds one privilege list entry, as PostgreSQL writes the grant into it
/// once.
#[test]
fn an_object_written_twice_is_granted_once() {
    let db = parse(
        "SET search_path = s;
         GRANT SELECT ON a, A, s.a TO reader;
         GRANT USAGE ON SCHEMA s, S TO reader;",
    )
    .expect("schema parses");

    let granted: Vec<String> = db.table_grants().map(ToString::to_string).collect();
    assert_eq!(granted, ["GRANT SELECT ON s.a TO reader", "GRANT USAGE ON SCHEMA s TO reader"]);
}
