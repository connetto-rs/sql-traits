//! Operator catalog identities and incomplete metadata.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared catalog entities.")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    dialect::{PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

#[test]
fn cross_type_comparison_keeps_its_operator_and_function() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let operator = db
        .operators()
        .unwrap()
        .find(|operator| {
            operator.target_name().name() == ">"
                && operator.left_type(&db).is_some_and(|ty| ty == pg("int4"))
                && operator.right_type(&db).is_some_and(|ty| ty == pg("int8"))
        })
        .unwrap();
    assert_eq!(operator.target_name().schema(), Some("pg_catalog"));
    assert_eq!(operator.kind(), OperatorKind::Binary);
    assert_eq!(operator.result_type(&db), Some(pg("bool")));
    let function = operator.implementation(&db).unwrap();
    assert_eq!(function.target_name(), pg("int48gt"));
    assert_eq!(function.argument_type_names(&db).collect::<Vec<_>>(), ["INT", "BIGINT"]);
}

#[test]
fn prefix_and_text_operators_preserve_declared_types() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let prefix = db
        .operators()
        .unwrap()
        .find(|operator| {
            operator.target_name().name() == "-"
                && operator.kind() == OperatorKind::Prefix
                && operator.right_type(&db).is_some_and(|ty| ty == pg("int4"))
        })
        .unwrap();
    assert_eq!(prefix.left_type(&db), None);
    assert_eq!(prefix.result_type(&db), Some(pg("int4")));
    assert_eq!(prefix.implementation(&db).unwrap().target_name(), pg("int4um"));
    let equality = db
        .operators()
        .unwrap()
        .find(|operator| {
            operator.target_name().name() == "="
                && operator.left_type(&db).is_some_and(|ty| ty == pg("text"))
                && operator.right_type(&db).is_some_and(|ty| ty == pg("text"))
        })
        .unwrap();
    assert_eq!(equality.implementation(&db).unwrap().target_name(), pg("texteq"));
}

#[test]
fn unavailable_catalog_is_distinct_from_an_empty_catalog() {
    let sqlite = ParserDB::parse::<SQLiteDialect>("").unwrap();
    assert!(sqlite.operators().is_none());
    let postgres = ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::empty())
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    assert!(postgres.operators().unwrap().next().is_none());
}

#[test]
fn configured_shell_and_defined_overloads_remain_distinct() {
    let catalog = PostgresCatalog::empty()
        .with_operator(
            PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
                .with_schema("app", false)
                .with_implementation(
                    TargetName::new("equal", false).with_schema("app", false),
                    pg("bool"),
                ),
        )
        .with_operator(
            PostgresCatalogOperator::new("===", Some(pg("int8")), pg("int8"))
                .with_schema("app", false),
        );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE FUNCTION app.equal(a INT, b INT) RETURNS BOOL LANGUAGE SQL AS 'SELECT a = b';",
        )
        .unwrap();
    let defined = db
        .operators()
        .unwrap()
        .find(|operator| operator.left_type(&db) == Some(pg("int4")))
        .unwrap();
    assert_eq!(defined.target_name(), TargetName::new("===", false).with_schema("app", false));
    assert_eq!(defined.result_type(&db), Some(pg("bool")));
    assert_eq!(defined.implementation(&db).unwrap().name(), "equal");
    let shell = db
        .operators()
        .unwrap()
        .find(|operator| operator.left_type(&db) == Some(pg("int8")))
        .unwrap();
    assert_eq!(shell.result_type(&db), None);
    assert!(shell.implementation(&db).is_none());
}

#[test]
fn array_operand_links_the_exact_builtin_overload() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let operator = db
        .operators()
        .unwrap()
        .find(|operator| {
            operator.target_name().name() == "-"
                && operator.left_type(&db) == Some(pg("jsonb"))
                && operator.right_type(&db) == Some(pg("_text"))
        })
        .unwrap();
    let function = operator.implementation(&db).unwrap();
    assert_eq!(function.target_name(), pg("jsonb_delete"));
    assert_eq!(function.argument_type_names(&db).collect::<Vec<_>>(), ["JSONB", "TEXT[]"]);
}

#[test]
fn implementation_lookup_does_not_coerce_operands() {
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
            .with_implementation(pg("equal"), pg("bool")),
    );
    let db = ParseOptions::default().with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE FUNCTION pg_catalog.equal(a BIGINT, b BIGINT) RETURNS BOOL LANGUAGE SQL AS 'SELECT a = b';",
        ).unwrap();
    let operator = db.operators().unwrap().next().unwrap();
    assert_eq!(operator.result_type(&db), Some(pg("bool")));
    assert!(operator.implementation(&db).is_none());
}

#[test]
fn replacement_uses_catalog_identity_including_schema_quoting() {
    let catalog = PostgresCatalog::empty()
        .with_operator(
            PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
                .with_schema("APP", false),
        )
        .with_operator(
            PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
                .with_schema("app", true)
                .with_implementation(pg("int4eq"), pg("bool")),
        )
        .with_operator(
            PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
                .with_schema("APP", true),
        );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    let actual = db
        .operators()
        .unwrap()
        .map(|operator| (operator.target_name(), operator.result_type(&db)))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            (TargetName::new("===", false).with_schema("app", true), Some(pg("bool"))),
            (TargetName::new("===", false).with_schema("APP", true), None),
        ]
    );
}

/// A schema containing a custom operand type and its comparison function.
const CUSTOM_SCHEMA: &str =
    "CREATE SCHEMA app;
     CREATE TYPE app.token AS ENUM ('a', 'b');
     CREATE FUNCTION app.equal(a app.token, b app.token) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';";

fn custom_options() -> ParseOptions {
    let token = TargetName::new("token", false).with_schema("app", false);
    ParseOptions::default().with_postgres_catalog(
        PostgresCatalog::empty().with_operator(
            PostgresCatalogOperator::new("===", Some(token.clone()), token)
                .with_schema("app", false)
                .with_implementation(
                    TargetName::new("equal", false).with_schema("app", false),
                    pg("bool"),
                ),
        ),
    )
}

#[test]
fn catalog_identities_follow_renames_without_changing_snapshots() {
    let original = custom_options().parse::<PostgreSqlDialect>(CUSTOM_SCHEMA).unwrap();
    let mut input = original.into_ingestor();
    let snapshot = input.snapshot();
    for statement in Parser::parse_sql(
        &PostgreSqlDialect {},
        "ALTER SCHEMA app RENAME TO moved;
         ALTER TYPE moved.token RENAME TO renamed;
         ALTER FUNCTION moved.equal(moved.renamed, moved.renamed) RENAME TO matches;
         CREATE SCHEMA implementation;
         ALTER FUNCTION moved.matches(moved.renamed, moved.renamed) SET SCHEMA implementation;",
    )
    .unwrap()
    {
        input = input.apply_statement(statement).unwrap();
    }
    let db = input.finish();
    let operator = db.operators().unwrap().next().unwrap();
    let token = TargetName::new("renamed", false).with_schema("moved", false);
    assert_eq!(operator.target_name(), TargetName::new("===", false).with_schema("moved", false));
    assert_eq!(operator.left_type(&db).as_ref(), Some(&token));
    assert_eq!(operator.right_type(&db), Some(token));
    assert_eq!(
        operator.implementation(&db).unwrap().target_name(),
        TargetName::new("matches", false).with_schema("implementation", false)
    );
    let prior = snapshot.operators().unwrap().next().unwrap();
    assert_eq!(prior.target_name(), TargetName::new("===", false).with_schema("app", false));
    assert_eq!(
        prior.left_type(&snapshot),
        Some(TargetName::new("token", false).with_schema("app", false))
    );
    assert_eq!(
        prior.implementation(&snapshot).unwrap().target_name(),
        TargetName::new("equal", false).with_schema("app", false)
    );
}

#[test]
fn dropping_an_unrelated_overload_keeps_the_operator_implementation() {
    let db = custom_options()
        .parse::<PostgreSqlDialect>(&format!(
            "{CUSTOM_SCHEMA}
         CREATE FUNCTION app.equal(a INT, b INT) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
         DROP FUNCTION app.equal(INT, INT);",
        ))
        .unwrap();
    let operator = db.operators().unwrap().next().unwrap();
    assert_eq!(
        operator.implementation(&db).unwrap().argument_type_names(&db).collect::<Vec<_>>(),
        ["app.token", "app.token"]
    );
}

#[test]
fn cascaded_dependencies_remove_operator_metadata() {
    for drop in [
        "DROP FUNCTION app.equal(app.token, app.token) CASCADE;",
        "DROP TYPE app.token CASCADE;",
        "DROP SCHEMA app CASCADE;",
    ] {
        let db = custom_options()
            .parse::<PostgreSqlDialect>(&format!("{CUSTOM_SCHEMA} {drop}"))
            .unwrap();
        assert!(db.operators().unwrap().next().is_none(), "{drop}");
    }
}

#[test]
fn operator_dependency_rejects_restricted_function_drop() {
    let result = custom_options().parse::<PostgreSqlDialect>(&format!(
        "{CUSTOM_SCHEMA} DROP FUNCTION app.equal(app.token, app.token);",
    ));
    assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
}

#[test]
fn quoted_array_type_identity_keeps_its_implementation() {
    let catalog = PostgresCatalog::default().with_operator(
        PostgresCatalogOperator::new(
            "-",
            Some(pg("jsonb")),
            TargetName::new("_text", true).with_schema("pg_catalog", false),
        )
        .with_implementation(pg("jsonb_delete"), pg("jsonb")),
    );
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    let operator = db
        .operators()
        .unwrap()
        .find(|operator| {
            operator.target_name().name() == "-"
                && operator.left_type(&db) == Some(pg("jsonb"))
                && operator.right_type(&db).is_some_and(|ty| ty.name() == "_text")
        })
        .unwrap();
    let function = operator.implementation(&db).unwrap();
    assert_eq!(function.target_name(), pg("jsonb_delete"));
    assert_eq!(function.argument_type_names(&db).collect::<Vec<_>>(), ["JSONB", "TEXT[]"]);
}

#[test]
fn leading_underscore_does_not_turn_a_declared_type_into_an_array() {
    let token = TargetName::new("_token", false).with_schema("app", false);
    let catalog = PostgresCatalog::empty().with_operator(
        PostgresCatalogOperator::new("===", Some(token.clone()), token)
            .with_schema("app", false)
            .with_implementation(
                TargetName::new("equal", false).with_schema("app", false),
                pg("bool"),
            ),
    );
    let db = ParseOptions::default().with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE TYPE app._token AS ENUM ('a');
             CREATE FUNCTION app.equal(a app._token, b app._token) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
        ).unwrap();
    let operator = db.operators().unwrap().next().unwrap();
    assert_eq!(
        operator.implementation(&db).unwrap().argument_type_names(&db).collect::<Vec<_>>(),
        ["app._token", "app._token"]
    );
}

#[test]
fn internal_char_is_distinct_from_sql_character() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let operator = db
        .operators()
        .unwrap()
        .find(|operator| {
            operator.target_name().name() == "="
                && operator.left_type(&db) == Some(pg("char"))
                && operator.right_type(&db) == Some(pg("char"))
        })
        .unwrap();
    assert_eq!(operator.implementation(&db).unwrap().target_name(), pg("chareq"));
}
