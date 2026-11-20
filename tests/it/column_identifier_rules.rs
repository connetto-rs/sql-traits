//! Tests that a column is read under the comparison the caller states, and
//! that a parse takes a column name under the comparison it states.
//!
//! Measured on the `sqlite3` shell: a column created as `"Body"` answers
//! `SELECT body` and `SELECT BODY`, `CREATE TABLE t ("Col" INT, "COL" INT)`
//! fails with `duplicate column name: COL`, `ALTER TABLE u RENAME COLUMN body
//! TO other` renames the column stored as `"Body"`, and `CREATE TABLE p ("ID"
//! INTEGER, "Code" TEXT, PRIMARY KEY (id), UNIQUE (code))` keys both columns,
//! enforcing a foreign key `REFERENCES p` and one `REFERENCES p (CODE)` on
//! SQLite 3.51.1. On MySQL 5.7 a column created as `` `Body` `` answers both
//! spellings too, whatever `lower_case_table_names` says. PostgreSQL is the
//! only one of the three that keeps a quoted column name apart.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn table<'a>(database: &'a ParserDB, name: &str) -> &'a sqlparser::ast::CreateTable {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
}

/// A folded read reaches a column the DDL wrote quoted, which is what SQLite
/// and MySQL do, while PostgreSQL's rule keeps the spelling apart.
#[test]
fn a_folded_read_reaches_a_quoted_column() -> Result<(), LookupError> {
    let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE t (\"Body\" TEXT, plain INT);")
        .expect("schema parses");
    let t = table(&db, "t");

    assert!(t.column("body", &db, IdentifierCase::AsWritten)?.is_none());
    assert_eq!(
        t.column("body", &db, IdentifierCase::Folded)?.map(ColumnLike::column_name),
        Some("Body")
    );
    assert_eq!(t.column_id_by_name("BODY", &db, IdentifierCase::Folded)?, Some(0));
    assert_eq!(t.column_id_by_name("BODY", &db, IdentifierCase::AsWritten)?, None);

    // An exact read compares the stored spelling byte for byte.
    assert!(t.column("plain", &db, IdentifierCase::Exact)?.is_some());
    assert!(t.column("PLAIN", &db, IdentifierCase::Exact)?.is_none());

    Ok(())
}

/// A check constraint reads its columns under the caller's rule too.
#[test]
fn a_check_constraint_reads_a_column_under_the_callers_rule() -> Result<(), LookupError> {
    let db = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE t (\"Body\" TEXT, CONSTRAINT c CHECK (\"Body\" <> ''));",
    )
    .expect("schema parses");
    let check =
        table(&db, "t").check_constraints(&db)?.next().expect("the table declares one check");

    assert!(check.column(&db, "body", IdentifierCase::AsWritten)?.is_none());
    assert!(check.column(&db, "body", IdentifierCase::Folded)?.is_some());

    Ok(())
}

/// A folding parse renames the column a divergent spelling names, which is
/// what both engines do.
#[test]
fn a_folding_parse_renames_a_column_under_another_spelling() -> Result<(), LookupError> {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE u (\"Body\" TEXT);
             ALTER TABLE u RENAME COLUMN body TO other;",
        )
        .expect("SQLite renames the column the folded name reaches");

    let u = table(&db, "u");
    assert!(u.column("other", &db, IdentifierCase::Folded)?.is_some());
    assert!(u.column("body", &db, IdentifierCase::Folded)?.is_none());

    Ok(())
}

/// A folding parse refuses two columns the engine reads as one name.
#[test]
fn a_folding_parse_refuses_columns_that_fold_together() {
    let sql = "CREATE TABLE t (\"Col\" INT, \"COL\" INT);";

    // PostgreSQL keeps the two quoted spellings apart and accepts both.
    assert!(ParserDB::parse::<PostgreSqlDialect>(sql).is_ok());

    assert!(
        ParseOptions::default()
            .with_identifier_case(IdentifierCase::Folded)
            .parse::<SQLiteDialect>(sql)
            .is_err(),
        "SQLite refuses the second as a duplicate column name"
    );
}

/// A folding parse resolves a granted column list at another spelling.
#[test]
fn a_folding_parse_grants_a_column_under_another_spelling() {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE t (\"Body\" TEXT);
             CREATE ROLE reader;
             GRANT SELECT (body) ON t TO reader;",
        )
        .expect("the grant names the column the folded spelling reaches");
    assert_eq!(db.column_grants().count(), 1);
}

/// A folding parse keys a column under another spelling and reads the key
/// back, as SQLite does.
#[test]
fn a_folding_parse_keys_a_column_under_another_spelling() -> Result<(), LookupError> {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE p (\"ID\" INTEGER, \"Code\" TEXT, PRIMARY KEY (id), UNIQUE (code));
             CREATE TABLE c (pid INTEGER REFERENCES p, code TEXT REFERENCES p (CODE));",
        )
        .expect("the keys name the columns the folded spellings reach");

    let key: Vec<&str> =
        table(&db, "p").primary_key_columns(&db)?.map(ColumnLike::column_name).collect();
    assert_eq!(key, ["ID"]);
    let referenced = table(&db, "c")
        .foreign_keys(&db)?
        .map(|key| Ok(key.referenced_columns(&db)?.map(ColumnLike::column_name).collect()))
        .collect::<Result<Vec<Vec<&str>>, LookupError>>()?;
    assert_eq!(referenced, [vec!["ID"], vec!["Code"]]);

    Ok(())
}

/// A folding parse requires a value of the columns a key or a table-level
/// `NOT NULL` names under another spelling.
#[test]
fn a_folding_parse_requires_a_value_under_another_spelling() -> Result<(), LookupError> {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE t (\"ID\" INT, \"Body\" TEXT, \"Note\" TEXT, PRIMARY KEY (id),
                 CONSTRAINT body_required NOT NULL body);
             CREATE TABLE u (\"Code\" INT);
             ALTER TABLE u ADD PRIMARY KEY (code);",
        )
        .expect("the key and the requirement name the columns the folded spellings reach");

    let nullable = |table_name: &str, column_name: &str| {
        table(&db, table_name)
            .column(column_name, &db, IdentifierCase::Folded)?
            .expect("the column exists")
            .is_nullable(&db)
    };
    assert!(!nullable("t", "id")?);
    assert!(!nullable("t", "body")?);
    assert!(nullable("t", "note")?);
    assert!(!nullable("u", "code")?);

    Ok(())
}

/// An exact parse keeps two spellings apart for a key as it does for a column
/// read.
#[test]
fn an_exact_parse_refuses_a_key_on_another_spelling() {
    let parsed = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Exact)
        .parse::<SQLiteDialect>("CREATE TABLE p (ID INTEGER, PRIMARY KEY (id));");
    assert!(matches!(
        parsed,
        Err(sql_traits::errors::Error::IdentifierLookupError(LookupError::ColumnNotFound { .. }))
    ));
}
