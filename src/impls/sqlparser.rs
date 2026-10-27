//! Submodule providing implementations of the traits defined in the `traits`
//! module for the `sqlparser` crate.

mod check_constraint;
mod column_def;
mod create_function;
mod create_index;
mod create_policy;
mod create_role;
mod create_rule;
mod create_table;
mod create_trigger;
mod data_statement;
mod dialect;
mod dml;
pub(crate) mod dql;
mod foreign_key_constraint;
mod grant;
mod schema;
mod unique_constraint;
mod view;

pub use dialect::SqlparserDialect;
pub use grant::apply_revoke_to_grant;
pub(crate) use grant::{
    grantee_is_public, grantee_matches_role, grantees_match, partition_grantees_for_revoke,
    validate_granted_columns,
};
