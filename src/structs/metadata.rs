//! Submodule containing metadata structs.

mod column_metadata;
pub use column_metadata::ColumnMetadata;
mod table_metadata;
pub use table_metadata::TableMetadata;
mod table_attribute;
pub use table_attribute::TableAttribute;
mod index_metadata;
pub use index_metadata::{IndexMetadata, UniqueIndexMetadata};
mod function_metadata;
pub use function_metadata::FunctionMetadata;
mod check_metadata;
pub use check_metadata::CheckMetadata;
mod expression_calls;
pub use expression_calls::ExpressionCalls;
mod policy_metadata;
pub use policy_metadata::PolicyMetadata;
mod view_metadata;
pub use view_metadata::ViewMetadata;
