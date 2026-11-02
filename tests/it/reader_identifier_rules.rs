//! Tests that the readers comparing an identifier apply the rule the rest of
//! the crate applies, rather than comparing raw bytes.
//!
//! Measured on PostgreSQL 16: `CREATE TABLE t (plain INT, CONSTRAINT c CHECK
//! (PLAIN > 0))` is accepted, because an unquoted reference folds onto the
//! stored column, and `CREATE VIEW v (col)` followed by `CREATE OR REPLACE
//! VIEW v (COL)` is accepted too, leaving the output column named `col`.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn table<'a>(database: &'a ParserDB, name: &str) -> &'a sqlparser::ast::CreateTable {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
}

/// A check constraint answers a column the way the table does, so an unquoted
/// lookup folds and a quoted one is read literally.
#[test]
fn a_check_constraint_reads_a_column_by_the_identifier_rule() -> Result<(), LookupError> {
    let database = db("CREATE TABLE t (plain INT, CONSTRAINT c CHECK (plain > 0));
         CREATE TABLE q (\"Body\" INT, CONSTRAINT c2 CHECK (\"Body\" > 0));");

    let plain = table(&database, "t")
        .check_constraints(&database)?
        .next()
        .expect("the table declares one check");
    assert!(plain.column(&database, "plain", IdentifierCase::AsWritten)?.is_some());
    assert!(
        plain.column(&database, "PLAIN", IdentifierCase::AsWritten)?.is_some(),
        "an unquoted lookup folds"
    );
    assert!(
        plain.column(&database, "\"PLAIN\"", IdentifierCase::AsWritten)?.is_none(),
        "a quoted lookup is literal"
    );

    let quoted = table(&database, "q")
        .check_constraints(&database)?
        .next()
        .expect("the table declares one check");
    assert!(quoted.column(&database, "\"Body\"", IdentifierCase::AsWritten)?.is_some());
    assert!(
        quoted.column(&database, "Body", IdentifierCase::AsWritten)?.is_none(),
        "the stored name keeps its case"
    );

    Ok(())
}

/// A check expression naming a column at another case still names it, which is
/// what the engine does, so the parse must not refuse the constraint.
#[test]
fn a_check_expression_folds_an_unquoted_column_reference() -> Result<(), LookupError> {
    let database = db("CREATE TABLE t (plain INT, CONSTRAINT c CHECK (PLAIN > 0));");
    let columns: Vec<&str> = table(&database, "t")
        .check_constraints(&database)?
        .next()
        .expect("the table declares one check")
        .columns(&database)?
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(columns, ["plain"]);

    // The qualified spelling folds on both parts.
    let qualified = db("CREATE TABLE t (plain INT, CONSTRAINT c CHECK (T.PLAIN > 0));");
    assert_eq!(
        table(&qualified, "t")
            .check_constraints(&qualified)?
            .next()
            .expect("the table declares one check")
            .columns(&qualified)?
            .count(),
        1
    );

    Ok(())
}

/// Replacing a view may respell an output name at another case, which the
/// engine reads as the same column.
#[test]
fn replacing_a_view_may_respell_an_output_name() -> Result<(), LookupError> {
    let database = db("CREATE TABLE t (plain INT);
         CREATE VIEW v (col) AS SELECT plain FROM t;
         CREATE OR REPLACE VIEW v (COL) AS SELECT plain + 1 FROM t;");

    let view = database
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
        .expect("the view survives the replacement");
    assert_eq!(view.declared_column_names().len(), 1);

    // A genuine rename is still refused.
    assert!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (plain INT);
             CREATE VIEW v (col) AS SELECT plain FROM t;
             CREATE OR REPLACE VIEW v (other) AS SELECT plain FROM t;",
        )
        .is_err()
    );

    Ok(())
}
