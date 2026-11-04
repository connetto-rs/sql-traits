//! Tests that every name a parse resolves follows the comparison the caller
//! stated, not only the table names.
//!
//! Measured on the `sqlite3` shell: `CREATE VIEW "V1"` then `DROP VIEW v1`
//! succeeds, and `CREATE TRIGGER trg` then `DROP TRIGGER "TRG"` succeeds too,
//! leaving the table alone in `sqlite_master`. SQLite folds every identifier
//! for ASCII whatever quoting either side carried.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};

fn folded() -> ParseOptions {
    ParseOptions::default().with_identifier_case(IdentifierCase::Folded)
}

/// A folding parse replaces the view a divergent spelling names, rather than
/// refusing the name as taken.
#[test]
fn a_folding_parse_replaces_a_view_under_another_spelling() {
    let db = folded()
        .parse::<SQLiteDialect>(
            "CREATE TABLE t (id INT);
             CREATE VIEW \"V1\" AS SELECT id FROM t;
             CREATE OR REPLACE VIEW v1 AS SELECT id FROM t WHERE id > 0;",
        )
        .expect("SQLite replaces the view the folded name reaches");
    assert_eq!(db.views().count(), 1);

    let dropped = folded()
        .parse::<SQLiteDialect>(
            "CREATE TABLE t (id INT);
             CREATE VIEW \"V1\" AS SELECT id FROM t;
             DROP VIEW v1;",
        )
        .expect("SQLite drops the view the folded name reaches");
    assert_eq!(dropped.views().count(), 0);

    // PostgreSQL's rule keeps the quoted spelling apart, so the same script
    // refuses the drop.
    assert!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (id INT);
             CREATE VIEW \"V1\" AS SELECT id FROM t;
             DROP VIEW v1;",
        )
        .is_err()
    );
}

/// A trigger is one object under a folding engine whatever spelling names it.
#[test]
fn a_folding_parse_drops_a_trigger_under_another_spelling() {
    let db = folded()
        .parse::<SQLiteDialect>(
            "CREATE TABLE t (id INT);
             CREATE TRIGGER trg AFTER INSERT ON t BEGIN SELECT 1; END;
             DROP TRIGGER \"TRG\";",
        )
        .expect("SQLite drops the trigger the folded name reaches");
    assert_eq!(db.triggers().count(), 0);
}

/// So is a function, whose name MySQL folds on every platform. The dialect
/// governs the syntax and the stated comparison governs the names, so this
/// reads the function declaration a dialect this crate parses can write.
#[test]
fn a_folding_parse_drops_a_function_under_another_spelling() {
    let declared = "CREATE FUNCTION \"Foo\"() RETURNS INT AS 'SELECT 1;';";
    let db = folded()
        .parse::<GenericDialect>(&alloc_concat(declared, " DROP FUNCTION foo;"))
        .expect("a folding engine drops the function the name reaches");
    assert!(db.functions().all(|function| function.name() != "Foo"), "the declaration is gone");

    // Under PostgreSQL's rule the quoted spelling is its own name, so the
    // same script finds nothing to drop.
    assert!(
        ParserDB::parse::<GenericDialect>(&alloc_concat(declared, " DROP FUNCTION foo;")).is_err()
    );
}

fn alloc_concat(left: &str, right: &str) -> String {
    let mut owned = String::from(left);
    owned.push_str(right);
    owned
}

/// Renaming a table moves the references a divergent spelling wrote, so a key
/// keeps pointing at the table it was on.
#[test]
fn a_folding_parse_rewrites_references_it_resolved() -> Result<(), LookupError> {
    let db = folded()
        .parse::<SQLiteDialect>(
            "CREATE TABLE \"Docs\" (id INT PRIMARY KEY);
             CREATE TABLE child (id INT REFERENCES \"Docs\"(id));
             ALTER TABLE docs RENAME TO archive;",
        )
        .expect("SQLite renames the table the folded name reaches");

    assert!(
        db.table_by_target(TargetName::new("archive", false), IdentifierCase::Folded)?.is_some()
    );
    let child = db
        .table_by_target(TargetName::new("child", false), IdentifierCase::Folded)?
        .expect("the referencing table survives");
    let key = child.foreign_keys(&db)?.next().expect("the key survives");
    assert_eq!(
        key.referenced_table_name().name(),
        "archive",
        "the key follows the table it was on"
    );

    Ok(())
}

/// Under an exact comparison two case-differing tables are two tables, so a
/// key on one does not hold the other back from being dropped.
#[test]
fn an_exact_parse_drops_the_table_no_key_names() -> Result<(), LookupError> {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Exact)
        .parse::<MySqlDialect>(
            "CREATE TABLE Docs (id INT PRIMARY KEY);
             CREATE TABLE docs (id INT PRIMARY KEY);
             CREATE TABLE child (id INT, FOREIGN KEY (id) REFERENCES Docs(id));
             DROP TABLE docs;",
        )
        .expect("the key names the other table");

    assert!(db.table_by_target(TargetName::new("Docs", false), IdentifierCase::Exact)?.is_some());
    assert!(db.table_by_target(TargetName::new("docs", false), IdentifierCase::Exact)?.is_none());

    Ok(())
}
