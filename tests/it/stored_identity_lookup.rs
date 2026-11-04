//! Tests that a caller holding a stored identity can ask for exactly the table
//! stored under it.
//!
//! A written reference folds an unquoted name, reads a quoted one literally,
//! and treats no schema and `public` as one place. An identity does none of
//! that: it is already the name the catalog holds, so the two lookups cannot
//! share an entry point without one of them lying.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*, structs::TargetName};
use sqlparser::dialect::PostgreSqlDialect;

const SQL: &str = "CREATE TABLE \"Docs\" (id INT PRIMARY KEY);
CREATE TABLE plain (id INT PRIMARY KEY);
CREATE TABLE public.explicit (id INT PRIMARY KEY);
CREATE SCHEMA app;
CREATE TABLE app.plain (id INT PRIMARY KEY);
CREATE SCHEMA \"Other\";
CREATE TABLE \"Other\".\"Docs\" (id INT PRIMARY KEY);
CREATE TABLE \"\"\"Doubled\"\"\" (id INT PRIMARY KEY);
";

fn db() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SQL).expect("schema parses")
}

/// The four lookups the downstream proposal asks for.
#[test]
fn a_stored_identity_is_matched_exactly() {
    let db = db();

    assert!(db.table_by_stored_identity(None, "Docs").is_some());
    assert!(db.table_by_stored_identity(None, "docs").is_none());
    assert!(db.table_by_stored_identity(Some("public"), "plain").is_none());
    assert!(db.table_by_stored_identity(None, "plain").is_some());
}

/// The `public` qualifier and no qualifier name two distinct identities, even
/// though a written reference reaches both tables either way.
#[test]
fn no_schema_and_public_are_distinct_identities() {
    let db = db();

    assert!(db.table_by_stored_identity(Some("public"), "explicit").is_some());
    assert!(db.table_by_stored_identity(None, "explicit").is_none());

    // The written lookup keeps aliasing them, which is why it cannot answer an
    // identity.
    assert!(
        db.table_by_target(TargetName::new("explicit", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
    assert!(
        db.table_by_target(
            TargetName::new("plain", false).with_schema("public", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
}

/// The name is taken as stored, so a quote character stands for itself: it
/// matches only a stored name that carries one.
#[test]
fn quoting_is_not_interpreted() -> Result<(), LookupError> {
    let db = db();

    assert!(db.table_by_stored_identity(None, "\"Docs\"").is_none());
    assert!(db.table_by_stored_identity(Some("\"Other\""), "\"Docs\"").is_none());
    assert!(db.table_by_stored_identity(Some("Other"), "Docs").is_some());
    assert!(db.table_by_stored_identity(Some("other"), "docs").is_none());

    // A table whose stored name really carries quote characters is reached by
    // exactly those characters, and a written reference reaches it too, since
    // it takes the value apart from its quoting.
    let doubled = db
        .table_by_stored_identity(None, "\"Doubled\"")
        .expect("the stored name carries the quote characters");
    assert_eq!(doubled.stored_table_name(), "\"Doubled\"");
    assert!(
        db.table_by_target(TargetName::new("\"Doubled\"", true), IdentifierCase::AsWritten)?
            .is_some()
    );
    assert!(db.table_by_stored_identity(None, "Doubled").is_none());

    Ok(())
}

/// The answer is the table stored under that exact schema, not a same-named one
/// elsewhere.
#[test]
fn the_schema_selects_among_same_named_tables() {
    let db = db();

    let bare = db.table_by_stored_identity(None, "plain").expect("bare table exists");
    assert_eq!(bare.stored_table_schema(), None);

    let qualified = db.table_by_stored_identity(Some("app"), "plain").expect("app table exists");
    assert_eq!(qualified.stored_table_schema().as_deref(), Some("app"));

    assert!(db.table_by_stored_identity(Some("audit"), "plain").is_none());
}

/// Every table the database holds is reachable by the identity it reports.
#[test]
fn every_stored_table_answers_its_own_identity() {
    let db = db();

    for table in db.tables() {
        let schema = table.stored_table_schema();
        let name = table.stored_table_name();
        let found = db
            .table_by_stored_identity(schema.as_deref(), &name)
            .expect("a stored table answers its own identity");
        assert_eq!(found.stored_table_name(), name);
        assert_eq!(found.stored_table_schema(), schema);
    }
}

/// Reproduces the scanning default body through the public API, which is what
/// an implementor without an index gets.
fn scan_for_identity<'db>(
    db: &'db ParserDB,
    schema: Option<&str>,
    name: &str,
) -> Option<&'db <ParserDB as DatabaseLike>::Table> {
    db.tables().find(|table| {
        table.stored_table_schema().as_deref() == schema && table.stored_table_name() == name
    })
}

/// The index answers exactly what a scan answers, including where the two
/// spellings of the default schema differ.
#[test]
fn the_index_answers_what_a_scan_answers() -> Result<(), LookupError> {
    let db = db();

    for (schema, name) in [
        (None, "Docs"),
        (None, "docs"),
        (None, "\"Docs\""),
        (None, "plain"),
        (None, "explicit"),
        (Some("public"), "plain"),
        (Some("public"), "explicit"),
        (Some("public"), "Docs"),
        (Some("app"), "plain"),
        (Some("app"), "Docs"),
        (Some("Other"), "Docs"),
        (Some("other"), "docs"),
        (Some("Other"), "docs"),
    ] {
        let indexed = db.table_by_stored_identity(schema, name).map(TableLike::table_name);
        let scanned = scan_for_identity(&db, schema, name).map(TableLike::table_name);
        assert_eq!(indexed, scanned, "identity ({schema:?}, {name:?}) answered differently");
    }

    // The written lookups keep working, so the new method is an addition
    // rather than a replacement.
    assert!(
        db.table_by_target(
            TargetName::new("plain", false).with_schema("public", false),
            IdentifierCase::AsWritten
        )?
        .is_some()
    );
    assert!(
        db.resolve_target_table(TargetName::new("plain", false), IdentifierCase::AsWritten)?
            .is_some()
    );

    Ok(())
}
