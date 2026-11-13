//! Submodule containing metadata structs.

mod column_metadata;
pub use column_metadata::ColumnMetadata;
pub(crate) use column_metadata::{ColumnCreation, CreatingQuery};
mod table_metadata;
pub(crate) use table_metadata::GeneratedConstraintName;
pub use table_metadata::TableMetadata;
mod table_attribute;
pub use table_attribute::TableAttribute;
mod index_metadata;
pub use index_metadata::{IndexMetadata, UniqueIndexMetadata};
mod function_metadata;
pub use function_metadata::FunctionMetadata;
pub(crate) use function_metadata::RangeConstructor;
mod check_metadata;
pub use check_metadata::CheckMetadata;
mod domain_metadata;
pub use domain_metadata::DomainMetadata;
mod expression_calls;
pub use expression_calls::ExpressionCalls;
pub(crate) use expression_calls::{BoundCall, Carried};
mod policy_metadata;
pub use policy_metadata::PolicyMetadata;
pub(crate) use policy_metadata::{ClauseLiterals, UnreadableLiteral};
mod view_metadata;
pub use view_metadata::ViewMetadata;
