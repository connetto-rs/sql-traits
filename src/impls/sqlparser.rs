//! Submodule providing implementations of the traits defined in the `traits`
//! module for the `sqlparser` crate.

mod body_calls;
mod check_constraint;
mod column_def;
mod create_domain;
mod create_extension;
mod create_function;
mod create_index;
mod create_policy;
mod create_role;
mod create_rule;
mod create_table;
mod create_trigger;
mod data_statement;
mod data_type;
mod dialect;
mod dml;
pub(crate) mod dql;
mod foreign_key_constraint;
mod grant;
mod index_clauses;
mod plpgsql_body;
mod role_membership;
mod schema;
mod sequence;
mod unique_constraint;
mod view;

pub(crate) use column_def::serial_sequence_type;
pub(crate) use create_domain::{bound_domain, domain_in_schema, domain_named};
pub use dialect::SqlparserDialect;
pub use grant::apply_revoke_to_grant;
pub(crate) use grant::{
    PrivilegeKind, grant_objects_match, grantee_is_public, grantee_is_session_principal,
    grantee_matches_role, grantees_match, partition_grantees_for_revoke, schema_matches_table,
    split_by_column_scope, validate_granted_columns,
};
