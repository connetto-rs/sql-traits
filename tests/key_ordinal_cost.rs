//! What the key ordinal answer costs as the key gets wider.
//!
//! `primary_key_column_ids` composes the key iterator with the column
//! iterator, and the composition can walk the columns once for the whole key
//! or once per key column. Both answer the same ordinals, so this file counts
//! allocations instead: past width one, where the key needs no storage of its
//! own, a wider key must not cost more.
#![cfg(not(coverage))]
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
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

#[test]
fn the_key_ordinal_cost_does_not_grow_with_the_key_width() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "
        CREATE TABLE width_one (a INT, b INT, c INT, d INT, PRIMARY KEY (b));
        CREATE TABLE width_two (a INT, b INT, c INT, d INT, PRIMARY KEY (c, a));
        CREATE TABLE width_three (a INT, b INT, c INT, d INT, PRIMARY KEY (c, a, b));
        CREATE TABLE width_four (a INT, b INT, c INT, d INT, PRIMARY KEY (c, a, d, b));
        ",
    )?;
    let width_one = db
        .table_by_target(TargetName::new("width_one", false), IdentifierCase::AsWritten)?
        .expect("table width_one was created");
    let width_two = db
        .table_by_target(TargetName::new("width_two", false), IdentifierCase::AsWritten)?
        .expect("table width_two was created");
    let width_three = db
        .table_by_target(TargetName::new("width_three", false), IdentifierCase::AsWritten)?
        .expect("table width_three was created");
    let width_four = db
        .table_by_target(TargetName::new("width_four", false), IdentifierCase::AsWritten)?
        .expect("table width_four was created");

    // A cheaper wrong answer cannot pass this file.
    assert_eq!(width_one.primary_key_column_ids(&db)?, vec![1]);
    assert_eq!(width_two.primary_key_column_ids(&db)?, vec![2, 0]);
    assert_eq!(width_three.primary_key_column_ids(&db)?, vec![2, 0, 1]);
    assert_eq!(width_four.primary_key_column_ids(&db)?, vec![2, 0, 3, 1]);

    let one = allocations_of(|| width_one.primary_key_column_ids(&db));
    let two = allocations_of(|| width_two.primary_key_column_ids(&db));
    let three = allocations_of(|| width_three.primary_key_column_ids(&db));
    let four = allocations_of(|| width_four.primary_key_column_ids(&db));

    assert_eq!(
        three, two,
        "a key of width three costs {three} allocations against {two} for width two"
    );
    assert_eq!(
        four, two,
        "a key of width four costs {four} allocations against {two} for width two"
    );
    assert!(
        one <= two,
        "a key of one column costs {one} allocations, more than the {two} a wider key costs"
    );

    Ok(())
}
