//! Submodule providing general structs for representing database schemas.

#[cfg_attr(coverage_nightly, coverage(off))]
mod absent;
mod column_scope;
mod deployment;
mod expression_context;
pub(crate) mod fingerprint;
mod foreign_table_source;
pub mod generic_db;
pub(crate) use generic_db::ConstraintObject;
pub use generic_db::{
    AccessResolution, GenericDB, Meta, ParseOptions, ParserDB, ParserDBBuilder, ParserDBIngestor,
    ParserIngestion, PostgresCatalog, PostgresCatalogCast, PostgresCatalogCollation,
    PostgresCatalogOperator, PostgresCatalogType, SchemaProfile, SqlparserProfile,
    UnresolvedAccessReference,
};
mod function_body;
mod function_kind;
mod helper_set;
mod identifier_case;
mod identity;
pub mod metadata;
mod operator_resolution;
mod partition_bound;
pub(crate) mod policy_analysis;
mod policy_expression;
mod privilege_holder;
mod revoked_privilege;
mod role_membership;
mod role_setting;
mod row_security;
mod schema;
mod sequence;
mod sequence_parameters;
mod table_privileges;
mod target_name;
mod type_facts;
mod user_defined_type;
mod view;

pub use absent::Absent;
pub use column_scope::{
    ColumnBinding, ColumnDefinition, ColumnDefinitionRef, ColumnDefinitionScope, ColumnQueryScope,
    ColumnScope, QueryOutput, RangeTableEntry, RelationBinding,
};
pub use deployment::{
    Deployment, JsonField, RequestTimeZone, ResolvedDeployment, SessionSetting, SetPacking,
    SettingContent, SettingPresence, SettingValue, TimeZoneSource,
};
pub use expression_context::{ArraySource, ExpressionContext, Operand, OperatorApplication};
pub use fingerprint::{AlgorithmId, FingerprintError, SchemaFingerprint, canonical_bytes_v1};
pub use foreign_table_source::ForeignTableSource;
pub use function_body::FunctionBody;
pub use function_kind::FunctionKind;
pub use helper_set::{GrantArm, HelperArm, HelperSet, LiteralSet, ParentArm};
pub use identifier_case::IdentifierCase;
pub(crate) use identity::IdentityCounter;
pub use identity::{ColumnIdentity, RelationIdentity};
pub use metadata::{ColumnMetadata, FunctionMetadata, TableAttribute, TableMetadata, ViewMetadata};
pub use operator_resolution::{
    CoercionStep, OperandEvidence, OperandType, OperatorResolution, SelectedOperator,
};
pub use partition_bound::{ListDatum, PartitionBound, RangeDatum};
pub use policy_analysis::{MissingPolicyClause, PolicyAnalysis, PolicyTarget};
pub use policy_expression::PolicyExpression;
pub use privilege_holder::PrivilegeHolder;
pub use revoked_privilege::RevokedPrivilege;
pub use role_membership::RoleMembership;
pub use role_setting::RoleSetting;
pub use row_security::{
    ActingRole, GateFailure, GateTerm, GatedCommand, PolicyClause, RowGate, RowSecurity,
    RowSecurityExemption, RowStatement, RowVersion,
};
pub use schema::Schema;
pub use sequence::Sequence;
pub(crate) use sequence::StoredSequenceOwner;
pub use sequence_parameters::{SequenceParameters, SequenceType};
pub use table_privileges::{ColumnPrivilege, TablePrivileges};
pub use target_name::TargetName;
pub use type_facts::{DomainFacts, TypeCategory, TypeFacts};
pub(crate) use user_defined_type::AttributeCollation;
pub use user_defined_type::UserDefinedType;
pub use view::{MaterializedView, View, ViewDeclaration};
