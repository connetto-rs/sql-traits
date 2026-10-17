//! What reading a resolved collation's name costs.
//!
//! A consumer resolves a column's comparison rule once and then asks the name
//! whenever it decides whether a comparison is reproducible, so the accessor
//! sits on the deciding path rather than beside it. An owned collation holds
//! both of its parts as `Cow::Owned`, so an accessor handing the name back by
//! value copies the identifier and the qualifier on every read. These tests
//! count allocations instead of asserting a signature, since the cost is the
//! reason the accessor lends.
#![cfg(not(coverage))]
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::Error,
    prelude::*,
    traits::{ColumnCollation, NamedColumnCollation},
};
use sqlparser::dialect::PostgreSqlDialect;

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/counting_allocator.rs"));

/// Runs `body` and answers how many times it allocated.
fn allocations_of<T>(body: impl FnOnce() -> T) -> usize {
    let before = allocations();
    let answer = body();
    let counted = allocations() - before;
    drop(answer);
    counted
}

/// Resolves the qualified collation of a column, owned so the catalog and the
/// SQL text behind it are gone by the time the name is read.
fn resolved_collation() -> Result<NamedColumnCollation<'static>, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "
        CREATE SCHEMA app;
        CREATE COLLATION app.ci (
            provider = icu,
            locale = 'und-u-ks-level2',
            deterministic = false
        );
        CREATE TABLE t (name TEXT COLLATE app.ci);
        ",
    )?;
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
        .expect("table t was created");
    let column =
        table.column("name", &db, IdentifierCase::AsWritten)?.expect("column name was declared");
    let ColumnCollation::Named(named) = column.collation(&db)? else {
        panic!("a declared COLLATE resolves to a named collation");
    };
    Ok(named.into_owned())
}

#[test]
fn reading_a_collation_name_allocates_nothing() -> Result<(), Error> {
    let collation = resolved_collation()?;

    // A reader that only compares text, which is what the name is for.
    let counted = allocations_of(|| {
        let name = collation.name();
        (
            name.name().eq_ignore_ascii_case("CI"),
            name.schema() == Some("app"),
            name.name_is_quoted(),
        )
    });

    assert_eq!(counted, 0, "reading the collation name allocated {counted} times");
    assert_eq!(collation.name().name(), "ci");
    assert_eq!(collation.name().schema(), Some("app"));

    Ok(())
}

#[test]
fn taking_a_collation_name_allocates_nothing() -> Result<(), Error> {
    let collation = resolved_collation()?;

    let taken = allocations_of(|| collation.into_name());
    assert_eq!(taken, 0, "taking the collation name allocated {taken} times");

    let name = resolved_collation()?.into_name();
    assert_eq!(name.name(), "ci");
    assert_eq!(name.schema(), Some("app"));
    assert!(!name.name_is_quoted());

    Ok(())
}
