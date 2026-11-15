//! Tests that an identifier comparison keeps apart the names the engines keep
//! apart, surrounding spaces and Unicode composition included.
//!
//! Measured on PostgreSQL 18 and on the `sqlite3` shell 3.51.1: `CREATE TABLE t
//! ("amount " INT, "amount" INT, "café" INT, "café" INT)`, the first `é`
//! precomposed and the second an `e` with a combining acute, holds four columns
//! of 7, 6, 5 and 6 bytes on both engines.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

const PRECOMPOSED: &str = "caf\u{e9}";
const DECOMPOSED: &str = "cafe\u{301}";

const FOUR_COLUMNS: &str =
    "CREATE TABLE t (\"amount \" INT, \"amount\" INT, \"caf\u{e9}\" INT, \"cafe\u{301}\" INT);";

const CASES: [IdentifierCase; 3] =
    [IdentifierCase::AsWritten, IdentifierCase::Folded, IdentifierCase::Exact];

#[test]
fn no_rule_merges_names_differing_in_spaces_or_composition() {
    for case in CASES {
        assert!(!case.identifiers_match("amount ", true, "amount", true), "{case}");
        assert!(!case.identifiers_match(" amount", false, "amount", false), "{case}");
        assert!(!case.identifiers_match(PRECOMPOSED, true, DECOMPOSED, true), "{case}");
        assert!(!case.identifiers_match(PRECOMPOSED, false, DECOMPOSED, false), "{case}");
    }
}

/// Each column answers its own lookup and no other, under PostgreSQL's rule
/// and under SQLite's.
#[test]
fn each_spelling_reaches_its_own_column() -> Result<(), LookupError> {
    let parses = [
        (IdentifierCase::AsWritten, ParserDB::parse::<PostgreSqlDialect>(FOUR_COLUMNS)),
        (
            IdentifierCase::Folded,
            ParseOptions::default()
                .with_identifier_case(IdentifierCase::Folded)
                .parse::<SQLiteDialect>(FOUR_COLUMNS),
        ),
    ];
    for (case, parsed) in parses {
        let db = parsed.expect("both engines accept the four columns");
        let t = db.table_by_target(TargetName::new("t", false), case)?.expect("the table exists");
        for (position, name) in
            ["amount ", "amount", PRECOMPOSED, DECOMPOSED].into_iter().enumerate()
        {
            let lookup = format!("\"{name}\"");
            assert_eq!(t.column_id_by_name(&lookup, &db, case)?, Some(position), "{case} {name:?}");
        }
        assert_eq!(t.column_id_by_name("\" amount\"", &db, case)?, None, "{case}");
    }
    Ok(())
}

/// PostgreSQL cuts a name to 63 bytes before comparing it, so two names that
/// differ only past that are one name there. SQLite cuts nothing.
#[test]
fn only_postgresqls_rule_cuts_names_to_63_bytes() {
    let long = "c".repeat(70);
    let cut = "c".repeat(63);
    assert!(IdentifierCase::AsWritten.identifiers_match(&long, false, &cut, true));
    assert!(!IdentifierCase::Folded.identifiers_match(&long, false, &cut, true));
    assert!(!IdentifierCase::Exact.identifiers_match(&long, false, &cut, true));
}

/// A relation whose name runs past 63 bytes is reached by its full name under
/// the rules that cut nothing, and two names sharing their first 63 bytes stay
/// two relations there while PostgreSQL's rule reads them as one.
#[test]
fn names_past_63_bytes_resolve_under_every_rule() -> Result<(), LookupError> {
    let first = format!("{}a", "c".repeat(69));
    let second = format!("{}b", "c".repeat(69));
    let sql = format!("CREATE TABLE {first} (x INT); CREATE TABLE {second} (y INT);");

    let db = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(&sql)
        .expect("SQLite keeps the two names apart");
    for case in [IdentifierCase::Folded, IdentifierCase::Exact] {
        for (name, column) in [(&first, "x"), (&second, "y")] {
            let table = db
                .resolve_target_table(TargetName::new(name, false), case)?
                .unwrap_or_else(|| panic!("{case} reaches {name}"));
            assert!(table.column(column, &db, case)?.is_some(), "{case} {name}");
        }
        let cut = "c".repeat(63);
        assert!(db.resolve_target_table(TargetName::new(&cut, false), case)?.is_none(), "{case}");
    }

    assert!(
        ParserDB::parse::<PostgreSqlDialect>(&sql).is_err(),
        "PostgreSQL cuts both names to the same 63 bytes"
    );
    Ok(())
}
