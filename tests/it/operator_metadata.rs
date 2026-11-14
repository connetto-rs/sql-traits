//! Operator catalog identities and incomplete metadata.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared catalog entities.")]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{
    dialect::{PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
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
        .map(|operator| {
            (
                TargetName::new(operator.name(), operator.name_is_quoted())
                    .with_schema(operator.schema().unwrap(), operator.schema_is_quoted()),
                operator.result_type(&db),
            )
        })
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
    assert!(
        matches!(
            result,
            Err(Error::FunctionReferenced { dependent_kind: ObjectKind::Operator, .. })
        ),
        "{result:?}"
    );
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

#[test]
fn an_unqualified_implementation_identity_defaults_to_pg_catalog() {
    let options =
        ParseOptions::default().with_postgres_catalog(PostgresCatalog::empty().with_operator(
            PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int8")).with_implementation(
                TargetName::new("equal", false),
                TargetName::new("bool", false),
            ),
        ));
    let db = options.parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE SCHEMA IF NOT EXISTS pg_catalog;
         CREATE FUNCTION app.equal(a int4, b int8) RETURNS BOOL LANGUAGE SQL AS 'SELECT FALSE';
         CREATE FUNCTION pg_catalog.equal(a int4, b int8) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
    ).unwrap();
    assert_eq!(
        db.operators().unwrap().next().unwrap().implementation(&db).unwrap().target_name(),
        pg("equal"),
    );
}

#[test]
fn an_implementation_requires_exactly_the_operator_operand_count() {
    for arguments in ["a app.token", "a app.token, b app.token, c app.token"] {
        let options = ParseOptions::default().with_postgres_catalog(
            PostgresCatalog::empty().with_operator(
                PostgresCatalogOperator::new("===", Some(app("token")), app("token"))
                    .with_schema("app", false)
                    .with_implementation(app("equal"), pg("bool")),
            ),
        );
        let db = options
            .parse::<PostgreSqlDialect>(&format!(
                "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.equal({arguments}) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
            ))
            .unwrap();
        assert!(db.operators().unwrap().next().unwrap().implementation(&db).is_none());
    }
}

#[test]
fn prefix_replacement_links_the_latest_implementation() {
    let first = PostgresCatalogOperator::new("~", None, app("token"))
        .with_implementation(app("first"), app("token"));
    let second = PostgresCatalogOperator::new("~", None, app("token"))
        .with_implementation(app("second"), app("token"));
    let catalog = PostgresCatalog::empty().with_operator(first).with_operator(second);
    let db = ParseOptions::default().with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
             CREATE TYPE app.token AS ENUM ('a');
             CREATE FUNCTION app.first(value app.token) RETURNS app.token LANGUAGE SQL AS 'SELECT value';
             CREATE FUNCTION app.second(value app.token) RETURNS app.token LANGUAGE SQL AS 'SELECT value';",
        ).unwrap();
    assert_eq!(db.operators().unwrap().count(), 1);
    let held = db.operators().unwrap().next().unwrap();
    assert_eq!(held.kind(), OperatorKind::Prefix);
    assert_eq!(held.implementation(&db).unwrap().target_name(), app("second"));
}

#[test]
fn a_shell_fact_does_not_hold_the_function_a_drop_releases() {
    let options = ParseOptions::default().with_postgres_catalog(
        PostgresCatalog::empty().with_operator(
            PostgresCatalogOperator::new("===", Some(app("token")), app("token"))
                .with_schema("app", false),
        ),
    );
    let db = options.parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.token AS ENUM ('a');
         CREATE FUNCTION app.equal(a app.token, b app.token) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';
         DROP FUNCTION app.equal(app.token, app.token);",
    ).unwrap();
    assert_eq!(db.operators().unwrap().count(), 1);
    let operator = db.operators().unwrap().next().unwrap();
    assert_eq!(operator.target_name(), app("==="));
    assert!(operator.implementation(&db).is_none());
    assert!(!db.functions().any(|function| function.target_name() == app("equal")));
}

#[test]
fn a_restricted_drop_identifies_the_dependent_operator() {
    let operator_name = "===";
    let result = ParseOptions::default()
        .with_postgres_catalog(
            PostgresCatalog::empty().with_operator(
                PostgresCatalogOperator::new(operator_name, None, app("_token"))
                    .with_schema("app", false),
            ),
        )
        .parse::<PostgreSqlDialect>(
            "CREATE SCHEMA app;
         CREATE TABLE app.token (a INT);
         DROP TABLE app.token;",
        );
    let err = result.unwrap_err();
    assert!(matches!(
        err,
        Error::RelationHasDependents { dependent_kind: ObjectKind::Operator, .. }
    ));
    assert!(err.to_string().contains(operator_name));
}

#[test]
fn unnamed_function_metadata_cannot_supply_an_operator_implementation() {
    let parsed = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION equal(a INT, b INT) RETURNS BOOL LANGUAGE SQL AS 'SELECT TRUE';",
    )
    .unwrap();
    let function = parsed.functions().find(|function| function.name() == "equal").unwrap();
    let mut unnamed = function.clone();
    unnamed.name.0.clear();
    let db: ParserDB = ParserDB::new("manual".into(), *parsed.dialect())
        .add_function(
            std::sync::Arc::new(unnamed),
            sql_traits::structs::FunctionMetadata::default(),
        )
        .add_function(
            std::sync::Arc::new(function.clone()),
            sql_traits::structs::FunctionMetadata::default(),
        )
        .into();
    let operator = PostgresCatalogOperator::new("===", Some(pg("int4")), pg("int4"))
        .with_implementation(TargetName::new("", false).with_schema("public", false), pg("bool"));
    assert!(operator.implementation(&db).is_none());
    let named = operator.with_implementation(
        TargetName::new("equal", false).with_schema("public", false),
        pg("bool"),
    );
    assert_eq!(named.implementation(&db).unwrap().name(), "equal");
}

/// The `pg_catalog` operator `name` over `left` and `right`, a `None` left
/// finding the prefix one.
fn builtin<'db>(
    db: &'db ParserDB,
    name: &str,
    left: Option<&str>,
    right: &str,
) -> &'db PostgresCatalogOperator {
    db.operators()
        .unwrap()
        .find(|operator| {
            operator.target_name() == pg(name)
                && operator.left_type(db) == left.map(pg)
                && operator.right_type(db) == Some(pg(right))
        })
        .unwrap()
}

/// The symbol and operand types of `operator`.
fn signature<'db>(
    operator: &'db PostgresCatalogOperator,
    db: &'db ParserDB,
) -> (TargetName<'db>, Option<TargetName<'db>>, Option<TargetName<'db>>) {
    (operator.target_name(), operator.left_type(db), operator.right_type(db))
}

#[test]
fn a_builtin_comparison_links_its_commutator_negator_and_estimators() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let less = builtin(&db, "<", Some("int4"), "int8");
    assert_eq!(
        signature(less.commutator(&db).unwrap(), &db),
        (pg(">"), Some(pg("int8")), Some(pg("int4")))
    );
    assert_eq!(
        signature(less.negator(&db).unwrap(), &db),
        (pg(">="), Some(pg("int4")), Some(pg("int8")))
    );
    assert!(!less.can_hash());
    assert!(!less.can_merge());
    assert_eq!(less.restriction_estimator(&db).unwrap().target_name(), pg("scalarltsel"));
    assert_eq!(less.join_estimator(&db).unwrap().target_name(), pg("scalarltjoinsel"));

    let equal = builtin(&db, "=", Some("int4"), "int4");
    assert_eq!(equal.commutator(&db), Some(equal));
    assert_eq!(
        signature(equal.negator(&db).unwrap(), &db),
        (pg("<>"), Some(pg("int4")), Some(pg("int4")))
    );
    assert!(equal.can_hash());
    assert!(equal.can_merge());
    assert_eq!(equal.restriction_estimator(&db).unwrap().target_name(), pg("eqsel"));
    assert_eq!(equal.join_estimator(&db).unwrap().target_name(), pg("eqjoinsel"));

    let negation = builtin(&db, "-", None, "int4");
    assert_eq!(negation.commutator(&db), None);
    assert_eq!(negation.negator(&db), None);
    assert!(!negation.can_hash());
    assert!(!negation.can_merge());
    assert!(negation.restriction_estimator(&db).is_none());
    assert!(negation.join_estimator(&db).is_none());
}

#[test]
fn a_configured_link_reaches_only_an_operator_the_catalog_holds() {
    let less = PostgresCatalogOperator::new("<<<", Some(pg("int4")), pg("int8"))
        .with_schema("app", false)
        .with_implementation(app("less"), pg("bool"))
        .with_commutator(app(">>>"))
        .with_negator(app(">>="))
        .with_hashes()
        .with_merges()
        .with_restriction_estimator(pg("scalarltsel"))
        .with_join_estimator(pg("scalarltjoinsel"));
    let lone = ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::empty().with_operator(less.clone()))
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    let held = lone.operators().unwrap().next().unwrap();
    assert_eq!(held.commutator(&lone), None);
    assert_eq!(held.negator(&lone), None);
    assert!(held.can_hash());
    assert!(held.can_merge());
    assert_eq!(held.restriction_estimator(&lone).unwrap().target_name(), pg("scalarltsel"));
    assert_eq!(held.join_estimator(&lone).unwrap().target_name(), pg("scalarltjoinsel"));

    // A commutator takes the operands swapped, so the one over the same
    // operands is no commutator.
    let greater =
        PostgresCatalogOperator::new(">>>", Some(pg("int8")), pg("int4")).with_schema("app", false);
    let same_operands =
        PostgresCatalogOperator::new(">>>", Some(pg("int4")), pg("int8")).with_schema("app", false);
    let negator =
        PostgresCatalogOperator::new(">>=", Some(pg("int4")), pg("int8")).with_schema("app", false);
    let linked = ParseOptions::default()
        .with_postgres_catalog(
            PostgresCatalog::empty()
                .with_operator(less)
                .with_operator(same_operands)
                .with_operator(greater.clone())
                .with_operator(negator.clone()),
        )
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    let held = linked.operators().unwrap().find(|operator| operator.name() == "<<<").unwrap();
    // The catalog numbers the rows it takes, so each link is compared with
    // the row holding the configured operator.
    let row = |wanted: &PostgresCatalogOperator| {
        linked.operators().unwrap().find(|operator| {
            operator.target_name() == wanted.target_name()
                && operator.left_type(&linked) == wanted.left_type(&linked)
        })
    };
    assert_eq!(held.commutator(&linked), Some(row(&greater).unwrap()));
    assert_eq!(held.negator(&linked), Some(row(&negator).unwrap()));
}
