//! Submodule providing general structs for representing database schemas.

mod column_scope;
pub(crate) mod fingerprint;
pub mod generic_db;
pub use generic_db::{
    AccessResolution, GenericDB, Meta, ParseOptions, ParserDB, ParserDBBuilder, ParserDBIngestor,
    ParserIngestion, PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType, SchemaProfile,
    SqlparserProfile, UnresolvedAccessReference,
};
mod identifier_case;
pub mod metadata;
mod schema;
mod target_name;
mod view;

pub use column_scope::{
    ColumnDefinition, ColumnDefinitionRef, ColumnDefinitionScope, ColumnQueryScope, ColumnScope,
    QueryOutput,
};
pub use fingerprint::{AlgorithmId, FingerprintError, SchemaFingerprint, canonical_bytes_v1};
pub use identifier_case::IdentifierCase;
pub use metadata::{ColumnMetadata, FunctionMetadata, TableAttribute, TableMetadata, ViewMetadata};
pub use schema::Schema;
pub use target_name::TargetName;
pub use view::{MaterializedView, View, ViewDeclaration};
