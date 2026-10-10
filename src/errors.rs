//! Error enumeration used in the `sql_traits` crate.

use alloc::{boxed::Box, string::String, vec::Vec};

use sqlparser::parser::ParserError;

use crate::structs::IdentifierCase;

/// Kind of database object a metadata lookup was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// A base table.
    Table,
    /// A view declared by a `CREATE VIEW` statement.
    View,
    /// A view declared by a `CREATE MATERIALIZED VIEW` statement.
    MaterializedView,
    /// A column declared inside a table.
    Column,
    /// An index declared by a `CREATE INDEX` statement.
    Index,
    /// A unique constraint declared inside a `CREATE TABLE` statement.
    UniqueIndex,
    /// A `CHECK` constraint declared inside a `CREATE TABLE` statement.
    CheckConstraint,
    /// A row level security policy.
    Policy,
    /// A function declared by a `CREATE FUNCTION` statement.
    Function,
    /// A trigger declared by a `CREATE TRIGGER` statement.
    Trigger,
    /// A role declared by a `CREATE ROLE` statement.
    Role,
    /// A schema declared by a `CREATE SCHEMA` statement.
    Schema,
}

impl core::fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Table => "Table",
            Self::View => "View",
            Self::MaterializedView => "Materialized view",
            Self::Column => "Column",
            Self::Index => "Index",
            Self::UniqueIndex => "Unique index",
            Self::CheckConstraint => "Check constraint",
            Self::Policy => "Policy",
            Self::Function => "Function",
            Self::Trigger => "Trigger",
            Self::Role => "Role",
            Self::Schema => "Schema",
        })
    }
}

/// A change to a table that PostgreSQL refuses to keep from the tables below.
///
/// `ONLY` asks for the named table alone, and PostgreSQL grants that only when
/// what is asked for can be undone one table at a time. Adding is not one of
/// those, because a table below would then be missing something its parent
/// declares, and neither is renaming, because the two would disagree on what
/// the column is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InheritedChange {
    /// `ALTER TABLE ONLY ... ADD CONSTRAINT`.
    AddConstraint,
    /// `ALTER TABLE ONLY ... ADD COLUMN`.
    AddColumn,
    /// `ALTER TABLE ONLY ... RENAME COLUMN`.
    RenameColumn,
}

impl core::fmt::Display for InheritedChange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::AddConstraint => "add a constraint to",
            Self::AddColumn => "add a column to",
            Self::RenameColumn => "rename a column of",
        })
    }
}

/// Why a column may not stop requiring a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequiredValue {
    /// A key on the same table covers the column, and a key never matches a row
    /// holding nothing.
    CoveredByKey,
    /// A parent requires it, so only the parent may lift the requirement.
    EnforcedByParent,
}

impl core::fmt::Display for RequiredValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::CoveredByKey => "a key covers it",
            Self::EnforcedByParent => "a parent requires it",
        })
    }
}

impl ObjectKind {
    /// Reports an object of this kind that the queried database does not hold,
    /// identified by its own `name`.
    #[must_use]
    pub fn not_in_database(self, name: &str) -> LookupError {
        LookupError::ObjectNotInDatabase { object_kind: self, object: format!("`{name}`") }
    }

    /// Reports an anonymous object of this kind that the queried database does
    /// not hold.
    ///
    /// A `CHECK` or `UNIQUE` constraint written without a name has no identity
    /// of its own, so it is reported by the table it is declared on.
    #[must_use]
    pub fn anonymous_not_in_database(self, table_name: &str) -> LookupError {
        LookupError::ObjectNotInDatabase {
            object_kind: self,
            object: format!("declared on table `{table_name}`"),
        }
    }
}

/// Errors produced by identifier-aware lookup and resolution APIs.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// The provided SQL object name is not supported by the resolver.
    #[error("Invalid object name `{object_name}`: {reason}")]
    InvalidObjectName {
        /// Original object name as rendered by sqlparser.
        object_name: String,
        /// Human-readable reason describing why the object name is invalid.
        reason: String,
    },
    /// Table resolution matched multiple candidates.
    #[error("Ambiguous table lookup `{object_name}`; candidates: {candidates:?}")]
    AmbiguousTableLookup {
        /// Lookup object name as rendered by sqlparser.
        object_name: String,
        /// Deterministically ordered list of matching candidates.
        candidates: Vec<String>,
    },
    /// Function resolution by name matched more than one declaration, which
    /// happens when a name carries several argument lists.
    #[error("Ambiguous function lookup `{object_name}`; candidates: {candidates:?}")]
    AmbiguousFunctionLookup {
        /// Name the lookup asked for, as written.
        object_name: String,
        /// Deterministically ordered list of matching declarations.
        candidates: Vec<String>,
    },
    /// Table resolution found no matching table for a name that is required to
    /// denote one.
    #[error("Table `{object_name}` not found.")]
    TableNotFound {
        /// Lookup object name as rendered by sqlparser.
        object_name: String,
    },
    /// Adding a table would create semantic lookup ambiguity.
    #[error(
        "Cannot add table `{table}` because existing table `{conflicting_table}` already uses that name under the {case} identifier comparison."
    )]
    TableLookupConflict {
        /// Table being inserted.
        table: String,
        /// Existing conflicting table.
        conflicting_table: String,
        /// Comparison under which the two names are one.
        case: IdentifierCase,
    },
    /// A database object handed to a metadata accessor is not present in the
    /// database being queried, for instance because it was renamed away, was
    /// dropped, or came from a different database.
    #[error("{object_kind} {object} is not present in the database being queried.")]
    ObjectNotInDatabase {
        /// Kind of object that was looked up.
        object_kind: ObjectKind,
        /// Best-effort rendering identifying the object: its own quoted name,
        /// or the table it is declared on when it has no name of its
        /// own. Build it with [`ObjectKind::not_in_database`] or
        /// [`ObjectKind::anonymous_not_in_database`] rather than by hand.
        object: String,
    },
    /// A column named by a constraint is not declared by the table that
    /// constraint resolves it against.
    #[error("Column `{column_name}` is not declared by table `{table_name}`.")]
    ColumnNotFound {
        /// Name of the table the column was looked up in.
        table_name: String,
        /// Name of the column that was not found.
        column_name: String,
    },
}

/// Why a text could not be read back as a target name by
/// [`TargetName::parse`](crate::structs::TargetName::parse).
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum TargetNameParseError {
    /// The text was empty, so it names nothing.
    #[error("cannot parse a target name from empty text")]
    Empty,
    /// A part of the name was empty, as in `app.`, `.docs` or `""`.
    #[error("target name `{text}` has an empty identifier part")]
    EmptyPart {
        /// The full text that was being parsed.
        text: String,
    },
    /// A part opened with a quote that was never closed.
    #[error("target name `{text}` has a quote that never closes")]
    UnterminatedQuote {
        /// The full text that was being parsed.
        text: String,
    },
    /// Text followed an identifier where a separating dot or the end of the
    /// name was required, as in `a"b` or `"a"x`.
    #[error("target name `{text}` has unexpected text `{found}` after an identifier")]
    UnexpectedText {
        /// The full text that was being parsed.
        text: String,
        /// The text found after the identifier.
        found: String,
    },
    /// More than one dot qualified the name, as in `database.app.docs`, which
    /// has no place in a name holding a qualifier and an identifier.
    #[error("target name `{text}` has more parts than a qualifier and a name")]
    TooManyParts {
        /// The full text that was being parsed.
        text: String,
    },
}

#[derive(Debug, thiserror::Error)]
/// Defines the `Error` enum representing various error types
pub enum Error {
    /// Wrapper around identifier-aware lookup errors.
    #[error(transparent)]
    IdentifierLookupError(#[from] LookupError),
    #[error("Unknown column `{column_name}` in table `{table_name}`.")]
    /// A check constraint contained columns which do not exist in the table.
    UnknownColumnInCheckConstraint {
        /// Name of the unknown column.
        column_name: String,
        /// Name of the table the check constraint belongs to.
        table_name: String,
    },
    #[error(
        "Column reference `{reference}` in table `{table_name}` names something other than that \
         table, so it is not one of its columns."
    )]
    /// A check constraint or an index expression qualified a column with a
    /// table, schema or catalog other than the one the expression belongs to.
    ForeignColumnReference {
        /// The reference as written.
        reference: String,
        /// Name of the table the expression is declared on.
        table_name: String,
    },
    #[error("Invalid primary key in table `{table_name}`: {reason}")]
    /// A primary key constraint referenced something other than a plain column,
    /// such as an expression like `PRIMARY KEY (a - b)`.
    InvalidPrimaryKey {
        /// Name of the table the primary key belongs to.
        table_name: String,
        /// Human-readable reason describing why the primary key is invalid.
        reason: String,
    },
    #[error(
        "Referenced table `{referenced_table}` not found for foreign key in table `{host_table}`."
    )]
    /// Error indicating that a foreign key references a table that does not
    /// exist.
    ReferencedTableNotFoundForForeignKey {
        /// Name of the referenced table.
        referenced_table: String,
        /// Name of the host table containing the foreign key.
        host_table: String,
    },
    #[error("Parent table `{parent_table}` not found for table `{child_table}`.")]
    /// Error indicating that a table derives its shape from a table that does
    /// not exist, spelled either `INHERITS` or `PARTITION OF`.
    ParentTableNotFound {
        /// Name of the parent table the child names.
        parent_table: String,
        /// Name of the table naming the parent.
        child_table: String,
    },
    #[error("Table `{copied_table}` not found for the `LIKE` in table `{table_name}`.")]
    /// Error indicating that a table copies its columns from a table that
    /// does not exist.
    CopiedTableNotFound {
        /// Name of the table the `LIKE` names.
        copied_table: String,
        /// Name of the table doing the copying.
        table_name: String,
    },
    #[error("Cannot drop column `{column_name}` of table `{table_name}` because it is inherited.")]
    /// Error indicating that a table tried to drop a column it receives from
    /// a parent, which only the parent can drop.
    InheritedColumnNotDroppable {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the inherited column.
        column_name: String,
    },
    #[error(
        "Cannot drop constraint `{constraint_name}` of table `{table_name}` because it is inherited."
    )]
    /// Error indicating that a table tried to drop a constraint it receives
    /// from a parent, which only the parent can drop.
    InheritedConstraintNotDroppable {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the inherited constraint.
        constraint_name: String,
    },
    #[error(
        "`ONLY` cannot be used to {change} table `{table_name}`, because other tables inherit from it."
    )]
    /// Error indicating that `ALTER TABLE ONLY` asked for a change PostgreSQL
    /// refuses to withhold from the tables below.
    OnlyRefusedWithChildren {
        /// Name of the table the statement names.
        table_name: String,
        /// The change that cannot stop at the named table.
        change: InheritedChange,
    },
    #[error(
        "Constraint `{constraint_name}` of table `{table_name}` conflicts with the constraint of the same name a parent passes down."
    )]
    /// Error indicating that a table holds a constraint under the same name as
    /// one arriving from a parent without being mergeable with it, either
    /// because the expressions differ or because the table's own is marked
    /// `NO INHERIT`.
    InheritedConstraintConflict {
        /// Name of the table holding the conflicting constraint.
        table_name: String,
        /// Name the two constraints share.
        constraint_name: String,
    },
    #[error("Cannot add a `NO INHERIT` check to partitioned table `{table_name}`.")]
    /// Error indicating that a `NO INHERIT` check was written on a partitioned
    /// table, which PostgreSQL refuses because every constraint of a
    /// partitioned table is enforced on its partitions.
    NoInheritCheckOnPartitionedTable {
        /// Name of the partitioned table.
        table_name: String,
    },
    #[error("Cannot use `ONLY` to add a foreign key on partitioned table `{table_name}`.")]
    /// Error indicating that `ALTER TABLE ONLY ... ADD FOREIGN KEY` named a
    /// partitioned table, which PostgreSQL refuses whether or not any
    /// partition exists yet.
    OnlyForeignKeyOnPartitionedTable {
        /// Name of the partitioned table.
        table_name: String,
    },
    #[error(
        "Column `{column_name}` of table `{table_name}` must require a value before `ONLY` may add a primary key above it."
    )]
    /// Error indicating that `ALTER TABLE ONLY ... ADD PRIMARY KEY` named a
    /// table below which a keyed column may still hold nothing. The `NOT NULL`
    /// a key implies is the one part that cannot stop at the named table, so
    /// PostgreSQL grants the statement only where every table below already
    /// requires the keyed columns.
    OnlyPrimaryKeyOnNullableColumn {
        /// Name of the table whose column may still hold nothing.
        table_name: String,
        /// Name of the keyed column.
        column_name: String,
    },
    #[error(
        "Column `{column_name}` of table `{table_name}` cannot stop requiring a value because {reason}."
    )]
    /// Error indicating that `ALTER COLUMN ... DROP NOT NULL` named a column
    /// something else requires a value in.
    RequiredValueNotDroppable {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the column.
        column_name: String,
        /// What keeps the requirement in place.
        reason: RequiredValue,
    },
    #[error(
        "Column `{column_name}` of table `{table_name}` must require a value before it can be given an identity."
    )]
    /// Error indicating that `ALTER COLUMN ... ADD GENERATED AS IDENTITY` named
    /// a column that may still hold nothing, which PostgreSQL refuses.
    IdentityNeedsRequiredValue {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the column.
        column_name: String,
    },
    #[error(
        "Column `{column_name}` of table `{child_table}` has type `{child_type}`, conflicting with type `{parent_type}` inherited from `{parent_table}`."
    )]
    /// Error indicating that a table redeclares an inherited column with a
    /// different type, which PostgreSQL refuses.
    InheritedColumnTypeConflict {
        /// Name of the column declared twice.
        column_name: String,
        /// Name of the table redeclaring the column.
        child_table: String,
        /// Type the child declares.
        child_type: String,
        /// Name of the parent the column comes from.
        parent_table: String,
        /// Type the parent declares.
        parent_type: String,
    },
    #[error(
        "Column `{column_name}` of table `{child_table}` has collation `{child_collation}`, conflicting with collation `{parent_collation}` inherited from `{parent_table}`."
    )]
    /// Error indicating that a table redeclares an inherited column with a
    /// different collation.
    InheritedColumnCollationConflict {
        /// Name of the column declared twice.
        column_name: Box<str>,
        /// Name of the table redeclaring the column.
        child_table: Box<str>,
        /// Collation the child declares.
        child_collation: Box<str>,
        /// Name of the parent the column comes from.
        parent_table: Box<str>,
        /// Collation the parent declares.
        parent_collation: Box<str>,
    },
    #[error("Collation `{collation_name}` cannot be copied.")]
    /// Error indicating that a built-in collation cannot be copied.
    CollationCannotBeCopied {
        /// Name of the collation the statement copies.
        collation_name: Box<str>,
    },
    #[error("Collation `{collation_name}` not found.")]
    /// Error indicating that a copied collation source does not exist.
    CollationNotFound {
        /// Name of the collation the statement copies.
        collation_name: Box<str>,
    },
    #[error("Collation `{collation_name}` already exists.")]
    /// Error indicating that a collation name is already taken.
    CollationAlreadyExists {
        /// Name of the collation the statement creates.
        collation_name: Box<str>,
    },
    #[error("Invalid value `{option_value}` for collation option `{option_name}`.")]
    /// Error indicating that a `CREATE COLLATION` option is malformed.
    InvalidCollationOption {
        /// Name of the option whose value is invalid.
        option_name: Box<str>,
        /// Value the statement supplied.
        option_value: Box<str>,
    },
    #[error("Collation option `{option_name}` appears more than once.")]
    /// Error indicating that a `CREATE COLLATION` option is repeated.
    RepeatedCollationOption {
        /// Name of the repeated option.
        option_name: Box<str>,
    },
    #[error("Column `{column_name}` declares more than one collation.")]
    /// Error indicating that a column declares multiple `COLLATE` clauses.
    RepeatedColumnCollation {
        /// Name of the column carrying repeated collation clauses.
        column_name: Box<str>,
    },
    #[error("Column `{column_name}` of type `{type_name}` cannot use a collation.")]
    /// Error indicating that a known PostgreSQL type is not collatable.
    NonCollatableColumnType {
        /// Name of the column carrying the collation.
        column_name: Box<str>,
        /// Type of the column carrying the collation.
        type_name: Box<str>,
    },
    #[error(
        "Column `{column_name}` uses custom type `{type_name}` whose collatability is not in the PostgreSQL catalog."
    )]
    /// Error indicating that a custom type is missing from the PostgreSQL
    /// catalog.
    ColumnTypeCollatabilityNotInCatalog {
        /// Name of the column carrying the collation.
        column_name: Box<str>,
        /// Type missing from the configured catalog.
        type_name: Box<str>,
    },
    #[error("Cannot drop table `{parent_table}` because table `{child_table}` inherits from it.")]
    /// Error indicating that a `DROP TABLE` would leave a child naming a
    /// parent that no longer exists.
    DropTableInheritedFrom {
        /// Name of the table the statement drops.
        parent_table: String,
        /// Name of a table inheriting from it.
        child_table: String,
    },
    #[error(
        "Referenced column `{referenced_column}` not found in table `{referenced_table}` for foreign key in table `{host_table}`."
    )]
    /// Error indicating that a foreign key references a column that does not
    /// exist.
    ReferencedColumnNotFoundForForeignKey {
        /// Name of the referenced column.
        referenced_column: String,
        /// Name of the referenced table.
        referenced_table: String,
        /// Name of the host table containing the foreign key.
        host_table: String,
    },
    #[error(
        "No unique constraint on table `{referenced_table}` matches the columns `{referenced_columns}` a foreign key in table `{host_table}` points at."
    )]
    /// Error indicating that a foreign key points at columns of the referenced
    /// table that no primary key, unique constraint or unique index covers.
    ///
    /// PostgreSQL, MySQL and SQLite all refuse such a constraint, since without
    /// a unique key on the far side a row could match more than one parent.
    ReferencedColumnsNotUniqueForForeignKey {
        /// Comma-separated names of the referenced columns.
        referenced_columns: String,
        /// Name of the referenced table.
        referenced_table: String,
        /// Name of the host table containing the foreign key.
        host_table: String,
    },
    #[error("Host column `{host_column}` not found in table `{host_table}` for foreign key.")]
    /// Error indicating that a foreign key references a host column that does
    /// not exist.
    HostColumnNotFoundForForeignKey {
        /// Name of the host column.
        host_column: String,
        /// Name of the host table containing the foreign key.
        host_table: String,
    },
    #[error("Table `{table_name}` not found for trigger `{trigger_name}`.")]
    /// Error indicating that a trigger references a table that does not exist.
    TableNotFoundForTrigger {
        /// Name of the table the trigger belongs to.
        table_name: String,
        /// Name of the trigger.
        trigger_name: String,
    },
    #[error("Table `{table_name}` not found for policy `{policy_name}`.")]
    /// Error indicating that a policy names a table that does not exist.
    ///
    /// A policy exists only on its table, so the database refuses one whose
    /// table is absent, and so does this crate.
    TableNotFoundForPolicy {
        /// Name of the table the policy is declared on.
        table_name: String,
        /// Name of the policy.
        policy_name: String,
    },
    #[error("Role `{role_name}` not found for the owner of `{object_name}`.")]
    /// Error indicating that an ownership statement names a role that does not
    /// exist.
    ///
    /// Covers `ALTER TABLE ... OWNER TO`, `ALTER FUNCTION ... OWNER TO`,
    /// `ALTER SCHEMA ... OWNER TO` and `CREATE SCHEMA ... AUTHORIZATION`, all
    /// of which the database refuses when the role is absent. Like the other
    /// role checks this one follows
    /// [`AccessResolution`](crate::structs::AccessResolution), because a schema
    /// dump names an owner while creating no role.
    RoleNotFoundForOwner {
        /// Name of the role named as the owner.
        role_name: String,
        /// Name of the object being owned.
        object_name: String,
    },
    #[error("Column `{column_name}` not found in table `{table_name}` for a grant or revoke.")]
    /// Error indicating that a column-level grant or revoke names a column the
    /// table it applies to does not have.
    ///
    /// The statement may name several tables, and the database requires the
    /// column on each of them, so this names the table that lacks it.
    ColumnNotFoundForGrant {
        /// Name of the column the grant names.
        column_name: String,
        /// Name of the table that does not have it.
        table_name: String,
    },
    #[error("Table `{table_name}` not found for index `{index_name}`.")]
    /// Error indicating that an index references a table that does not exist.
    TableNotFoundForIndex {
        /// Name of the table the index belongs to.
        table_name: String,
        /// Name of the index.
        index_name: String,
    },
    #[error("Invalid index `{index_name}`: {reason}")]
    /// Error indicating that an index definition is invalid.
    InvalidIndex {
        /// Name of the invalid index.
        index_name: String,
        /// Reason why the index is invalid.
        reason: String,
    },
    #[error("Function `{function_name}` not found for trigger `{trigger_name}`.")]
    /// Error indicating that a trigger references a function that does not
    /// exist.
    FunctionNotFoundForTrigger {
        /// Name of the function the trigger executes.
        function_name: String,
        /// Name of the trigger.
        trigger_name: String,
    },
    /// Wrapper around SQL parser errors.
    #[cfg_attr(feature = "std", error("SQL parser error: {error} in {file:?}"))]
    #[cfg_attr(not(feature = "std"), error("SQL parser error: {error}"))]
    SqlParserError {
        /// The error from the SQL parser.
        #[source]
        error: ParserError,
        /// The file containing the offending code (only carried under the `std`
        /// feature; `no_std` consumers receive `None`).
        #[cfg(feature = "std")]
        file: Option<std::path::PathBuf>,
    },
    /// Wrapper around git errors. Only available with the `git` feature.
    #[cfg(feature = "git")]
    #[error("Git error: {0}")]
    GitError(#[from] git2::Error),
    /// Wrapper around IO errors. Only available with the `std` feature.
    #[cfg(feature = "std")]
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    /// Wrapper around sql_doc errors
    #[error("Table Doc Error: {0}")]
    TableDocError(#[from] sql_docs::error::DocError),
    /// Error indicating that a REVOKE statement uses semantics we cannot
    /// represent in the current grant model.
    #[error("Unsupported revoke statement `{statement}`: {reason}")]
    UnsupportedRevoke {
        /// The original REVOKE statement rendered by sqlparser.
        statement: String,
        /// Human-readable explanation of the unsupported semantics.
        reason: String,
    },
    #[error("Role `{role_name}` not found for a grant or revoke.")]
    /// Error indicating that a grant or a revoke names a role that does not
    /// exist.
    RoleNotFoundForGrant {
        /// Name of the undefined role.
        role_name: String,
    },
    #[error("Table `{table_name}` not found for a grant or revoke.")]
    /// Error indicating that a grant or a revoke names a table that does not
    /// exist.
    TableNotFoundForGrant {
        /// Name of the undefined table.
        table_name: String,
    },
    #[error("Function `{function_name}` not found for DROP FUNCTION statement.")]
    /// Error indicating that a DROP FUNCTION statement references a function
    /// that does not exist.
    DropFunctionNotFound {
        /// Name of the function that was not found.
        function_name: String,
    },
    #[error("Cannot drop function `{function_name}`: still referenced in the schema.")]
    /// Error indicating that a DROP FUNCTION statement references a function
    /// that is still used by other schema objects (check constraints, policies,
    /// or triggers).
    FunctionReferenced {
        /// Name of the function being dropped.
        function_name: String,
    },
    #[error("Table `{table_name}` not found for DROP TABLE statement.")]
    /// Error indicating that a DROP TABLE statement references a table
    /// that does not exist.
    DropTableNotFound {
        /// Name of the table that was not found.
        table_name: String,
    },
    #[error("Cannot drop table `{table_name}`: still referenced in the schema.")]
    /// Error indicating that a DROP TABLE statement references a table
    /// that is still referenced by foreign keys from other tables.
    TableReferenced {
        /// Name of the table being dropped.
        table_name: String,
    },
    #[error(
        "Cannot drop column `{column_name}` of table `{table_name}`: still referenced from outside the table."
    )]
    /// Error indicating that an `ALTER TABLE ... DROP COLUMN` statement names a
    /// column something outside its table depends on, without saying `CASCADE`.
    ///
    /// Indexes and constraints on the table itself are dropped along with the
    /// column and never raise this.
    ColumnReferenced {
        /// Name of the table the column belongs to.
        table_name: String,
        /// Name of the column being dropped.
        column_name: String,
    },
    #[error("Column `{column_name}` already exists on table `{table_name}`.")]
    /// Error indicating that a statement tries to introduce a column name the
    /// table already declares.
    ColumnAlreadyExists {
        /// Name of the table the column belongs to.
        table_name: String,
        /// Name of the column that already exists.
        column_name: String,
    },
    #[error(
        "Unsupported `ALTER TABLE` operation on `{table_name}`: `{operation}` changes something this model represents but does not yet apply."
    )]
    /// Error indicating that an `ALTER TABLE` operation would change part of
    /// the schema the model represents, and is not yet applied.
    ///
    /// Operations that change something the model represents at all are either
    /// applied or reported here, so nothing that would leave the model wrong is
    /// silently discarded.
    UnsupportedAlterTableOperation {
        /// Name of the table the statement targets.
        table_name: String,
        /// The operation rendered by sqlparser.
        operation: String,
    },
    #[error("Schema `{schema_name}` not found for {object_kind} `{relation_name}`.")]
    /// Error indicating that a statement creating a relation qualifies its
    /// name with a schema no `CREATE SCHEMA` in the input creates.
    ///
    /// The default schema is exempt, since no dump emits a statement creating
    /// it.
    SchemaNotFoundForRelation {
        /// Name of the schema that was not found.
        schema_name: String,
        /// Kind of relation qualified with it.
        object_kind: ObjectKind,
        /// Name of the relation qualified with it.
        relation_name: String,
    },
    #[error("Schema `{schema_name}` not found for collation `{collation_name}`.")]
    /// Error indicating that a collation is created in an absent schema.
    SchemaNotFoundForCollation {
        /// Name of the schema that was not found.
        schema_name: String,
        /// Name of the collation qualified with it.
        collation_name: String,
    },
    #[error("No schema has been selected to create {object_kind} `{relation_name}` in.")]
    /// Error indicating that a statement creating a relation names no schema at
    /// a point where the search path selects none either, because `SET
    /// search_path TO ''` emptied it.
    NoSchemaSelectedForRelation {
        /// Kind of relation that named no schema.
        object_kind: ObjectKind,
        /// Name of the relation that named no schema.
        relation_name: String,
    },
    #[error(
        "Temporary {object_kind} `{relation_name}` cannot be created in non-temporary schema `{schema_name}`."
    )]
    /// Error indicating that a temporary relation is qualified with a schema
    /// other than `pg_temp`.
    ///
    /// PostgreSQL keeps temporary relations only in the session's temporary
    /// schema. This also covers a view the statement wrote without `TEMP`
    /// whose definition reads a temporary relation, which PostgreSQL makes
    /// temporary and then cannot place in the schema the statement named.
    TemporaryRelationInPermanentSchema {
        /// Kind of relation the statement creates.
        object_kind: ObjectKind,
        /// Name of the relation the statement creates.
        relation_name: String,
        /// The non-temporary schema the statement named.
        schema_name: String,
    },
    #[error("Materialized view `{view_name}` cannot be declared `TEMP`.")]
    /// Error indicating that the input writes `CREATE TEMP MATERIALIZED
    /// VIEW`, which the parser accepts and PostgreSQL rejects as a syntax
    /// error. A materialized view becomes temporary only by being created in
    /// `pg_temp`.
    TemporaryMaterializedView {
        /// Name the statement wrote.
        view_name: String,
    },
    #[error("Materialized view `{view_name}` cannot read temporary relation `{relation_name}`.")]
    /// Error indicating that a materialized view's definition reads a
    /// temporary table or view, which PostgreSQL refuses.
    MaterializedViewReadsTemporaryRelation {
        /// Name of the materialized view the statement creates.
        view_name: String,
        /// Name the definition wrote for the temporary relation.
        relation_name: String,
    },
    #[error(
        "Foreign key of table `{host_table}` cannot reference `{referenced_table}`: constraints on temporary tables may reference only temporary tables, and constraints on permanent tables only permanent ones."
    )]
    /// Error indicating that a foreign key joins a temporary table and a
    /// permanent one, in either direction, which PostgreSQL refuses.
    ForeignKeyCrossesTemporaryStorage {
        /// Table declaring the foreign key.
        host_table: String,
        /// Table the foreign key references.
        referenced_table: String,
        /// Whether the declaring table is the temporary one.
        host_is_temporary: bool,
    },
    #[error(
        "Permanent table `{table_name}` cannot inherit from temporary relation `{parent_name}`."
    )]
    /// Error indicating that a permanent table names a temporary parent in
    /// `INHERITS` or `PARTITION OF`, which PostgreSQL refuses.
    PermanentRelationInheritsTemporary {
        /// Table the statement creates.
        table_name: String,
        /// Temporary parent it names.
        parent_name: String,
    },
    #[error(
        "Temporary table `{table_name}` cannot be created as a partition of permanent relation `{parent_name}`."
    )]
    /// Error indicating that a temporary table is declared `PARTITION OF` a
    /// permanent one, which PostgreSQL refuses. A temporary table may still
    /// inherit from a permanent one through `INHERITS`.
    TemporaryPartitionOfPermanent {
        /// Table the statement creates.
        table_name: String,
        /// Permanent parent it names.
        parent_name: String,
    },
    #[error("No schema has been selected to create collation `{collation_name}` in.")]
    /// Error indicating that a `CREATE COLLATION` statement names no creatable
    /// schema.
    NoSchemaSelectedForCollation {
        /// Name of the collation that named no schema.
        collation_name: String,
    },
    #[error("Role `{role_name}` not found for policy `{policy_name}`.")]
    /// Error indicating that a `CREATE POLICY` statement applies to a role no
    /// `CREATE ROLE` in the input creates.
    ///
    /// Reported under [`crate::structs::AccessResolution::ClosedWorld`], the
    /// same setting that governs a grant naming an absent role, because a dump
    /// of a schema omits role creation either way.
    RoleNotFoundForPolicy {
        /// Name of the role that was not found.
        role_name: String,
        /// Name of the policy applying to it.
        policy_name: String,
    },
    #[error("Index `{index_name}` not found for DROP INDEX statement.")]
    /// Error indicating that a DROP INDEX statement references an index
    /// that does not exist.
    DropIndexNotFound {
        /// Name of the index that was not found.
        index_name: String,
    },
    #[error("Trigger `{trigger_name}` not found for DROP TRIGGER statement.")]
    /// Error indicating that a DROP TRIGGER statement references a trigger
    /// that does not exist.
    DropTriggerNotFound {
        /// Name of the trigger that was not found.
        trigger_name: String,
    },
    #[error("Policy `{policy_name}` not found for DROP POLICY statement.")]
    /// Error indicating that a DROP POLICY statement references a policy
    /// that does not exist.
    DropPolicyNotFound {
        /// Name of the policy that was not found.
        policy_name: String,
    },
    #[error("Role `{role_name}` not found for DROP ROLE statement.")]
    /// Error indicating that a DROP ROLE statement references a role
    /// that does not exist.
    DropRoleNotFound {
        /// Name of the role that was not found.
        role_name: String,
    },
    #[error("Cannot drop role `{role_name}`: still referenced by grants.")]
    /// Error indicating that a DROP ROLE statement references a role
    /// that is still used as a grantee in existing grants.
    RoleReferenced {
        /// Name of the role being dropped.
        role_name: String,
    },
    #[error("Schema `{schema_name}` already exists.")]
    /// Error indicating that a CREATE SCHEMA statement tries to create a schema
    /// that already exists.
    SchemaAlreadyExists {
        /// Name of the schema that already exists.
        schema_name: String,
    },
    #[error(
        "{object_kind} `{object_name}` cannot be created: {conflicting_kind} `{object_name}` already uses that name in the same schema."
    )]
    /// Error indicating that a statement names a relation with a name
    /// something else in the same schema already holds.
    ///
    /// PostgreSQL keeps tables, views, materialized views and index names in
    /// one pool per schema, and a named `UNIQUE` or `PRIMARY KEY` constraint
    /// puts the name of the index behind it in that pool too. Sequences share
    /// the pool as well, and this crate does not model those, so the rule it
    /// enforces is the part of PostgreSQL's it can see.
    RelationNameAlreadyTaken {
        /// Kind of object the statement tried to create.
        object_kind: ObjectKind,
        /// Kind of object already holding the name.
        conflicting_kind: ObjectKind,
        /// The contested name.
        object_name: String,
    },
    #[error("{object_kind} `{object_name}` not found.")]
    /// Error indicating that a statement names a relation the input never
    /// creates, where the statement carried no `IF EXISTS`.
    RelationNotFound {
        /// Kind of relation the statement asked for.
        object_kind: ObjectKind,
        /// Name the statement wrote.
        object_name: String,
    },
    #[error("`{object_name}` is a {actual_kind}, not a {expected_kind}.")]
    /// Error indicating that a statement names an existing relation of the
    /// wrong kind, which PostgreSQL refuses rather than treating the spellings
    /// as interchangeable: `DROP TABLE` will not drop a view, `DROP VIEW` will
    /// not drop a table or a materialized view, and `DROP MATERIALIZED VIEW`
    /// will not drop a plain view.
    RelationKindMismatch {
        /// Name the statement wrote.
        object_name: String,
        /// Kind the statement's spelling asked for.
        expected_kind: ObjectKind,
        /// Kind the name actually holds.
        actual_kind: ObjectKind,
    },
    #[error(
        "Cannot change name of view column `{existing_column}` to `{new_column}` of view `{view_name}`."
    )]
    /// Error indicating that `CREATE OR REPLACE VIEW` renames an output column.
    ///
    /// PostgreSQL allows a replacement to add columns on the end and nothing
    /// else, so a rename is refused with the hint to use `ALTER VIEW ... RENAME
    /// COLUMN` instead. This is checked only when both the recorded and the
    /// replacing shapes can be read, which an explicit column list always
    /// allows and a definition the resolver cannot see into does not.
    ViewColumnRenamedByReplace {
        /// Name of the view being replaced.
        view_name: String,
        /// Name the recorded shape exposes at that position.
        existing_column: String,
        /// Name the replacement would expose there.
        new_column: String,
    },
    #[error("Cannot drop columns from view `{view_name}`.")]
    /// Error indicating that `CREATE OR REPLACE VIEW` leaves out an output
    /// column the recorded shape exposes.
    ///
    /// A replacement may only add columns on the end.
    ViewColumnsDroppedByReplace {
        /// Name of the view being replaced.
        view_name: String,
    },
    #[error("Materialized view `{view_name}` cannot be replaced.")]
    /// Error indicating that the input writes `CREATE OR REPLACE MATERIALIZED
    /// VIEW`, which the parser accepts and PostgreSQL rejects as a syntax
    /// error: a materialized view has no replace form at all.
    MaterializedViewCannotBeReplaced {
        /// Name the statement wrote.
        view_name: String,
    },
    #[error("`CREATE VIEW` does not take `IF NOT EXISTS`, so view `{view_name}` cannot use it.")]
    /// Error indicating that the input writes `CREATE VIEW ... IF NOT EXISTS`,
    /// which the parser accepts and PostgreSQL rejects as a syntax error.
    /// `CREATE OR REPLACE VIEW` is the spelling PostgreSQL offers instead, and
    /// the materialized form does take `IF NOT EXISTS`.
    ViewIfNotExistsUnsupported {
        /// Name the statement wrote.
        view_name: String,
    },
    #[error("View `{view_name}` specifies more column names than columns.")]
    /// Error indicating that a view's explicit column list is longer than what
    /// its definition produces.
    ///
    /// Checked only when the definition's output columns can be counted.
    ViewColumnListTooLong {
        /// Name the statement wrote.
        view_name: String,
    },
    #[error(
        "{object_kind} `{object_name}` cannot be dropped because {dependent_kind} `{dependent_name}` reads it."
    )]
    /// Error indicating that a statement drops a relation a view reads.
    ///
    /// PostgreSQL refuses without `CASCADE`, and `CASCADE` drops the reading
    /// views along with it. A view whose definition read a relation dropped
    /// from under it would name nothing, which is the dangling reference this
    /// refusal exists to prevent.
    RelationHasDependents {
        /// Kind of relation the statement drops.
        object_kind: ObjectKind,
        /// Name of the relation the statement drops.
        object_name: String,
        /// Kind of the view reading it.
        dependent_kind: ObjectKind,
        /// Name of the view reading it.
        dependent_name: String,
    },
    #[error(
        "ALTER action {operation} cannot be performed on {object_kind} `{relation_name}`: this operation is not supported for it."
    )]
    /// Error indicating that an `ALTER TABLE` names a view and asks for
    /// something only a table can do.
    ///
    /// PostgreSQL accepts `ALTER TABLE` against a view for the handful of
    /// actions a view supports, renaming it and changing its owner among them,
    /// and refuses the rest naming the action.
    AlterActionUnsupportedOnRelation {
        /// Kind of relation the name holds.
        object_kind: ObjectKind,
        /// Name the statement wrote.
        relation_name: String,
        /// The action, rendered by the parser.
        operation: String,
    },
    #[error("Policy `{policy_name}` already exists on table `{table_name}`.")]
    /// Error indicating that a `CREATE POLICY` statement names a policy the
    /// table already carries. A policy name is unique per table, whatever
    /// command it is declared `FOR`.
    PolicyAlreadyExists {
        /// Name of the policy that already exists.
        policy_name: String,
        /// Name of the table carrying it.
        table_name: String,
    },
    #[error("Trigger `{trigger_name}` already exists on table `{table_name}`.")]
    /// Error indicating that a `CREATE TRIGGER` statement names a trigger the
    /// table already carries. A trigger name is unique per table, so the same
    /// name on another table is fine.
    TriggerAlreadyExists {
        /// Name of the trigger that already exists.
        trigger_name: String,
        /// Name of the table carrying it.
        table_name: String,
    },
    #[error("Role `{role_name}` already exists.")]
    /// Error indicating that a `CREATE ROLE` statement names a role that
    /// already exists.
    ///
    /// Unlike the checks on a role a grant, a policy or an ownership statement
    /// *names*, this one is not governed by
    /// [`AccessResolution`](crate::structs::AccessResolution): that setting
    /// excuses a dump that omits role creation, and this statement is the
    /// creation.
    RoleAlreadyExists {
        /// Name of the role that already exists.
        role_name: String,
    },
    #[error("Function `{function_name}` already exists with the same argument types.")]
    /// Error indicating that a `CREATE FUNCTION` statement repeats a signature.
    ///
    /// A function is identified by its schema, its name and its argument types
    /// with `OUT` parameters removed, so two functions may share a name as long
    /// as they take different arguments. The return type is not part of the
    /// identity. `CREATE OR REPLACE` replaces the existing function instead.
    FunctionAlreadyExists {
        /// Name of the function whose signature is repeated.
        function_name: String,
    },
    #[error(
        "Function name `{function_name}` is not unique: a `DROP FUNCTION` naming no argument list cannot say which one to drop."
    )]
    /// Error indicating that a `DROP FUNCTION` statement omits the argument
    /// list while the name it gives covers more than one function.
    AmbiguousDropFunction {
        /// The name covering more than one function.
        function_name: String,
    },
    #[error("Schema `{schema_name}` not found for DROP SCHEMA statement.")]
    /// Error indicating that a DROP SCHEMA statement references a schema
    /// that does not exist.
    DropSchemaNotFound {
        /// Name of the schema that was not found.
        schema_name: String,
    },
    #[error("Cannot drop schema `{schema_name}`: still contains objects.")]
    /// Error indicating that a DROP SCHEMA statement references a schema
    /// that still contains objects (tables, functions, etc.).
    SchemaNotEmpty {
        /// Name of the schema being dropped.
        schema_name: String,
    },
    #[error("Table `{table_name}` not found for RENAME TABLE statement.")]
    /// Error indicating that a RENAME TABLE statement references a table
    /// that does not exist.
    RenameTableNotFound {
        /// Name of the table that was not found.
        table_name: String,
    },
    #[error("Policy `{policy_name}` not found for ALTER POLICY statement.")]
    /// Error indicating that an ALTER POLICY statement references a policy
    /// that does not exist.
    AlterPolicyNotFound {
        /// Name of the policy that was not found.
        policy_name: String,
    },
    #[error("Function `{function_name}` not found for ALTER FUNCTION statement.")]
    /// Error indicating that an ALTER FUNCTION statement carrying a security
    /// clause or an owner reassignment references a function that does not
    /// exist.
    AlterFunctionNotFound {
        /// Name of the function that was not found.
        function_name: String,
    },
    #[error(
        "Function name `{function_name}` is not unique: an `ALTER FUNCTION` naming no argument list cannot say which one to alter."
    )]
    /// Error indicating that an `ALTER FUNCTION` statement carrying a security
    /// clause or an owner reassignment omits the argument list while the name
    /// it gives covers more than one function.
    AmbiguousAlterFunction {
        /// The name covering more than one function.
        function_name: String,
    },
    #[error(
        "Aggregate `{aggregate_name}` cannot be given an owner: this model holds no aggregates at all."
    )]
    /// Error indicating that an `ALTER AGGREGATE ... OWNER TO` statement names
    /// an aggregate, which this model never holds.
    ///
    /// The parser this crate builds on rejects `CREATE AGGREGATE`, so no
    /// aggregate can exist here for an owner to belong to. Reading the
    /// statement as naming a function of the same name would contradict
    /// PostgreSQL, which refuses to reach a function through `ALTER AGGREGATE`,
    /// and passing it over would read an ownership change and drop it, so the
    /// statement is refused instead.
    AggregateOwnerUnsupported {
        /// Name of the aggregate the statement names.
        aggregate_name: String,
    },
    #[error("Index `{index_name}` not found for ALTER INDEX statement.")]
    /// Error indicating that an ALTER INDEX statement references an index that
    /// does not exist.
    AlterIndexNotFound {
        /// Name of the index that was not found.
        index_name: String,
    },
    #[error("Role `{role_name}` not found for ALTER ROLE statement.")]
    /// Error indicating that an ALTER ROLE statement references a role that
    /// does not exist.
    AlterRoleNotFound {
        /// Name of the role that was not found.
        role_name: String,
    },
    #[error("Table `{table_name}` not found for ALTER TABLE statement.")]
    /// Error indicating that an ALTER TABLE statement references a table that
    /// does not exist.
    AlterTableNotFound {
        /// Name of the table that was not found.
        table_name: String,
    },
    #[error(
        "Constraint `{constraint_name}` not found on table `{table_name}` for ALTER TABLE DROP CONSTRAINT statement."
    )]
    /// Error indicating that an `ALTER TABLE ... DROP CONSTRAINT` statement
    /// names a constraint the table does not declare. Constraints declared
    /// without a name have no name to drop them by.
    DropConstraintNotFound {
        /// Name of the table the constraint was looked up on.
        table_name: String,
        /// Name of the constraint that was not found.
        constraint_name: String,
    },
    #[error("Schema `{schema_name}` not found for ALTER SCHEMA statement.")]
    /// Error indicating that an ALTER SCHEMA statement references a schema
    /// that does not exist.
    AlterSchemaNotFound {
        /// Name of the schema that was not found.
        schema_name: String,
    },
    #[error("Failed to build the table dependency graph of database `{catalog_name}`: {reason}")]
    /// Error indicating that the foreign key graph of a database could not be
    /// assembled.
    TableDependencyGraph {
        /// Name of the database whose graph could not be assembled.
        catalog_name: String,
        /// Human-readable reason describing the failure.
        reason: String,
    },
    #[error("The tables of database `{catalog_name}` form a foreign key cycle.")]
    /// Error indicating that the foreign keys of a database form a cycle, so
    /// its tables have no topological order.
    CyclicTableDependencies {
        /// Name of the database whose tables form a cycle.
        catalog_name: String,
    },
    #[error("A {object_kind} was declared with no name.")]
    /// A `CREATE` statement declared an object with no name.
    ///
    /// The parser never produces an `ObjectName` with no parts. A caller
    /// reaching this built the name by hand.
    UnnamedObject {
        /// Kind of object the statement tried to create.
        object_kind: ObjectKind,
    },
}

impl From<ParserError> for Error {
    fn from(error: ParserError) -> Self {
        Error::SqlParserError {
            error,
            #[cfg(feature = "std")]
            file: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_error_reasons_render_every_variant() {
        assert_eq!(InheritedChange::AddConstraint.to_string(), "add a constraint to");
        assert_eq!(InheritedChange::AddColumn.to_string(), "add a column to");
        assert_eq!(InheritedChange::RenameColumn.to_string(), "rename a column of");
        assert_eq!(RequiredValue::CoveredByKey.to_string(), "a key covers it");
        assert_eq!(RequiredValue::EnforcedByParent.to_string(), "a parent requires it");
    }

    #[test]
    fn parser_errors_keep_the_parser_cause() {
        let error = Error::from(ParserError::ParserError("broken SQL".to_string()));
        assert!(matches!(error, Error::SqlParserError { .. }));
    }
}
