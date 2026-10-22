//! Submodule providing traits for describing SQL-like entities.

pub mod column;
pub mod database;
pub mod dialect;
pub mod table;
pub mod type_match;
use core::fmt::Debug;

pub use column::{ColumnCollation, ColumnLike, MySqlCollationPadding, NamedColumnCollation};
pub mod index;
pub use database::DatabaseLike;
pub use dialect::DialectLike;
pub use index::IndexLike;
pub use table::{PartitionStrategy, TableLike};
pub use type_match::{TypeMatch, TypeMatchLike};
pub mod check_constraint;
pub use check_constraint::CheckConstraintLike;
pub mod unique_index;
pub use unique_index::UniqueIndexLike;
pub mod foreign_key;
pub use foreign_key::ForeignKeyLike;
pub mod function_like;
pub use function_like::FunctionLike;
pub mod trigger;
pub use trigger::TriggerLike;
pub mod policy;
pub use policy::PolicyLike;
pub mod role;
pub use role::RoleLike;
pub mod schema;
pub use schema::SchemaLike;
pub mod grant;
pub use grant::{ColumnGrantLike, GrantLike, TableGrantLike};
pub mod data_statement;
pub use data_statement::DataStatementLike;
pub mod dql;
pub use dql::DQLLike;
pub mod dml;
pub use dml::{DMLLike, DmlKind, DmlStatement};
pub mod view;
pub use view::ViewLike;

/// Trait for associating a metadata struct to a given type.
pub trait Metadata {
    /// The associated metadata type.
    type Meta: Clone + Debug + Send + Sync;
}

impl<M: Metadata> Metadata for &M {
    type Meta = M::Meta;
}

impl<M: Metadata> Metadata for alloc::sync::Arc<M> {
    type Meta = M::Meta;
}
