//! Submodule providing general structs for representing database schemas.

mod column_scope;
mod deployment;
pub(crate) mod fingerprint;
mod foreign_table_source;
pub mod generic_db;
pub(crate) use generic_db::ConstraintObject;
pub use generic_db::{
    AccessResolution, GenericDB, Meta, ParseOptions, ParserDB, ParserDBBuilder, ParserDBIngestor,
    ParserIngestion, PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType, SchemaProfile,
    SqlparserProfile, UnresolvedAccessReference,
};
mod function_body;
mod function_kind;
mod helper_set;
mod identifier_case;
mod identity;
pub mod metadata;
mod partition_bound;
mod revoked_privilege;
mod role_membership;
mod row_security;
mod schema;
mod sequence;
mod sequence_parameters;
mod target_name;
mod view;

pub use column_scope::{
    ColumnBinding, ColumnDefinition, ColumnDefinitionRef, ColumnDefinitionScope, ColumnQueryScope,
    ColumnScope, QueryOutput, RelationBinding,
};
pub use deployment::{
    Deployment, JsonField, ResolvedDeployment, SessionSetting, SetPacking, SettingContent,
    SettingPresence, SettingValue,
};
pub use fingerprint::{AlgorithmId, FingerprintError, SchemaFingerprint, canonical_bytes_v1};
pub use foreign_table_source::ForeignTableSource;
pub use function_body::FunctionBody;
pub use function_kind::FunctionKind;
pub use helper_set::{GrantArm, HelperArm, HelperSet, LiteralSet, ParentArm};
pub use identifier_case::IdentifierCase;
pub(crate) use identity::IdentityCounter;
pub use identity::{ColumnIdentity, RelationIdentity};
pub use metadata::{ColumnMetadata, FunctionMetadata, TableAttribute, TableMetadata, ViewMetadata};
pub use partition_bound::{ListDatum, PartitionBound, RangeDatum};
pub use revoked_privilege::RevokedPrivilege;
pub use role_membership::RoleMembership;
pub use row_security::{
    ActingRole, GateFailure, GateTerm, GatedCommand, PolicyClause, RowGate, RowSecurity,
    RowSecurityExemption, RowStatement, RowVersion,
};
pub use schema::Schema;
pub use sequence::Sequence;
pub(crate) use sequence::StoredSequenceOwner;
pub use sequence_parameters::{SequenceParameters, SequenceType};
pub use target_name::TargetName;
pub use view::{MaterializedView, View, ViewDeclaration};
