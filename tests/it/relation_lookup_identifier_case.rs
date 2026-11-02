//! Tests that a lookup compares identifiers the way the caller's engine does.
//!
//! PostgreSQL folds an unquoted identifier and reads a quoted one literally.
//! SQLite folds ASCII whether or not the name was quoted, and MySQL follows
//! `lower_case_table_names`, which is fixed when the server is initialised and
//! appears in no DDL. One catalog parsed from a dump can be read by engines
//! that disagree, so the rule belongs to the caller rather than to the
//! database.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect, SQLiteDialect};

/// The case the report opens with: a table the DDL wrote quoted, reached by
/// the spelling a SQLite statement may write for it.
#[test]
fn a_folded_lookup_reaches_a_quoted_stored_name() -> Result<(), LookupError> {
    let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE \"Docs\" (id INT PRIMARY KEY);")
        .expect("schema parses");
    let written = || TargetName::new("docs", false);

    assert!(db.resolve_target_table(written(), IdentifierCase::AsWritten)?.is_none());
    let folded = db
        .resolve_target_table(written(), IdentifierCase::Folded)?
        .expect("SQLite resolves the folded spelling");
    assert_eq!(folded.table_name(), "Docs");

    Ok(())
}

/// The mirror of it: MySQL with `lower_case_table_names = 0` refuses a
/// spelling that differs in case from the stored name, quoted or not.
#[test]
fn an_exact_lookup_refuses_a_folded_spelling() -> Result<(), LookupError> {
    let db = ParserDB::parse::<MySqlDialect>("CREATE TABLE Docs (id INT PRIMARY KEY);")
        .expect("schema parses");

    assert!(
        db.resolve_target_table(TargetName::new("docs", false), IdentifierCase::Exact)?.is_none()
    );
    assert!(
        db.resolve_target_table(TargetName::new("docs", true), IdentifierCase::Exact)?.is_none()
    );
    assert!(
        db.resolve_target_table(TargetName::new("Docs", false), IdentifierCase::Exact)?.is_some()
    );
    // Quoting says nothing under an exact comparison.
    assert!(
        db.resolve_target_table(TargetName::new("Docs", true), IdentifierCase::Exact)?.is_some()
    );
    // PostgreSQL's rule resolves what the MySQL server refuses.
    assert!(
        db.resolve_target_table(TargetName::new("docs", false), IdentifierCase::AsWritten)?
            .is_some()
    );

    Ok(())
}

/// Two spellings a folding engine reads as one name are an ambiguous lookup,
/// not an arbitrary winner.
#[test]
fn folding_two_stored_spellings_into_one_name_is_ambiguous() {
    let db = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE docs (id INT PRIMARY KEY);
         CREATE TABLE \"Docs\" (id INT PRIMARY KEY);",
    )
    .expect("schema parses");

    assert!(matches!(
        db.resolve_target_table(TargetName::new("DOCS", false), IdentifierCase::Folded),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
    // Each spelling still names exactly one table under PostgreSQL's rule.
    assert!(
        db.resolve_target_table(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("one table")
            .is_some()
    );
}

/// The qualifier follows the same rule as the name.
#[test]
fn a_folded_lookup_folds_the_qualifier() -> Result<(), LookupError> {
    let db = ParserDB::parse::<SQLiteDialect>(
        "CREATE SCHEMA \"App\";
         CREATE TABLE \"App\".docs (id INT PRIMARY KEY);",
    )
    .expect("schema parses");
    let written = || TargetName::new("docs", false).with_schema("app", false);

    assert!(db.resolve_target_table(written(), IdentifierCase::AsWritten)?.is_none());
    assert!(db.resolve_target_table(written(), IdentifierCase::Folded)?.is_some());

    Ok(())
}

/// So do the schemas on the search path, which is resolution machinery the
/// caller never spelled.
#[test]
fn a_folded_lookup_folds_the_search_path() -> Result<(), LookupError> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA \"App\";
         SET search_path TO \"App\";
         CREATE TABLE \"App\".\"Docs\" (id INT PRIMARY KEY);",
    )
    .expect("schema parses");
    let written = || TargetName::new("docs", false);

    assert!(db.resolve_target_table(written(), IdentifierCase::AsWritten)?.is_none());
    assert!(db.resolve_target_table(written(), IdentifierCase::Folded)?.is_some());

    Ok(())
}

/// Folding is ASCII-only, which is SQLite's rule rather than an
/// approximation of it: `sqlite3UpperToLower` maps bytes 65 to 90 and leaves
/// every byte above 127 alone.
#[test]
fn folding_is_ascii_only() -> Result<(), LookupError> {
    let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE \"Ünïcode\" (id INT PRIMARY KEY);")
        .expect("schema parses");

    assert!(
        db.resolve_target_table(TargetName::new("ünïcode", false), IdentifierCase::Folded)?
            .is_none()
    );
    assert!(
        db.resolve_target_table(TargetName::new("Ünïcode", false), IdentifierCase::Folded)?
            .is_some()
    );

    Ok(())
}

/// Views and materialized views share the walk, so they share the rule.
#[test]
fn views_take_the_same_rule() -> Result<(), LookupError> {
    let db = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE docs (id INT PRIMARY KEY);
         CREATE VIEW \"Recent\" AS SELECT id FROM docs;
         CREATE MATERIALIZED VIEW \"Counted\" AS SELECT id FROM docs;",
    )
    .expect("schema parses");

    assert!(
        db.resolve_target_view(TargetName::new("recent", false), IdentifierCase::Folded)?.is_some()
    );
    assert!(
        db.resolve_target_view(TargetName::new("recent", false), IdentifierCase::AsWritten)?
            .is_none()
    );
    assert!(
        db.resolve_target_materialized_view(
            TargetName::new("counted", false),
            IdentifierCase::Folded,
        )?
        .is_some()
    );
    assert!(
        db.resolve_target_materialized_view(
            TargetName::new("counted", false),
            IdentifierCase::Exact,
        )?
        .is_none()
    );

    Ok(())
}

/// Functions resolve through their own pool of names and take the rule too.
#[test]
fn a_function_lookup_takes_the_same_rule() -> Result<(), LookupError> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION \"Touch\"() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;",
    )
    .expect("schema parses");

    assert!(
        db.resolve_target_function(TargetName::new("touch", false), IdentifierCase::Folded)?
            .is_some()
    );
    assert!(
        db.resolve_target_function(TargetName::new("touch", false), IdentifierCase::AsWritten)?
            .is_none()
    );

    Ok(())
}

/// A caller holding an identifier value rather than written SQL reaches a
/// table through its parts, so a name carrying a dot or a quote is reachable
/// at all. The text lookups read their argument as SQL, which cannot spell
/// these.
#[test]
fn a_parts_lookup_reaches_a_name_text_cannot_spell() -> Result<(), LookupError> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE \"my.table\" (id INT PRIMARY KEY);
         CREATE TABLE \"we\"\"ird\" (id INT PRIMARY KEY);",
    )
    .expect("schema parses");

    let dotted = db
        .table_by_target(TargetName::new("my.table", true), IdentifierCase::AsWritten)?
        .expect("the dotted name is one identifier");
    assert_eq!(dotted.table_name(), "my.table");

    let quoted = db
        .table_by_target(TargetName::new("we\"ird", true), IdentifierCase::AsWritten)?
        .expect("the embedded quote is part of the identifier");
    assert_eq!(quoted.table_name(), "we\"ird");

    Ok(())
}

/// The parts lookup takes the comparison too, and consults no search path: an
/// unqualified target names the default schema.
#[test]
fn a_parts_lookup_takes_the_rule_and_ignores_the_search_path() -> Result<(), LookupError> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE app.\"Docs\" (id INT PRIMARY KEY);",
    )
    .expect("schema parses");

    let written = || TargetName::new("docs", false);
    assert!(db.table_by_target(written(), IdentifierCase::Folded)?.is_none());
    assert!(
        db.table_by_target(written().with_schema("APP", false), IdentifierCase::Folded,)?.is_some()
    );

    Ok(())
}
