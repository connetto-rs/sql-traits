#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Test and doctest helpers legitimately use `unwrap`/`expect`/`panic` for
// conciseness; the panic-family restriction lints are enforced only on the
// non-test build so production code stays panic-audited.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]

#[macro_use]
extern crate alloc;

pub mod errors;
mod impls;
pub mod structs;
pub mod traits;
pub mod upstream_pending;
pub mod utils;

/// Prelude module re-exporting commonly used items from the crate.
pub mod prelude {
    pub use sqlparser::dialect::GenericDialect;

    pub use crate::{structs::*, traits::*};
}
