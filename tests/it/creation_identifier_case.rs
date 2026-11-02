//! Tests that a parse takes a relation name under the comparison its caller
//! states, rather than always under PostgreSQL's rule.
//!
//! Measured against the engines: on the `sqlite3` shell, `CREATE TABLE "Docs"`
//! after `docs` fails with `table "Docs" already exists`. On MySQL 5.7 with
//! `lower_case_table_names = 1` the same pair fails with error 1050, while
//! with the setting at `0`, the Unix default, `docs`, `Docs` and `DOCS`
//! coexist as three tables. PostgreSQL sits between the two: it folds an
//! unquoted name and keeps a quoted one.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{GenericDialect, MySqlDialect, SQLiteDialect};

const CASE_VARIANTS: &str = "CREATE TABLE docs (id INT PRIMARY KEY);
     CREATE TABLE \"Docs\" (id INT PRIMARY KEY);";

const UNQUOTED_VARIANTS: &str = "CREATE TABLE docs (id INT PRIMARY KEY);
     CREATE TABLE Docs (id INT PRIMARY KEY);";

/// PostgreSQL's rule stays the default, so nothing about a parse that says
/// nothing changes, and the refusal says which rule read the two names as one.
#[test]
fn the_default_parse_keeps_postgresqls_rule() {
    assert_eq!(ParseOptions::default().identifier_case(), IdentifierCase::AsWritten);

    let db = ParserDB::parse::<GenericDialect>(CASE_VARIANTS).expect("PostgreSQL accepts the pair");
    assert_eq!(db.tables().count(), 2);

    let refused = ParserDB::parse::<GenericDialect>(UNQUOTED_VARIANTS)
        .expect_err("PostgreSQL folds the unquoted pair into one name");
    assert!(
        refused.to_string().contains("quoting decides"),
        "the refusal names the rule that decided it: {refused}"
    );
}

/// Two spellings a case-sensitive engine keeps apart still collide when they
/// are the same bytes, and the refusal names that rule too.
#[test]
fn an_exact_parse_refuses_the_same_name_twice() {
    let options = ParseOptions::default().with_identifier_case(IdentifierCase::Exact);
    assert_eq!(options.identifier_case(), IdentifierCase::Exact);

    let refused = options
        .parse::<MySqlDialect>(
            "CREATE TABLE docs (id INT PRIMARY KEY);
             CREATE TABLE docs (id INT PRIMARY KEY);",
        )
        .expect_err("one name twice is one name under every rule");
    assert!(
        refused.to_string().contains("exact"),
        "the refusal names the rule that decided it: {refused}"
    );
}

/// A folding parse refuses the pair SQLite refuses, and reports the rule that
/// decided it.
#[test]
fn a_folding_parse_refuses_a_quoted_case_variant() {
    let refused = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(CASE_VARIANTS);

    let Err(Error::IdentifierLookupError(LookupError::TableLookupConflict {
        table,
        conflicting_table,
        case,
    })) = refused
    else {
        panic!("expected a conflict, got {refused:?}")
    };
    assert_eq!(table, "\"Docs\"");
    assert_eq!(conflicting_table, "docs");
    assert_eq!(case, IdentifierCase::Folded);
    assert!(
        LookupError::TableLookupConflict { table, conflicting_table, case }
            .to_string()
            .contains("folded")
    );
}

/// An exact parse accepts the three spellings a case-sensitive MySQL server
/// holds side by side, and each stays reachable under that comparison.
#[test]
fn an_exact_parse_accepts_the_spellings_mysql_keeps_apart() -> Result<(), LookupError> {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Exact)
        .parse::<MySqlDialect>(
            "CREATE TABLE docs (id INT PRIMARY KEY);
             CREATE TABLE Docs (id INT PRIMARY KEY);
             CREATE TABLE DOCS (id INT PRIMARY KEY);",
        )
        .expect("a case-sensitive server holds all three");
    assert_eq!(db.tables().count(), 3);

    for written in ["docs", "Docs", "DOCS"] {
        let found = db
            .table_by_target(TargetName::new(written, false), IdentifierCase::Exact)?
            .expect("each spelling names its own table");
        assert_eq!(found.table_name(), written);
    }

    // Read under PostgreSQL's rule the three are one name, which is the
    // ambiguity the lookups already report rather than resolve.
    assert!(matches!(
        db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));

    Ok(())
}

/// The three relation kinds share one pool of names under every comparison, so
/// a folding parse refuses a view taking a table's folded name.
#[test]
fn a_folding_parse_refuses_a_view_folding_onto_a_table() {
    let refused = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE docs (id INT PRIMARY KEY);
             CREATE VIEW \"Docs\" AS SELECT id FROM docs;",
        );

    assert!(
        refused.is_err(),
        "a folding engine reads the view name as the table's, got {refused:?}"
    );
}

/// The comparison survives incremental ingestion, so a resumed line refuses
/// what the first statements would have refused.
#[test]
fn the_comparison_survives_a_resumed_ingestion() {
    let mut ingestor = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .ingestor::<SQLiteDialect>(String::from("memory"));
    let statements = sqlparser::parser::Parser::parse_sql(
        &SQLiteDialect {},
        "CREATE TABLE docs (id INT PRIMARY KEY);",
    )
    .expect("the first statement parses");
    for statement in statements {
        ingestor = ingestor.apply_statement(statement).expect("the first statement applies");
    }

    let second = sqlparser::parser::Parser::parse_sql(
        &SQLiteDialect {},
        "CREATE TABLE \"Docs\" (id INT PRIMARY KEY);",
    )
    .expect("the second statement parses");
    let refused = second.into_iter().try_fold(ingestor, ParserDBIngestor::apply_statement);

    assert!(refused.is_err(), "the resumed line kept the folding rule");
}

/// The comparison decides what a later statement reaches, not only what a
/// creation may take: SQLite resolves a quoted reference to an unquoted name.
#[test]
fn a_folding_parse_resolves_a_quoted_reference() -> Result<(), LookupError> {
    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE docs (id INT PRIMARY KEY);
             CREATE TABLE child (id INT REFERENCES \"Docs\"(id));",
        )
        .expect("SQLite resolves the quoted reference to the stored name");
    let child = db
        .table_by_target(TargetName::new("child", false), IdentifierCase::Folded)?
        .expect("the referencing table exists");
    assert_eq!(child.foreign_keys(&db)?.count(), 1);

    let dropped = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE docs (id INT PRIMARY KEY);
             DROP TABLE \"Docs\";",
        )
        .expect("SQLite drops the table the quoted name reaches");
    assert_eq!(dropped.tables().count(), 0);

    // An exact parse reads the same two statements the way a case-sensitive
    // MySQL does, where the quoted spelling names nothing.
    assert!(
        ParseOptions::default()
            .with_identifier_case(IdentifierCase::Exact)
            .parse::<MySqlDialect>(
                "CREATE TABLE docs (id INT PRIMARY KEY);
                 DROP TABLE Docs;",
            )
            .is_err()
    );

    Ok(())
}
