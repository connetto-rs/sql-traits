//! Operator catalog facts across the lifecycles of the types, relations and
//! functions they name.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared catalog entities.")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
}

/// Parses `sql` with one configured operator fact, with the named `TargetName`
/// identities as operands and result.
fn configured(left: Option<TargetName<'_>>, right: TargetName<'_>) -> ParseOptions {
    ParseOptions::default().with_postgres_catalog(
        PostgresCatalog::empty().with_operator(
            PostgresCatalogOperator::new("===", left, right)
                .with_schema("app", false)
                .with_implementation(app("equal"), pg("bool")),
        ),
    )
}

fn parse(catalog: ParseOptions, sql: &str) -> ParserDB {
    catalog.parse::<PostgreSqlDialect>(sql).unwrap()
}

/// The one configured fact the database holds, when it holds one.
fn the_operator(db: &ParserDB) -> &PostgresCatalogOperator {
    db.operators().unwrap().next().unwrap()
}

/// Whether the parse refused with the type drop an operator holds.
fn refused_by_operator(result: &Result<ParserDB, Error>) -> bool {
    matches!(result, Err(Error::TypeDropRequiredBy { .. }))
}

#[test]
fn a_restricted_schema_drop_is_refused_by_the_operator_it_holds() {
    let result = configured(None, pg("int4")).parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         DROP SCHEMA app;",
    );
    assert!(matches!(
        result,
        Err(Error::SchemaNotEmpty { schema_name }) if schema_name == "app"
    ));
}

#[test]
fn a_transitive_domain_drop_takes_the_fact_of_its_domain() {
    let db = parse(
        configured(None, app("wrapped")),
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE DOMAIN app.wrapped AS app.token;
         CREATE FUNCTION app.equal(a app.wrapped, b app.wrapped) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
         DROP TYPE app.token CASCADE;",
    );
    assert_eq!(db.operators().unwrap().count(), 0);
}

#[test]
fn a_restricted_drop_is_refused_by_the_type_its_array_and_a_range_multirange() {
    let direct = configured(None, app("token")).parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.token;
         DROP TYPE app.token;",
    );
    assert!(refused_by_operator(&direct));
    let array = configured(None, app("_token")).parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         DROP TYPE app.token;",
    );
    assert!(refused_by_operator(&array));
    let multirange = configured(None, app("token_multirange")).parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.sub AS ENUM ('a');
         CREATE TYPE app.token AS RANGE (subtype = app.sub);
         DROP TYPE app.token;",
    );
    assert!(refused_by_operator(&multirange));
}

#[test]
fn a_cascading_drop_takes_the_facts_of_every_doomed_spelling() {
    let array = parse(
        configured(None, app("_token")),
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         DROP TYPE app.token CASCADE;",
    );
    assert_eq!(array.operators().unwrap().count(), 0);
    let multirange = parse(
        configured(None, app("token_multirange")),
        "CREATE SCHEMA app;
         CREATE TYPE app.sub AS ENUM ('a');
         CREATE TYPE app.token AS RANGE (subtype = app.sub);
         DROP TYPE app.token CASCADE;",
    );
    assert_eq!(multirange.operators().unwrap().count(), 0);
    let domain = parse(
        configured(None, app("wrapped")),
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE DOMAIN app.wrapped AS app.token;
         DROP TYPE app.token CASCADE;",
    );
    assert_eq!(domain.operators().unwrap().count(), 0);
}

#[test]
fn a_fact_about_an_absent_type_survives_an_if_exists_drop() {
    let db = parse(
        configured(None, app("missing")),
        "CREATE SCHEMA app;
         DROP TYPE IF EXISTS app.missing;",
    );
    let operator = the_operator(&db);
    assert_eq!(operator.left_type(&db), None);
    assert_eq!(operator.right_type(&db), Some(app("missing")));
}

#[test]
fn a_restricted_domain_drop_is_refused_and_a_cascading_one_takes_the_fact() {
    let refused = configured(None, app("wrapped")).parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE DOMAIN app.wrapped AS app.token;
         DROP DOMAIN app.wrapped;",
    );
    assert!(refused_by_operator(&refused));
    let dropped = parse(
        configured(None, app("wrapped")),
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE DOMAIN app.wrapped AS app.token;
         DROP DOMAIN app.wrapped CASCADE;",
    );
    assert_eq!(dropped.operators().unwrap().count(), 0);
}

#[test]
fn a_range_rename_follows_its_array_and_keeps_its_multirange_name() {
    let db = parse(
        configured(None, app("_token")),
        "CREATE SCHEMA app;
         CREATE TYPE app.sub AS ENUM ('a');
         CREATE TYPE app.token AS RANGE (subtype = app.sub);
         ALTER TYPE app.token RENAME TO mytype;",
    );
    let operator = the_operator(&db);
    assert_eq!(operator.right_type(&db), Some(app("_mytype")));
    let multirange = parse(
        configured(None, app("token_multirange")),
        "CREATE SCHEMA app;
         CREATE TYPE app.sub AS ENUM ('a');
         CREATE TYPE app.token AS RANGE (subtype = app.sub);
         ALTER TYPE app.token RENAME TO mytype;",
    );
    let operator = the_operator(&multirange);
    assert_eq!(operator.right_type(&multirange), Some(app("token_multirange")));
}

#[test]
fn a_multirange_rename_follows_its_array() {
    let db = parse(
        configured(None, app("_token_multirange")),
        "CREATE SCHEMA app;
         CREATE TYPE app.sub AS ENUM ('a');
         CREATE TYPE app.token AS RANGE (subtype = app.sub);
         ALTER TYPE app.token_multirange RENAME TO newm;",
    );
    let operator = the_operator(&db);
    assert_eq!(operator.right_type(&db), Some(app("_newm")));
}

#[test]
fn a_held_array_spelling_stays_put_when_its_base_goes() {
    let db = parse(
        configured(None, app("_token")),
        "CREATE SCHEMA app;
         CREATE TYPE app._token AS ENUM ('b');
         CREATE TYPE app.token AS ENUM ('a');
         DROP TYPE app.token CASCADE;",
    );
    let operator = the_operator(&db);
    assert_eq!(operator.right_type(&db), Some(app("_token")));
}

#[test]
fn a_table_rename_follows_its_row_type() {
    let db = parse(
        configured(None, app("token")),
        "CREATE SCHEMA app;
         CREATE TABLE app.token (a INT);
         ALTER TABLE app.token RENAME TO renamed;",
    );
    let operator = the_operator(&db);
    assert_eq!(operator.right_type(&db), Some(app("renamed")));
}

#[test]
fn a_view_rename_follows_its_row_type() {
    let db = parse(
        configured(None, app("view")),
        "CREATE SCHEMA app;
         CREATE VIEW app.view AS SELECT 1 AS a;
         ALTER VIEW app.view RENAME TO exposed;",
    );
    let operator = the_operator(&db);
    assert_eq!(operator.right_type(&db), Some(app("exposed")));
}

#[test]
fn a_relation_drop_takes_the_fact_of_its_row_type() {
    let table = parse(
        configured(None, app("token")),
        "CREATE SCHEMA app;
         CREATE TABLE app.token (a INT);
         DROP TABLE app.token CASCADE;",
    );
    assert_eq!(table.operators().unwrap().count(), 0);
    let reader = parse(
        configured(None, TargetName::new("reader", false).with_schema("public", false)),
        "CREATE SCHEMA app;
         CREATE TABLE app.base (a INT);
         CREATE VIEW public.reader AS SELECT * FROM app.base;
         DROP TABLE app.base CASCADE;",
    );
    assert_eq!(reader.operators().unwrap().count(), 0);
}

#[test]
fn a_view_drop_takes_the_fact_of_its_row_type() {
    let db = parse(
        configured(None, app("view")),
        "CREATE SCHEMA app;
         CREATE VIEW app.view AS SELECT 1 AS a;
         DROP VIEW app.view CASCADE;",
    );
    assert_eq!(db.operators().unwrap().count(), 0);
}

#[test]
fn a_schema_cascade_takes_every_fact_it_moves() {
    let db = ParseOptions::default().with_postgres_catalog(
        PostgresCatalog::empty()
            .with_operator(
                PostgresCatalogOperator::new("===", Some(app("token")), app("token"))
                    .with_schema("app", false)
                    .with_implementation(app("equal"), pg("bool")),
            )
            .with_operator(
                PostgresCatalogOperator::new("<->", Some(app("token")), app("token"))
                    .with_schema("other", false)
                    .with_implementation(
                        TargetName::new("compare", false).with_schema("other", false),
                        pg("int8"),
                    ),
            )
            .with_operator(
                PostgresCatalogOperator::new("###", Some(pg("int4")), pg("int4"))
                    .with_schema("other", false)
                    .with_implementation(app("equal"), pg("bool")),
            )
            .with_operator(
                PostgresCatalogOperator::new(
                    "|||",
                    Some(TargetName::new("read", false).with_schema("public", false)),
                    TargetName::new("read", false).with_schema("public", false),
                )
                .with_schema("other", false)
                .with_implementation(pg("readrow"), pg("int4")),
            ),
    );
    let db = parse(
        db,
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE FUNCTION app.equal(a app.token, b app.token) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
         CREATE TABLE app.base (a INT);
         CREATE VIEW public.read AS SELECT * FROM app.base;
         DROP SCHEMA app CASCADE;",
    );
    assert_eq!(db.operators().unwrap().count(), 0);
}

#[test]
fn a_snapshot_keeps_a_fact_a_cascade_takes() {
    let options = configured(None, app("wrapped"));
    let original = options
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE DOMAIN app.wrapped AS app.token;",
        )
        .unwrap();
    let mut input = original.into_ingestor();
    let snapshot = input.snapshot();
    let statement = Parser::parse_sql(&PostgreSqlDialect {}, "DROP TYPE app.token CASCADE;")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    input = input.apply_statement(statement).unwrap();
    let db = input.finish();
    assert_eq!(db.operators().unwrap().count(), 0);
    let prior = the_operator(&snapshot);
    assert_eq!(prior.right_type(&snapshot), Some(app("wrapped")));
}

#[test]
fn an_operator_on_a_killed_function_goes_with_the_type() {
    let db = parse(
        configured(None, app("token")),
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE FUNCTION app.equal(a app.token, b app.token) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
         DROP TYPE app.token CASCADE;",
    );
    assert_eq!(db.operators().unwrap().count(), 0);
}

#[test]
fn restricted_relation_drops_preserve_operator_dependencies() {
    for (declaration, kind) in [
        ("CREATE TABLE app.token (a INT)", "TABLE"),
        ("CREATE VIEW app.token AS SELECT 1 AS a", "VIEW"),
        ("CREATE MATERIALIZED VIEW app.token AS SELECT 1 AS a", "MATERIALIZED VIEW"),
    ] {
        for operand in ["token", "_token"] {
            let result = configured(None, app(operand)).parse::<PostgreSqlDialect>(&format!(
                "CREATE SCHEMA app; {declaration}; DROP {kind} app.token;"
            ));
            assert!(
                matches!(
                    result,
                    Err(Error::RelationHasDependents {
                        dependent_kind: sql_traits::errors::ObjectKind::Operator,
                        ..
                    })
                ),
                "{kind} {operand}"
            );
        }
    }
}

#[test]
fn a_schema_move_carries_both_rowtype_identities() {
    for operand in ["token", "_token"] {
        let db = parse(
            configured(None, app(operand)),
            "CREATE SCHEMA app;
             CREATE SCHEMA moved;
             CREATE TABLE app.token (a INT);
             ALTER TABLE app.token RENAME TO moved.token;",
        );
        assert_eq!(
            the_operator(&db).right_type(&db),
            Some(TargetName::new(operand, false).with_schema("moved", false))
        );
    }
}

#[test]
fn rollback_restores_the_array_identity_an_operator_names() {
    let db = parse(
        configured(None, app("_token")),
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         BEGIN;
         CREATE TYPE app._token AS ENUM ('b');
         ROLLBACK;",
    );
    assert_eq!(the_operator(&db).right_type(&db), Some(app("_token")));
}

#[test]
fn a_column_cascade_removes_operators_on_reader_arrays() {
    let db = parse(
        configured(None, app("_reader")),
        "CREATE SCHEMA app;
         CREATE TABLE app.base (a INT, b INT);
         CREATE VIEW app.reader AS SELECT a FROM app.base;
         ALTER TABLE app.base DROP COLUMN a CASCADE;",
    );
    assert!(db.operators().unwrap().next().is_none());
}

#[test]
fn a_sequence_cascade_removes_operators_on_reader_arrays() {
    let db = parse(
        configured(None, app("_reader")),
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.counter;
         CREATE VIEW app.reader AS SELECT last_value AS a FROM app.counter;
         DROP SEQUENCE app.counter CASCADE;",
    );
    assert!(db.operators().unwrap().next().is_none());
}

#[test]
fn a_relation_cascade_removes_operators_declaring_reader_functions() {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
            .with_schema("other", false)
            .with_implementation(app("equal"), pg("bool")),
    );
    let db = parse(
        ParseOptions::default().with_postgres_catalog(catalog),
        "CREATE SCHEMA app;
         CREATE SCHEMA other;
         CREATE TABLE app.base (value INT);
         CREATE FUNCTION app.equal(p INT, q INT) RETURNS BOOL LANGUAGE SQL
         BEGIN ATOMIC SELECT p = q FROM app.base; END;
         DROP TABLE app.base CASCADE;",
    );
    assert!(db.operators().unwrap().next().is_none());
}
