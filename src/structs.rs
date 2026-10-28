//! Submodule providing general structs for representing database schemas.

mod column_scope;
pub(crate) mod fingerprint;
pub mod generic_db;
pub use generic_db::{
    AccessResolution, GenericDB, Meta, ParseOptions, ParserDB, ParserDBBuilder, ParserDBIngestor,
    ParserIngestion, PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType, SchemaProfile,
    SqlparserProfile, UnresolvedAccessReference,
};
mod function_kind;
mod identifier_case;
mod identity;
pub mod metadata;
mod role_membership;
mod schema;
mod target_name;
mod view;

pub use column_scope::{
    ColumnBinding, ColumnDefinition, ColumnDefinitionRef, ColumnDefinitionScope, ColumnQueryScope,
    ColumnScope, QueryOutput, RelationBinding,
};
pub use fingerprint::{AlgorithmId, FingerprintError, SchemaFingerprint, canonical_bytes_v1};
pub use function_kind::FunctionKind;
pub use identifier_case::IdentifierCase;
pub(crate) use identity::IdentityCounter;
pub use identity::{ColumnIdentity, RelationIdentity};
pub use metadata::{ColumnMetadata, FunctionMetadata, TableAttribute, TableMetadata, ViewMetadata};
pub use role_membership::RoleMembership;
pub use schema::Schema;
pub use target_name::TargetName;
pub use view::{MaterializedView, View, ViewDeclaration};
