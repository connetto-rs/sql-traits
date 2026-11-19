//! Submodule providing general structs for representing database schemas.

#[cfg_attr(coverage_nightly, coverage(off))]
mod absent;
mod call_resolution;
mod column_scope;
mod constraint_deferral;
mod deployment;
mod dynamic_execution;
mod embedded_sql;
mod expression_context;
mod expression_scope;
pub(crate) mod fingerprint;
mod foreign_table_source;
pub mod generic_db;
pub use generic_db::{
    AccessResolution, GenericDB, Meta, ParseOptions, ParserDB, ParserDBBuilder, ParserDBIngestor,
    ParserIngestion, PostgresCatalog, PostgresCatalogCast, PostgresCatalogCollation,
    PostgresCatalogOperator, PostgresCatalogType, SchemaProfile, SqlparserProfile,
    UnresolvedAccessReference,
};
pub(crate) use generic_db::{ConstraintObject, OperatorRow};
mod function_body;
mod function_kind;
mod helper_set;
mod identifier_case;
mod identity;
mod index_key;
mod maintenance_refusal;
mod materialized_view_index;
pub mod metadata;
mod operator_resolution;
mod partition_bound;
pub(crate) mod plpgsql_flow;
pub(crate) mod plpgsql_scope;
mod plpgsql_site;
pub(crate) mod policy_analysis;
mod policy_expression;
mod privilege_holder;
mod revoked_privilege;
mod role_membership;
mod role_setting;
mod row_security;
mod schema;
#[cfg(feature = "canonicalize")]
mod semantic_proofs;
mod sequence;
mod sequence_parameters;
mod table_privileges;
mod target_name;
mod trigger_enablement;
mod type_facts;
mod unqualified_name;
mod user_defined_type;
mod view;
mod written_call;

pub use absent::Absent;
pub use call_resolution::{ArgumentEvidence, CallResolution, SelectedFunction};
pub use column_scope::{
    ColumnBinding, ColumnDefinition, ColumnDefinitionList, ColumnDefinitionRef,
    ColumnDefinitionScope, ColumnQueryScope, ColumnScope, QueryOutput, RangeTableEntry,
    RelationBinding,
};
pub use constraint_deferral::ConstraintDeferral;
pub use deployment::{
    Deployment, JsonField, RequestTimeZone, ResolvedDeployment, SessionSetting, SetPacking,
    SettingContent, SettingPresence, SettingValue, TimeZoneSource,
};
pub use dynamic_execution::{DynamicExecution, DynamicExecutionSites};
pub(crate) use dynamic_execution::{SiteFlow, SiteScope};
pub use embedded_sql::EmbeddedSql;
pub use expression_context::{
    ArraySource, CallApplication, ExpressionContext, Operand, OperatorApplication,
};
pub(crate) use expression_context::{
    CallRefusal, ReparsePins, Site, applies_operators, array_of, column_type, declared_type,
    declared_type_named, plain_arguments, row_of,
};
pub use expression_scope::ExpressionScope;
pub use fingerprint::{AlgorithmId, FingerprintError, SchemaFingerprint, canonical_bytes_v1};
pub use foreign_table_source::ForeignTableSource;
pub use function_body::FunctionBody;
pub use function_kind::{AggregateKind, FunctionKind};
pub use helper_set::{
    AntiJoin, CalledArm, CallerGate, CapLift, CapOperand, Correlation, GrantArm, GrantedTo,
    GranteeSet, HelperArm, HelperSet, JoinedRow, LiteralSet, ParentArm, RequestNormalization,
    Restriction, Returned, RowCap, SettingComparison, TimeBound, TimeComparison, WalkArm,
    WalkDirection,
};
pub use identifier_case::IdentifierCase;
pub(crate) use identity::IdentityCounter;
pub use identity::{ColumnIdentity, RelationIdentity};
pub use index_key::IndexKey;
pub use maintenance_refusal::{BodyExcerpt, MaintenanceRefusal};
pub use materialized_view_index::MaterializedViewIndex;
pub use metadata::{ColumnMetadata, FunctionMetadata, TableAttribute, TableMetadata, ViewMetadata};
pub use operator_resolution::{
    CoercionStep, OperandEvidence, OperandType, OperatorResolution, SelectedOperator,
};
pub use partition_bound::{ListDatum, PartitionBound, RangeDatum};
pub use plpgsql_flow::{
    Definition, DefinitionId, DefinitionSource, DiagnosticsItem, Output, Reaching, Rows,
    TargetStep, VariableRead,
};
pub use plpgsql_scope::{PlPgSqlScope, PlPgSqlVariable, VariableKind};
pub use plpgsql_site::{PlPgSqlSite, PlPgSqlSites};
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
#[cfg(feature = "canonicalize")]
pub use semantic_proofs::{SemanticProofView, SemanticProofs};
pub use sequence::Sequence;
pub(crate) use sequence::StoredSequenceOwner;
pub use sequence_parameters::{SequenceParameters, SequenceType};
pub use table_privileges::{ColumnPrivilege, TablePrivileges};
pub use target_name::TargetName;
pub use trigger_enablement::TriggerEnablement;
pub use type_facts::{DomainFacts, TypeCategory, TypeFacts, TypeKind};
pub use unqualified_name::UnqualifiedName;
pub(crate) use user_defined_type::AttributeCollation;
pub use user_defined_type::UserDefinedType;
pub use view::{MaterializedView, View, ViewDeclaration};
pub use written_call::WrittenCall;
