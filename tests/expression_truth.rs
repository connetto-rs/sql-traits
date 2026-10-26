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
        "tenant_id > 7",
        "tenant_id IS NULL",
        "tenant_id IS NOT NULL",
        "unknown_column IS NOT NULL",
        "\"ID\" IS NOT NULL",
        "elsewhere.id IS NOT NULL",
        "other.invoices.id IS NOT NULL",
        "random() IS NULL OR random() IS NOT NULL",
        "missing IS NULL OR missing IS NOT NULL",
        "elsewhere.id IS NULL OR elsewhere.id IS NOT NULL",
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
         CREATE POLICY denied ON invoices USING (false);
         CREATE POLICY only_check ON invoices WITH CHECK (NOT false);
         CREATE POLICY nullable_writes ON invoices USING (NOT false) WITH CHECK (tenant_id IS NOT NULL);",
    )?;
    for policy in db.policies() {
        let table = policy.table(&db)?;
        let using = policy
            .using_expression(&db)
            .map(|expr| table.expression_truth(&db, expr, IdentifierCase::AsWritten))
            .transpose()?;
        let check = policy
            .check_expression(&db)
            .map(|expr| table.expression_truth(&db, expr, IdentifierCase::AsWritten))
            .transpose()?;
        let expected = match policy.name() {
            "public_rows" => (Some(Some(true)), None),
            "tenant_rows" => (Some(None), None),
            "unchecked_writes" => (Some(None), Some(Some(true))),
            "denied" => (Some(Some(false)), None),
            "only_check" => (None, Some(Some(true))),
            "nullable_writes" => (Some(Some(true)), Some(None)),
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

#[test]
fn integer_proofs_preserve_precision_and_refuse_either_unrepresentable_operand() -> Result<(), Error>
{
    let max = u128::MAX;
    assert_eq!(truth(&format!("0{max} = {max}"))?, Some(true));
    assert_eq!(truth(&format!("{max} = {}", max - 1))?, Some(false));
    assert_eq!(truth("0 = 340282366920938463463374607431768211456")?, None);
    assert_eq!(truth("340282366920938463463374607431768211456 = 0")?, None);
    assert_eq!(truth("1.0 = 1")?, None);
    Ok(())
}

#[test]
fn nested_predicates_preserve_unknowns_and_absorbing_booleans() -> Result<(), Error> {
    for (sql, expected) in [
        ("NOT (tenant_id = 7 AND false)", Some(true)),
        ("(tenant_id = 7 OR true) AND id IS NOT NULL", Some(true)),
        ("NOT (tenant_id IS NOT NULL OR false)", None),
        ("NOT NOT ((tenant_id) IS NULL OR tenant_id IS NOT NULL)", Some(true)),
        ("id IS NULL OR tenant_id IS NOT NULL", None),
        ("tenant_id IS NULL OR \"ID\" IS NOT NULL", None),
    ] {
        assert_eq!(truth(sql)?, expected, "{sql}");
    }
    Ok(())
}

#[test]
fn check_truth_analysis_reports_absent_constraint_metadata() -> Result<(), Error> {
    let owned =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (id INT, CHECK (NOT false));")?;
    let elsewhere = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (id INT);")?;
    let table = owned.tables().next().unwrap();
    let check = table.check_constraints(&owned)?.next().unwrap();
    for result in [check.is_tautology(&elsewhere), check.is_negation(&elsewhere)] {
        assert!(
            matches!(result, Err(sql_traits::errors::LookupError::ObjectNotInDatabase { .. }),)
        );
    }
    Ok(())
}
