//! Tests that ingestion reads the stored definitions it rewrites under the
//! catalog's identifier case.

#![expect(clippy::expect_used, reason = "Test fixtures declare the queried relations")]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{
    ast::DataType,
    dialect::{PostgreSqlDialect, SQLiteDialect},
};

#[test]
fn folded_ctas_takes_the_referenced_column() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        r#"CREATE TABLE "T"("X" INT); CREATE TABLE c AS SELECT x FROM t;"#,
    )?;
    let table = database
        .table_by_target(TargetName::new("c", false), IdentifierCase::Folded)?
        .expect("c is declared");
    assert_eq!(table.columns.len(), 1);
    assert_eq!(table.columns[0].name.value, "x");
    assert_eq!(table.columns[0].data_type, DataType::Int(None));
    Ok(())
}

#[test]
fn folded_view_wildcard_stores_the_output_column() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        r#"CREATE TABLE "T"("X" INT); CREATE VIEW v AS SELECT * FROM t;"#,
    )?;
    let view = database.views().next().expect("v is declared");
    let definition = view.definition().to_string();
    assert!(definition.contains("\"X\""), "{definition}");
    assert!(!definition.contains('*'), "{definition}");
    Ok(())
}

#[test]
fn folded_table_rename_respells_a_view() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        r#"CREATE TABLE "T"("X" INT);
               CREATE VIEW v AS SELECT x FROM t;
               ALTER TABLE "T" RENAME TO u;"#,
    )?;
    let view = database.views().next().expect("v is declared");
    let definition = view.definition().to_string();
    assert!(definition.contains("FROM u"), "{definition}");
    assert!(!definition.contains("FROM t"), "{definition}");
    Ok(())
}

#[test]
fn folded_table_drop_sees_the_view_dependency() {
    let result = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            r#"CREATE TABLE "T"("X" INT);
               CREATE VIEW v AS SELECT x FROM t;
               DROP TABLE "T";"#,
        );
    assert!(
        matches!(result, Err(Error::RelationHasDependents {
            object_kind: ObjectKind::Table,
            dependent_kind: ObjectKind::View,
            ref dependent_name,
            ..
        }) if dependent_name == "v"),
        "{result:?}"
    );
}

#[test]
fn folded_view_drop_sees_the_next_view_in_the_chain() {
    let result = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            r#"CREATE TABLE "T"("X" INT);
               CREATE VIEW "V" AS SELECT x FROM t;
               CREATE VIEW dependent AS SELECT x FROM v;
               DROP VIEW "V";"#,
        );
    assert!(
        matches!(result, Err(Error::RelationHasDependents {
            object_kind: ObjectKind::View,
            dependent_kind: ObjectKind::View,
            ref dependent_name,
            ..
        }) if dependent_name == "dependent"),
        "{result:?}"
    );
}

#[test]
fn folded_cascade_removes_the_quoted_view_chain() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        r#"CREATE TABLE "T"("X" INT);
               CREATE VIEW "V" AS SELECT x FROM t;
               CREATE VIEW dependent AS SELECT x FROM v;
               DROP TABLE "T" CASCADE;"#,
    )?;
    assert_eq!(database.views().count(), 0);
    Ok(())
}

#[test]
fn as_written_catalog_keeps_differently_cased_references_unbound() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::AsWritten)
        .parse::<SQLiteDialect>(
        r#"CREATE TABLE "T"("X" INT);
           CREATE VIEW v AS SELECT * FROM t;
           ALTER TABLE "T" RENAME TO u;
           DROP TABLE u;"#,
    )?;
    let view = database.views().next().expect("v is declared");
    assert_eq!(view.definition().to_string(), "SELECT * FROM t");
    Ok(())
}

#[test]
fn folded_sequence_drop_sees_the_view_dependency() {
    let result = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<PostgreSqlDialect>(
        r#"CREATE SEQUENCE "S";
               CREATE VIEW "V" AS SELECT * FROM s;
               DROP SEQUENCE "S";"#,
    );
    assert!(
        matches!(result, Err(Error::RelationHasDependents {
            object_kind: ObjectKind::Sequence,
            dependent_kind: ObjectKind::View,
            ref dependent_name,
            ..
        }) if dependent_name == "V"),
        "{result:?}"
    );
}

#[test]
fn exact_sequence_drop_sees_the_view_dependency() {
    let result = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Exact)
        .parse::<PostgreSqlDialect>(
        "CREATE SEQUENCE Seq;
             CREATE VIEW v AS SELECT * FROM Seq;
             DROP SEQUENCE Seq;",
    );
    assert!(
        matches!(
            result,
            Err(Error::RelationHasDependents {
                object_kind: ObjectKind::Sequence,
                dependent_kind: ObjectKind::View,
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn exact_cascade_keeps_distinct_view_keys() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Exact)
        .parse::<SQLiteDialect>(
        "CREATE TABLE t (id INT);
             CREATE VIEW Foo AS SELECT * FROM t;
             CREATE VIEW foo AS SELECT * FROM t;
             CREATE VIEW w AS SELECT * FROM foo;
             DROP TABLE t CASCADE;",
    )?;
    assert_eq!(database.views().count(), 0);
    Ok(())
}

#[test]
fn folded_function_cascade_removes_quoted_view_readers() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<PostgreSqlDialect>(
        r#"CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
               CREATE VIEW "V" AS SELECT f() AS id;
               CREATE VIEW dependent AS SELECT id FROM v;
               DROP FUNCTION f() CASCADE;"#,
    )?;
    assert_eq!(database.views().count(), 0);
    Ok(())
}
