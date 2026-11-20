//! Tests that stored references read back after ingestion compare under
//! the catalog's identifier case.

#![expect(clippy::expect_used, reason = "the fixtures declare every object a test reads")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::Statement, dialect::SQLiteDialect, parser::Parser};

#[test]
fn folded_foreign_key_reaches_quoted_table() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        "CREATE TABLE \"T\" (id INT PRIMARY KEY);
             CREATE TABLE child (parent INT REFERENCES t(id));",
    )?;
    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::Folded)?
        .expect("child exists");
    let foreign_key = child.foreign_keys(&database)?.next().expect("foreign key exists");
    assert_eq!(foreign_key.referenced_table(&database)?.table_name(), "T");
    Ok(())
}

#[test]
fn default_foreign_key_keeps_quoted_and_unquoted_tables_apart() -> Result<(), Error> {
    let database = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE \"T\" (id INT PRIMARY KEY);
         CREATE TABLE t (id INT PRIMARY KEY);
         CREATE TABLE child (parent INT REFERENCES t(id));",
    )?;
    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)?
        .expect("child exists");
    let foreign_key = child.foreign_keys(&database)?.next().expect("foreign key exists");
    assert_eq!(foreign_key.referenced_table(&database)?.table_name(), "t");
    Ok(())
}

#[test]
fn folded_trigger_reaches_quoted_table() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        "CREATE TABLE \"T\" (id INT);
             CREATE TRIGGER audit AFTER INSERT ON t BEGIN SELECT 1; END;",
    )?;
    let trigger = database.triggers().next().expect("trigger exists");
    assert!(matches!(
        trigger.relation(&database)?,
        Relation::Table(target) if target.table_name() == "T"
    ));
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?
        .expect("table exists");
    assert_eq!(table.triggers(&database)?.count(), 1);
    Ok(())
}

#[test]
fn default_trigger_keeps_quoted_and_unquoted_tables_apart() -> Result<(), Error> {
    let database = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE \"T\" (id INT);
         CREATE TABLE t (id INT);
         CREATE TRIGGER audit AFTER INSERT ON t BEGIN SELECT 1; END;",
    )?;
    let trigger = database.triggers().next().expect("trigger exists");
    assert!(matches!(
        trigger.relation(&database)?,
        Relation::Table(target) if target.table_name() == "t"
    ));
    Ok(())
}

#[test]
fn folded_checks_reach_quoted_columns() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        "CREATE TABLE t (
                \"X\" TEXT,
                CHECK (x <> ''),
                CHECK (length(x) < 8),
                CHECK (x IN ('ab', 'cde')),
                CHECK (length(x) < 8 OR length(\"X\") < 12),
                CHECK (x <> '' OR \"X\" <> '')
             );",
    )?;
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?
        .expect("table exists");
    let checks = table.check_constraints(&database)?.collect::<Vec<_>>();
    assert_eq!(checks.len(), 5);
    for check in &checks {
        assert_eq!(check.columns(&database)?.next().expect("column exists").column_name(), "X");
    }
    assert!(checks[0].is_not_empty_text_constraint(&database)?);
    assert_eq!(checks[1].is_upper_bounded_text_constraint(&database)?, Some(8));
    assert_eq!(checks[2].is_upper_bounded_text_constraint(&database)?, Some(4));
    assert_eq!(checks[3].is_upper_bounded_text_constraint(&database)?, Some(12));
    assert!(checks[4].is_not_empty_text_constraint(&database)?);
    Ok(())
}

#[test]
fn folded_checks_decide_truth_on_quoted_columns() -> Result<(), Error> {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
        "CREATE TABLE t (\"X\" INT NOT NULL, CHECK (x IS NOT NULL), CHECK (x IS NULL));",
    )?;
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?
        .expect("table exists");
    let checks = table.check_constraints(&database)?.collect::<Vec<_>>();
    assert!(checks[0].is_tautology(&database)?);
    assert!(checks[1].is_negation(&database)?);
    Ok(())
}

#[test]
fn default_checks_reach_the_written_column() -> Result<(), Error> {
    let database = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE t (\"X\" TEXT, CHECK (\"X\" IN ('ab', 'cde')));",
    )?;
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
        .expect("table exists");
    let check = table.check_constraints(&database)?.next().expect("check exists");
    assert_eq!(check.is_upper_bounded_text_constraint(&database)?, Some(4));
    assert!(check.column(&database, "x", IdentifierCase::AsWritten)?.is_none());
    Ok(())
}

#[test]
fn dml_targets_take_the_catalog_case() -> Result<(), Error> {
    let folded = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>("CREATE TABLE \"T\" (id INT);")?;
    let as_written = ParserDB::parse::<SQLiteDialect>("CREATE TABLE \"T\" (id INT);")?;
    for statement in Parser::parse_sql(
        &SQLiteDialect {},
        "INSERT INTO t VALUES (1); UPDATE t SET id = 2; DELETE FROM t;",
    )
    .expect("DML parses")
    {
        let (resolved, missing) = match statement {
            Statement::Insert(insert) => {
                Some((insert.target_table(&folded), insert.target_table(&as_written)))
            }
            Statement::Update(update) => {
                Some((update.target_table(&folded), update.target_table(&as_written)))
            }
            Statement::Delete(delete) => {
                Some((delete.target_table(&folded), delete.target_table(&as_written)))
            }
            _ => None,
        }
        .expect("statement mutates a table");
        assert_eq!(resolved?.table_name(), "T");
        assert!(missing.is_err());
    }
    Ok(())
}
