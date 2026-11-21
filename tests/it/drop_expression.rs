//! Generated-expression removal follows PostgreSQL column semantics.
#![expect(clippy::unwrap_used, reason = "Tests require successful schema construction and lookup.")]

use sql_traits::{
    errors::{Error, GenerationExpressionRefusal, InheritedChange},
    prelude::*,
};
use sqlparser::{
    dialect::{GenericDialect, PostgreSqlDialect},
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, sql_traits::errors::Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn assert_shape(database: &ParserDB, table_name: &str, default: Option<&str>, identity: bool) {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .unwrap()
        .unwrap();
    let column = table.column("value", database, IdentifierCase::AsWritten).unwrap().unwrap();
    assert!(column.generation_expression().is_none());
    assert_eq!(column.default_expression().map(ToString::to_string).as_deref(), default);
    assert_eq!(column.identity_generation(database).is_some(), identity);
    assert_eq!(column.is_generated(database), identity);
    assert!(!column.is_nullable(database).unwrap());
}

#[test]
fn column_kind_matrix() {
    for suffix in ["", " IF EXISTS"] {
        for (kind, accepted, default, identity) in [
            ("GENERATED ALWAYS AS (base + 1) STORED", true, None, false),
            ("GENERATED ALWAYS AS IDENTITY", !suffix.is_empty(), None, true),
            ("DEFAULT 4", !suffix.is_empty(), Some("4"), false),
            ("", !suffix.is_empty(), None, false),
        ] {
            let result = parse(&format!(
                "CREATE TABLE probe(base int, value int {kind} NOT NULL); \
                 ALTER TABLE probe ALTER COLUMN value DROP EXPRESSION{suffix};"
            ));
            assert_eq!(result.is_ok(), accepted, "{kind} / {suffix}: {result:?}");
            match result {
                Ok(database) => assert_shape(&database, "probe", default, identity),
                Err(error @ Error::GenerationExpressionNotDroppable { reason, .. }) => {
                    assert_eq!(reason, GenerationExpressionRefusal::NotGenerated);
                    assert_eq!(
                        error.to_string(),
                        "Cannot drop generation expression of `probe.value` because the column is not generated."
                    );
                }
                Err(error) => assert!(accepted, "unexpected refusal {error}"),
            }
        }
    }
}

#[test]
fn inherited_and_partitioned_matrix() {
    for layout in [
        "CREATE TABLE parent(base int, value int GENERATED ALWAYS AS (base+1) STORED NOT NULL); \
         CREATE TABLE child() INHERITS(parent);",
        "CREATE TABLE parent(base int, value int GENERATED ALWAYS AS (base+1) STORED NOT NULL) \
         PARTITION BY RANGE(base); CREATE TABLE child PARTITION OF parent FOR VALUES FROM (0) TO (10);",
    ] {
        for target in ["ONLY parent", "parent", "ONLY child", "child"] {
            for suffix in ["", " IF EXISTS"] {
                let result = parse(&format!(
                    "{layout} ALTER TABLE {target} ALTER COLUMN value DROP EXPRESSION{suffix};"
                ));
                assert_eq!(result.is_ok(), target == "parent", "{target}: {result:?}");
                match result {
                    Ok(database) => {
                        assert_shape(&database, "parent", None, false);
                        assert_shape(&database, "child", None, false);
                    }
                    Err(error @ Error::OnlyRefusedWithChildren { change, .. }) => {
                        assert_eq!(target, "ONLY parent");
                        assert_eq!(change, InheritedChange::DropExpression);
                        assert_eq!(
                            error.to_string(),
                            "`ONLY` cannot be used to drop a generation expression of table `parent`, because other tables inherit from it."
                        );
                    }
                    Err(error @ Error::GenerationExpressionNotDroppable { reason, .. }) => {
                        assert!(target.ends_with("child"));
                        assert_eq!(reason, GenerationExpressionRefusal::Inherited);
                        assert_eq!(
                            error.to_string(),
                            "Cannot drop generation expression of `child.value` because the column is inherited."
                        );
                    }
                    Err(error) => assert_eq!(target, "parent", "unexpected refusal {error}"),
                }
            }
        }
    }
}

#[test]
fn dependents_survive_expression_removal() {
    for dependent in [
        "",
        "CREATE INDEX probe_idx ON probe(value);",
        "CREATE VIEW probe_view AS SELECT value FROM probe;",
    ] {
        for suffix in ["", " IF EXISTS"] {
            let setup = format!(
                "CREATE TABLE probe(base int, value int GENERATED ALWAYS AS (base+1) STORED NOT NULL); {dependent}"
            );
            let before = parse(&setup).unwrap();
            let database = parse(&format!(
                "{setup} ALTER TABLE probe ALTER COLUMN value DROP EXPRESSION{suffix};"
            ))
            .unwrap();
            assert_shape(&database, "probe", None, false);
            assert_eq!(database.indexes().count(), before.indexes().count());
            assert_eq!(database.views().count(), before.views().count());
            for operations in [
                format!(
                    "ALTER COLUMN value DROP EXPRESSION{suffix}, ALTER COLUMN value TYPE bigint"
                ),
                format!(
                    "ALTER COLUMN value TYPE bigint, ALTER COLUMN value DROP EXPRESSION{suffix}"
                ),
            ] {
                let result = parse(&format!("{setup} ALTER TABLE probe {operations};"));
                assert_eq!(result.is_ok(), !dependent.contains("VIEW"), "{result:?}");
                if let Ok(database) = result {
                    assert_shape(&database, "probe", None, false);
                }
            }
        }
    }
}

#[test]
fn missing_column_is_refused_even_with_if_exists() {
    assert!(parse("CREATE TABLE probe(base int); ALTER TABLE probe ALTER COLUMN missing DROP EXPRESSION IF EXISTS;").is_err());
}

#[test]
fn rename_and_drop_use_separate_statements() {
    let database = parse(
        "CREATE TABLE probe(base int, generated int GENERATED ALWAYS AS (base+1) STORED NOT NULL); \
         ALTER TABLE probe RENAME COLUMN generated TO value; \
         ALTER TABLE probe ALTER COLUMN value DROP EXPRESSION;",
    )
    .unwrap();
    assert_shape(&database, "probe", None, false);
}

#[test]
fn plain_inherited_columns_still_require_parent_recursion() {
    for target in ["ONLY parent", "child"] {
        assert!(
            parse(&format!(
                "CREATE TABLE parent(base int, value int DEFAULT 4); \
             CREATE TABLE child() INHERITS(parent); \
             ALTER TABLE {target} ALTER COLUMN value DROP EXPRESSION IF EXISTS;"
            ))
            .is_err()
        );
    }
}

#[test]
fn virtual_generation_is_not_droppable() {
    for storage in ["VIRTUAL", ""] {
        for suffix in ["", " IF EXISTS"] {
            let statements = Parser::parse_sql(
                &GenericDialect {},
                &format!(
                    "CREATE TABLE probe(base int, value int GENERATED ALWAYS AS (base+1) {storage}); \
                     ALTER TABLE probe ALTER COLUMN value DROP EXPRESSION{suffix};"
                ),
            )
            .unwrap();
            let error = ParserDB::from_statements_with_dialect(
                statements,
                "postgres".to_owned(),
                SqlparserDialect::PostgreSql,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                Error::GenerationExpressionNotDroppable {
                    reason: GenerationExpressionRefusal::Virtual,
                    ..
                }
            ));
            assert_eq!(
                error.to_string(),
                "Cannot drop generation expression of `probe.value` because the column is virtual."
            );
        }
    }
}
