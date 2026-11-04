//! What the built-in PostgreSQL catalog costs a parse.
//!
//! Every constructor that takes no explicit catalog starts from
//! `PostgresCatalog::postgres_18()`, whatever the dialect, so building it sits
//! on the path of every parse. These tests count allocations, since the cost of
//! that construction is the property they pin.
#![cfg(not(coverage))]

use sql_traits::prelude::*;

use crate::support::counting_allocator::allocations_of;

#[test]
fn building_the_postgres_18_catalog_allocates_nothing() {
    let counted = allocations_of(PostgresCatalog::postgres_18);
    assert_eq!(counted, 0, "building the PostgreSQL 18 catalog allocated {counted} times");
}

#[test]
fn default_parse_options_allocate_nothing() {
    let counted = allocations_of(ParseOptions::default);
    assert_eq!(counted, 0, "building the default parse options allocated {counted} times");
}
