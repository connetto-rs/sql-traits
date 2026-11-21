//! The SQLSTATE and the message PostgreSQL reports for each error that models
//! one of its refusals.

use alloc::string::String;

pub use sqlstate::SqlState;
use sqlstate::pg;

use super::{Error, LookupError};

mod lookup_columns;
mod types_settings;

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
            | Self::AlterActionUnsupportedOnRelation { .. }
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
            Self::MissingDefaultOperatorClass { type_name, access_method } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "data type {type_name} has no default operator class for access method \"{access_method}\""
                    ),
                ))
            }
            #[cfg(feature = "git")]
            Self::GitError(..) => None,
            #[cfg(feature = "std")]
            Self::IoError(..) => None,
            Self::NoSchemaSelectedForCollation { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_SCHEMA_NAME,
                    "no schema has been selected to create in",
                ))
            }
            Self::RoleNotFoundForPolicy { role_name, .. }
            | Self::DropRoleNotFound { role_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("role {} does not exist", quoted(role_name)),
                ))
            }
            Self::DropIndexNotFound { index_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("index {} does not exist", quoted(index_name)),
                ))
            }
            Self::DropTriggerNotFound { trigger_name, table_name, table_exists } => {
                table_name.as_ref().map(|table_name| {
                    if *table_exists {
                        PostgresError::new(
                            pg::UNDEFINED_OBJECT,
                            format!(
                                "trigger {} for table {} does not exist",
                                quoted(trigger_name),
                                quoted(table_name),
                            ),
                        )
                    } else {
                        PostgresError::new(
                            pg::UNDEFINED_TABLE,
                            format!("relation {} does not exist", quoted(table_name)),
                        )
                    }
                })
            }
            Self::DropPolicyNotFound { policy_name, table_name, table_exists } => {
                Some(if *table_exists {
                    PostgresError::new(
                        pg::UNDEFINED_OBJECT,
                        format!(
                            "policy {} for table {} does not exist",
                            quoted(policy_name),
                            quoted(table_name),
                        ),
                    )
                } else {
                    PostgresError::new(
                        pg::UNDEFINED_TABLE,
                        format!("relation {} does not exist", quoted(table_name)),
                    )
                })
            }
            Self::RoleReferenced { role_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "role {} cannot be dropped because some objects depend on it",
                        quoted(role_name)
                    ),
                ))
            }
            Self::RoleInUseBySession { refusal, .. } => Some(types_settings::role_in_use(*refusal)),
            Self::RoleNotFoundForSession { role_name } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("role {} does not exist", quoted(role_name)),
                ))
            }
            Self::InvalidSessionRole { written_token: token, .. }
            | Self::NonLiteralSetting { written_token: token, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(token)),
                ))
            }
            Self::MultipleSessionRoles { setting, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("SET {setting} takes only one argument"),
                ))
            }
            Self::InvalidTimeZone { value, reason } => {
                Some(types_settings::timezone(value, *reason))
            }
            Self::QualifiedDefaultPrivilegesSchema { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "syntax error at or near \".\""))
            }
            Self::SchemaNotFoundForDefaultPrivileges { schema_name } => {
                Some(PostgresError::new(
                    pg::INVALID_SCHEMA_NAME,
                    format!("schema {} does not exist", quoted(schema_name)),
                ))
            }
            Self::DefaultPrivilegesInSchema { object_type } => {
                Some(PostgresError::new(
                    pg::INVALID_GRANT_OPERATION,
                    format!("cannot use IN SCHEMA clause when using GRANT/REVOKE ON {object_type}"),
                ))
            }
            Self::InvalidDefaultPrivilege { privilege, object_type } => {
                Some(types_settings::default_privilege(privilege, object_type))
            }
            Self::SchemaAlreadyExists { schema_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_SCHEMA,
                    format!("schema {} already exists", quoted(schema_name)),
                ))
            }
            Self::RelationNameAlreadyTaken { type_collision, object_name, .. } => {
                Some(types_settings::name_taken(*type_collision, object_name))
            }
            Self::RelationNotFound { object_kind, object_name, drop_statement } => {
                Some(types_settings::relation_missing(object_name, *object_kind, *drop_statement))
            }
            Self::DropDomainNotFound { domain_name: type_name }
            | Self::TypeNotFound { type_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("type {} does not exist", quoted(type_name)),
                ))
            }
            Self::DomainHasDependents { type_display: type_name, .. }
            | Self::TypeDropRequiredBy {
                type_display: type_name,
                requiring_kind: super::ObjectKind::Operator | super::ObjectKind::Cast,
                ..
            }
            | Self::TypeHasDependents { type_display: type_name, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop type {type_name} because other objects depend on it"),
                ))
            }
            Self::RelationKindMismatch { object_name, expected_kind, context, .. } => {
                Some(types_settings::kind_mismatch(object_name, *expected_kind, *context))
            }
            Self::TypeRequiredBySystem { type_display: type_name, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop type {type_name} because it is required by the database system"
                    ),
                ))
            }
            Self::TypeIsOnlyShell { type_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("type {} is only a shell", quoted(type_name)),
                ))
            }
            Self::TypeDropRequiredBy {
                type_display: type_name,
                requiring_kind,
                requiring_display: requiring_name,
                ..
            } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop type {type_name} because {} {requiring_name} requires it",
                        types_settings::kind(*requiring_kind)
                    ),
                ))
            }
            Self::TypeIsRowType { type_display: type_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{type_name} is a table's row type"),
                ))
            }
            Self::CannotAlterArrayType { type_display: array_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("cannot alter array type {array_name}"),
                ))
            }
            Self::EnumLabelExists { label, .. } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!("enum label {} already exists", quoted(label)),
                ))
            }
            Self::EnumLabelNotFound { label, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("{} is not an existing enum label", quoted(label)),
                ))
            }
            Self::NotAnEnum { type_display: type_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{type_name} is not an enum"),
                ))
            }
            Self::RangeSubtypeRequired { empty_attributes, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    if *empty_attributes {
                        "syntax error at or near \")\""
                    } else {
                        "type attribute \"subtype\" is required"
                    },
                ))
            }
            Self::DuplicateEnumLabel { .. } => {
                Some(PostgresError::new(
                    pg::UNIQUE_VIOLATION,
                    "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
                ))
            }
            Self::DuplicateCompositeAttribute { attribute, .. } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_COLUMN,
                    format!("column {} specified more than once", quoted(attribute)),
                ))
            }
            Self::ConstraintUnsupportedOnForeignTable { constraint, .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!(
                        "{} constraints are not supported on foreign tables",
                        types_settings::constraint(*constraint)
                    ),
                ))
            }
            Self::RepeatedKeyColumn { column_name, constraint, .. } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_COLUMN,
                    format!(
                        "column {} appears twice in {} constraint",
                        quoted(column_name),
                        types_settings::constraint(*constraint)
                    ),
                ))
            }
            Self::ForeignOptionsOnTableColumn { table_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is not a foreign table", quoted(table_name)),
                ))
            }
            Self::RepeatedForeignOption { option_name, .. } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!("option {} provided more than once", quoted(option_name)),
                ))
            }
            Self::DropSchemaNotFound { schema_name }
            | Self::AlterSchemaNotFound { schema_name }
            | Self::AlterFunctionSchemaNotFound { schema_name, .. }
            | Self::SchemaNotFound { schema_name } => {
                Some(PostgresError::new(
                    pg::INVALID_SCHEMA_NAME,
                    format!("schema {} does not exist", quoted(schema_name)),
                ))
            }
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
