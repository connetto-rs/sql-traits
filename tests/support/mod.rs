//! Helpers several integration test modules share.

pub(crate) mod column_names;
pub(crate) mod column_nullability;
pub(crate) mod column_type_probes;
#[cfg(not(coverage))]
pub(crate) mod counting_allocator;
pub(crate) mod function_overloads;
pub(crate) mod query_parsing;
pub(crate) mod relation_labels;
pub(crate) mod sequence_names;
pub(crate) mod table_lookup;

/// Declared type names and the scalar family each one names, also included by
/// the unit tests of `sql_traits::utils::scalar_family`.
#[cfg(not(coverage))]
pub(crate) mod scalar_family_cases {
    use sql_traits::utils::scalar_family::ScalarFamily;

    include!("scalar_family_cases.rs");
}
