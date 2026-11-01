//! Submodule providing implementations of the traits defined in the `traits`
//! module for the `sqlparser` crate.

mod check_constraint;
mod column_def;
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
mod role_membership;
mod schema;
mod sequence;
mod unique_constraint;
mod view;

pub(crate) use column_def::serial_sequence_type;
pub use dialect::SqlparserDialect;
pub use grant::apply_revoke_to_grant;
pub(crate) use grant::{
    check_granted_columns, grantee_is_public, grantee_matches_role, grantees_match,
    object_names_match, partition_grantees_for_revoke, schema_matches_sequence,
    schema_matches_table, schema_matches_view, split_by_column_scope, validate_granted_columns,
};
