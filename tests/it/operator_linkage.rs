//! Exact operator implementation links across array identity boundaries.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared catalog entities.")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

/// A `pg_catalog` identity written quoted, so its case stays exact.
fn quoted(name: &str) -> TargetName<'_> {
    TargetName::new(name, true).with_schema("pg_catalog", false)
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
}

/// Returns the implementing function's input types when its signature matches.
fn spelled(element: &str, sql: &str) -> Option<Vec<String>> {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(app(element)), app(element))
            .with_schema("app", false)
            .with_implementation(app("equal"), pg("bool")),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(sql)
        .unwrap();
    let function = db.operators().unwrap().next().unwrap().implementation(&db)?;
    Some(function.argument_type_names(&db).map(std::borrow::Cow::into_owned).collect())
}

#[test]
fn an_unknown_catalog_type_does_not_prove_an_array() {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(pg("_missing")), pg("_missing"))
            .with_implementation(app("equal"), pg("bool")),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE FUNCTION app.equal(a pg_catalog.missing[], b pg_catalog.missing[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
        )
        .unwrap();
    assert!(db.operators().unwrap().next().unwrap().implementation(&db).is_none());
}

#[test]
fn a_quoted_uppercase_underscore_identity_links_no_function() {
    let db = ParseOptions::default()
        .with_postgres_catalog(
            PostgresCatalog::empty().with_operator(
                PostgresCatalogOperator::new("-", Some(pg("jsonb")), quoted("_TEXT"))
                    .with_implementation(pg("jsonb_delete"), pg("jsonb")),
            ),
        )
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    assert!(db.operators().unwrap().next().unwrap().implementation(&db).is_none());
}

#[test]
fn a_held_type_shadows_the_array_of_its_base() {
    assert_eq!(
        spelled("_token", "CREATE SCHEMA app;
             CREATE TYPE app._token AS ENUM ('b');
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';"),
        None
    );
}

#[test]
fn the_model_proves_a_user_array() {
    assert_eq!(
        spelled("_token", "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';"),
        Some(vec!["app.token[]".into(), "app.token[]".into()])
    );
}

#[test]
fn an_unproven_array_spells_its_stored_name() {
    assert_eq!(
        spelled("_foo", "CREATE SCHEMA app;
             CREATE FUNCTION app.equal(a app._foo, b app._foo) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
             CREATE FUNCTION app.equal(a app.foo[], b app.foo[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';"),
        Some(vec!["app._foo".into(), "app._foo".into()])
    );
}

#[test]
fn a_proven_user_array_follows_a_function_rename() {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(app("_token")), app("_token"))
            .with_schema("app", false)
            .with_implementation(app("equal"), pg("bool")),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
             ALTER FUNCTION app.equal(a app.token[], b app.token[]) RENAME TO equivalent;",
        )
        .unwrap();
    let function = db.operators().unwrap().next().unwrap().implementation(&db).unwrap();
    assert_eq!(function.target_name(), app("equivalent"));
}

#[test]
fn a_proven_user_array_goes_with_a_cascading_function_drop() {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(app("_token")), app("_token"))
            .with_schema("app", false)
            .with_implementation(app("equal"), pg("bool")),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
             DROP FUNCTION app.equal(a app.token[], b app.token[]) CASCADE;",
        )
        .unwrap();
    assert!(db.operators().unwrap().next().is_none());
}

#[test]
fn a_shell_does_not_prove_an_array() {
    assert_eq!(
        spelled("_token", "CREATE SCHEMA app;
             CREATE TYPE app.token;
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';"),
        None
    );
}

#[test]
fn arrays_of_domains_and_relations_link_to_their_element_signatures() {
    for declaration in [
        "CREATE DOMAIN app.token AS INT",
        "CREATE TABLE app.token (a INT)",
        "CREATE VIEW app.token AS SELECT 1 AS a",
        "CREATE MATERIALIZED VIEW app.token AS SELECT 1 AS a",
    ] {
        assert_eq!(
            spelled("_token", &format!(
                "CREATE SCHEMA app; {declaration};
                 CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';"
            )),
            Some(vec!["app.token[]".into(), "app.token[]".into()]),
            "{declaration}"
        );
    }
}

#[test]
fn a_displaced_array_keeps_its_implementation_link() {
    assert_eq!(
        spelled("_token", "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
             CREATE TYPE app._token AS ENUM ('b');"),
        Some(vec!["app.token[]".into(), "app.token[]".into()])
    );
}

#[test]
fn a_numeric_array_suffix_proves_the_stored_owner() {
    assert_eq!(
        spelled("_token_1", "CREATE SCHEMA app;
             CREATE TYPE app._token AS ENUM ('b');
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app.token[], b app.token[]) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';"),
        Some(vec!["app.token[]".into(), "app.token[]".into()])
    );
}

#[test]
fn an_operator_over_a_default_schema_type_links_its_function() {
    let public = |name| TargetName::new(name, false).with_schema("public", false);
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(public("token")), public("token"))
            .with_schema("public", false)
            .with_implementation(public("equal"), pg("bool")),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE TYPE token AS ENUM ('a');
             CREATE FUNCTION equal(a token, b token) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
        )
        .unwrap();
    let function = db.operators().unwrap().next().unwrap().implementation(&db).unwrap();
    assert_eq!(function.name(), "equal");
}

#[test]
fn a_function_over_a_type_no_catalog_spells_links_no_operator() {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4")).with_implementation(
            TargetName::new("equal", false).with_schema("public", false),
            pg("bool"),
        ),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE FUNCTION equal(a DATETIME, b DATETIME) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
        )
        .unwrap();
    assert!(db.operators().unwrap().next().unwrap().implementation(&db).is_none());
}
