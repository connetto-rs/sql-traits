//! The SQLSTATE and the message PostgreSQL reports for each error that models
//! one of its refusals.

use alloc::string::String;

pub use sqlstate::SqlState;
use sqlstate::pg;

use super::{Error, LookupError};

mod lookup_columns;

/// An error as PostgreSQL 18 reports it, its SQLSTATE beside its primary
/// message, without the detail, hint or position it may add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresError {
    sqlstate: SqlState,
    message: String,
}

impl PostgresError {
    /// The error PostgreSQL reports with `sqlstate` and `message`.
    pub(crate) fn new(sqlstate: SqlState, message: impl Into<String>) -> Self {
        Self { sqlstate, message: message.into() }
    }

    /// The SQLSTATE, such as `42703` for `undefined_column`.
    #[must_use]
    #[inline]
    pub const fn sqlstate(&self) -> SqlState {
        self.sqlstate
    }

    /// The primary message PostgreSQL reports beside the SQLSTATE.
    #[must_use]
    #[inline]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// `name` in the double quotes PostgreSQL's messages put around an
/// identifier.
fn quoted(name: &str) -> String {
    format!("\"{name}\"")
}

impl Error {
    /// The SQLSTATE and the message PostgreSQL 18 refuses the statement with,
    /// or `None` for an error PostgreSQL has no counterpart of.
    ///
    /// ```
    /// use sql_traits::{errors::SqlState, prelude::*};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let error = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT NoSuch FROM t;",
    /// )
    /// .unwrap_err();
    /// let refusal = error.postgres_error().expect("PostgreSQL refuses the view");
    /// assert_eq!(refusal.sqlstate(), "42703".parse::<SqlState>()?);
    /// assert_eq!(refusal.message(), r#"column "nosuch" does not exist"#);
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    #[expect(clippy::too_many_lines, reason = "one arm per error variant")]
    #[must_use]
    pub fn postgres_error(&self) -> Option<PostgresError> {
        match self {
            Self::IdentifierLookupError(error) => error.postgres_error(),
            Self::ColumnTypeCollatabilityNotInCatalog { .. }
            | Self::UnresolvedOperator { .. }
            | Self::UnresolvedCall { .. }
            | Self::MisappliedCall { .. }
            | Self::InvalidCollationOption { .. }
            | Self::RepeatedCollationOption { .. }
            | Self::RepeatedColumnCollation { .. }
            | Self::NonCollatableColumnType { .. }
            | Self::DropTableInheritedFrom { .. }
            | Self::ReferencedColumnNotFoundForForeignKey { .. }
            | Self::HostColumnNotFoundForForeignKey { .. }
            | Self::ReferencedColumnsNotUniqueForForeignKey { .. }
            | Self::ForeignKeyColumnCountMismatch { .. }
            | Self::InitiallyDeferredNotDeferrable { .. }
            | Self::RelationNotFoundForTrigger { .. }
            | Self::TableNotFoundForPolicy { .. }
            | Self::RelationNotFoundForRule { .. }
            | Self::RelationNotFoundForRuleAction { .. }
            | Self::TableNotFoundForIndex { .. }
            | Self::RelationNotFoundForSequenceOwner { .. }
            | Self::RelationNotFoundForDefault { .. }
            | Self::TriggerFiringRefused { .. }
            | Self::RuleOnUnsupportedRelation { .. }
            | Self::RuleActionReferenceAmbiguous { .. }
            | Self::RuleActionInsertTooWide { .. }
            | Self::RoleNotFoundForOwner { .. }
            | Self::ColumnNotFoundForGrant { .. }
            | Self::ColumnNotFoundForSequenceOwner { .. }
            | Self::IndexInAnotherSchema { .. }
            | Self::InvalidSequenceOwner { .. }
            | Self::UnqualifiedCommentColumn { .. }
            | Self::SequenceOwnerInAnotherSchema { .. }
            | Self::SequenceOwnerKindUnsupported { .. }
            | Self::SequenceRequiredByColumn { .. }
            | Self::SequenceUsedByDefault { .. }
            | Self::InvalidSequenceParameters { .. }
            | Self::InvalidRelationNameForDefault { .. }
            | Self::InvalidIndex { .. }
            | Self::SubqueryInExpression { .. }
            | Self::ColumnReferenceInDefault { .. }
            | Self::FunctionNotFoundForTrigger { .. }
            | Self::RoleNotFoundForGrant { .. }
            | Self::RoleNotFoundForMembership { .. }
            | Self::CircularRoleMembership { .. }
            | Self::UnrecognizedRoleOption { .. }
            | Self::DependentRoleMemberships { .. }
            | Self::TableNotFoundForGrant { .. }
            | Self::FunctionNotFoundForGrant { .. }
            | Self::AmbiguousFunctionForGrant { .. }
            | Self::InvalidFunctionPrivilege { .. }
            | Self::DropFunctionNotFound { .. }
            | Self::DropProcedureNotFound { .. }
            | Self::NotAFunction { .. }
            | Self::NotAProcedure { .. }
            | Self::ProcedureBodyUnsupported { .. }
            | Self::FunctionReferenced { .. }
            | Self::FunctionRequiredByExtension { .. }
            | Self::FunctionRequiredByType { .. }
            | Self::DropTableNotFound { .. }
            | Self::TableReferenced { .. }
            | Self::ColumnReferenced { .. }
            | Self::ColumnTypeChangeBlocked { .. }
            | Self::ColumnAlreadyExists { .. }
            | Self::SchemaNotFoundForRelation { .. }
            | Self::SchemaNotFoundForCollation { .. }
            | Self::SchemaNotFoundForExtension { .. }
            | Self::NoSchemaSelectedForExtension { .. }
            | Self::NoSchemaSelectedForRelation { .. }
            | Self::SystemSchemaRelationRefused { .. }
            | Self::ForeignOptionNotFound { .. }
            | Self::UniqueIndexOverForeignPartition { .. }
            | Self::ViewColumnRenamedByReplace { .. }
            | Self::ViewColumnsDroppedByReplace { .. }
            | Self::MaterializedViewCannotBeReplaced { .. }
            | Self::ViewIfNotExistsUnsupported { .. }
            | Self::ViewColumnListTooLong { .. }
            | Self::TableColumnListTooLong { .. }
            | Self::UnrecognizedViewOption { .. }
            | Self::InvalidViewOptionValue { .. }
            | Self::RepeatedViewOption { .. }
            | Self::RelationHasDependents { .. }
            | Self::RenameConstraintNotFound { .. }
            | Self::InheritedConstraintNotRenamable { .. }
            | Self::ConstraintAlreadyExists { .. }
            | Self::PolicyAlreadyExists { .. }
            | Self::TriggerAlreadyExists { .. }
            | Self::ClonedTriggerDropped { .. }
            | Self::ClonedTriggerReplaced { .. }
            | Self::ConstraintTriggerOnForeignTable { .. }
            | Self::AlterTableTriggerNotFound { .. }
            | Self::RuleAlreadyExists { .. }
            | Self::ExtensionAlreadyExists { .. }
            | Self::RoleAlreadyExists { .. }
            | Self::FunctionAlreadyExists { .. }
            | Self::FunctionNotExecutableByCreator { .. }
            | Self::FunctionKindChange { .. }
            | Self::RelationNotFoundForFunctionBody { .. }
            | Self::FunctionBodyResultMissing { .. }
            | Self::FunctionBodyReferenceAmbiguous { .. }
            | Self::FunctionBodyInsertTooWide { .. }
            | Self::QualifiedForeignKeyTarget { .. }
            | Self::SelectRuleUnsupported { .. }
            | Self::UnsupportedSequenceOwner { .. }
            | Self::DatabaseAlreadyInUse { .. }
            | Self::CrossDatabaseReference { .. }
            | Self::UnknownCurrentSetting { .. }
            | Self::UnnamedSessionRoleSetting { .. }
            | Self::UnknownVirtualTableModule { .. }
            | Self::VirtualTableColumnsMismatch { .. }
            | Self::VirtualTableShadowSuffixRefused { .. }
            | Self::NotAVirtualTable { .. }
            | Self::VirtualTableArgumentsRefused { .. }
            | Self::VirtualTableContentColumnsUnknown { .. }
            | Self::VirtualTableRenameBlocked { .. }
            | Self::RepeatedIndexColumn { .. }
            | Self::NotValidRefused { .. }
            | Self::AggregateOwnerUnsupported { .. }
            | Self::MovedFunctionCallAmbiguous { .. }
            | Self::AggregateAlterationUnsupported { .. }
            | Self::TableDependencyGraph { .. }
            | Self::CyclicTableDependencies { .. }
            | Self::UnnamedObject { .. }
            | Self::UnsupportedTransactionControl { .. }
            | Self::UnsupportedDoBlock { .. }
            | Self::TransactionAlreadyOpen { .. }
            | Self::DefinitionInTransaction { .. }
            | Self::StatementAfterDefinition { .. }
            | Self::SqlParserError { .. }
            | Self::UnsupportedRevoke { .. }
            | Self::UnsupportedAlterTableOperation { .. }
            | Self::FunctionBodyBlocksColumnRename { .. }
            | Self::FunctionBodyStatementUnsupported { .. }
            | Self::AmbiguousDropFunction { .. }
            | Self::AmbiguousAlterFunction { .. }
            | Self::AmbiguousDropProcedure { .. }
            | Self::AmbiguousRoutineSignature { .. }
            | Self::DropExtensionNotFound { .. }
            | Self::AlterFunctionExtensionNotFound { .. }
            | Self::SchemaNotEmpty { .. }
            | Self::RenameTableNotFound { .. }
            | Self::AlterTableNotFound { .. }
            | Self::AlterIndexNotFound { .. }
            | Self::AlterPolicyNotFound { .. }
            | Self::AlterFunctionNotFound { .. }
            | Self::AlterRoleNotFound { .. }
            | Self::DropConstraintNotFound { .. }
            | Self::ValidateConstraintNotFound { .. }
            | Self::ConstraintNotValidatable { .. }
            | Self::NotValidConstraintConflict { .. }
            | Self::NoOpenTransaction { .. }
            | Self::SavepointNotFound { .. }
            | Self::ReferencedTableNotFoundForForeignKey { .. }
            | Self::ParentTableNotFound { .. }
            | Self::CopiedTableNotFound { .. }
            | Self::InheritsFromNonTable { .. }
            | Self::InheritedColumnNotDroppable { .. }
            | Self::InheritedColumnNotRenamable { .. }
            | Self::ColumnAddedToPartition { .. }
            | Self::IdentityAddedToPartitionColumn { .. }
            | Self::IdentityAddedToOnlyPartitionedTable { .. }
            | Self::InheritedColumnTypeNotAlterable { .. }
            | Self::InheritedConstraintNotDroppable { .. }
            | Self::RequiredValueNotDroppable { .. }
            | Self::OnlyRefusedWithChildren { .. }
            | Self::InheritedConstraintConflict { .. }
            | Self::NoInheritCheckOnPartitionedTable { .. }
            | Self::OnlyForeignKeyOnPartitionedTable { .. }
            | Self::NoInheritNotNullOnPartitionedTable { .. }
            | Self::ConflictingNotNullConstraintNames { .. }
            | Self::ConflictingNoInheritNotNull { .. }
            | Self::NotNullConstraintAlreadyExists { .. }
            | Self::NotNullNoInheritStatusChange { .. }
            | Self::OnlyPrimaryKeyOnNullableColumn { .. }
            | Self::IdentityNeedsRequiredValue { .. }
            | Self::InheritedColumnTypeConflict { .. }
            | Self::InheritedColumnCollationConflict { .. }
            | Self::CollationCannotBeCopied { .. }
            | Self::CollationNotFound { .. }
            | Self::CollationAlreadyExists { .. }
            | Self::OperatorRightArgumentRequired { .. }
            | Self::OperatorFunctionNotFound { .. }
            | Self::OperatorAlreadyExists { .. }
            | Self::OperatorOwnNegator { .. }
            | Self::OperatorAttributeNeedsBoolean { .. }
            | Self::OperatorAttributeNeedsBinary { .. }
            | Self::OperatorLinkTaken { .. }
            | Self::OperatorFunctionNotExecutable { .. }
            | Self::OperatorNotFound { .. }
            | Self::OperatorRequiredBySystem { .. }
            | Self::OperatorReferenced { .. }
            | Self::OperatorAttributeAlreadySet { .. }
            | Self::TemporaryRelationInPermanentSchema { .. }
            | Self::TemporaryMaterializedView { .. }
            | Self::PartitionCommandNotAlone { .. }
            | Self::MaterializedViewReadsTemporaryRelation { .. }
            | Self::ForeignKeyCrossesTemporaryStorage { .. }
            | Self::ForeignKeyReferencesUnloggedTable { .. }
            | Self::UnloggedTemporaryTable { .. }
            | Self::UnloggedPartitionedTable { .. }
            | Self::PersistenceNotChangeable { .. }
            | Self::PersistenceChangedTwice { .. }
            | Self::StorageParametersOnPartitionedTable { .. }
            | Self::PermanentRelationInheritsTemporary { .. }
            | Self::TemporaryPartitionOfPermanent { .. }
            | Self::ParentNotPartitioned { .. }
            | Self::PartitionedTableInherits { .. }
            | Self::ParentRepeated { .. }
            | Self::DefaultOnInheritedGeneratedColumn { .. }
            | Self::InheritsFromPartitionedTable { .. }
            | Self::InheritanceChildMismatch { .. }
            | Self::InheritanceChangeOnPartition { .. }
            | Self::InheritanceChangeOnPartitioned { .. }
            | Self::CircularInheritance { .. }
            | Self::NotAParentOf { .. }
            | Self::NotAPartitionOf { .. }
            | Self::DetachConcurrentlyInTransaction { .. }
            | Self::DetachConcurrentlyWithDefault { .. }
            | Self::NoDetachPending { .. }
            | Self::InheritsFromPartition { .. }
            | Self::PartitionKeyColumnNotFound { .. }
            | Self::GeneratedColumnInPartitionKey { .. }
            | Self::ListPartitionKeyHasManyColumns { .. }
            | Self::UniqueConstraintMissesPartitionKey { .. }
            | Self::UniqueConstraintOverExpressionPartitionKey { .. }
            | Self::PartitionBoundInvalid { .. }
            | Self::PartitionBoundOverlaps { .. }
            | Self::PartitionNotAttachable { .. }
            | Self::NoSchemaSelectedForCollation { .. }
            | Self::RoleNotFoundForPolicy { .. }
            | Self::DropRoleNotFound { .. }
            | Self::DropIndexNotFound { .. }
            | Self::DropTriggerNotFound { .. }
            | Self::DropPolicyNotFound { .. }
            | Self::RoleReferenced { .. }
            | Self::RoleInUseBySession { .. }
            | Self::RoleNotFoundForSession { .. }
            | Self::InvalidSessionRole { .. }
            | Self::NonLiteralSetting { .. }
            | Self::InvalidTimeZone { .. }
            | Self::SchemaNotFoundForDefaultPrivileges { .. }
            | Self::DefaultPrivilegesInSchema { .. }
            | Self::InvalidDefaultPrivilege { .. }
            | Self::SchemaAlreadyExists { .. }
            | Self::RelationNameAlreadyTaken { .. }
            | Self::RelationNotFound { .. }
            | Self::DropDomainNotFound { .. }
            | Self::TypeNotFound { .. }
            | Self::DomainHasDependents { .. }
            | Self::TypeDropRequiredBy { .. }
            | Self::TypeHasDependents { .. }
            | Self::RelationKindMismatch { .. }
            | Self::TypeRequiredBySystem { .. }
            | Self::TypeIsOnlyShell { .. }
            | Self::TypeIsRowType { .. }
            | Self::CannotAlterArrayType { .. }
            | Self::EnumLabelExists { .. }
            | Self::EnumLabelNotFound { .. }
            | Self::NotAnEnum { .. }
            | Self::RangeSubtypeRequired { .. }
            | Self::DuplicateEnumLabel { .. }
            | Self::DuplicateCompositeAttribute { .. }
            | Self::ConstraintUnsupportedOnForeignTable { .. }
            | Self::RepeatedKeyColumn { .. }
            | Self::ForeignOptionsOnTableColumn { .. }
            | Self::RepeatedForeignOption { .. }
            | Self::DropSchemaNotFound { .. }
            | Self::AlterSchemaNotFound { .. }
            | Self::AlterFunctionSchemaNotFound { .. }
            | Self::AlterActionUnsupportedOnRelation { .. }
            | Self::MissingDefaultOperatorClass { .. }
            | Self::ForeignKeyCannotBeImplemented { .. }
            | Self::GenerationExpressionNotDroppable { .. }
            | Self::DatabaseOutsideSqlite { .. }
            | Self::RenameCommandNotAlone { .. }
            | Self::AdoptedIndexInvalid { .. }
            | Self::ReplicaIdentityIndexNotFound { .. }
            | Self::ReplicaIdentityIndexOfOtherTable { .. }
            | Self::ReplicaIdentityIndexRefused { .. }
            | Self::DuplicateReferencedColumnsForForeignKey
            | Self::InvalidForeignKeyUpdateActionForGeneratedColumn
            | Self::InvalidForeignKeyDeleteActionForGeneratedColumn
            | Self::DeferrablePrimaryKeyForForeignKey { .. }
            | Self::DeferrableUniqueKeyForForeignKey { .. }
            | Self::AccessMethodWithoutUniqueSupport { .. }
            | Self::AccessMethodWithoutExclusionSupport { .. }
            | Self::KeyReferenced { .. }
            | Self::AdoptedIndexNotUnique { .. }
            | Self::AdoptedIndexPartial { .. }
            | Self::AdoptedIndexExpressions { .. }
            | Self::AdoptedIndexNotFound { .. }
            | Self::AdoptedIndexWrongTable { .. }
            | Self::AdoptedIndexAlreadyConstrained { .. }
            | Self::AdoptedIndexOnPartitionedTable
            | Self::AdoptedIndexSorting { .. }
            | Self::IndexRequiredByConstraint { .. }
            | Self::IndexRequiredByParent { .. }
            | Self::IndexHasDependentConstraints { .. }
            | Self::MultiplePrimaryKeys { .. }
            | Self::AttachIndexNotSupported { .. }
            | Self::AttachIndexRefused { .. }
            | Self::FunctionRowsNotPositive { .. }
            | Self::FunctionRowsWithoutSet { .. } => None,
            #[cfg(feature = "git")]
            Self::GitError(..) => None,
            #[cfg(feature = "std")]
            Self::IoError(..) => None,
            Self::UnknownColumn { reference } | Self::ForeignColumnReference { reference, .. } => {
                Some(reference.postgres_error())
            }
            Self::InvalidPrimaryKey { written_token: token, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(token)),
                ))
            }
        }
    }
}

impl LookupError {
    /// The SQLSTATE and the message PostgreSQL 18 refuses the statement with,
    /// or `None` for an error PostgreSQL has no counterpart of.
    #[must_use]
    pub fn postgres_error(&self) -> Option<PostgresError> {
        match self {
            Self::InvalidSetting { written_token } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(written_token)),
                ))
            }
            Self::AmbiguousTableLookup { object_name, site, .. } => {
                lookup_columns::ambiguous_lookup(object_name, *site)
            }
            Self::AmbiguousFunctionLookup { object_name, .. } => {
                Some(PostgresError::new(
                    pg::AMBIGUOUS_FUNCTION,
                    format!("function name {} is not unique", quoted(object_name)),
                ))
            }
            Self::TableNotFound { object_name } | Self::RelationNotFound { object_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("relation {} does not exist", quoted(object_name)),
                ))
            }
            Self::TriggerRelationWrongKind { object_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("relation {} cannot have triggers", quoted(object_name)),
                ))
            }
            Self::ReferencedRelationWrongKind { object_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("referenced relation {} is not a table", quoted(object_name)),
                ))
            }
            Self::TableLookupConflict { table, same_stored_identity: true, .. } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_TABLE,
                    format!("relation {} already exists", quoted(table)),
                ))
            }
            Self::ColumnNotFound { table_name, column_name, site } => {
                Some(lookup_columns::missing_column(table_name, column_name, *site))
            }
            Self::ReferencedPrimaryKeyNotFound { referenced_table, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "there is no primary key for referenced table {}",
                        quoted(referenced_table)
                    ),
                ))
            }
            Self::ReferencedColumnsNotUnique { referenced_table, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FOREIGN_KEY,
                    format!(
                        "there is no unique constraint matching given keys for referenced table {}",
                        quoted(referenced_table)
                    ),
                ))
            }
            Self::ForeignKeyColumnCountMismatch { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FOREIGN_KEY,
                    "number of referencing and referenced columns for foreign key disagree",
                ))
            }
            Self::PolicyLiteralUnreadable { literal, data_type, reason, .. } => {
                lookup_columns::policy_literal(literal, *data_type, *reason)
            }
            Self::InvalidObjectName { .. }
            | Self::TableLookupConflict { same_stored_identity: false, .. }
            | Self::ObjectNotInDatabase { .. }
            | Self::CallNotInExpression { .. }
            | Self::ExpressionNotInQuery { .. }
            | Self::ArgumentTypeCount { .. }
            | Self::FunctionBodyNotSql { .. }
            | Self::FunctionBodyUnavailable { .. }
            | Self::FunctionBodyUnresolvable { .. }
            | Self::FunctionBodyUnparsable { .. } => None,
        }
    }
}
