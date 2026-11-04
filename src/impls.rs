//! Submodule providing implementations of the traits defined in the `traits`
//! module.

mod sqlparser;

pub(crate) use sqlparser::{
    PrivilegeKind, body_calls, body_expression, body_reads, bound_domain, domain_in_schema,
    domain_named, dql, foreign_options_of, grant_objects_match, grantee_is_public,
    grantee_is_session_principal, grantee_matches_role, grantees_match, helper_set,
    partition_grantees_for_revoke, schema_matches_table, serial_sequence_type,
    split_by_column_scope, validate_granted_columns,
};
pub use sqlparser::{SqlparserDialect, apply_revoke_to_grant};
