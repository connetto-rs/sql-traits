//! Submodule providing implementations of the traits defined in the `traits`
//! module.

mod sqlparser;

pub use sqlparser::{SqlparserDialect, apply_revoke_to_grant};
pub(crate) use sqlparser::{
    check_granted_columns, dql, grantee_is_public, grantee_matches_role, grantees_match,
    object_names_match, partition_grantees_for_revoke, schema_matches_sequence,
    schema_matches_table, schema_matches_view, serial_sequence_type, split_by_column_scope,
    validate_granted_columns,
};
