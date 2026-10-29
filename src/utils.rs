//! Submodule providing utilities for SQL traits.

mod normalize_postgres_type;
pub use normalize_postgres_type::normalize_postgres_type;
pub(crate) use normalize_postgres_type::normalize_postgres_type_cow;
mod normalize_sqlparser_type;
pub use normalize_sqlparser_type::normalize_sqlparser_type;
pub(crate) mod call_sites;
pub(crate) mod call_typing;
pub mod columns_in_expression;
pub use columns_in_expression::columns_in_expression;
mod last_str;
pub use last_str::last_str;
mod is_identity;
pub(crate) use is_identity::identity_generation;
pub use is_identity::is_identity;
mod common_snake_affix;
pub use common_snake_affix::{common_column_name_snake_prefix, common_column_name_snake_suffix};
pub(crate) mod figured_names;
pub mod fingerprint_type_token;
pub mod identifier_resolution;
pub mod maintenance_trigger_parser;
pub(crate) mod object_name;
pub(crate) mod postgres_types;
pub use postgres_types::common_type_name;
mod output_from_wildcard;
pub(crate) use output_from_wildcard::output_from_wildcard;
pub mod scalar_family;
pub(crate) mod with_scopes;
