//! Submodule providing implementations of the traits defined in the `traits`
//! module.

mod sqlparser;

pub use sqlparser::{SqlparserDialect, apply_revoke_to_grant};
pub(crate) use sqlparser::{
    dql, grantee_is_public, grantee_matches_role, grantees_match, partition_grantees_for_revoke,
    validate_granted_columns,
};
