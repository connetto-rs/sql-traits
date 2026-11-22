//! The SQLSTATE and the message PostgreSQL reports for each error that models
//! one of its refusals.

use alloc::string::String;

pub use sqlstate::SqlState;
use sqlstate::pg;

use super::{
    AttachIndexRefusal, Error, LookupError, SequenceIntegerRefusal, SequenceParameterError,
};
use crate::utils::quote_identifier::quote_identifier;

mod alterations_transactions;
mod inheritance_operators;
mod keys_triggers_grants;
mod lookup_columns;
mod persistence_partitions;
mod roles_functions_schemas;
mod types_settings;
mod views_constraints_functions;

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

/// A stored relation name, qualified by its schema when PostgreSQL needs the
/// schema to tell it apart from what the search path reaches.
fn relation_name(name: &str, schema: Option<&str>) -> String {
    let name = quote_identifier(name);
    match schema {
        Some(schema) => format!("{}.{name}", quote_identifier(schema)),
        None => name,
    }
}

/// The keywords that declare a constraint of `kind`, as PostgreSQL's
/// messages print them.
const fn constraint_keyword(kind: super::KeyConstraint) -> &'static str {
    match kind {
        super::KeyConstraint::PrimaryKey => "PRIMARY KEY",
        super::KeyConstraint::Unique => "UNIQUE",
        super::KeyConstraint::Exclusion => "EXCLUDE",
        super::KeyConstraint::ForeignKey => "FOREIGN KEY",
    }
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
            Self::UnresolvedOperator { operator_name, operand_types, reason } => {
                Some(keys_triggers_grants::unresolved(operator_name, operand_types, *reason, true))
            }
            Self::UnresolvedCall { function_name, argument_types, reason } => {
                Some(keys_triggers_grants::unresolved(
                    function_name,
                    argument_types,
                    *reason,
                    false,
                ))
            }
            Self::MisappliedCall { function_name, rule, procedure_argument_types } => {
                if *rule == super::Misapplied::ProcedureCalled {
                    Some(PostgresError::new(
                        pg::WRONG_OBJECT_TYPE,
                        format!(
                            "{function_name}({}) is a procedure",
                            procedure_argument_types.join(", ")
                        ),
                    ))
                } else {
                    Some(keys_triggers_grants::call_rule(function_name, *rule))
                }
            }
            Self::InvalidCollationOption {
                option_name,
                option_value,
                qualified,
                nondeterministic_provider,
            } => {
                Some(keys_triggers_grants::collation_option(
                    option_name,
                    option_value,
                    *qualified,
                    *nondeterministic_provider,
                ))
            }
            Self::RepeatedCollationOption { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "conflicting or redundant options"))
            }
            Self::RepeatedColumnCollation { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "multiple COLLATE clauses not allowed"))
            }
            Self::NonCollatableColumnType { type_name, .. } => {
                Some(PostgresError::new(
                    pg::DATATYPE_MISMATCH,
                    format!("collations are not supported by type {type_name}"),
                ))
            }
            Self::DropTableInheritedFrom { parent_table, schema_name, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop table {} because other objects depend on it",
                        relation_name(parent_table, schema_name.as_deref())
                    ),
                ))
            }
            Self::ReferencedColumnNotFoundForForeignKey { referenced_column, .. }
            | Self::HostColumnNotFoundForForeignKey { host_column: referenced_column, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_COLUMN,
                    format!(
                        "column {} referenced in foreign key constraint does not exist",
                        quoted(referenced_column)
                    ),
                ))
            }
            Self::ReferencedColumnsNotUniqueForForeignKey {
                referenced_table,
                referenced_columns,
                ..
            } => {
                if referenced_columns.is_empty() {
                    Some(PostgresError::new(
                        pg::UNDEFINED_OBJECT,
                        format!(
                            "there is no primary key for referenced table {}",
                            quoted(referenced_table)
                        ),
                    ))
                } else {
                    Some(PostgresError::new(
                        pg::INVALID_FOREIGN_KEY,
                        format!(
                            "there is no unique constraint matching given keys for referenced table {}",
                            quoted(referenced_table)
                        ),
                    ))
                }
            }
            Self::ForeignKeyColumnCountMismatch { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FOREIGN_KEY,
                    "number of referencing and referenced columns for foreign key disagree",
                ))
            }
            Self::DuplicateReferencedColumnsForForeignKey => {
                Some(PostgresError::new(
                    pg::INVALID_FOREIGN_KEY,
                    "foreign key referenced-columns list must not contain duplicates",
                ))
            }
            Self::InvalidForeignKeyUpdateActionForGeneratedColumn => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    "invalid ON UPDATE action for foreign key constraint containing generated column",
                ))
            }
            Self::InvalidForeignKeyDeleteActionForGeneratedColumn => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    "invalid ON DELETE action for foreign key constraint containing generated column",
                ))
            }
            Self::GeneratedColumnReference { column } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    format!(
                        "cannot use generated column {} in column generation expression",
                        quoted(column)
                    ),
                ))
            }
            Self::GeneratedSystemColumnReference { column } => {
                Some(PostgresError::new(
                    pg::INVALID_COLUMN_REFERENCE,
                    format!(
                        "cannot use system column {} in column generation expression",
                        quoted(column)
                    ),
                ))
            }
            Self::GenerationExpressionNotImmutable => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    "generation expression is not immutable",
                ))
            }
            Self::GenerationExpressionUserDefinedType => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "generation expression uses user-defined type",
                ))
            }
            Self::GenerationExpressionUserDefinedFunction => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "generation expression uses user-defined function",
                ))
            }
            Self::DefaultAndGeneration { table, column } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!(
                        "both default and generation expression specified for column {} of table {}",
                        quoted(column),
                        quoted(table)
                    ),
                ))
            }
            Self::VirtualGeneratedIndex => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "indexes on virtual generated columns are not supported",
                ))
            }
            Self::VirtualGeneratedUnique => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "unique constraints on virtual generated columns are not supported",
                ))
            }
            Self::VirtualGeneratedPrimaryKey => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "primary keys on virtual generated columns are not supported",
                ))
            }
            Self::VirtualGeneratedForeignKey => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "foreign key constraints on virtual generated columns are not supported",
                ))
            }
            Self::VirtualGeneratedDomainType { column_name } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!(
                        "virtual generated column {} cannot have a domain type",
                        quoted(column_name)
                    ),
                ))
            }
            Self::VirtualGeneratedUserDefinedType { column_name } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!(
                        "virtual generated column {} cannot have a user-defined type",
                        quoted(column_name)
                    ),
                ))
            }
            Self::GeneratedColumnDefault { table_name, column_name } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!(
                        "column {} of relation {} is a generated column",
                        quoted(column_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::GeneratedColumnTypeUsing => {
                Some(PostgresError::new(
                    pg::INVALID_COLUMN_DEFINITION,
                    "cannot specify USING when altering type of generated column",
                ))
            }
            Self::InheritedGeneratedKindConflict { column_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_COLUMN_DEFINITION,
                    format!(
                        "column {} inherits from generated column of different kind",
                        quoted(column_name)
                    ),
                ))
            }
            Self::BeforeTriggerGeneratedColumn { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    "BEFORE trigger's WHEN condition cannot reference NEW generated columns",
                ))
            }
            Self::BeforeTriggerSystemColumn { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "BEFORE trigger's WHEN condition cannot reference NEW system columns",
                ))
            }
            Self::DeferrablePrimaryKeyForForeignKey { referenced_table, .. } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "cannot use a deferrable primary key for referenced table {}",
                        quoted(referenced_table)
                    ),
                ))
            }
            Self::DeferrableUniqueKeyForForeignKey { referenced_table, .. } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "cannot use a deferrable unique constraint for referenced table {}",
                        quoted(referenced_table)
                    ),
                ))
            }
            Self::AccessMethodWithoutUniqueSupport { access_method } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!(
                        "access method {} does not support unique indexes",
                        quoted(access_method)
                    ),
                ))
            }
            Self::AccessMethodWithoutExclusionSupport { access_method } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!(
                        "access method {} does not support exclusion constraints",
                        quoted(access_method)
                    ),
                ))
            }
            Self::MultiplePrimaryKeys { table_name } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!(
                        "multiple primary keys for table {} are not allowed",
                        quoted(table_name)
                    ),
                ))
            }
            Self::FunctionRowsNotPositive { .. } => {
                Some(PostgresError::new(pg::INVALID_PARAMETER_VALUE, "ROWS must be positive"))
            }
            Self::FunctionRowsWithoutSet { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    "ROWS is not applicable when function does not return a set",
                ))
            }
            Self::InitiallyDeferredNotDeferrable { .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
                ))
            }
            Self::RelationNotFoundForTrigger { relation_name, .. }
            | Self::TableNotFoundForPolicy { table_name: relation_name, .. }
            | Self::RelationNotFoundForRule { relation_name, .. }
            | Self::RelationNotFoundForRuleAction { relation_name, .. }
            | Self::TableNotFoundForIndex { table_name: relation_name, .. }
            | Self::RelationNotFoundForSequenceOwner { relation_name, .. }
            | Self::RelationNotFoundForDefault { relation_name, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("relation {} does not exist", quoted(relation_name)),
                ))
            }
            Self::TriggerFiringRefused { relation_name, reason, .. } => {
                keys_triggers_grants::trigger_firing(relation_name, *reason)
            }
            Self::RuleOnUnsupportedRelation { relation_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("relation {} cannot have rules", quoted(relation_name)),
                ))
            }
            Self::SelectRuleUnsupported { relation_name, refusal: Some(refusal), .. } => {
                Some(match refusal {
                    super::SelectRuleRefusal::Table => {
                        PostgresError::new(
                            pg::WRONG_OBJECT_TYPE,
                            format!(
                                "relation {} cannot have ON SELECT rules",
                                quoted(relation_name)
                            ),
                        )
                    }
                    super::SelectRuleRefusal::ExistingView => {
                        PostgresError::new(
                            pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            format!("{} is already a view", quoted(relation_name)),
                        )
                    }
                    super::SelectRuleRefusal::InvalidViewAction => {
                        PostgresError::new(
                            pg::FEATURE_NOT_SUPPORTED,
                            "rules on SELECT must have action INSTEAD SELECT",
                        )
                    }
                })
            }
            Self::RuleActionReferenceAmbiguous { column_name, .. } => {
                Some(PostgresError::new(
                    pg::AMBIGUOUS_COLUMN,
                    format!("column reference {} is ambiguous", quoted(column_name)),
                ))
            }
            Self::RuleActionInsertTooWide { .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    "INSERT has more expressions than target columns",
                ))
            }
            Self::RoleNotFoundForOwner { role_name, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("role {} does not exist", quoted(role_name)),
                ))
            }
            Self::ColumnNotFoundForGrant { column_name, table_name }
            | Self::ColumnNotFoundForSequenceOwner {
                column_name, relation_name: table_name, ..
            } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_COLUMN,
                    format!(
                        "column {} of relation {} does not exist",
                        quoted(column_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::IndexInAnotherSchema { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "syntax error at or near \".\""))
            }
            Self::InvalidSequenceOwner { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "invalid OWNED BY option"))
            }
            Self::UnqualifiedCommentColumn { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "column name must be qualified"))
            }
            Self::SequenceOwnerInAnotherSchema { .. } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "sequence must be in same schema as table it is linked to",
                ))
            }
            Self::SequenceOwnerKindUnsupported { relation_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("sequence cannot be owned by relation {}", quoted(relation_name)),
                ))
            }
            Self::SequenceRequiredByColumn {
                sequence_name,
                sequence_schema,
                table_name,
                table_schema,
                column_name,
            } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop sequence {} because column {column_name} of table {} requires it",
                        relation_name(sequence_name, sequence_schema.as_deref()),
                        relation_name(table_name, table_schema.as_deref())
                    ),
                ))
            }
            Self::SequenceUsedByDefault { sequence_name, sequence_schema, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop sequence {} because other objects depend on it",
                        relation_name(sequence_name, sequence_schema.as_deref())
                    ),
                ))
            }
            Self::InvalidSequenceParameters { reason, identity, .. } => {
                Some(keys_triggers_grants::sequence_parameters(reason, *identity))
            }
            Self::InvalidRelationNameForDefault { name_parts, .. } => {
                Some(keys_triggers_grants::relation_name(name_parts))
            }
            Self::InvalidIndex { written_token, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(written_token)),
                ))
            }
            Self::SubqueryInExpression { place } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!("cannot use subquery in {place}"),
                ))
            }
            Self::CallInExpression { call, place } => {
                let sqlstate = match call {
                    super::RefusedCall::Aggregate => pg::GROUPING_ERROR,
                    super::RefusedCall::Window => pg::WINDOWING_ERROR,
                    super::RefusedCall::SetReturning => pg::FEATURE_NOT_SUPPORTED,
                };
                Some(PostgresError::new(
                    sqlstate,
                    format!("{call} are not allowed in {}", place.in_call_refusal()),
                ))
            }
            Self::ColumnReferenceInDefault { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "cannot use column reference in DEFAULT expression",
                ))
            }
            Self::FunctionNotFoundForTrigger { function_name, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_FUNCTION,
                    format!("function {function_name}() does not exist"),
                ))
            }
            Self::ColumnTypeCollatabilityNotInCatalog { .. }
            | Self::QualifiedForeignKeyTarget { .. }
            | Self::SelectRuleUnsupported { refusal: None, .. }
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
            | Self::NotValidRefused { kind: None, .. }
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
            | Self::FunctionBodyStatementUnsupported { command_tag: None, .. }
            | Self::DatabaseOutsideSqlite { .. }
            | Self::AlterActionUnsupportedOnRelation { action: None, .. } => None,
            Self::IndexRequiredByParent { index_name, parent_index_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop index {} because index {} requires it",
                        quote_identifier(index_name),
                        quote_identifier(parent_index_name)
                    ),
                ))
            }
            Self::IndexHasDependentConstraints { index_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop index {} because other objects depend on it",
                        quote_identifier(index_name)
                    ),
                ))
            }
            Self::AttachIndexNotSupported { index_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "ALTER action ATTACH PARTITION cannot be performed on relation {}",
                        quoted(index_name)
                    ),
                ))
            }
            Self::AttachIndexRefused { index_name, parent_index_name, reason } => {
                Some(PostgresError::new(
                    match reason {
                        AttachIndexRefusal::NotOnPartition { .. }
                        | AttachIndexRefusal::AttachedElsewhere
                        | AttachIndexRefusal::PartitionHasIndex { .. } => {
                            pg::OBJECT_NOT_IN_PREREQUISITE_STATE
                        }
                        AttachIndexRefusal::DefinitionsDiffer
                        | AttachIndexRefusal::ConstraintMissing { .. } => {
                            pg::INVALID_OBJECT_DEFINITION
                        }
                    },
                    format!(
                        "cannot attach index {} as a partition of index {}",
                        quoted(index_name),
                        quoted(parent_index_name)
                    ),
                ))
            }
            Self::StorageParameterNotNamed { option, .. } => {
                Some(types_settings::storage_option_not_named(option))
            }
            Self::UnrecognizedStorageParameter { parameter_name } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("unrecognized parameter {}", quoted(parameter_name)),
                ))
            }
            Self::UnrecognizedStorageParameterNamespace { namespace } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("unrecognized parameter namespace {}", quoted(namespace)),
                ))
            }
            Self::RepeatedStorageParameter { parameter_name } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("parameter {} specified more than once", quoted(parameter_name)),
                ))
            }
            Self::InvalidStorageParameterValue { parameter_name, value_type, value } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!(
                        "invalid value for {value_type} option {}: {value}",
                        quoted(parameter_name)
                    ),
                ))
            }
            Self::StorageParameterOutOfBounds { parameter_name, value, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("value {value} out of bounds for option {}", quoted(parameter_name)),
                ))
            }
            Self::ForeignKeyCannotBeImplemented { constraint_name, .. } => {
                Some(PostgresError::new(
                    pg::DATATYPE_MISMATCH,
                    format!(
                        "foreign key constraint {} cannot be implemented",
                        quoted(constraint_name)
                    ),
                ))
            }
            Self::MissingDefaultOperatorClass { type_name, access_method } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "data type {type_name} has no default operator class for access method \"{access_method}\""
                    ),
                ))
            }
            Self::ReplicaIdentityIndexNotFound { table_name, index_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "index {} for table {} does not exist",
                        quoted(index_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::ReplicaIdentityIndexOfOtherTable { table_name, index_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "{} is not an index for table {}",
                        quoted(index_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::ReplicaIdentityIndexRefused { index_name, reason, .. } => {
                Some(alterations_transactions::replica_identity_refused(index_name, reason))
            }
            Self::KeyReferenced { key_name, dropping_index: true, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop index {} because other objects depend on it",
                        quote_identifier(key_name)
                    ),
                ))
            }
            Self::KeyReferenced { key_name, table_name, dropping_index: false, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop constraint {key_name} on table {} because other objects depend on it",
                        quote_identifier(table_name)
                    ),
                ))
            }
            Self::IndexRequiredByConstraint { index_name, table_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop index {} because constraint {index_name} on table {} requires it",
                        quote_identifier(index_name),
                        quote_identifier(table_name)
                    ),
                ))
            }
            Self::AdoptedIndexNotUnique { index_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is not a unique index", quoted(index_name)),
                ))
            }
            Self::AdoptedIndexInvalid { index_name } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("index {} is not valid", quoted(index_name)),
                ))
            }
            Self::AdoptedIndexPartial { index_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is a partial index", quoted(index_name)),
                ))
            }
            Self::AdoptedIndexExpressions { index_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("index {} contains expressions", quoted(index_name)),
                ))
            }
            Self::AdoptedIndexNotFound { index_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("index {} does not exist", quoted(index_name)),
                ))
            }
            Self::AdoptedIndexWrongTable { index_name, table_name } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "index {} does not belong to table {}",
                        quoted(index_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::AdoptedIndexAlreadyConstrained { index_name } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("index {} is already associated with a constraint", quoted(index_name)),
                ))
            }
            Self::AdoptedIndexOnPartitionedTable => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "ALTER TABLE / ADD CONSTRAINT USING INDEX is not supported on partitioned tables",
                ))
            }
            Self::AdoptedIndexSorting { index_name, column_number } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "index {} column number {column_number} does not have default sorting behavior",
                        quoted(index_name)
                    ),
                ))
            }
            #[cfg(feature = "git")]
            Self::GitError(..) => None,
            #[cfg(feature = "std")]
            Self::IoError(..) => None,
            Self::TemporaryRelationInPermanentSchema { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    "cannot create temporary relation in non-temporary schema",
                ))
            }
            Self::TemporaryMaterializedView { written_token, .. }
            | Self::PartitionCommandNotAlone { written_token, .. }
            | Self::SetSchemaCommandNotAlone { written_token }
            | Self::ForeignTableCommandUnavailable { written_token, .. }
            | Self::RenameCommandNotAlone { written_token, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(written_token)),
                ))
            }
            Self::SetSchemaNameCollision { object_name, schema_name, type_collision } => {
                let (state, kind) = if *type_collision {
                    (pg::DUPLICATE_OBJECT, "type")
                } else {
                    (pg::DUPLICATE_TABLE, "relation")
                };
                Some(PostgresError::new(
                    state,
                    format!(
                        "{kind} {} already exists in schema {}",
                        quoted(object_name),
                        quoted(schema_name)
                    ),
                ))
            }
            Self::SetSchemaWrongKind { relation_name, object_kind } => {
                let message = if *object_kind == super::ObjectKind::Index {
                    format!("cannot change schema of index {}", quoted(relation_name))
                } else {
                    format!("{} is a composite type", quoted(relation_name))
                };
                Some(PostgresError::new(pg::WRONG_OBJECT_TYPE, message))
            }
            Self::SetSchemaRestrictedNamespace { toast } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    if *toast {
                        "cannot move objects into or out of TOAST schema"
                    } else {
                        "cannot move objects into or out of temporary schemas"
                    },
                ))
            }
            Self::SetSchemaOwnedSequence { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "cannot move an owned sequence into another schema",
                ))
            }
            Self::MaterializedViewReadsTemporaryRelation { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "materialized views must not use temporary tables or views",
                ))
            }
            Self::ForeignKeyCrossesTemporaryStorage {
                host_is_temporary, host_is_unlogged, ..
            } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    if *host_is_temporary {
                        "constraints on temporary tables may reference only temporary tables"
                    } else if *host_is_unlogged {
                        "constraints on unlogged tables may reference only permanent or unlogged tables"
                    } else {
                        "constraints on permanent tables may reference only permanent tables"
                    },
                ))
            }
            Self::ForeignKeyReferencesUnloggedTable {
                host_table,
                referenced_table,
                set_unlogged,
            } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    match set_unlogged {
                        None => {
                            "constraints on permanent tables may reference only permanent tables"
                                .into()
                        }
                        Some(true) => {
                            format!(
                                "could not change table {} to unlogged because it references logged table {}",
                                quoted(referenced_table),
                                quoted(host_table)
                            )
                        }
                        Some(false) => {
                            format!(
                                "could not change table {} to logged because it references unlogged table {}",
                                quoted(host_table),
                                quoted(referenced_table)
                            )
                        }
                    },
                ))
            }
            Self::UnloggedTemporaryTable { explicit_temporary, written_token, .. } => {
                Some(if *explicit_temporary {
                    PostgresError::new(
                        pg::SYNTAX_ERROR,
                        format!("syntax error at or near {}", quoted(written_token)),
                    )
                } else {
                    PostgresError::new(
                        pg::INVALID_TABLE_DEFINITION,
                        "only temporary relations may be created in temporary schemas",
                    )
                })
            }
            Self::UnloggedPartitionedTable { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "partitioned tables cannot be unlogged",
                ))
            }
            Self::PersistenceNotChangeable { table_name, reason, unlogged } => {
                Some(persistence_partitions::persistence(table_name, *reason, *unlogged))
            }
            Self::PersistenceChangedTwice { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "cannot change persistence setting twice",
                ))
            }
            Self::StorageParametersOnPartitionedTable { .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    "cannot specify storage parameters for a partitioned table",
                ))
            }
            Self::PermanentRelationInheritsTemporary { parent_name, partition, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    match partition {
                        None => {
                            format!(
                                "cannot inherit from temporary relation {}",
                                quoted(parent_name)
                            )
                        }
                        Some(attach) => {
                            format!(
                                "cannot {} a permanent relation as partition of temporary relation {}",
                                if *attach { "attach" } else { "create" },
                                quoted(parent_name)
                            )
                        }
                    },
                ))
            }
            Self::TemporaryPartitionOfPermanent { parent_name, attach, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot {} a temporary relation as partition of permanent relation {}",
                        if *attach { "attach" } else { "create" },
                        quoted(parent_name)
                    ),
                ))
            }
            Self::ParentNotPartitioned { parent_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    format!("{} is not partitioned", quoted(parent_name)),
                ))
            }
            Self::PartitionedTableInherits { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    "cannot create partitioned table as inheritance child",
                ))
            }
            Self::ParentRepeated { parent_name, .. } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_TABLE,
                    format!(
                        "relation {} would be inherited from more than once",
                        quoted(parent_name)
                    ),
                ))
            }
            Self::DefaultOnInheritedGeneratedColumn { column_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_COLUMN_DEFINITION,
                    format!(
                        "column {} inherits from generated column but specifies default",
                        quoted(column_name)
                    ),
                ))
            }
            Self::InheritsFromPartitionedTable { parent_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("cannot inherit from partitioned table {}", quoted(parent_name)),
                ))
            }
            Self::InheritanceChildMismatch { table_name, reason, .. } => {
                Some(persistence_partitions::shape(table_name, reason))
            }
            Self::InheritanceChangeOnPartition { .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    "cannot change inheritance of a partition",
                ))
            }
            Self::InheritanceChangeOnPartitioned { .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    "cannot change inheritance of partitioned table",
                ))
            }
            Self::CircularInheritance { .. } => {
                Some(PostgresError::new(pg::DUPLICATE_TABLE, "circular inheritance not allowed"))
            }
            Self::NotAParentOf { table_name, parent_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!(
                        "relation {} is not a parent of relation {}",
                        quoted(parent_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::NotAPartitionOf { table_name, partition_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!(
                        "relation {} is not a partition of relation {}",
                        quoted(partition_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::DetachConcurrentlyInTransaction { .. } => {
                Some(PostgresError::new(
                    pg::ACTIVE_SQL_TRANSACTION,
                    "ALTER TABLE ... DETACH CONCURRENTLY cannot run inside a transaction block",
                ))
            }
            Self::DetachConcurrentlyWithDefault { .. } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot detach partitions concurrently when a default partition exists",
                ))
            }
            Self::NoDetachPending { partition_name, .. } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot complete detaching partition {}", quoted(partition_name)),
                ))
            }
            Self::InheritsFromPartition { parent_name, altering, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    if *altering {
                        "cannot inherit from a partition".into()
                    } else {
                        format!("cannot inherit from partition {}", quoted(parent_name))
                    },
                ))
            }
            Self::PartitionKeyColumnNotFound { column_name, expression, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_COLUMN,
                    if *expression {
                        format!("column {} does not exist", quoted(column_name))
                    } else {
                        format!(
                            "column {} named in partition key does not exist",
                            quoted(column_name)
                        )
                    },
                ))
            }
            Self::GeneratedColumnInPartitionKey { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    "cannot use generated column in partition key",
                ))
            }
            Self::ListPartitionKeyHasManyColumns { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    "cannot use \"list\" partition strategy with more than one column",
                ))
            }
            Self::UniqueConstraintMissesPartitionKey { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "unique constraint on partitioned table must include all partitioning columns",
                ))
            }
            Self::UniqueConstraintOverExpressionPartitionKey { constraint, .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!(
                        "unsupported {} constraint with partition key definition",
                        constraint_keyword(*constraint)
                    ),
                ))
            }
            Self::PartitionBoundInvalid { partition_name, reason, .. } => {
                Some(persistence_partitions::bound(partition_name, reason))
            }
            Self::PartitionBoundOverlaps { partition_name, other_partition, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    format!(
                        "partition {} would overlap partition {}",
                        quoted(partition_name),
                        quoted(other_partition)
                    ),
                ))
            }
            Self::PartitionNotAttachable { table_name, partition_name, reason } => {
                Some(persistence_partitions::attachment(table_name, partition_name, reason))
            }
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
            Self::DefaultPrivilegesForColumns => {
                Some(PostgresError::new(
                    pg::INVALID_GRANT_OPERATION,
                    "default privileges cannot be set for columns",
                ))
            }
            Self::ColumnPrivilegeOnNonRelation { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_GRANT_OPERATION,
                    "column privileges are only valid for relations",
                ))
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
            Self::TypedTableTypeNotComposite { type_display, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("type {type_display} is not a composite type"),
                ))
            }
            Self::TypedTableOfRowType { type_display, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("type {type_display} is the row type of another table"),
                ))
            }
            Self::TypedTable { refusal, .. } => {
                Some(PostgresError::new(
                    match refusal {
                        super::TypedTableRefusal::IdentityColumn
                        | super::TypedTableRefusal::GeneratedColumn => pg::FEATURE_NOT_SUPPORTED,
                        super::TypedTableRefusal::AddColumn
                        | super::TypedTableRefusal::DropColumn
                        | super::TypedTableRefusal::AlterColumnType
                        | super::TypedTableRefusal::RenameColumn
                        | super::TypedTableRefusal::ChangeInheritance => pg::WRONG_OBJECT_TYPE,
                    },
                    refusal.to_string(),
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
            Self::AmbiguousDropFunction { function_name }
            | Self::AmbiguousAlterFunction { function_name } => {
                Some(PostgresError::new(
                    pg::AMBIGUOUS_FUNCTION,
                    format!("function name {} is not unique", quoted(function_name)),
                ))
            }
            Self::AmbiguousDropProcedure { procedure_name } => {
                Some(PostgresError::new(
                    pg::AMBIGUOUS_FUNCTION,
                    format!("procedure name {} is not unique", quoted(procedure_name)),
                ))
            }
            Self::AmbiguousRoutineSignature { routine_name, procedure, .. } => {
                let kind = if *procedure { "procedure" } else { "routine" };
                Some(PostgresError::new(
                    pg::AMBIGUOUS_FUNCTION,
                    format!("{kind} name {} is not unique", quoted(routine_name)),
                ))
            }
            Self::DropExtensionNotFound { extension_name }
            | Self::AlterFunctionExtensionNotFound { extension_name, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("extension {} does not exist", quoted(extension_name)),
                ))
            }
            Self::SchemaNotEmpty { schema_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop schema {schema_name} because other objects depend on it"),
                ))
            }
            Self::RenameTableNotFound { table_name }
            | Self::AlterTableNotFound { table_name }
            | Self::AlterPolicyTableNotFound { table_name }
            | Self::AlterIndexNotFound { index_name: table_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("relation {} does not exist", quoted(table_name)),
                ))
            }
            Self::AlterPolicyRelationNotTable { table_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is not a table", quoted(table_name)),
                ))
            }
            Self::AlterPolicyNotFound { policy_name, table_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "policy {} for table {} does not exist",
                        quoted(policy_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::AlterFunctionNotFound { function_name, argument_types } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_FUNCTION,
                    match argument_types {
                        Some(types) => {
                            format!("function {function_name}({}) does not exist", types.join(", "))
                        }
                        None => {
                            format!("could not find a function named {}", quoted(function_name))
                        }
                    },
                ))
            }
            Self::AlterRoleNotFound { role_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("role {} does not exist", quoted(role_name)),
                ))
            }
            Self::DropConstraintNotFound { table_name, constraint_name }
            | Self::ValidateConstraintNotFound { table_name, constraint_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "constraint {} of relation {} does not exist",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::ConstraintNotValidatable { table_name, constraint_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot validate constraint {} of relation {}",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::NotEnforcedNotValidatable { .. } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot validate NOT ENFORCED constraint",
                ))
            }
            Self::NotValidRefused { kind: Some(kind), .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!("{} constraints cannot be marked NOT VALID", constraint_keyword(*kind)),
                ))
            }
            Self::NotValidConstraintConflict { table_name, constraint_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    format!(
                        "constraint {} conflicts with NOT VALID constraint on relation {}",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::NoOpenTransaction { statement } => {
                alterations_transactions::no_transaction(statement)
            }
            Self::SavepointNotFound { savepoint_name } => {
                Some(PostgresError::new(
                    pg::S_E_INVALID_SPECIFICATION,
                    format!("savepoint {} does not exist", quoted(savepoint_name)),
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
            Self::ReferencedTableNotFoundForForeignKey { referenced_table: name, .. }
            | Self::ParentTableNotFound { parent_table: name, .. }
            | Self::CopiedTableNotFound { copied_table: name, object_kind: None, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("relation {} does not exist", quoted(name)),
                ))
            }
            Self::CopiedTableNotFound { copied_table, object_kind: Some(kind), .. } => {
                match kind {
                    super::ObjectKind::View
                    | super::ObjectKind::MaterializedView
                    | super::ObjectKind::Type => None,
                    _ => {
                        Some(PostgresError::new(
                            pg::WRONG_OBJECT_TYPE,
                            format!("relation {} is invalid in LIKE clause", quoted(copied_table)),
                        ))
                    }
                }
            }
            Self::InheritsFromNonTable {
                parent_name,
                object_kind:
                    super::ObjectKind::Type | super::ObjectKind::Index | super::ObjectKind::UniqueIndex,
                ..
            } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("cannot open relation {}", quoted(parent_name)),
                ))
            }
            Self::InheritsFromNonTable { parent_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "inherited relation {} is not a table or foreign table",
                        quoted(parent_name)
                    ),
                ))
            }
            Self::InheritedColumnNotDroppable { column_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!("cannot drop inherited column {}", quoted(column_name)),
                ))
            }
            Self::InheritedColumnNotRenamable { column_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!("cannot rename inherited column {}", quoted(column_name)),
                ))
            }
            Self::ColumnAddedToPartition { .. } => {
                Some(PostgresError::new(pg::WRONG_OBJECT_TYPE, "cannot add column to a partition"))
            }
            Self::IdentityAddedToPartitionColumn { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    "cannot add identity to a column of a partition",
                ))
            }
            Self::IdentityAddedToOnlyPartitionedTable { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    "cannot add identity to a column of only the partitioned table",
                ))
            }
            Self::GenerationExpressionNotDroppable { table_name, column_name, reason } => {
                Some(inheritance_operators::generation_drop(*reason, column_name, table_name))
            }
            Self::InheritedColumnTypeNotAlterable { column_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!("cannot alter inherited column {}", quoted(column_name)),
                ))
            }
            Self::InheritedConstraintNotDroppable { table_name, constraint_name }
            | Self::RequiredValueNotDroppable {
                table_name,
                constraint_name: Some(constraint_name),
                ..
            } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!(
                        "cannot drop inherited constraint {} of relation {}",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::OnlyRefusedWithChildren { change, subject_name, .. } => {
                Some(inheritance_operators::only(*change, subject_name.as_deref()))
            }
            Self::InheritedConstraintConflict { table_name, constraint_name, reason } => {
                inheritance_operators::constraint_conflict(reason, constraint_name, table_name)
            }
            Self::NoInheritCheckOnPartitionedTable { table_name } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!(
                        "cannot add NO INHERIT constraint to partitioned table {}",
                        quoted(table_name)
                    ),
                ))
            }
            Self::OnlyForeignKeyOnPartitionedTable { table_name, referenced_table } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot use ONLY for foreign key on partitioned table {} referencing relation {}",
                        quoted(table_name),
                        quoted(referenced_table)
                    ),
                ))
            }
            Self::NoInheritNotNullOnPartitionedTable { .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    "not-null constraints on partitioned tables cannot be NO INHERIT",
                ))
            }
            Self::ConflictingNotNullConstraintNames { first_name, second_name, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!(
                        "conflicting not-null constraint names {} and {}",
                        quoted(first_name),
                        quoted(second_name)
                    ),
                ))
            }
            Self::ConflictingNoInheritNotNull { column_name, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!(
                        "conflicting NO INHERIT declaration for not-null constraint on column {}",
                        quoted(column_name)
                    ),
                ))
            }
            Self::NotNullConstraintAlreadyExists {
                table_name,
                column_name,
                constraint_name,
                ..
            } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "cannot create not-null constraint {} on column {} of table {}",
                        quoted(constraint_name),
                        quoted(column_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::NotNullNoInheritStatusChange { table_name, constraint_name, set_not_null } => {
                Some(PostgresError::new(
                    if *set_not_null {
                        pg::FEATURE_NOT_SUPPORTED
                    } else {
                        pg::OBJECT_NOT_IN_PREREQUISITE_STATE
                    },
                    format!(
                        "cannot change NO INHERIT status of NOT NULL constraint {} on relation {}",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::OnlyPrimaryKeyOnNullableColumn { table_name, column_name } => {
                Some(PostgresError::new(
                    pg::INTERNAL_ERROR,
                    format!(
                        "column {} of table {} is not marked NOT NULL",
                        quoted(column_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::RequiredValueNotDroppable { column_name, reason, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    match reason {
                        super::RequiredValue::CoveredByKey => {
                            format!("column {} is in a primary key", quoted(column_name))
                        }
                        super::RequiredValue::EnforcedByParent => {
                            format!(
                                "column {} is marked NOT NULL in parent table",
                                quoted(column_name)
                            )
                        }
                        super::RequiredValue::InReplicaIdentity => {
                            format!(
                                "column {} is in index used as replica identity",
                                quoted(column_name)
                            )
                        }
                    },
                ))
            }
            Self::IdentityNeedsRequiredValue { table_name, column_name } => {
                Some(PostgresError::new(
                    pg::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "column {} of relation {} must be declared NOT NULL before identity can be added",
                        quoted(column_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::InheritedColumnTypeConflict { column_name, child_table, origin, .. } => {
                Some(PostgresError::new(
                    pg::DATATYPE_MISMATCH,
                    inheritance_operators::column_conflict(
                        *origin,
                        column_name,
                        child_table,
                        "type",
                    ),
                ))
            }
            Self::InheritedColumnCollationConflict { column_name, child_table, origin, .. } => {
                Some(PostgresError::new(
                    pg::COLLATION_MISMATCH,
                    inheritance_operators::column_conflict(
                        *origin,
                        column_name,
                        child_table,
                        "collation",
                    ),
                ))
            }
            Self::CollationCannotBeCopied { collation_name } => {
                Some(PostgresError::new(
                    pg::INVALID_OBJECT_DEFINITION,
                    format!("collation {} cannot be copied", quoted(collation_name)),
                ))
            }
            Self::CollationNotFound { collation_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "collation {} for encoding \"UTF8\" does not exist",
                        quoted(collation_name)
                    ),
                ))
            }
            Self::CollationAlreadyExists { collation_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!("collation {} already exists", quoted(collation_name)),
                ))
            }
            Self::OperatorRightArgumentRequired { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    "operator right argument type must be specified",
                ))
            }
            Self::OperatorFunctionNotFound { function_name, argument_types, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_FUNCTION,
                    format!(
                        "function {}({}) does not exist",
                        function_name.join("."),
                        argument_types.join(", ")
                    ),
                ))
            }
            Self::OperatorAlreadyExists { operator_name, schema_move } => {
                Some(if *schema_move {
                    PostgresError::new(
                        pg::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_operator_oprname_l_r_n_index\"",
                    )
                } else {
                    PostgresError::new(
                        pg::DUPLICATE_FUNCTION,
                        format!("operator {operator_name} already exists"),
                    )
                })
            }
            Self::OperatorOwnNegator { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    "operator cannot be its own negator",
                ))
            }
            Self::OperatorAttributeNeedsBoolean { attribute, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "only boolean operators can {}",
                        inheritance_operators::attribute_requirement(*attribute)
                    ),
                ))
            }
            Self::OperatorAttributeNeedsBinary { attribute, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "only binary operators can {}",
                        inheritance_operators::attribute_requirement(*attribute)
                    ),
                ))
            }
            Self::OperatorLinkTaken { attribute, linked_name, linked_to, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "{attribute} operator {linked_name} is already the {attribute} of operator {linked_to}"
                    ),
                ))
            }
            Self::OperatorFunctionNotExecutable { function_name, .. } => {
                Some(PostgresError::new(
                    pg::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for function {function_name}"),
                ))
            }
            Self::OperatorNotFound { signature, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_FUNCTION,
                    format!("operator does not exist: {signature}"),
                ))
            }
            Self::OperatorRequiredBySystem { signature, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop operator {signature} because it is required by the database system"
                    ),
                ))
            }
            Self::OperatorReferenced { signature, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop operator {signature} because other objects depend on it"),
                ))
            }
            Self::OperatorAttributeAlreadySet { attribute, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "operator attribute \"{attribute}\" cannot be changed if it has already been set"
                    ),
                ))
            }
            Self::RoleNotFoundForGrant { role_name }
            | Self::RoleNotFoundForMembership { role_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("role {} does not exist", quoted(role_name)),
                ))
            }
            Self::CircularRoleMembership { role_name, member_name } => {
                Some(PostgresError::new(
                    pg::INVALID_GRANT_OPERATION,
                    format!(
                        "role {} is a member of role {}",
                        quoted(role_name),
                        quoted(member_name)
                    ),
                ))
            }
            Self::UnrecognizedRoleOption { option } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("unrecognized role option {}", quoted(option)),
                ))
            }
            Self::DependentRoleMemberships { .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    "dependent privileges exist",
                ))
            }
            Self::TableNotFoundForGrant { table_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("relation {} does not exist", quoted(table_name)),
                ))
            }
            Self::GrantOnIndex { index_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is an index", quoted(index_name)),
                ))
            }
            Self::GrantOnCompositeType { type_name } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is a composite type", quoted(type_name)),
                ))
            }
            Self::FunctionNotFoundForGrant { function_name, signature, procedure } => {
                Some(roles_functions_schemas::missing_routine(
                    function_name,
                    signature.as_deref(),
                    *procedure,
                ))
            }
            Self::AmbiguousFunctionForGrant { function_name, routine } => {
                Some(PostgresError::new(
                    pg::AMBIGUOUS_FUNCTION,
                    format!(
                        "{} name {} is not unique",
                        roles_functions_schemas::routine_kind(*routine),
                        quoted(function_name)
                    ),
                ))
            }
            Self::InvalidFunctionPrivilege { privilege, routine, columns } => {
                Some(roles_functions_schemas::invalid_privilege(privilege, *routine, *columns))
            }
            Self::DropFunctionNotFound { function_name, signature } => {
                Some(roles_functions_schemas::missing_routine(
                    function_name,
                    signature.as_deref(),
                    false,
                ))
            }
            Self::DropProcedureNotFound { procedure_name, signature } => {
                Some(roles_functions_schemas::missing_routine(
                    procedure_name,
                    signature.as_deref(),
                    true,
                ))
            }
            Self::NotAFunction { signature, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{signature} is not a function"),
                ))
            }
            Self::NotAProcedure { signature, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{signature} is not a procedure"),
                ))
            }
            Self::ProcedureBodyUnsupported { written_token, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(written_token)),
                ))
            }
            Self::FunctionReferenced { signature, procedure, dropping_object, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    match dropping_object {
                        Some(super::FunctionOwner::Extension { extension_name }) => {
                            format!(
                                "cannot drop extension {extension_name} because other objects depend on it"
                            )
                        }
                        Some(super::FunctionOwner::Type { type_description }) => {
                            format!(
                                "cannot drop type {type_description} because other objects depend on it"
                            )
                        }
                        None => {
                            let kind = if *procedure { "procedure" } else { "function" };
                            format!(
                                "cannot drop {kind} {signature} because other objects depend on it"
                            )
                        }
                    },
                ))
            }
            Self::FunctionRequiredByExtension { signature, extension_name, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop function {signature} because extension {extension_name} requires it"
                    ),
                ))
            }
            Self::FunctionRequiredByType { signature, type_description, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop function {signature} because type {type_description} requires it"
                    ),
                ))
            }
            Self::DropTableNotFound { table_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("table {} does not exist", quoted(table_name)),
                ))
            }
            Self::TableReferenced { table_name, schema_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop table {} because other objects depend on it",
                        relation_name(table_name, schema_name.as_deref())
                    ),
                ))
            }
            Self::ColumnReferenced { table_name, column_name, schema_name } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop column {column_name} of table {} because other objects depend on it",
                        relation_name(table_name, schema_name.as_deref())
                    ),
                ))
            }
            Self::ColumnTypeChangeBlocked { dependent_kind, .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    match dependent_kind {
                        super::ObjectKind::Function => {
                            "cannot alter type of a column used by a function or procedure"
                        }
                        super::ObjectKind::Policy => {
                            "cannot alter type of a column used in a policy definition"
                        }
                        super::ObjectKind::Trigger => {
                            "cannot alter type of a column used in a trigger definition"
                        }
                        _ => "cannot alter type of a column used by a view or rule",
                    },
                ))
            }
            Self::ColumnAlreadyExists { table_name, column_name, declaration } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_COLUMN,
                    if *declaration {
                        format!("column {} specified more than once", quoted(column_name))
                    } else {
                        format!(
                            "column {} of relation {} already exists",
                            quoted(column_name),
                            quoted(table_name)
                        )
                    },
                ))
            }
            Self::SchemaNotFoundForRelation { schema_name, .. }
            | Self::SchemaNotFoundForCollation { schema_name, .. }
            | Self::SchemaNotFoundForExtension { schema_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_SCHEMA_NAME,
                    format!("schema {} does not exist", quoted(schema_name)),
                ))
            }
            Self::NoSchemaSelectedForExtension { .. }
            | Self::NoSchemaSelectedForRelation { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_SCHEMA_NAME,
                    "no schema has been selected to create in",
                ))
            }
            Self::SystemSchemaRelationRefused { relation_name, .. } => {
                Some(PostgresError::new(
                    pg::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to create \"pg_catalog.{relation_name}\""),
                ))
            }
            Self::ForeignOptionNotFound { option_name, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!("option {} not found", quoted(option_name)),
                ))
            }
            Self::UniqueIndexOverForeignPartition { table_name, creating_partition, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!(
                        "cannot create {} partitioned table {}",
                        if *creating_partition {
                            "foreign partition of"
                        } else {
                            "unique index on"
                        },
                        quoted(table_name)
                    ),
                ))
            }
            Self::ViewColumnRenamedByReplace { existing_column, new_column, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!(
                        "cannot change name of view column {} to {}",
                        quoted(existing_column),
                        quoted(new_column)
                    ),
                ))
            }
            Self::ViewColumnsDroppedByReplace { .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    "cannot drop columns from view",
                ))
            }
            Self::MaterializedViewCannotBeReplaced { written_token, .. }
            | Self::ViewIfNotExistsUnsupported { written_token, .. }
            | Self::FunctionBodySyntaxUnsupported { written_token, .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("syntax error at or near {}", quoted(written_token)),
                ))
            }
            Self::ViewColumnListTooLong { .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    "CREATE VIEW specifies more column names than columns",
                ))
            }
            Self::TableColumnListTooLong { .. } => {
                Some(PostgresError::new(pg::SYNTAX_ERROR, "too many column names were specified"))
            }
            Self::UnrecognizedViewOption { option_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("unrecognized parameter {}", quoted(option_name)),
                ))
            }
            Self::InvalidViewOptionValue { option_name, option_value, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!(
                        "invalid value for {} option {}: {option_value}",
                        if option_name == "check_option" { "enum" } else { "boolean" },
                        quoted(option_name)
                    ),
                ))
            }
            Self::RepeatedViewOption { option_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_PARAMETER_VALUE,
                    format!("parameter {} specified more than once", quoted(option_name)),
                ))
            }
            Self::RelationHasDependents { object_kind, object_name, schema_name, .. } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop {} {} because other objects depend on it",
                        views_constraints_functions::object_kind(*object_kind),
                        relation_name(object_name, schema_name.as_deref())
                    ),
                ))
            }
            Self::AlterActionUnsupportedOnRelation {
                relation_name, action: Some(action), ..
            } => {
                use super::RelationAlterAction;
                Some(match action {
                    RelationAlterAction::Command(action) => {
                        PostgresError::new(
                            pg::WRONG_OBJECT_TYPE,
                            format!(
                                "ALTER action {action} cannot be performed on relation {}",
                                quoted(relation_name)
                            ),
                        )
                    }
                    RelationAlterAction::RenameColumns => {
                        PostgresError::new(
                            pg::WRONG_OBJECT_TYPE,
                            format!("cannot rename columns of relation {}", quoted(relation_name)),
                        )
                    }
                    RelationAlterAction::ForeignConstraint(kind) => {
                        PostgresError::new(
                            pg::FEATURE_NOT_SUPPORTED,
                            format!(
                                "{} constraints are not supported on foreign tables",
                                types_settings::constraint(*kind)
                            ),
                        )
                    }
                })
            }
            Self::RenameConstraintNotFound { table_name, constraint_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "constraint {} for table {} does not exist",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::InheritedConstraintNotRenamable { constraint_name, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_TABLE_DEFINITION,
                    format!("cannot rename inherited constraint {}", quoted(constraint_name)),
                ))
            }
            Self::ConstraintAlreadyExists { table_name, constraint_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!(
                        "constraint {} for relation {} already exists",
                        quoted(constraint_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::PolicyAlreadyExists { policy_name, table_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!(
                        "policy {} for table {} already exists",
                        quoted(policy_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::TriggerAlreadyExists { trigger_name, relation_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!(
                        "trigger {} for relation {} already exists",
                        quoted(trigger_name),
                        quoted(relation_name)
                    ),
                ))
            }
            Self::ClonedTriggerDropped {
                trigger_name,
                table_name,
                parent_table_name,
                table_schema,
                parent_table_schema,
            } => {
                Some(PostgresError::new(
                    pg::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop trigger {trigger_name} on table {} because trigger {trigger_name} on table {} requires it",
                        relation_name(table_name, table_schema.as_deref()),
                        relation_name(parent_table_name, parent_table_schema.as_deref())
                    ),
                ))
            }
            Self::ClonedTriggerReplaced { trigger_name, table_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!(
                        "trigger {} for relation {} is an internal or a child trigger",
                        quoted(trigger_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::ConstraintTriggerOnForeignTable { table_name, .. } => {
                Some(PostgresError::new(
                    pg::WRONG_OBJECT_TYPE,
                    format!("{} is a foreign table", quoted(table_name)),
                ))
            }
            Self::AlterTableTriggerNotFound { trigger_name, table_name } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_OBJECT,
                    format!(
                        "trigger {} for table {} does not exist",
                        quoted(trigger_name),
                        quoted(table_name)
                    ),
                ))
            }
            Self::RuleAlreadyExists { rule_name, relation_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!(
                        "rule {} for relation {} already exists",
                        quoted(rule_name),
                        quoted(relation_name)
                    ),
                ))
            }
            Self::ExtensionAlreadyExists { extension_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!("extension {} already exists", quoted(extension_name)),
                ))
            }
            Self::RoleAlreadyExists { role_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_OBJECT,
                    format!("role {} already exists", quoted(role_name)),
                ))
            }
            Self::FunctionAlreadyExists { function_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_FUNCTION,
                    format!(
                        "function {} already exists with same argument types",
                        quoted(function_name)
                    ),
                ))
            }
            Self::FunctionAlreadyExistsInSchema { function_name, argument_types, schema_name } => {
                Some(PostgresError::new(
                    pg::DUPLICATE_FUNCTION,
                    format!(
                        "function {}({}) already exists in schema {}",
                        quote_identifier(function_name),
                        argument_types.join(", "),
                        quoted(schema_name)
                    ),
                ))
            }
            Self::FunctionNotExecutableByCreator { function_name, .. } => {
                Some(PostgresError::new(
                    pg::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for function {function_name}"),
                ))
            }
            Self::FunctionKindChange { .. } => {
                Some(PostgresError::new(pg::WRONG_OBJECT_TYPE, "cannot change routine kind"))
            }
            Self::RelationNotFoundForFunctionBody { relation_name, .. } => {
                Some(PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("relation {} does not exist", quoted(relation_name)),
                ))
            }
            Self::FunctionBodyStatementUnsupported { command_tag: Some(command_tag), .. } => {
                Some(PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!("{command_tag} is not yet supported in unquoted SQL function body"),
                ))
            }
            Self::FunctionBodyResultMissing { return_type, .. } => {
                Some(PostgresError::new(
                    pg::INVALID_FUNCTION_DEFINITION,
                    format!("return type mismatch in function declared to return {return_type}"),
                ))
            }
            Self::FunctionBodyReferenceAmbiguous { column_name, .. } => {
                Some(PostgresError::new(
                    pg::AMBIGUOUS_COLUMN,
                    format!("column reference {} is ambiguous", quoted(column_name)),
                ))
            }
            Self::FunctionBodyInsertTooWide { .. } => {
                Some(PostgresError::new(
                    pg::SYNTAX_ERROR,
                    "INSERT has more expressions than target columns",
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
            | Self::CheckNotOfTable { .. }
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

impl Error {
    /// The token PostgreSQL reports this syntax error at, as written, for an
    /// error that carries one.
    pub(crate) fn written_token_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::InvalidIndex { written_token, .. }
            | Self::InvalidPrimaryKey { written_token, .. }
            | Self::IdentifierLookupError(LookupError::InvalidSetting { written_token })
            | Self::ProcedureBodyUnsupported { written_token, .. }
            | Self::TemporaryMaterializedView { written_token, .. }
            | Self::UnloggedTemporaryTable { written_token, .. }
            | Self::PartitionCommandNotAlone { written_token, .. }
            | Self::RenameCommandNotAlone { written_token, .. }
            | Self::SetSchemaCommandNotAlone { written_token }
            | Self::ForeignTableCommandUnavailable { written_token, .. }
            | Self::InvalidSessionRole { written_token, .. }
            | Self::NonLiteralSetting { written_token, .. }
            | Self::MaterializedViewCannotBeReplaced { written_token, .. }
            | Self::ViewIfNotExistsUnsupported { written_token, .. }
            | Self::FunctionBodySyntaxUnsupported { written_token, .. }
            | Self::InvalidSequenceParameters {
                reason:
                    SequenceParameterError::NotABigint {
                        refusal: SequenceIntegerRefusal::UnexpectedToken { written_token },
                        ..
                    },
                ..
            } => Some(written_token),
            _ => None,
        }
    }
}
