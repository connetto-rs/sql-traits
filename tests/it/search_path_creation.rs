//! Tests that an object created without a schema lands where PostgreSQL puts
//! it, the first schema on the search path that exists.
//!
//! A schema the path names but nobody created is skipped, `"$user"` names the
//! schema called after the role the statement runs as, and a path selecting
//! nothing refuses the statement as selecting no schema. `pg_catalog` takes
//! functions and domains but refuses relations. The expectations below were
//! read from PostgreSQL 16.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// A statement creating each kind the search path places, with that kind.
const CREATIONS: [(&str, ObjectKind); 6] = [
    ("CREATE TABLE x (a INT);", ObjectKind::Table),
    ("CREATE VIEW x AS SELECT 1 AS a;", ObjectKind::View),
    ("CREATE MATERIALIZED VIEW x AS SELECT 1 AS a;", ObjectKind::MaterializedView),
    ("CREATE SEQUENCE x;", ObjectKind::Sequence),
    ("CREATE DOMAIN x AS INT;", ObjectKind::Domain),
    ("CREATE FUNCTION x() RETURNS INT AS 'SELECT 1';", ObjectKind::Function),
];

#[test]
fn a_path_of_missing_schemas_selects_none() {
    for path in ["nowhere", "nowhere, \"$user\"", "''"] {
        for (statement, kind) in CREATIONS {
            let result = parse(&format!("SET search_path = {path}; {statement}"));
            assert!(
                matches!(
                    &result,
                    Err(Error::NoSchemaSelectedForRelation { object_kind, relation_name })
                        if *object_kind == kind && relation_name == "x"
                ),
                "{path}: {statement} got {result:?}"
            );
        }
    }
}

/// The schema of `x`, read from whichever store holds it.
fn schema_of_x(db: &ParserDB) -> Option<String> {
    let schema = |schema: Option<&str>| schema.unwrap_or("public").to_string();
    db.tables()
        .find(|table| table.table_name() == "x")
        .map(|table| schema(table.table_schema()))
        .or_else(|| {
            db.functions().find(|f| f.name() == "x").map(|f| schema(f.target_name().schema()))
        })
}

#[test]
fn dollar_user_names_the_schema_of_the_role_the_statement_runs_as() {
    let base = "CREATE ROLE alice;
         CREATE SCHEMA alice AUTHORIZATION alice;";
    for (setup, expected) in [
        ("SET search_path = \"$user\", public;", "public"),
        ("SET search_path = \"$user\", public; SET ROLE alice;", "alice"),
        ("SET search_path = \"$user\", public; SET SESSION AUTHORIZATION alice;", "alice"),
        ("SET search_path = '$user', public; SET ROLE alice;", "alice"),
        ("SET search_path = \"$USER\", public; SET ROLE alice;", "public"),
    ] {
        for statement in
            ["CREATE TABLE x (a INT);", "CREATE FUNCTION x() RETURNS INT AS 'SELECT 1';"]
        {
            let result = parse(&format!("{base} {setup} {statement}"));
            assert!(result.is_ok(), "{setup} {statement} got {result:?}");
            let db = result.expect("accepted");
            assert_eq!(schema_of_x(&db).as_deref(), Some(expected), "{setup} {statement}");
        }
    }
}

#[test]
fn pg_catalog_takes_functions_and_domains_but_refuses_relations() {
    for path in ["pg_catalog, public", "nowhere, pg_catalog, public"] {
        for (statement, kind) in CREATIONS {
            let result = parse(&format!("SET search_path = {path}; {statement}"));
            if matches!(kind, ObjectKind::Function | ObjectKind::Domain) {
                assert!(result.is_ok(), "{path}: {statement} got {result:?}");
                let db = result.expect("accepted");
                let placed = db
                    .functions()
                    .find(|function| function.name() == "x")
                    .map(|function| function.target_name().schema().map(str::to_string))
                    .or_else(|| {
                        db.domains()
                            .find(|domain| domain.name() == "x")
                            .map(|domain| domain.schema().map(str::to_string))
                    });
                assert_eq!(placed, Some(Some("pg_catalog".to_string())), "{path}: {statement}");
            } else {
                assert!(
                    matches!(
                        &result,
                        Err(Error::SystemSchemaRelationRefused { object_kind, relation_name })
                            if *object_kind == kind && relation_name == "x"
                    ),
                    "{path}: {statement} got {result:?}"
                );
            }
        }
    }
    let result = parse("CREATE TABLE pg_catalog.x (a INT);");
    assert!(matches!(result, Err(Error::SystemSchemaRelationRefused { .. })), "got {result:?}");
    let db = parse("CREATE DOMAIN pg_catalog.x AS INT;").expect("a domain may go there");
    let domain = db.domains().find(|domain| domain.name() == "x").expect("x is held");
    assert_eq!(domain.schema(), Some("pg_catalog"));
    let db = parse("SET search_path = public, pg_catalog; CREATE TABLE x (a INT);")
        .expect("public comes first");
    assert_eq!(schema_of_x(&db).as_deref(), Some("public"));
}
