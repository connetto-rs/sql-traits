//! Submodule providing implementations of the traits defined in the `traits`
//! module.

mod sqlparser;

pub(crate) use sqlparser::{
    PrivilegeKind, dql, grantee_is_public, grantee_matches_role, grantees_match,
    partition_grantees_for_revoke, schema_matches_table, serial_sequence_type,
    validate_granted_columns,
};
pub use sqlparser::{SqlparserDialect, apply_revoke_to_grant};
