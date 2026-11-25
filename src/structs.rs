//! Submodule providing general structs for representing database schemas.

#[cfg_attr(coverage_nightly, coverage(off))]
mod absent;
mod call_resolution;
mod column_scope;
mod constraint_deferral;
mod declared_virtual_table;
mod default_privilege;
mod definer_read;
mod deployment;
mod dynamic_execution;
mod embedded_sql;
mod exclusion_index;
mod expression_context;
mod expression_scope;
mod expression_truth;
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
mod replica_identity;
mod revoked_privilege;
mod role_membership;
mod role_setting;
mod row_security;
mod schema;
#[cfg(feature = "canonicalize")]
mod semantic_proofs;
mod sequence;
mod sequence_parameters;
mod sqlite_index_column;
mod table_privileges;
mod target_name;
mod transition_table;
mod trigger_enablement;
mod type_facts;
mod undeclared_row_field;
mod unqualified_name;
mod updatable_view_base;
mod user_defined_type;
mod view;
mod virtual_table_source;
mod written_call;

pub use absent::Absent;
pub use call_resolution::{ArgumentEvidence, CallResolution, SelectedFunction};
pub use column_scope::{
    ColumnBinding, ColumnDefinition, ColumnDefinitionList, ColumnDefinitionRef,
    ColumnDefinitionScope, ColumnQueryScope, ColumnScope, FunctionResultType, MergedColumn,
    QueryOutput, RangeTableEntry, RelationBinding,
};
pub use constraint_deferral::ConstraintDeferral;
pub use declared_virtual_table::{DeclaredColumn, DeclaredVirtualTable};
pub use default_privilege::{DefaultGrant, DefaultGrantee, DefaultPrivilege, DefaultPrivilegeKind};
pub use definer_read::{DefinerReading, UnconfinedRead, Unconfinement};
pub use deployment::{
    Deployment, JsonField, RequestTimeZone, ResolvedDeployment, SessionSetting, SetPacking,
    SettingContent, SettingPresence, SettingValue, TimeZoneSource,
};
pub use dynamic_execution::{DynamicExecution, DynamicExecutionSites};
pub(crate) use dynamic_execution::{SiteFlow, SiteScope};
pub use embedded_sql::EmbeddedSql;
pub use exclusion_index::ExclusionIndex;
pub use expression_context::{
    ArraySource, CallApplication, ExpressionContext, Operand, OperatorApplication,
};
pub(crate) use expression_context::{
    CallRefusal, ReparsePins, Site, applies_operators, array_of, column_type, declared_type,
    declared_type_named, places_call, plain_arguments, row_of,
};
pub use expression_scope::ExpressionScope;
pub use expression_truth::{Assumption, ExpressionTruth, Raising};
pub(crate) use expression_truth::{
    BodyTruth, domain_truth, site_truth, table_truth, validated_checks,
};
pub use fingerprint::{AlgorithmId, FingerprintError, SchemaFingerprint, canonical_bytes_v1};
pub use foreign_table_source::ForeignTableSource;
pub use function_body::FunctionBody;
pub use function_kind::{AggregateKind, FunctionKind};
pub use helper_set::{
    AntiJoin, ArmCondition, CalledArm, CallerGate, CapOperand, Correlation, ExcludedSet, GrantArm,
    GrantedTo, GranteeSet, HelperArm, HelperSet, JoinedRow, LiteralSet, ParentArm,
    RequestNormalization, Restriction, Returned, RowCap, SettingComparison, TimeBound,
    TimeComparison, UnsetSetting, WalkArm, WalkDirection,
};
pub use identifier_case::IdentifierCase;
pub use identity::{ColumnIdentity, RelationIdentity};
pub(crate) use identity::{IdentityCounter, KeyIdentity};
pub use index_key::IndexKey;
pub use maintenance_refusal::{BodyExcerpt, MaintenanceRefusal};
pub use materialized_view_index::MaterializedViewIndex;
pub use metadata::{ColumnMetadata, FunctionMetadata, TableAttribute, TableMetadata, ViewMetadata};
pub use operator_resolution::{
    CoercionStep, OperandEvidence, OperandType, OperatorResolution, SelectedOperator,
};
pub use partition_bound::{ListDatum, PartitionBound, RangeDatum};
pub use plpgsql_flow::{
    Definition, DefinitionId, DefinitionSource, DiagnosticsItem, Found, Output, Reaching, Rows,
    TargetStep, VariableRead,
};
pub use plpgsql_scope::{PlPgSqlScope, PlPgSqlVariable, VariableKind};
pub use plpgsql_site::{PlPgSqlSite, PlPgSqlSiteRole, PlPgSqlSites};
pub use policy_analysis::{MissingPolicyClause, PolicyAnalysis, PolicyTarget};
pub use policy_expression::PolicyExpression;
pub use privilege_holder::PrivilegeHolder;
pub(crate) use replica_identity::StoredReplicaIdentity;
pub use replica_identity::{ReplicaIdentity, ReplicaIdentityIndex};
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
pub use sqlite_index_column::{SqliteIndexColumn, SqliteIndexed};
pub use sqlparser_expr_eval::Outcomes;
pub use table_privileges::{ColumnPrivilege, TablePrivileges};
pub use target_name::TargetName;
pub use transition_table::TransitionTable;
pub use trigger_enablement::TriggerEnablement;
pub use type_facts::{DomainFacts, TypeCategory, TypeFacts, TypeKind};
pub use undeclared_row_field::{TriggerRow, UndeclaredRowField};
pub use unqualified_name::UnqualifiedName;
pub use updatable_view_base::UpdatableViewBase;
pub(crate) use user_defined_type::AttributeCollation;
pub use user_defined_type::UserDefinedType;
pub use view::{MaterializedView, View, ViewDeclaration};
pub use virtual_table_source::VirtualTableSource;
pub use written_call::WrittenCall;
