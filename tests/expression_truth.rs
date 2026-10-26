//! Table-scoped truth analysis for policy and check expressions.
#![expect(
    clippy::unwrap_used,
    clippy::unreachable,
    reason = "Tests require parsed fixtures and exhaustive expected policy names."
)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::Expr, dialect::PostgreSqlDialect, parser::Parser};

fn expression(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

fn truth(sql: &str) -> Result<Option<bool>, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE app.invoices (id INT NOT NULL, tenant_id INT, \"ID\" INT);
         CREATE TABLE elsewhere (id INT);",
    )?;
    let table = db
        .table_by_target(
            TargetName::new("invoices", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .unwrap();
    Ok(table.expression_truth(&db, &expression(sql), IdentifierCase::AsWritten)?)
}

#[test]
fn proven_truths_and_negations() -> Result<(), Error> {
    for sql in [
        "true",
        "NOT false",
        "1 = 1",
        "01 = 1",
        "true = true",
        "'tenant' = 'tenant'",
        "false OR true",
        "true AND true",
    ] {
        assert_eq!(truth(sql)?, Some(true), "{sql}");
    }
    for sql in ["false", "NOT true", "1 = 0", "true = false", "false OR false", "true AND false"] {
        assert_eq!(truth(sql)?, Some(false), "{sql}");
    }
    Ok(())
}

#[test]
fn nulls_mixed_literals_and_collation_sensitive_comparisons_are_unknown() -> Result<(), Error> {
    for sql in [
        "NULL",
        "NULL = NULL",
        "NULL = 1",
        "1 = NULL",
        "NOT (NULL = NULL)",
        "1 = 1.0",
        "'a' = 'A'",
        "'1' = 1",
        "tenant_id = 7",
        "tenant_id IS NULL",
        "tenant_id IS NOT NULL",
        "unknown_column IS NOT NULL",
        "\"ID\" IS NOT NULL",
        "elsewhere.id IS NOT NULL",
        "other.invoices.id IS NOT NULL",
        "random() IS NULL OR random() IS NOT NULL",
        "missing IS NULL OR missing IS NOT NULL",
        "elsewhere.id IS NULL OR elsewhere.id IS NOT NULL",
        "340282366920938463463374607431768211456 = 1",
    ] {
        assert_eq!(truth(sql)?, None, "{sql}");
    }
    assert_eq!(truth("NULL OR true")?, Some(true));
    assert_eq!(truth("NULL AND false")?, Some(false));
    assert_eq!(truth("NULL OR false")?, None);
    assert_eq!(truth("NULL AND true")?, None);
    Ok(())
}

#[test]
fn nullability_respects_the_table_and_written_identifiers() -> Result<(), Error> {
    for sql in [
        "id IS NOT NULL",
        "ID IS NOT NULL",
        "(id) IS NOT NULL",
        "invoices.id IS NOT NULL",
        "app.invoices.id IS NOT NULL",
        "tenant_id IS NULL OR tenant_id IS NOT NULL",
        "tenant_id IS NOT NULL OR tenant_id IS NULL",
    ] {
        assert_eq!(truth(sql)?, Some(true), "{sql}");
    }
    assert_eq!(truth("id IS NULL")?, Some(false));
    assert_eq!(truth("NOT (id IS NOT NULL)")?, Some(false));
    Ok(())
}

#[test]
fn policy_expressions_can_be_analyzed_independently() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (id INT NOT NULL, tenant_id INT);
         CREATE POLICY public_rows ON invoices USING (NOT false);
         CREATE POLICY tenant_rows ON invoices USING (tenant_id = 7);
         CREATE POLICY unchecked_writes ON invoices USING (tenant_id = 7) WITH CHECK (1 = 1);
         CREATE POLICY denied ON invoices USING (false);",
    )?;
    for policy in db.policies() {
        let table = policy.table(&db)?;
        let using = table.expression_truth(
            &db,
            policy.using_expression(&db).unwrap(),
            IdentifierCase::AsWritten,
        )?;
        let check = policy
            .check_expression(&db)
            .map(|expr| table.expression_truth(&db, expr, IdentifierCase::AsWritten))
            .transpose()?;
        let expected = match policy.name() {
            "public_rows" => (Some(true), None),
            "tenant_rows" => (None, None),
            "unchecked_writes" => (None, Some(Some(true))),
            "denied" => (Some(false), None),
            _ => unreachable!(),
        };
        assert_eq!((using, check), expected, "{}", policy.name());
    }
    Ok(())
}

#[test]
fn checks_share_sound_null_and_literal_analysis() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (
            id INT NOT NULL,
            CHECK (NULL = NULL), CHECK (1 = 1.0), CHECK (NOT false),
            CHECK (invoices.id IS NOT NULL)
         );",
    )?;
    let table = db.tables().next().unwrap();
    let results = table
        .check_constraints(&db)?
        .map(|check| Ok((check.is_tautology(&db)?, check.is_negation(&db)?)))
        .collect::<Result<Vec<_>, Error>>()?;
    assert_eq!(results, [(false, false), (false, false), (true, false), (true, false)]);
    Ok(())
}

#[test]
fn callers_choose_identifier_comparison() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (\"ID\" INT NOT NULL);")?;
    let table = db.tables().next().unwrap();
    let expr = expression("id IS NOT NULL");
    assert_eq!(table.expression_truth(&db, &expr, IdentifierCase::AsWritten)?, None);
    assert_eq!(table.expression_truth(&db, &expr, IdentifierCase::Folded)?, Some(true));
    assert_eq!(table.expression_truth(&db, &expr, IdentifierCase::Exact)?, None);
    Ok(())
}

#[test]
fn inherited_columns_keep_their_nullability() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE parent (id INT NOT NULL);
         CREATE TABLE child (tenant_id INT) INHERITS (parent);",
    )?;
    let child =
        db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)?.unwrap();
    assert_eq!(
        child.expression_truth(
            &db,
            &expression("child.id IS NOT NULL"),
            IdentifierCase::AsWritten
        )?,
        Some(true),
    );
    Ok(())
}

#[test]
fn an_absent_table_is_an_error_even_for_a_literal() -> Result<(), Error> {
    let owned = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (id INT);")?;
    let elsewhere = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE other (id INT);")?;
    let table = owned.tables().next().unwrap();
    assert!(matches!(
        table.expression_truth(&elsewhere, &expression("true"), IdentifierCase::AsWritten),
        Err(sql_traits::errors::LookupError::ObjectNotInDatabase { .. }),
    ));
    Ok(())
}
