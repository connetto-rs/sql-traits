//! Error enumeration used in the `sql_traits` crate.

use alloc::{boxed::Box, format, string::String, vec::Vec};

use sqlparser::{ast::FunctionBehavior, parser::ParserError};

use crate::{
    structs::{FunctionKind, IdentifierCase, SequenceType},
    traits::PartitionStrategy,
};

/// Kind of database object a metadata lookup was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// A base table.
    Table,
    /// A table whose rows live behind a foreign server, declared by a `CREATE
    /// FOREIGN TABLE` statement.
    ForeignTable,
    /// A view declared by a `CREATE VIEW` statement.
    View,
    /// A view declared by a `CREATE MATERIALIZED VIEW` statement.
    MaterializedView,
    /// A column declared inside a table.
    Column,
    /// The default value of a column, which PostgreSQL holds apart from the
    /// column itself.
    ColumnDefault,
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
    /// A rewrite rule declared by a `CREATE RULE` statement.
    Rule,
    /// An extension installed by a `CREATE EXTENSION` statement.
    Extension,
    /// A role declared by a `CREATE ROLE` statement.
    Role,
    /// A schema declared by a `CREATE SCHEMA` statement.
    Schema,
    /// A sequence declared by a `CREATE SEQUENCE` statement.
    Sequence,
    /// A domain declared by a `CREATE DOMAIN` statement.
    Domain,
    /// A user-defined type declared by a `CREATE TYPE` statement.
    Type,
    /// An operator the catalog holds, by the schema-qualified symbol it runs
    /// under.
    Operator,
    /// A cast the catalog holds, by its `(source AS target)` spelling.
    Cast,
}

impl core::fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Table => "Table",
            Self::ForeignTable => "Foreign table",
            Self::View => "View",
            Self::MaterializedView => "Materialized view",
            Self::Column => "Column",
            Self::ColumnDefault => "Column default",
            Self::Index => "Index",
            Self::UniqueIndex => "Unique index",
            Self::CheckConstraint => "Check constraint",
            Self::Policy => "Policy",
            Self::Function => "Function",
            Self::Trigger => "Trigger",
            Self::Rule => "Rule",
            Self::Extension => "Extension",
            Self::Role => "Role",
            Self::Schema => "Schema",
            Self::Sequence => "Sequence",
            Self::Domain => "Domain",
            Self::Type => "Type",
            Self::Operator => "Operator",
            Self::Cast => "Cast",
        })
    }
}

/// An attribute `CREATE OPERATOR` and `ALTER OPERATOR` set, a column of
/// `pg_operator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorAttribute {
    /// `COMMUTATOR`, `oprcom`.
    Commutator,
    /// `NEGATOR`, `oprnegate`.
    Negator,
    /// `HASHES`, `oprcanhash`.
    Hashes,
    /// `MERGES`, `oprcanmerge`.
    Merges,
    /// `RESTRICT`, `oprrest`.
    Restrict,
    /// `JOIN`, `oprjoin`.
    Join,
}

impl core::fmt::Display for OperatorAttribute {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Commutator => "commutator",
            Self::Negator => "negator",
            Self::Hashes => "hashes",
            Self::Merges => "merges",
            Self::Restrict => "restrict",
            Self::Join => "join",
        })
    }
}

/// Why PostgreSQL refuses an operator application or a call of a stored
/// expression when it types the expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Unresolved {
    /// No operator or function of the name takes the operand types,
    /// SQLSTATE `42883`.
    DoesNotExist,
    /// More than one takes them and nothing tells them apart, SQLSTATE
    /// `42725`.
    NotUnique,
    /// The one selected has a polymorphic parameter only untyped literals
    /// reach, SQLSTATE `42804`.
    PolymorphicUnknown,
}

impl core::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::DoesNotExist => "does not exist",
            Self::NotUnique => "is not unique",
            Self::PolymorphicUnknown => "cannot bind a polymorphic type to `unknown`",
        })
    }
}

/// A call PostgreSQL refuses for the clauses it writes, given the kind of
/// function it selects, or for clauses no call may combine.
///
/// Each variant names the rule `ParseFuncOrColumn` or the grammar enforces,
/// and [`Misapplied::message`] gives PostgreSQL's wording for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Misapplied {
    /// `f(*)` selecting no aggregate or window function.
    StarOnPlainFunction,
    /// `DISTINCT` among the arguments of no aggregate or window function.
    DistinctOnPlainFunction,
    /// `WITHIN GROUP` on no aggregate or window function.
    WithinGroupOnPlainFunction,
    /// An argument `ORDER BY` on no aggregate or window function.
    OrderByOnPlainFunction,
    /// `FILTER` on no aggregate or window function.
    FilterOnPlainFunction,
    /// `OVER` on no aggregate or window function.
    OverOnPlainFunction,
    /// An ordered-set aggregate called without `WITHIN GROUP`.
    WithinGroupRequired,
    /// An ordered-set aggregate called over a window.
    OverOnOrderedSetAggregate,
    /// `WITHIN GROUP` on an aggregate that is no ordered-set one.
    WithinGroupOnNormalAggregate,
    /// A window function called without `OVER`.
    OverRequired,
    /// `WITHIN GROUP` on a window function.
    WithinGroupOnWindowFunction,
    /// An aggregate called with no arguments and no `*`.
    StarRequired,
    /// An aggregate called without `OVER` and with an argument by name.
    NamedArgumentOnAggregate,
    /// `DISTINCT` among the arguments of a call over a window.
    DistinctOverWindow,
    /// An argument `ORDER BY` in a call over a window.
    OrderByOverWindow,
    /// `FILTER` on a window function.
    FilterOnWindowFunction,
    /// An argument `ORDER BY` beside `WITHIN GROUP`.
    OrderByWithinGroup,
    /// `DISTINCT` beside `WITHIN GROUP`.
    DistinctWithinGroup,
}

impl Misapplied {
    /// Returns the message PostgreSQL 18 refuses a call of `function`
    /// with, the function named as the call writes it.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::errors::Misapplied;
    ///
    /// assert_eq!(
    ///     Misapplied::OverRequired.message("row_number"),
    ///     "window function row_number requires an OVER clause",
    /// );
    /// ```
    #[must_use]
    pub fn message(self, function: &str) -> String {
        let not_aggregate = |clause: &str| {
            format!("{clause} specified, but {function} is not an aggregate function")
        };
        match self {
            Self::StarOnPlainFunction => not_aggregate(&format!("{function}(*)")),
            Self::DistinctOnPlainFunction => not_aggregate("DISTINCT"),
            Self::WithinGroupOnPlainFunction => not_aggregate("WITHIN GROUP"),
            Self::OrderByOnPlainFunction => not_aggregate("ORDER BY"),
            Self::FilterOnPlainFunction => not_aggregate("FILTER"),
            Self::OverOnPlainFunction => {
                format!(
                    "OVER specified, but {function} is not a window function nor an aggregate function"
                )
            }
            Self::WithinGroupRequired => {
                format!("WITHIN GROUP is required for ordered-set aggregate {function}")
            }
            Self::OverOnOrderedSetAggregate => {
                format!("OVER is not supported for ordered-set aggregate {function}")
            }
            Self::WithinGroupOnNormalAggregate => {
                format!(
                    "{function} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"
                )
            }
            Self::OverRequired => format!("window function {function} requires an OVER clause"),
            Self::WithinGroupOnWindowFunction => {
                format!("window function {function} cannot have WITHIN GROUP")
            }
            Self::StarRequired => {
                format!("{function}(*) must be used to call a parameterless aggregate function")
            }
            Self::NamedArgumentOnAggregate => "aggregates cannot use named arguments".into(),
            Self::DistinctOverWindow => "DISTINCT is not implemented for window functions".into(),
            Self::OrderByOverWindow => {
                "aggregate ORDER BY is not implemented for window functions".into()
            }
            Self::FilterOnWindowFunction => {
                "FILTER is not implemented for non-aggregate window functions".into()
            }
            Self::OrderByWithinGroup => {
                "cannot use multiple ORDER BY clauses with WITHIN GROUP".into()
            }
            Self::DistinctWithinGroup => "cannot use DISTINCT with WITHIN GROUP".into(),
        }
    }
}

/// A constraint PostgreSQL enforces against the rows a table stores, which a
/// foreign table, storing none, cannot carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyConstraint {
    /// A `PRIMARY KEY` constraint.
    PrimaryKey,
    /// A `UNIQUE` constraint.
    Unique,
    /// A `FOREIGN KEY` or `REFERENCES` constraint.
    ForeignKey,
    /// An `EXCLUDE` constraint.
    Exclusion,
}

impl core::fmt::Display for KeyConstraint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::PrimaryKey => "Primary key",
            Self::Unique => "Unique",
            Self::ForeignKey => "Foreign key",
            Self::Exclusion => "Exclusion",
        })
    }
}

/// A place PostgreSQL stores an expression it evaluates without running a
/// query, and so refuses a subquery in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpressionPlace {
    /// A `CHECK` constraint of a table or a domain.
    CheckConstraint,
    /// The `DEFAULT` of a column or a domain.
    Default,
    /// The expression a generated column is computed from.
    Generation,
    /// An expression an index or an exclusion constraint is built over.
    IndexExpression,
    /// The `WHERE` predicate of a partial index or exclusion constraint.
    IndexPredicate,
    /// An expression a table is partitioned by.
    PartitionKey,
    /// A value bounding a partition.
    PartitionBound,
    /// The `USING` expression of `ALTER COLUMN ... TYPE`.
    Transform,
    /// The `WHEN` condition of a trigger.
    TriggerWhen,
}

impl core::fmt::Display for ExpressionPlace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::CheckConstraint => "check constraint",
            Self::Default => "DEFAULT expression",
            Self::Generation => "column generation expression",
            Self::IndexExpression => "index expression",
            Self::IndexPredicate => "index predicate",
            Self::PartitionKey => "partition key expression",
            Self::PartitionBound => "partition bound",
            Self::Transform => "transform expression",
            Self::TriggerWhen => "trigger WHEN condition",
        })
    }
}

/// A change to a table that PostgreSQL refuses to keep from the tables below.
///
/// `ONLY` asks for the named table alone, and PostgreSQL grants that only when
/// what is asked for can be undone one table at a time. Adding is not one of
/// those, because a table below would then be missing something its parent
/// declares, and neither is renaming, because the two would disagree on what
/// the column is called. Validating a check is refused too, because the
/// parent's could then vouch for rows a table below never had checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InheritedChange {
    /// `ALTER TABLE ONLY ... ADD CONSTRAINT`.
    AddConstraint,
    /// `ALTER TABLE ONLY ... ADD COLUMN`.
    AddColumn,
    /// `ALTER TABLE ONLY ... RENAME COLUMN`.
    RenameColumn,
    /// `ALTER TABLE ONLY ... VALIDATE CONSTRAINT`.
    ValidateConstraint,
    /// `ALTER TABLE ONLY ... ALTER COLUMN ... TYPE`.
    AlterColumnType,
    /// `ALTER TABLE ONLY ... DROP COLUMN` on a partitioned table with
    /// partitions, which hold every column of their root.
    DropColumn,
    /// `ALTER TABLE ONLY ... ALTER COLUMN ... SET NOT NULL` on a partitioned
    /// table with partitions.
    SetNotNull,
    /// `ALTER TABLE ONLY ... RENAME CONSTRAINT` of a constraint the tables
    /// below hold copies of.
    RenameConstraint,
}

impl core::fmt::Display for InheritedChange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::AddConstraint => "add a constraint to",
            Self::AddColumn => "add a column to",
            Self::RenameColumn => "rename a column of",
            Self::ValidateConstraint => "validate a constraint of",
            Self::AlterColumnType => "change a column type of",
            Self::DropColumn => "drop a column of",
            Self::SetNotNull => "require a value in a column of",
            Self::RenameConstraint => "rename a constraint of",
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

/// Why a table's persistence may not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixedPersistence {
    /// A temporary table lives and dies with its session, so it is never
    /// logged and never unlogged.
    Temporary,
    /// A partitioned table holds no rows of its own, so PostgreSQL 18 gives it
    /// no persistence to change.
    Partitioned,
}

impl core::fmt::Display for FixedPersistence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Temporary => "it is temporary",
            Self::Partitioned => "it is partitioned",
        })
    }
}

/// What an existing table lacks of the shape of the table it is made a child
/// of, by `ALTER TABLE ... INHERIT` or `ATTACH PARTITION`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChildMismatch {
    /// The table lacks a column of the parent.
    MissingColumn {
        /// The parent's column.
        column_name: String,
    },
    /// The column has another type than the parent's.
    ColumnType {
        /// The column.
        column_name: String,
    },
    /// The column has another collation than the parent's.
    ColumnCollation {
        /// The column.
        column_name: String,
    },
    /// The parent requires the column to hold a value and the table does not.
    ColumnNullable {
        /// The column.
        column_name: String,
    },
    /// The parent generates the column and the table does not.
    MustBeGenerated {
        /// The column.
        column_name: String,
    },
    /// The table generates the column and the parent does not.
    MustNotBeGenerated {
        /// The column.
        column_name: String,
    },
    /// The table lacks a check the parent passes down, named as the parent
    /// names it or, for an unnamed check, written as the check.
    MissingConstraint {
        /// The check's name, or the check itself.
        constraint_name: String,
    },
    /// The table holds a check of the parent's name with another expression.
    ConstraintDiffers {
        /// The check's name.
        constraint_name: String,
    },
    /// The table's check of the parent's name is marked `NO INHERIT`.
    ConstraintNotInherited {
        /// The check's name.
        constraint_name: String,
    },
    /// The table holds the parent's validated check `NOT VALID`.
    ConstraintNotValid {
        /// The check's name.
        constraint_name: String,
    },
}

impl core::fmt::Display for ChildMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingColumn { column_name } => write!(f, "it lacks column `{column_name}`"),
            Self::ColumnType { column_name } => {
                write!(f, "column `{column_name}` has another type")
            }
            Self::ColumnCollation { column_name } => {
                write!(f, "column `{column_name}` has another collation")
            }
            Self::ColumnNullable { column_name } => {
                write!(f, "column `{column_name}` must be marked `NOT NULL`")
            }
            Self::MustBeGenerated { column_name } => {
                write!(f, "column `{column_name}` must be a generated column")
            }
            Self::MustNotBeGenerated { column_name } => {
                write!(f, "column `{column_name}` must not be a generated column")
            }
            Self::MissingConstraint { constraint_name } => {
                write!(f, "it lacks check `{constraint_name}`")
            }
            Self::ConstraintDiffers { constraint_name } => {
                write!(f, "its check `{constraint_name}` differs from the parent's")
            }
            Self::ConstraintNotInherited { constraint_name } => {
                write!(f, "its check `{constraint_name}` is marked `NO INHERIT`")
            }
            Self::ConstraintNotValid { constraint_name } => {
                write!(f, "its check `{constraint_name}` is `NOT VALID` and the parent's is not")
            }
        }
    }
}

/// Why PostgreSQL refuses the bound a partition is given.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InvalidBound {
    /// The bound has the form of another strategy than its table's.
    StrategyMismatch {
        /// The strategy the partitioned table routes rows by.
        strategy: PartitionStrategy,
    },
    /// A side of a range bound lists another number of values than the key
    /// has columns.
    ValueCount {
        /// The number of key columns.
        expected: usize,
        /// The number of values written.
        found: usize,
    },
    /// A range bound lists `NULL`.
    NullInRange,
    /// A value follows `MINVALUE` or `MAXVALUE` in a range bound, which takes
    /// only more of the same infinity there.
    ValueAfterInfinity,
    /// A bound value reads a column.
    ColumnReference,
    /// The partition is the default of a hash-partitioned table.
    DefaultOnHash,
    /// The partitioned table already has a default partition.
    DefaultExists {
        /// The existing default partition.
        default_partition: String,
    },
    /// The hash modulus is zero.
    ModulusNotPositive,
    /// The hash remainder is not below the modulus.
    RemainderNotBelowModulus,
    /// The hash modulus and the modulus of a sibling partition do not divide
    /// one another.
    ModulusNotFactor {
        /// The sibling partition.
        other_partition: String,
    },
    /// The range takes no value, its lower bound not being below its upper.
    EmptyRange,
    /// A value cannot be read as the type of its key column.
    ValueNotOfKeyType {
        /// The value, as written.
        value: String,
        /// The key column's type.
        type_name: String,
    },
}

impl core::fmt::Display for InvalidBound {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::StrategyMismatch { strategy } => {
                let name = match strategy {
                    PartitionStrategy::Range => "range",
                    PartitionStrategy::List => "list",
                    PartitionStrategy::Hash => "hash",
                };
                write!(f, "it is not a {name} bound")
            }
            Self::ValueCount { expected, found } => {
                write!(f, "it lists {found} values for {expected} key columns")
            }
            Self::NullInRange => f.write_str("a range bound cannot list NULL"),
            Self::ValueAfterInfinity => {
                f.write_str("every value after MINVALUE or MAXVALUE must be the same")
            }
            Self::ColumnReference => f.write_str("a bound cannot read a column"),
            Self::DefaultOnHash => f.write_str("a hash-partitioned table takes no default"),
            Self::DefaultExists { default_partition } => {
                write!(f, "`{default_partition}` already is the default partition")
            }
            Self::ModulusNotPositive => f.write_str("the modulus must be greater than zero"),
            Self::RemainderNotBelowModulus => {
                f.write_str("the remainder must be less than the modulus")
            }
            Self::ModulusNotFactor { other_partition } => {
                write!(f, "its modulus and that of `{other_partition}` do not divide one another")
            }
            Self::EmptyRange => f.write_str("its range is empty"),
            Self::ValueNotOfKeyType { value, type_name } => {
                write!(f, "`{value}` is not a value of type `{type_name}`")
            }
        }
    }
}

/// Why `ALTER TABLE ... ATTACH PARTITION` refuses the table it names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UnattachablePartition {
    /// The table already is a partition.
    AlreadyPartition,
    /// The table inherits from a parent through `INHERITS`.
    InheritanceChild,
    /// Another table inherits from it through `INHERITS`.
    InheritanceParent,
    /// The table is the partitioned table itself or one of its ancestors.
    Circular,
    /// The table declares a column the partitioned table does not.
    ExtraColumn {
        /// Name of the column.
        column_name: String,
    },
    /// The table declares an identity column of its own.
    IdentityColumn {
        /// Name of the column.
        column_name: String,
    },
    /// The table lacks the partitioned table's shape.
    Shape(ChildMismatch),
    /// The table holds a primary key on other columns than the partitioned
    /// table's.
    OtherPrimaryKey,
    /// The table is a foreign table and the partitioned table holds a unique
    /// index, which a foreign table cannot carry.
    ForeignTableUnderUniqueIndex,
}

impl core::fmt::Display for UnattachablePartition {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyPartition => f.write_str("it already is a partition"),
            Self::InheritanceChild => f.write_str("it inherits from another table"),
            Self::InheritanceParent => f.write_str("another table inherits from it"),
            Self::Circular => f.write_str("it is the partitioned table or one of its ancestors"),
            Self::ExtraColumn { column_name } => {
                write!(f, "column `{column_name}` is not found in the partitioned table")
            }
            Self::IdentityColumn { column_name } => {
                write!(f, "column `{column_name}` is an identity column")
            }
            Self::Shape(mismatch) => mismatch.fmt(f),
            Self::OtherPrimaryKey => f.write_str("it holds a primary key of its own"),
            Self::ForeignTableUnderUniqueIndex => {
                f.write_str("it is a foreign table and the partitioned table holds a unique index")
            }
        }
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
    /// Relation resolution found neither a table nor a view for a name that is
    /// required to denote one of the two.
    #[error("Relation `{object_name}` not found.")]
    RelationNotFound {
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
        /// Best-effort rendering identifying the object by name, its declaring
        /// table, or the sequence it owns. Use [`ObjectKind::not_in_database`]
        /// for a name or [`ObjectKind::anonymous_not_in_database`] for a
        /// declaring table.
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
    /// A call handed to a per-call resolver is not a node of the expressions
    /// the queried object holds, for instance because it was borrowed from a
    /// copy of them.
    #[error("Call `{call}` is not a node of the queried object's expressions.")]
    CallNotInExpression {
        /// The call as rendered by sqlparser.
        call: String,
    },
    /// An expression handed to an expression context is not a node of the
    /// query it was built over, for instance because it was borrowed from a
    /// copy of that query.
    #[error("Expression `{expression}` is not a node of the context's query.")]
    ExpressionNotInQuery {
        /// The expression as rendered by sqlparser.
        expression: String,
    },
    /// The argument types handed in with a call do not give one type per
    /// argument the call writes.
    #[error("Call `{call}` writes {arguments} arguments, but {types} argument types were given.")]
    ArgumentTypeCount {
        /// The call as rendered by sqlparser.
        call: String,
        /// How many arguments the call writes.
        arguments: usize,
        /// How many argument types were given.
        types: usize,
    },
    /// A function body is written in a language other than SQL or PL/pgSQL.
    #[error("Function `{function}` is written in `{language}`, which is not read as SQL.")]
    FunctionBodyNotSql {
        /// Name of the function, as written.
        function: String,
        /// Language the function declares, as PostgreSQL stores it.
        language: String,
    },
    /// A function's code lives outside the schema: it declares no body, calls a
    /// remote endpoint, or loads a class from a `USING` resource.
    #[error("Function `{function}` has no body in the schema to read.")]
    FunctionBodyUnavailable {
        /// Name of the function, as written.
        function: String,
    },
    /// A function body runs code outside it, such as `CALL`, `EXECUTE` or a
    /// `set_config` of `search_path` before an unqualified name.
    #[error(
        "The body of function `{function}` runs `{statement}`, so what it reads is not known here."
    )]
    FunctionBodyUnresolvable {
        /// Name of the function, as written.
        function: String,
        /// The statement that hides what the body reads.
        statement: String,
    },
    /// A SQL function body does not parse under the database's dialect, a
    /// PL/pgSQL body or the SQL it embeds does not parse, or the definition a
    /// catalog prints for a bound body does not parse or holds no body bound
    /// at creation.
    #[error("The body of function `{function}` does not parse: {reason}")]
    FunctionBodyUnparsable {
        /// Name of the function, as written.
        function: String,
        /// The parser's account of what it could not read.
        reason: String,
    },
    /// A quoted literal of a policy expression is read as a date or time value
    /// when the policy is created, under session state the input does not
    /// settle, such as the `TimeZone`, the `DateStyle` order or the current
    /// time, or in a spelling PostgreSQL refuses.
    #[error(
        "Policy `{policy}` reads the literal '{literal}' as {data_type} when it is created, and that value is not known here: {reason}"
    )]
    PolicyLiteralUnreadable {
        /// Name of the policy, as written.
        policy: String,
        /// The literal's text, as written.
        literal: String,
        /// The type PostgreSQL reads the literal as.
        data_type: sql_values::PgType,
        /// What reading the literal needed or refused.
        reason: sql_values::DateTimeError,
    },
}

/// Why a text could not be read back as a target name by
/// [`TargetName::parse`](crate::structs::TargetName::parse).
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum TargetNameParseError {
    /// The text was empty or only whitespace, so it names nothing.
    #[error("cannot parse a target name from empty or blank text")]
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
    /// name was required, as in `a"b`, `"a"x` or `ord ers`.
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

/// Why a [`Deployment`](crate::structs::Deployment) description is refused.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum DeploymentError {
    /// No acting role was named, so a request would run as nothing.
    #[error("a deployment needs at least one acting role")]
    NoActingRole,
    /// The same acting role was named twice.
    #[error("acting role `{role}` is named twice")]
    DuplicateActingRole {
        /// The repeated role.
        role: String,
    },
    /// Two settings share a name, compared without case as PostgreSQL does.
    #[error("setting `{name}` is declared twice")]
    DuplicateSetting {
        /// The second declaration's name.
        name: String,
    },
    /// The name is not a custom setting name, two or more non-empty parts
    /// joined by dots.
    #[error("`{name}` is not a custom setting name such as `app.user_id`")]
    InvalidSettingName {
        /// The refused name.
        name: String,
    },
    /// A set is split on the empty string, which `string_to_array` reads as
    /// one field.
    #[error("setting `{setting}` splits a set on an empty separator")]
    EmptySeparator {
        /// The setting holding the set.
        setting: String,
    },
    /// A JSON field was declared with no key.
    #[error("a JSON field needs at least one key")]
    EmptyFieldPath,
    /// A JSON setting was declared with no field.
    #[error("JSON setting `{setting}` declares no field")]
    NoJsonFields {
        /// The setting.
        setting: String,
    },
    /// Two fields of one JSON setting share a path.
    #[error("JSON setting `{setting}` declares the field {path:?} twice")]
    DuplicateJsonField {
        /// The setting.
        setting: String,
        /// The repeated path.
        path: Vec<String>,
    },
    /// The database declares no role by this name.
    #[error("role `{role}` is not declared in the database")]
    UndeclaredRole {
        /// The missing role.
        role: String,
    },
    /// The default `TimeZone` is one PostgreSQL refuses.
    #[error("`{zone}` is no `TimeZone` PostgreSQL accepts: {reason}")]
    InvalidTimeZone {
        /// The refused zone.
        zone: String,
        /// Why PostgreSQL refuses it.
        #[source]
        reason: sql_values::TimeZoneError,
    },
    /// The role is not one the deployment acts as.
    #[error("role `{role}` is not an acting role of the deployment")]
    NotAnActingRole {
        /// The role asked about.
        role: String,
    },
    /// No `ALTER ROLE ... SET` reaching a request sets `TimeZone`, and the
    /// deployment declares no server default.
    #[error(
        "no role setting gives the request a `TimeZone` and the deployment declares no default"
    )]
    NoDefaultTimeZone,
    /// A `TimeZone` set `IN DATABASE` may reach a request, and the deployment
    /// names no database to match it against.
    #[error("a `TimeZone` set in one database reaches the request, and the deployment names none")]
    NoDatabase {
        /// The role the setting is for, [`None`] for every role.
        role: Option<String>,
    },
}

/// An illegal combination of a policy command and stored clauses.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyClauseError {
    /// An `INSERT` policy stores `USING`.
    #[error("only WITH CHECK expression allowed for INSERT")]
    UsingForInsert,
    /// A `SELECT` or `DELETE` policy stores `WITH CHECK`.
    #[error("WITH CHECK cannot be applied to SELECT or DELETE")]
    WithCheckForSelectOrDelete,
}

/// Why [`TableLike::row_security`](crate::traits::TableLike::row_security)
/// cannot answer.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RowSecurityError {
    /// The table is not in the database it was asked about.
    #[error(transparent)]
    Lookup(#[from] LookupError),
    /// A policy's `TO` list reaches the role through no declared role, and one
    /// entry cannot be settled from the schema: a role the schema never
    /// declares, or `CURRENT_USER`, `CURRENT_ROLE` or `SESSION_USER`, which
    /// name whoever ran `CREATE POLICY`.
    #[error("policy `{policy}` names `{role}`, which the schema cannot settle")]
    UnsettledRole {
        /// The policy.
        policy: String,
        /// The entry, as written.
        role: String,
    },
    /// A policy gating the statement is scoped to a role, and the caller holds
    /// none.
    #[error("policy `{policy}` is scoped to a role, and the caller holds none")]
    RoleRequired {
        /// The policy.
        policy: String,
    },
    /// A plain view the policies read runs its reads as its owner, and the
    /// schema never names the owner.
    #[error("the policies read view `{view}`, whose owner the schema does not name")]
    UnsettledViewOwner {
        /// The view.
        view: String,
    },
}

/// Why PostgreSQL refuses the parameters of a sequence.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SequenceParameterError {
    /// `AS` names a type other than `smallint`, `integer` and `bigint`, or an
    /// identity is declared on a column of such a type.
    #[error("sequence type must be smallint, integer, or bigint, not {data_type}")]
    UnsupportedType {
        /// The type, as written.
        data_type: String,
    },
    /// An option's value is not an integer literal a `bigint` holds.
    #[error("{option} value `{value}` is not a bigint")]
    NotABigint {
        /// The option's keyword.
        option: &'static str,
        /// The value, as written.
        value: String,
    },
    /// `INCREMENT` is zero.
    #[error("INCREMENT must not be zero")]
    ZeroIncrement,
    /// `MAXVALUE` falls outside the sequence's type.
    #[error("MAXVALUE ({max_value}) is out of range for sequence data type {data_type}")]
    MaxValueOutOfRange {
        /// The largest value.
        max_value: i64,
        /// The sequence's type.
        data_type: SequenceType,
    },
    /// `MINVALUE` falls outside the sequence's type.
    #[error("MINVALUE ({min_value}) is out of range for sequence data type {data_type}")]
    MinValueOutOfRange {
        /// The smallest value.
        min_value: i64,
        /// The sequence's type.
        data_type: SequenceType,
    },
    /// `MINVALUE` is not below `MAXVALUE`.
    #[error("MINVALUE ({min_value}) must be less than MAXVALUE ({max_value})")]
    MinValueNotBelowMaxValue {
        /// The smallest value.
        min_value: i64,
        /// The largest value.
        max_value: i64,
    },
    /// `START` is below `MINVALUE`.
    #[error("START value ({start}) cannot be less than MINVALUE ({min_value})")]
    StartBelowMinValue {
        /// The first value.
        start: i64,
        /// The smallest value.
        min_value: i64,
    },
    /// `START` is above `MAXVALUE`.
    #[error("START value ({start}) cannot be greater than MAXVALUE ({max_value})")]
    StartAboveMaxValue {
        /// The first value.
        start: i64,
        /// The largest value.
        max_value: i64,
    },
    /// `CACHE` is not positive.
    #[error("CACHE ({cache}) must be greater than zero")]
    CacheNotPositive {
        /// The number of values drawn ahead.
        cache: i64,
    },
}

/// Why [`FunctionLike::helper_set`](crate::traits::FunctionLike::helper_set)
/// refuses to answer what a helper returns.
///
/// Each refusal names the helper it is met in, a helper a parent arm calls
/// where it is met there.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum HelperSetError {
    /// A table, function or column of the body cannot be looked up.
    #[error(transparent)]
    Lookup(#[from] LookupError),
    /// The helper is not written in `sql`.
    #[error("helper `{function}` is written in {}, not sql", language.as_deref().unwrap_or("no language"))]
    Language {
        /// The helper.
        function: String,
        /// The language it names, if any.
        language: Option<String>,
    },
    /// The helper is not `STABLE`.
    #[error("helper `{function}` is {volatility}, not STABLE")]
    Volatility {
        /// The helper.
        function: String,
        /// Its volatility.
        volatility: FunctionBehavior,
    },
    /// The helper runs as its caller, so its body reads through the caller's
    /// policies.
    #[error("helper `{function}` is not SECURITY DEFINER")]
    SecurityInvoker {
        /// The helper.
        function: String,
    },
    /// The helper does not return `SETOF` one scalar type.
    #[error("helper `{function}` does not return SETOF a scalar type")]
    ReturnType {
        /// The helper.
        function: String,
    },
    /// The helper does not set its own `search_path`, so its names resolve
    /// under whatever path the caller set.
    #[error("helper `{function}` does not pin its search_path")]
    UnpinnedSearchPath {
        /// The helper.
        function: String,
    },
    /// The argument list does not match the helper's input parameters.
    #[error("helper `{function}` takes {expected} arguments, not {given}")]
    ArgumentCount {
        /// The helper.
        function: String,
        /// Its input parameters.
        expected: usize,
        /// The arguments passed.
        given: usize,
    },
    /// An argument is not a string, number, boolean or `NULL` literal.
    #[error("helper `{function}` is passed `{argument}`, which is not a comparable literal")]
    UnsupportedArgument {
        /// The helper.
        function: String,
        /// The argument.
        argument: String,
    },
    /// The schema records no owner for the helper.
    #[error("the schema records no owner for helper `{function}`")]
    UnsettledOwner {
        /// The helper.
        function: String,
    },
    /// The helper's owner is a role the schema does not declare.
    #[error("helper `{function}` is owned by `{owner}`, which the schema does not declare")]
    UndeclaredOwner {
        /// The helper.
        function: String,
        /// The owner.
        owner: String,
    },
    /// The owner is not exempt from the row-level security of a table the
    /// body reads.
    #[error("owner `{owner}` of helper `{function}` reads `{table}` through its policies")]
    NotExempt {
        /// The helper.
        function: String,
        /// The owner.
        owner: String,
        /// The table.
        table: String,
    },
    /// The owner holds `SELECT` neither on a table the body reads nor on
    /// every column of it the body reads, so every call raises `42501`.
    #[error("owner `{owner}` of helper `{function}` lacks SELECT on `{table}`")]
    MissingSelect {
        /// The helper.
        function: String,
        /// The owner.
        owner: String,
        /// The table.
        table: String,
    },
    /// The owner lacks `USAGE` on the schema of a table or helper the body
    /// names. A qualified name then raises `42501`, and an unqualified one
    /// skips the schema on the path and finds nothing there.
    #[error("owner `{owner}` of helper `{function}` lacks USAGE on schema `{schema}`")]
    MissingUsage {
        /// The helper.
        function: String,
        /// The owner.
        owner: String,
        /// The schema.
        schema: String,
    },
    /// The owner lacks `EXECUTE` on a helper the body calls, so every call
    /// raises `42501`.
    #[error("owner `{owner}` of helper `{function}` lacks EXECUTE on `{called}`")]
    MissingExecute {
        /// The helper.
        function: String,
        /// The owner.
        owner: String,
        /// The helper called.
        called: String,
    },
    /// The body reads a setting the deployment does not declare.
    #[error("helper `{function}` reads setting `{setting}`, which the deployment does not declare")]
    UndeclaredSetting {
        /// The helper.
        function: String,
        /// The setting, as written.
        setting: String,
    },
    /// The body compares a column to a setting the deployment declares as
    /// something other than the caller.
    #[error(
        "helper `{function}` compares a column to setting `{setting}`, which is not the caller"
    )]
    SettingNotCaller {
        /// The helper.
        function: String,
        /// The setting.
        setting: String,
    },
    /// A grant arm compares a column to a parameter where it would compare it
    /// to the caller, so whoever calls the helper chooses whose grants it
    /// returns.
    #[error("helper `{function}` takes its caller from parameter `{parameter}`")]
    CallerFromParameter {
        /// The helper.
        function: String,
        /// The parameter.
        parameter: String,
    },
    /// A grant arm's condition, its arguments substituted, reduces to
    /// neither `TRUE` nor a set of literal values of one column.
    #[error("helper `{function}` filters by `{condition}`, which reduces to no literal set")]
    Irreducible {
        /// The helper.
        function: String,
        /// The condition, as written.
        condition: String,
    },
    /// The body is not a `UNION` of grant arms and parent arms.
    #[error("helper `{function}` returns `{arm}`, which is neither a grant arm nor a parent arm")]
    ArmShape {
        /// The helper.
        function: String,
        /// The query that has neither shape, as written.
        arm: String,
    },
    /// A parent arm compares a column with no single-column foreign key to a
    /// helper whose set covers no one table, so nothing names the parent
    /// table.
    #[error(
        "helper `{function}` compares `{column}`, which has no single-column foreign key, to `{called}`, whose set covers no single table"
    )]
    UncoveredParent {
        /// The helper.
        function: String,
        /// The column.
        column: String,
        /// The helper called.
        called: String,
    },
    /// A parent arm's call reaches no function or several.
    #[error("helper `{function}` calls `{call}`, which reaches no single function")]
    UnresolvedCall {
        /// The helper.
        function: String,
        /// The call, as written.
        call: String,
    },
    /// The parent arms reach a helper already being answered, which PostgreSQL
    /// calls until the stack runs out (`54001`).
    #[error("helper `{function}` reaches itself through its parent arms")]
    CallCycle {
        /// The helper reached again.
        function: String,
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
    #[error(
        "{object_kind} `{parent_name}` cannot be inherited from by table `{table_name}`, since only a table or foreign table can."
    )]
    /// Error indicating that `INHERITS` names a view, materialized view or
    /// sequence.
    InheritsFromNonTable {
        /// Table naming the parent.
        table_name: String,
        /// Parent it names, as written.
        parent_name: String,
        /// What the name reaches.
        object_kind: ObjectKind,
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
        "Cannot rename column `{column_name}` of table `{table_name}` because it is inherited."
    )]
    /// Error indicating that a table tried to rename a column it receives from
    /// a parent, which only the parent can rename, for every table below it at
    /// once.
    InheritedColumnNotRenamable {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the inherited column.
        column_name: String,
    },
    #[error("Cannot add column `{column_name}` to table `{table_name}` because it is a partition.")]
    /// Error indicating that `ALTER TABLE ... ADD COLUMN` names a partition,
    /// whose columns are all its root's, even when `IF NOT EXISTS` would skip
    /// the column.
    ColumnAddedToPartition {
        /// Name of the partition the statement names.
        table_name: String,
        /// Name of the column the statement adds.
        column_name: String,
    },
    #[error(
        "Cannot add an identity to column `{column_name}` of table `{table_name}` because it is a partition."
    )]
    /// Error indicating that `ALTER COLUMN ... ADD GENERATED ... AS IDENTITY`
    /// names a column of a partition, which only its root may make an
    /// identity.
    IdentityAddedToPartitionColumn {
        /// Name of the partition the statement names.
        table_name: String,
        /// Name of the column.
        column_name: String,
    },
    #[error(
        "Cannot add an identity to column `{column_name}` of only the partitioned table `{table_name}`."
    )]
    /// Error indicating that `ALTER TABLE ONLY ... ADD GENERATED ... AS
    /// IDENTITY` names a partitioned table, whose partitions would then lack
    /// the identity, which PostgreSQL refuses even before any partition exists.
    IdentityAddedToOnlyPartitionedTable {
        /// Name of the partitioned table the statement names.
        table_name: String,
        /// Name of the column.
        column_name: String,
    },
    #[error(
        "Cannot alter type of column `{column_name}` of table `{table_name}` because it is inherited."
    )]
    /// Error indicating that a table tried to change the type of a column it
    /// receives from a parent, which only the parent can change, for every
    /// table below it at once.
    InheritedColumnTypeNotAlterable {
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
    #[error("Cannot add a `NO INHERIT` not-null constraint to partitioned table `{table_name}`.")]
    /// Error indicating that a `NO INHERIT` table-level `NOT NULL` was written
    /// on a partitioned table, which PostgreSQL refuses because a partitioned
    /// table's not-null constraints are enforced on its partitions.
    NoInheritNotNullOnPartitionedTable {
        /// Name of the partitioned table.
        table_name: String,
    },
    #[error(
        "Column `{column_name}` of table `{table_name}` is written `NOT NULL` under both `{first_name}` and `{second_name}`."
    )]
    /// Error indicating that one table-level `NOT NULL` constraint of a table
    /// is written twice under two different names, which PostgreSQL refuses
    /// because the two spellings are one constraint and carry one name.
    ConflictingNotNullConstraintNames {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the column the constraint holds.
        column_name: String,
        /// Name of the first spelling.
        first_name: String,
        /// Name of the second spelling.
        second_name: String,
    },
    #[error(
        "Table `{table_name}` writes a `NO INHERIT` not-null constraint on column `{column_name}` beside another spelling of it."
    )]
    /// Error indicating that a `NO INHERIT` table-level `NOT NULL` of a table
    /// meets another spelling of the same requirement, a column option or a
    /// second table-level one, which PostgreSQL refuses because the two
    /// spellings are one constraint that cannot hold both inheritabilities.
    ConflictingNoInheritNotNull {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the column the constraint holds.
        column_name: String,
    },
    #[error(
        "Cannot create not-null constraint `{constraint_name}` on column `{column_name}` of table `{table_name}`, a not-null constraint named `{existing_name}` already exists for that column."
    )]
    /// Error indicating that `ALTER TABLE ... ADD CONSTRAINT ... NOT NULL`
    /// named a column the table already requires a value of, whichever
    /// spelling states it, so PostgreSQL refuses the second requirement.
    NotNullConstraintAlreadyExists {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the column the constraint would hold.
        column_name: String,
        /// Name the statement wrote for the arriving constraint.
        constraint_name: String,
        /// Name of the requirement the column already holds.
        existing_name: String,
    },
    #[error(
        "Cannot change the `NO INHERIT` status of not-null constraint `{constraint_name}` on table `{table_name}`."
    )]
    /// Error indicating that a statement would flip the inheritability of a
    /// table's not-null requirement, either by adding a `NOT NULL` of another
    /// status beside the one the column already holds or by `SET NOT NULL`
    /// over a `NO INHERIT` requirement, which PostgreSQL refuses.
    NotNullNoInheritStatusChange {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the requirement the column already holds.
        constraint_name: String,
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
    #[error("Operator `{operator_name}` declares no right operand type.")]
    /// Error indicating that a `CREATE OPERATOR` names no `RIGHTARG`, which
    /// every operator takes since PostgreSQL 14 dropped postfix operators.
    OperatorRightArgumentRequired {
        /// Symbol of the operator the statement creates.
        operator_name: Box<str>,
    },
    #[error("Function `{function_name}` named by operator `{operator_name}` not found.")]
    /// Error indicating that the function, restriction estimator or join
    /// estimator an operator names takes no arguments of exactly the types the
    /// operator requires.
    OperatorFunctionNotFound {
        /// Symbol of the operator the statement creates or alters.
        operator_name: Box<str>,
        /// Name of the function as written.
        function_name: Box<str>,
    },
    #[error("Operator `{operator_name}` already exists.")]
    /// Error indicating that an operator of the same schema, symbol and
    /// operand types is already defined, a shell excepted.
    OperatorAlreadyExists {
        /// Symbol of the operator the statement creates or moves.
        operator_name: Box<str>,
    },
    #[error("Operator `{operator_name}` cannot be its own negator.")]
    /// Error indicating that an operator names itself as its negator.
    OperatorOwnNegator {
        /// Symbol of the operator the statement creates or alters.
        operator_name: Box<str>,
    },
    #[error("Only boolean operators can carry the {attribute} attribute set on `{operator_name}`.")]
    /// Error indicating that an operator returning no `bool` takes a negator,
    /// `HASHES`, `MERGES` or an estimator.
    OperatorAttributeNeedsBoolean {
        /// Symbol of the operator the statement creates or alters.
        operator_name: Box<str>,
        /// The attribute refused.
        attribute: OperatorAttribute,
    },
    #[error("Only binary operators can carry the {attribute} attribute set on `{operator_name}`.")]
    /// Error indicating that a prefix operator takes a commutator, `HASHES`,
    /// `MERGES` or a join estimator.
    OperatorAttributeNeedsBinary {
        /// Symbol of the operator the statement creates or alters.
        operator_name: Box<str>,
        /// The attribute refused.
        attribute: OperatorAttribute,
    },
    #[error(
        "The {attribute} operator `{linked_name}` of `{operator_name}` is already the {attribute} of operator `{linked_to}`."
    )]
    /// Error indicating that the commutator or negator an operator names
    /// already links to another operator.
    OperatorLinkTaken {
        /// Symbol of the operator the statement creates or alters.
        operator_name: Box<str>,
        /// The link refused.
        attribute: OperatorAttribute,
        /// Symbol of the operator the link names.
        linked_name: Box<str>,
        /// Symbol of the operator that operator links to.
        linked_to: Box<str>,
    },
    #[error(
        "Role `{role_name}` creating operator `{operator_name}` cannot execute `{function_name}`."
    )]
    /// Error indicating that the role creating an operator holds no `EXECUTE`
    /// on its function, which PostgreSQL checks as it creates the operator.
    OperatorFunctionNotExecutable {
        /// Symbol of the operator the statement creates.
        operator_name: Box<str>,
        /// Name of the function.
        function_name: Box<str>,
        /// Name of the creating role.
        role_name: Box<str>,
    },
    #[error("Operator `{operator_name}` not found.")]
    /// Error indicating that `ALTER OPERATOR`, `DROP OPERATOR` without `IF
    /// EXISTS`, or a commutator or negator `ALTER OPERATOR` sets, names an
    /// operator of operand types the catalog does not hold.
    OperatorNotFound {
        /// Symbol of the operator as written.
        operator_name: Box<str>,
    },
    #[error(
        "Operator `{operator_name}` cannot be dropped because the database system requires it."
    )]
    /// Error indicating that `DROP OPERATOR` or `DROP SCHEMA` would drop a
    /// built-in operator, which PostgreSQL pins wherever `ALTER OPERATOR ...
    /// SET SCHEMA` moved it.
    OperatorRequiredBySystem {
        /// Symbol of the built-in operator.
        operator_name: Box<str>,
    },
    #[error(
        "Operator `{operator_name}` cannot be dropped because {dependent_kind} `{dependent_name}` uses it."
    )]
    /// Error indicating that `DROP OPERATOR` without `CASCADE` names an
    /// operator a stored expression applies.
    ///
    /// PostgreSQL records the operator each application selects when the
    /// expression is stored, and refuses the drop naming each object holding
    /// one.
    OperatorReferenced {
        /// Symbol of the operator being dropped.
        operator_name: Box<str>,
        /// [`ObjectKind::Column`] for a generated column,
        /// [`ObjectKind::ColumnDefault`], [`ObjectKind::CheckConstraint`],
        /// [`ObjectKind::Index`] for an index or an exclusion constraint,
        /// [`ObjectKind::View`], [`ObjectKind::MaterializedView`],
        /// [`ObjectKind::Policy`], [`ObjectKind::Rule`],
        /// [`ObjectKind::Trigger`], [`ObjectKind::Function`],
        /// [`ObjectKind::Domain`] or [`ObjectKind::Table`] for a partition
        /// key.
        dependent_kind: ObjectKind,
        /// The column or the check as `table.name`, or the name of the other
        /// object.
        dependent_name: String,
    },
    #[error("Operator attribute {attribute} of `{operator_name}` cannot be changed once set.")]
    /// Error indicating that `ALTER OPERATOR ... SET` names a commutator or
    /// negator other than the one the operator already links to.
    OperatorAttributeAlreadySet {
        /// Symbol of the operator the statement alters.
        operator_name: Box<str>,
        /// The attribute refused.
        attribute: OperatorAttribute,
    },
    #[error("Operator `{operator_name}` over ({}) {reason}.", .operand_types.join(", "))]
    /// Error indicating that a stored expression applies an operator no
    /// operator, or no single one, takes the operand types of.
    ///
    /// PostgreSQL types an expression when the statement storing it runs,
    /// and again when `ALTER COLUMN ... TYPE` parses it over the column's
    /// new type. A database holding an extension never raises it, and
    /// neither does an operator whose lookup may land in a schema the
    /// database does not hold, qualified with it or on the search path,
    /// since the extension or the schema may supply the operator.
    UnresolvedOperator {
        /// Symbol of the operator as written.
        operator_name: Box<str>,
        /// Type of each operand, the left one first, `unknown` for an
        /// untyped literal.
        operand_types: Vec<Box<str>>,
        /// Why PostgreSQL refuses the application.
        reason: Unresolved,
    },
    #[error("Function `{function_name}({})` {reason}.", .argument_types.join(", "))]
    /// Error indicating that a stored expression calls a function no
    /// function, or no single one, takes the argument types of.
    ///
    /// PostgreSQL types an expression when the statement storing it runs,
    /// and again when `ALTER COLUMN ... TYPE` parses it over the column's
    /// new type. A database holding an extension never raises it, and
    /// neither does a call whose lookup may land in a schema the database
    /// does not hold, qualified with it or on the search path, since the
    /// extension or the schema may supply the function.
    UnresolvedCall {
        /// Name of the function as written.
        function_name: String,
        /// Type of each argument, `unknown` for an untyped literal.
        argument_types: Vec<Box<str>>,
        /// Why PostgreSQL refuses the call.
        reason: Unresolved,
    },
    #[error("{}", .rule.message(.function_name))]
    /// Error indicating that a stored expression calls a function with
    /// clauses PostgreSQL refuses for the kind of function the call selects,
    /// or with clauses no call may combine.
    ///
    /// PostgreSQL checks a call's clauses when the statement storing the
    /// expression runs, and again when `ALTER COLUMN ... TYPE` parses it
    /// over the column's new type. A call whose lookup may land in a schema
    /// the database does not hold is refused only for clauses no function
    /// combines, since that schema may supply a function of another kind.
    MisappliedCall {
        /// Name of the function as written, each part unquoted.
        function_name: String,
        /// The rule the call breaks.
        rule: Misapplied,
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
    #[error(
        "Foreign key in table `{host_table}` names {host_columns} column(s), but its target in table `{referenced_table}` has {referenced_columns}."
    )]
    /// Error indicating that a foreign key names a different number of host
    /// columns than the referenced key holds.
    ///
    /// The columns pair up by position, so PostgreSQL, MySQL and SQLite all
    /// refuse lists of different lengths. An omitted referenced list stands
    /// for the primary key of the referenced table, as in PostgreSQL.
    ForeignKeyColumnCountMismatch {
        /// Name of the host table containing the foreign key.
        host_table: String,
        /// Number of host columns the foreign key names.
        host_columns: usize,
        /// Name of the referenced table.
        referenced_table: String,
        /// Number of columns in the referenced key.
        referenced_columns: usize,
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
    #[error("Relation `{relation_name}` not found for rule `{rule_name}`.")]
    /// Error indicating that a rule names a table or view that does not exist.
    ///
    /// A rule exists only on its relation, so the database refuses one whose
    /// relation is absent, and so does this crate.
    RelationNotFoundForRule {
        /// Name of the relation the rule is declared on.
        relation_name: String,
        /// Name of the rule.
        rule_name: String,
    },
    #[error(
        "{relation_kind} `{relation_name}` cannot have rules, so rule `{rule_name}` is refused."
    )]
    /// Error indicating that a rule names a relation PostgreSQL keeps rules
    /// off: rules exist on tables and plain views only, so a foreign table, a
    /// materialized view or a sequence refuses one.
    RuleOnUnsupportedRelation {
        /// Name of the rule.
        rule_name: String,
        /// Kind of relation the rule names.
        relation_kind: ObjectKind,
        /// Name of the relation the rule names.
        relation_name: String,
    },
    #[error(
        "Rule `{rule_name}` on `{relation_name}` is an `ON SELECT` rule, which is not supported."
    )]
    /// Error indicating that a `CREATE RULE` fires `ON SELECT`.
    ///
    /// PostgreSQL refuses an `ON SELECT` rule on a table, and on a view it
    /// only replaces the query behind the view, which `CREATE OR REPLACE
    /// VIEW` spells directly. The model records neither conversion.
    SelectRuleUnsupported {
        /// Name of the rule.
        rule_name: String,
        /// Name of the relation the rule names.
        relation_name: String,
    },
    #[error("Relation `{relation_name}` not found for rule `{rule_name}`.")]
    /// Error indicating that a rule's action or condition names a relation
    /// the database does not hold.
    ///
    /// A rule's action and condition bind their relation names at creation
    /// the way a `BEGIN ATOMIC` function body does, so a statement whose
    /// action or condition names a relation it cannot reach is refused
    /// before it records the rule.
    RelationNotFoundForRuleAction {
        /// Name of the relation the action or condition names.
        relation_name: String,
        /// Name of the rule the statement declares.
        rule_name: String,
    },
    #[error("Column `{column_name}` is ambiguous in the action of rule `{rule_name}`.")]
    /// Error indicating that a rule's action references a column bare while
    /// more than one relation in its scope exposes that column, which
    /// PostgreSQL refuses at creation.
    RuleActionReferenceAmbiguous {
        /// Name of the column the action references ambiguously.
        column_name: String,
        /// Name of the rule the statement declares.
        rule_name: String,
    },
    #[error("Rule `{rule_name}`'s `INSERT` has more expressions than target columns.")]
    /// Error indicating that an `INSERT` in a rule's action offers more
    /// expressions than its target has columns, which PostgreSQL refuses at
    /// creation.
    RuleActionInsertTooWide {
        /// Name of the rule whose action holds the `INSERT`.
        rule_name: String,
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
    #[error(
        "Index `{index_name}` is qualified by schema `{schema_name}`, but table `{table_name}` is in another schema."
    )]
    /// Error indicating that the name of an index carries a schema qualifier
    /// other than the schema of the table it is built on.
    ///
    /// An index always lives in its table's schema. PostgreSQL refuses any
    /// qualifier on the name, and SQLite takes one only when it names the
    /// table's schema.
    IndexInAnotherSchema {
        /// Name of the index.
        index_name: String,
        /// Schema the index name was qualified by.
        schema_name: String,
        /// Name of the table the index is built on.
        table_name: String,
    },
    #[error("Relation `{relation_name}` not found for the owner of sequence `{sequence_name}`.")]
    /// Error indicating that `OWNED BY` names a relation that does not exist.
    RelationNotFoundForSequenceOwner {
        /// Name of the relation `OWNED BY` names.
        relation_name: String,
        /// Name of the sequence.
        sequence_name: String,
    },
    #[error(
        "Column `{column_name}` of relation `{relation_name}` not found for the owner of sequence `{sequence_name}`."
    )]
    /// Error indicating that `OWNED BY` names a column its relation does not
    /// have.
    ColumnNotFoundForSequenceOwner {
        /// Name of the column `OWNED BY` names.
        column_name: String,
        /// Name of the relation `OWNED BY` names.
        relation_name: String,
        /// Name of the sequence.
        sequence_name: String,
    },
    #[error("Sequence `{sequence_name}` cannot be owned by `{owner}`: write a table and a column.")]
    /// Error indicating that `OWNED BY` names something other than a column
    /// of a relation or `NONE`, which PostgreSQL refuses.
    InvalidSequenceOwner {
        /// Name of the sequence.
        sequence_name: String,
        /// The owner the statement wrote.
        owner: String,
    },
    #[error("`COMMENT ON COLUMN {column_name}` names no relation before the column.")]
    /// Error indicating that `COMMENT ON COLUMN` names a bare column, which
    /// PostgreSQL refuses because a column name must be qualified.
    UnqualifiedCommentColumn {
        /// The column the statement wrote.
        column_name: String,
    },
    #[error(
        "Sequence `{sequence_name}` cannot be owned by `{relation_name}`, which is in another schema."
    )]
    /// Error indicating that `OWNED BY` names a relation outside the schema
    /// the sequence lands in.
    ///
    /// PostgreSQL keeps a sequence in the schema of the relation owning it,
    /// and the temporary schema is a schema of its own, so a temporary
    /// sequence cannot be owned by a permanent table nor the other way round.
    SequenceOwnerInAnotherSchema {
        /// Name of the sequence.
        sequence_name: String,
        /// Name of the relation `OWNED BY` names.
        relation_name: String,
    },
    #[error("Sequence `{sequence_name}` cannot be owned by {relation_kind} `{relation_name}`.")]
    /// Error indicating that `OWNED BY` names a relation of a kind that
    /// cannot own a sequence.
    ///
    /// PostgreSQL lets a table or a plain view own one, and refuses a
    /// materialized view, an index and another sequence.
    SequenceOwnerKindUnsupported {
        /// Name of the sequence.
        sequence_name: String,
        /// Name of the relation `OWNED BY` names.
        relation_name: String,
        /// Kind of that relation.
        relation_kind: ObjectKind,
    },
    #[error(
        "Sequence `{sequence_name}` cannot be owned by `{owner}`: the columns of the view cannot be listed."
    )]
    /// Error indicating that `OWNED BY` names a column of a view whose output
    /// columns the model cannot list, and which its declaration's own column
    /// list does not name, so the column the owner means is unknown.
    UnsupportedSequenceOwner {
        /// Name of the sequence.
        sequence_name: String,
        /// The owner the statement wrote.
        owner: String,
    },
    #[error(
        "Sequence `{sequence_name}` cannot be dropped: column `{column_name}` of table `{table_name}` requires it."
    )]
    /// Error indicating that `DROP SEQUENCE` names the sequence of an
    /// identity column, which PostgreSQL refuses even under `CASCADE`. The
    /// sequence leaves with the column.
    SequenceRequiredByColumn {
        /// Name of the sequence.
        sequence_name: String,
        /// Name of the table holding the identity column.
        table_name: String,
        /// Name of the identity column.
        column_name: String,
    },
    #[error(
        "Sequence `{sequence_name}` cannot be dropped: the default of column `{column_name}` of table `{table_name}` draws from it."
    )]
    /// Error indicating that a statement without `CASCADE` drops a sequence,
    /// or the column or relation owning one, while a column default still
    /// draws from it.
    ///
    /// A serial column draws from the sequence it owns, and any default naming
    /// the sequence as a `regclass` constant draws from it too. `CASCADE`
    /// removes those defaults instead.
    SequenceUsedByDefault {
        /// Name of the sequence.
        sequence_name: String,
        /// Name of the table holding the default.
        table_name: String,
        /// Name of the column holding the default.
        column_name: String,
    },
    #[error("Sequence `{sequence_name}` is refused: {reason}.")]
    /// Error indicating that a sequence a statement declares, or one created
    /// for an identity column, has parameters PostgreSQL refuses, including
    /// after a type change of its identity column.
    InvalidSequenceParameters {
        /// Name of the sequence.
        sequence_name: String,
        /// Why the parameters are refused.
        reason: SequenceParameterError,
    },
    #[error(
        "Relation `{relation_name}` not found for the default of column `{column_name}` of table `{table_name}`."
    )]
    /// Error indicating that a column default names, as a `regclass`
    /// constant, a relation the search path does not reach.
    ///
    /// PostgreSQL resolves such a constant when the default is created, the
    /// argument of `nextval('s')` included, and refuses one reaching nothing.
    RelationNotFoundForDefault {
        /// The name the constant wrote.
        relation_name: String,
        /// Name of the table holding the default.
        table_name: String,
        /// Name of the column holding the default.
        column_name: String,
    },
    #[error(
        "Invalid relation name `{relation_name}` in the default of column `{column_name}` of table `{table_name}`."
    )]
    /// Error indicating that a `regclass` constant of a column default is not
    /// a relation name: empty, unterminated, or qualified by more than a
    /// schema and this database.
    InvalidRelationNameForDefault {
        /// The text the constant wrote.
        relation_name: String,
        /// Name of the table holding the default.
        table_name: String,
        /// Name of the column holding the default.
        column_name: String,
    },
    #[error("Invalid index `{index_name}`: {reason}")]
    /// Error indicating that an index definition is invalid.
    InvalidIndex {
        /// Name of the invalid index.
        index_name: String,
        /// Reason why the index is invalid.
        reason: String,
    },
    #[error("cannot use subquery in {place}")]
    /// Error indicating that an expression PostgreSQL stores and evaluates
    /// without running a query holds a subquery.
    SubqueryInExpression {
        /// Where the expression is stored.
        place: ExpressionPlace,
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
    #[error("Role `{role_name}` not found for a role membership.")]
    /// Error indicating that a role membership names a role that does not
    /// exist, as the granted role or as the member.
    ///
    /// Covers `GRANT role TO role`, `REVOKE role FROM role` and the
    /// `IN ROLE`, `IN GROUP`, `ROLE`, `USER` and `ADMIN` clauses of
    /// `CREATE ROLE`. Like the other role checks this one follows
    /// [`AccessResolution`](crate::structs::AccessResolution), except for
    /// `PUBLIC`, which names no role under either setting and which
    /// PostgreSQL refuses as `role "public" does not exist`.
    RoleNotFoundForMembership {
        /// Name of the undefined role.
        role_name: String,
    },
    #[error(
        "Role `{role_name}` cannot be granted to `{member_name}`: `{role_name}` would become a member of itself."
    )]
    /// Error indicating that a membership would make a role a member of
    /// itself, directly or through other memberships, which PostgreSQL refuses
    /// as `role "..." is a member of role "..."`.
    CircularRoleMembership {
        /// Name of the role being granted.
        role_name: String,
        /// Name of the role it would be granted to.
        member_name: String,
    },
    #[error("Unrecognized role option `{option}` in a role membership grant or revoke.")]
    /// Error indicating that `GRANT role TO role ... WITH` or
    /// `REVOKE option OPTION FOR role FROM role` names an option other than
    /// `ADMIN`, `INHERIT` and `SET`, which PostgreSQL refuses.
    UnrecognizedRoleOption {
        /// The option as written.
        option: String,
    },
    #[error(
        "Revoking role `{role_name}` from `{member_name}` strands the memberships `{member_name}` \
         granted with its ADMIN option."
    )]
    /// Error indicating that `REVOKE role FROM role` without `CASCADE` takes
    /// away the last `ADMIN` option the member holds on the role while
    /// memberships it granted depend on it, which PostgreSQL refuses with
    /// `dependent privileges exist`.
    DependentRoleMemberships {
        /// Name of the revoked role, as written.
        role_name: String,
        /// Name of the member losing the `ADMIN` option, as written.
        member_name: String,
    },
    #[error("Table `{table_name}` not found for a grant or revoke.")]
    /// Error indicating that a grant or a revoke names a table that does not
    /// exist.
    TableNotFoundForGrant {
        /// Name of the undefined table.
        table_name: String,
    },
    #[error("Function `{function_name}` not found for a grant or revoke.")]
    /// Error indicating that a grant or a revoke names a function or procedure
    /// that does not exist.
    ///
    /// The name is reported with the argument list the statement wrote, since
    /// that list is what tells overloads apart.
    FunctionNotFoundForGrant {
        /// Name of the undefined function, with its argument list if written.
        function_name: String,
    },
    #[error(
        "Function name `{function_name}` is not unique: a grant or revoke naming no argument list cannot say which one it covers."
    )]
    /// Error indicating that a grant or a revoke omits the argument list of a
    /// function whose name covers more than one function.
    AmbiguousFunctionForGrant {
        /// The name covering more than one function.
        function_name: String,
    },
    #[error("Privilege `{privilege}` cannot be granted on a function.")]
    /// Error indicating that a grant or a revoke on a function names a
    /// privilege other than `EXECUTE`, the only one PostgreSQL defines there.
    InvalidFunctionPrivilege {
        /// The privilege as the statement spelled it.
        privilege: String,
    },
    #[error("Function `{function_name}` not found for DROP FUNCTION statement.")]
    /// Error indicating that a DROP FUNCTION statement references a function
    /// that does not exist.
    DropFunctionNotFound {
        /// Name of the function that was not found.
        function_name: String,
    },
    #[error(
        "Function `{function_name}` cannot be dropped because {dependent_kind} `{dependent_name}` uses it."
    )]
    /// Error indicating that a statement drops a function another object of
    /// the schema still calls or runs.
    ///
    /// PostgreSQL refuses without `CASCADE`, naming each object depending on
    /// the function. Functions dropped by the same statement do not hold each
    /// other, so the function named is the one something else holds.
    FunctionReferenced {
        /// Name of the function being dropped.
        function_name: String,
        /// [`ObjectKind::Column`] for a generated column,
        /// [`ObjectKind::ColumnDefault`], [`ObjectKind::CheckConstraint`],
        /// [`ObjectKind::Index`], [`ObjectKind::View`],
        /// [`ObjectKind::MaterializedView`], [`ObjectKind::Policy`],
        /// [`ObjectKind::Rule`], [`ObjectKind::Trigger`],
        /// [`ObjectKind::Function`], [`ObjectKind::Domain`],
        /// [`ObjectKind::Type`], [`ObjectKind::Table`] for a partition key,
        /// [`ObjectKind::Operator`] or [`ObjectKind::Cast`].
        dependent_kind: ObjectKind,
        /// The column or the check as `table.name`, or the name of the other
        /// object.
        dependent_name: String,
    },
    #[error("Cannot drop function `{function_name}`: extension `{extension_name}` requires it.")]
    /// Error indicating that a DROP FUNCTION statement names a function an
    /// extension created, which only dropping the extension removes.
    FunctionRequiredByExtension {
        /// Name of the function being dropped.
        function_name: String,
        /// Name of the extension that created it.
        extension_name: String,
    },
    #[error("Cannot drop function `{function_name}`: type `{type_name}` requires it.")]
    /// Error indicating that a DROP FUNCTION statement names a constructor
    /// `CREATE TYPE ... AS RANGE` created, which only dropping the range
    /// removes. The type is the range for its own constructors and the
    /// multirange for the multirange's.
    FunctionRequiredByType {
        /// Name of the function being dropped.
        function_name: String,
        /// Name of the type that requires it.
        type_name: String,
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
    #[error(
        "Cannot alter type of column `{column_name}` of table `{table_name}`: {dependent_kind} `{dependent_name}` uses it."
    )]
    /// Error indicating that an `ALTER TABLE ... ALTER COLUMN ... TYPE`
    /// statement names a column a view, a materialized view, a rule, a bound
    /// function body, a policy or a trigger reads.
    ///
    /// PostgreSQL offers no `CASCADE` for a type change and refuses it even
    /// when the type stays the same. Indexes, checks and foreign keys on the
    /// column are rebuilt instead and never raise this.
    ColumnTypeChangeBlocked {
        /// Name of the table the column belongs to.
        table_name: String,
        /// Name of the column whose type the statement changes.
        column_name: String,
        /// Kind of the object reading the column.
        dependent_kind: ObjectKind,
        /// Name of the object reading the column.
        dependent_name: String,
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
    #[error("Schema `{schema_name}` not found for extension `{extension_name}`.")]
    /// Error indicating that a `CREATE EXTENSION` statement names, in `WITH
    /// SCHEMA`, a schema the input never creates.
    ///
    /// `public` and `pg_catalog` are exempt, since every database holds them.
    /// `pg_temp` is refused, since PostgreSQL resolves no schema by that
    /// spelling here.
    SchemaNotFoundForExtension {
        /// Name of the schema that was not found.
        schema_name: String,
        /// Name of the extension the statement installs.
        extension_name: String,
    },
    #[error("No schema has been selected to create extension `{extension_name}` in.")]
    /// Error indicating that a `CREATE EXTENSION` statement names no schema
    /// while the search path names no schema that exists either.
    NoSchemaSelectedForExtension {
        /// Name of the extension the statement installs.
        extension_name: String,
    },
    #[error("No schema has been selected to create {object_kind} `{relation_name}` in.")]
    /// Error indicating that a statement creating an object names no schema
    /// and the search path selects none either, because every schema it names
    /// is missing or it is empty.
    NoSchemaSelectedForRelation {
        /// Kind of object that named no schema.
        object_kind: ObjectKind,
        /// Name of the object that named no schema.
        relation_name: String,
    },
    #[error("Permission denied to create {object_kind} `pg_catalog.{relation_name}`.")]
    /// Error indicating that a statement creates a table, view, materialized
    /// view or sequence in `pg_catalog`, by name or as the first schema on the
    /// search path, which PostgreSQL refuses to every role.
    SystemSchemaRelationRefused {
        /// Kind of relation the statement creates.
        object_kind: ObjectKind,
        /// Name of the relation.
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
        "Foreign key of permanent table `{host_table}` cannot reference unlogged table `{referenced_table}`: constraints on permanent tables may reference only permanent tables."
    )]
    /// Error indicating that a permanent table would reference an unlogged one
    /// through a foreign key, which PostgreSQL refuses.
    ///
    /// Raised for a foreign key declared or added on a permanent table, for
    /// `SET LOGGED` on a table whose foreign key references an unlogged one,
    /// and for `SET UNLOGGED` on a table a permanent one references.
    ForeignKeyReferencesUnloggedTable {
        /// The permanent table declaring the foreign key.
        host_table: String,
        /// The unlogged table it references.
        referenced_table: String,
    },
    #[error("Table `{table_name}` cannot be both temporary and unlogged.")]
    /// Error indicating that an unlogged table is declared temporary or placed
    /// in `pg_temp`.
    ///
    /// PostgreSQL rejects `CREATE TEMP UNLOGGED TABLE` as a syntax error and
    /// `CREATE UNLOGGED TABLE pg_temp.t` because only temporary relations live
    /// in the temporary schema.
    UnloggedTemporaryTable {
        /// Name of the table the statement creates.
        table_name: String,
    },
    #[error("Partitioned table `{table_name}` cannot be unlogged.")]
    /// Error indicating that `CREATE UNLOGGED TABLE` declares a partitioned
    /// table, which PostgreSQL 18 refuses.
    UnloggedPartitionedTable {
        /// Name of the table the statement creates.
        table_name: String,
    },
    #[error("Cannot change logged status of table `{table_name}` because {reason}.")]
    /// Error indicating that `ALTER TABLE ... SET LOGGED` or `SET UNLOGGED`
    /// names a table whose persistence is fixed, refused even when the table
    /// already holds the state named.
    PersistenceNotChangeable {
        /// Name of the table the statement names.
        table_name: String,
        /// What fixes the table's persistence.
        reason: FixedPersistence,
    },
    #[error("Cannot change persistence of table `{table_name}` twice in one statement.")]
    /// Error indicating that one `ALTER TABLE` changes a table's persistence
    /// and then names a persistence again.
    ///
    /// A clause naming the state the table already holds changes nothing, so
    /// only a clause after an actual change is refused.
    PersistenceChangedTwice {
        /// Name of the table the statement names.
        table_name: String,
    },
    #[error("Partitioned table `{table_name}` cannot take storage parameters.")]
    /// Error indicating that `ALTER TABLE ... SET (...)` names a partitioned
    /// table, which PostgreSQL 18 refuses because it holds no rows.
    StorageParametersOnPartitionedTable {
        /// Name of the table the statement names.
        table_name: String,
    },
    #[error(
        "Permanent table `{table_name}` cannot inherit from temporary relation `{parent_name}`."
    )]
    /// Error indicating that a permanent table names a temporary parent in
    /// `INHERITS` or `PARTITION OF`, or is attached as its partition.
    PermanentRelationInheritsTemporary {
        /// Table the statement creates or attaches.
        table_name: String,
        /// Temporary parent it names.
        parent_name: String,
    },
    #[error(
        "Temporary table `{table_name}` cannot be a partition of permanent relation `{parent_name}`."
    )]
    /// Error indicating that a temporary table is declared `PARTITION OF` a
    /// permanent one or attached as its partition. A temporary table may still
    /// inherit from a permanent one through `INHERITS`.
    TemporaryPartitionOfPermanent {
        /// Table the statement creates or attaches.
        table_name: String,
        /// Permanent parent it names.
        parent_name: String,
    },
    #[error(
        "Table `{table_name}` cannot be a partition of `{parent_name}`, which is not partitioned."
    )]
    /// Error indicating that a table is declared `PARTITION OF` a table that
    /// has no partition key, which PostgreSQL refuses as "is not partitioned".
    ParentNotPartitioned {
        /// Table the statement creates.
        table_name: String,
        /// Parent it names, as written.
        parent_name: String,
    },
    #[error("Partitioned table `{table_name}` cannot inherit from another table.")]
    /// Error indicating that a table declared `PARTITION BY` also names
    /// `INHERITS` parents, which PostgreSQL refuses as "cannot create
    /// partitioned table as inheritance child".
    PartitionedTableInherits {
        /// Table the statement creates.
        table_name: String,
    },
    #[error("Table `{table_name}` names parent `{parent_name}` more than once.")]
    /// Error indicating that `INHERITS` reaches the same table twice, however
    /// the two names are spelled.
    ParentRepeated {
        /// Table the statement creates.
        table_name: String,
        /// Repeated parent, as stored.
        parent_name: String,
    },
    #[error(
        "Column `{column_name}` of table `{table_name}` receives a generated column and cannot specify a default."
    )]
    /// Error indicating that a table redeclares a stored generated column it
    /// receives with a `DEFAULT`, which PostgreSQL refuses.
    DefaultOnInheritedGeneratedColumn {
        /// Table the statement creates.
        table_name: String,
        /// The generated column.
        column_name: String,
    },
    #[error("Table `{table_name}` cannot inherit from partitioned table `{parent_name}`.")]
    /// Error indicating that `INHERITS` names a partitioned table, which only
    /// takes partitions through `PARTITION OF`.
    InheritsFromPartitionedTable {
        /// Table the statement creates.
        table_name: String,
        /// Partitioned parent it names, as written.
        parent_name: String,
    },
    #[error("Table `{table_name}` cannot inherit from `{parent_name}` because {reason}.")]
    /// Error indicating that `ALTER TABLE ... INHERIT` names a parent whose
    /// shape the table does not already have.
    InheritanceChildMismatch {
        /// Table the statement alters.
        table_name: String,
        /// Parent it names, as written.
        parent_name: String,
        /// What the table lacks.
        reason: ChildMismatch,
    },
    #[error("Cannot change the parents of table `{table_name}` because it is a partition.")]
    /// Error indicating that `INHERIT` or `NO INHERIT` names a partition, whose
    /// only parent is its partitioned table.
    InheritanceChangeOnPartition {
        /// Table the statement alters.
        table_name: String,
    },
    #[error("Cannot change the parents of partitioned table `{table_name}`.")]
    /// Error indicating that `INHERIT` names a partitioned table, which takes
    /// no `INHERITS` parent.
    InheritanceChangeOnPartitioned {
        /// Table the statement alters.
        table_name: String,
    },
    #[error("Table `{table_name}` cannot inherit from `{parent_name}`, which inherits from it.")]
    /// Error indicating that `INHERIT` names the table itself or one of the
    /// tables inheriting from it.
    CircularInheritance {
        /// Table the statement alters.
        table_name: String,
        /// Parent it names, as written.
        parent_name: String,
    },
    #[error("Relation `{parent_name}` is not a parent of table `{table_name}`.")]
    /// Error indicating that `NO INHERIT` names a relation the table does not
    /// inherit from.
    NotAParentOf {
        /// Table the statement alters.
        table_name: String,
        /// Parent it names, as written.
        parent_name: String,
    },
    #[error("Relation `{partition_name}` is not a partition of table `{table_name}`.")]
    /// Error indicating that `DETACH PARTITION` names a relation that is not
    /// one of the table's partitions.
    NotAPartitionOf {
        /// Partitioned table the statement alters.
        table_name: String,
        /// Relation the statement detaches, as written.
        partition_name: String,
    },
    #[error(
        "Partition `{partition_name}` of table `{table_name}` cannot be detached concurrently inside a transaction block."
    )]
    /// Error indicating that `DETACH PARTITION ... CONCURRENTLY` runs inside
    /// `BEGIN`, which PostgreSQL refuses.
    DetachConcurrentlyInTransaction {
        /// Partitioned table the statement alters.
        table_name: String,
        /// Partition the statement detaches.
        partition_name: String,
    },
    #[error(
        "Partition `{partition_name}` of table `{table_name}` cannot be detached concurrently while the table has a default partition."
    )]
    /// Error indicating that `DETACH PARTITION ... CONCURRENTLY` names a table
    /// with a default partition, the detached one included.
    DetachConcurrentlyWithDefault {
        /// Partitioned table the statement alters.
        table_name: String,
        /// Partition the statement detaches.
        partition_name: String,
    },
    #[error("No detach of partition `{partition_name}` of table `{table_name}` is pending.")]
    /// Error indicating that `DETACH PARTITION ... FINALIZE` names a partition
    /// no interrupted `CONCURRENTLY` left pending, which an ingested schema
    /// never holds.
    NoDetachPending {
        /// Partitioned table the statement alters.
        table_name: String,
        /// Partition the statement names.
        partition_name: String,
    },
    #[error(
        "`ATTACH PARTITION` and `DETACH PARTITION` on table `{table_name}` must be the only action of their statement."
    )]
    /// Error indicating that an `ALTER TABLE` writes `ATTACH PARTITION` or
    /// `DETACH PARTITION` beside another action, which PostgreSQL's grammar
    /// does not accept.
    PartitionCommandNotAlone {
        /// Table the statement alters.
        table_name: String,
    },
    #[error("Table `{table_name}` cannot inherit from partition `{parent_name}`.")]
    /// Error indicating that `INHERITS` names a partition, whose columns
    /// belong to the partitioned table it is part of.
    InheritsFromPartition {
        /// Table the statement creates.
        table_name: String,
        /// Partition it names, as written.
        parent_name: String,
    },
    #[error(
        "Column `{column_name}` named in the partition key of table `{table_name}` does not exist."
    )]
    /// Error indicating that a `PARTITION BY` key names a column its table does
    /// not declare or receive.
    PartitionKeyColumnNotFound {
        /// Table the statement partitions.
        table_name: String,
        /// Column the key names, as written.
        column_name: String,
    },
    #[error(
        "Generated column `{column_name}` cannot be part of the partition key of table `{table_name}`."
    )]
    /// Error indicating that a `PARTITION BY` key reads a stored generated
    /// column, which PostgreSQL refuses.
    GeneratedColumnInPartitionKey {
        /// Table the statement partitions.
        table_name: String,
        /// Generated column the key reads.
        column_name: String,
    },
    #[error(
        "Table `{table_name}` cannot use the list partition strategy with more than one key column."
    )]
    /// Error indicating that `PARTITION BY LIST` names more than one key.
    ListPartitionKeyHasManyColumns {
        /// Table the statement partitions.
        table_name: String,
    },
    #[error(
        "A unique constraint on partitioned table `{table_name}` must include every partitioning column."
    )]
    /// Error indicating that a primary key, unique or exclusion constraint, or
    /// a unique index, on a partitioned table leaves out a column of its
    /// partition key, so the partitions could not enforce it on their own.
    UniqueConstraintMissesPartitionKey {
        /// Partitioned table the constraint or index is on.
        table_name: String,
    },
    #[error(
        "Partitioned table `{table_name}` cannot hold a unique constraint, because its partition key has an expression."
    )]
    /// Error indicating that a unique constraint or index is put on a table
    /// partitioned by an expression, which PostgreSQL does not support.
    UniqueConstraintOverExpressionPartitionKey {
        /// Partitioned table the constraint or index is on.
        table_name: String,
    },
    #[error(
        "Partition `{partition_name}` of table `{table_name}` cannot take its bound because {reason}."
    )]
    /// Error indicating that `CREATE TABLE ... PARTITION OF` or `ATTACH
    /// PARTITION` gives a partition a bound PostgreSQL refuses.
    PartitionBoundInvalid {
        /// Name of the partitioned table.
        table_name: String,
        /// Name of the partition.
        partition_name: String,
        /// What makes the bound invalid.
        reason: InvalidBound,
    },
    #[error(
        "Partition `{partition_name}` of table `{table_name}` would overlap partition `{other_partition}`."
    )]
    /// Error indicating that a partition's bound takes rows a sibling
    /// partition already takes.
    PartitionBoundOverlaps {
        /// Name of the partitioned table.
        table_name: String,
        /// Name of the partition.
        partition_name: String,
        /// The sibling partition the bound overlaps.
        other_partition: String,
    },
    #[error(
        "Table `{partition_name}` cannot be attached as a partition of `{table_name}` because {reason}."
    )]
    /// Error indicating that `ALTER TABLE ... ATTACH PARTITION` names a table
    /// PostgreSQL refuses to make a partition of the one the statement alters.
    PartitionNotAttachable {
        /// Name of the partitioned table the statement alters.
        table_name: String,
        /// Name of the table being attached.
        partition_name: String,
        /// What keeps the table from becoming a partition.
        reason: UnattachablePartition,
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
    #[error(
        "Role `{role_name}` is the current or session role, so it cannot be dropped or renamed."
    )]
    /// Error indicating that `DROP ROLE` or `ALTER ROLE ... RENAME TO` names
    /// the role the session runs as, as `SET ROLE` or `SET SESSION
    /// AUTHORIZATION` left it, which PostgreSQL refuses.
    RoleInUseBySession {
        /// Name of the role, as written.
        role_name: String,
    },
    #[error("Role `{role_name}` not found for the session to run as.")]
    /// Error indicating that `SET ROLE` or `SET SESSION AUTHORIZATION` names a
    /// role that does not exist.
    ///
    /// Like the other role checks this one follows
    /// [`AccessResolution`](crate::structs::AccessResolution).
    RoleNotFoundForSession {
        /// Name of the role, as written.
        role_name: String,
    },
    #[error("`{statement}` names no single role for the session to run as.")]
    /// Error indicating that `SET role` or `SET session_authorization` takes
    /// more than one value, or a value that is no name, string, number or
    /// boolean, which PostgreSQL refuses.
    InvalidSessionRole {
        /// The statement, rendered by sqlparser.
        statement: String,
    },
    #[error(
        "`{statement}` sets a value that is no name, string, number or boolean, which PostgreSQL \
         refuses."
    )]
    /// Error indicating that `ALTER ROLE ... SET` gives a setting a value
    /// PostgreSQL's grammar reads as no literal, such as a function call or
    /// an interval.
    NonLiteralSetting {
        /// The statement, rendered by sqlparser.
        statement: String,
    },
    #[error(
        "`{statement}` stores the session's current `{setting}`, which the input leaves unknown."
    )]
    /// Error indicating that `ALTER ROLE ... SET ... FROM CURRENT` reads a
    /// setting the input never set, or returned to the value the session
    /// started with, which the server's configuration and the role running
    /// the input decide.
    UnknownCurrentSetting {
        /// The setting, as written.
        setting: String,
        /// The statement, rendered by sqlparser.
        statement: String,
    },
    #[error("`{value}` is no `TimeZone` PostgreSQL accepts: {reason}")]
    /// Error indicating that `ALTER ROLE ... SET timezone`, a session `SET`
    /// of it, or a [`RoleSetting`](crate::structs::RoleSetting) of it names a
    /// zone PostgreSQL refuses.
    InvalidTimeZone {
        /// The refused value.
        value: String,
        /// Why PostgreSQL refuses it.
        #[source]
        reason: sql_values::TimeZoneError,
    },
    #[error("`{statement}` names the role the session runs as, which the input never names.")]
    /// Error indicating that `ALTER ROLE CURRENT_USER`, `CURRENT_ROLE` or
    /// `SESSION_USER` sets a setting while the session runs as the role the
    /// input runs as without naming it.
    UnnamedSessionRoleSetting {
        /// The statement, rendered by sqlparser.
        statement: String,
    },
    #[error("Schema `{schema_name}` not found for ALTER DEFAULT PRIVILEGES.")]
    /// Error indicating that `ALTER DEFAULT PRIVILEGES ... IN SCHEMA` names a
    /// schema no `CREATE SCHEMA` in the input creates.
    ///
    /// `public` is exempt, since every database holds it.
    SchemaNotFoundForDefaultPrivileges {
        /// Name of the schema, as written.
        schema_name: String,
    },
    #[error("ALTER DEFAULT PRIVILEGES cannot use IN SCHEMA with ON {object_type}.")]
    /// Error indicating that `ALTER DEFAULT PRIVILEGES ... IN SCHEMA` names
    /// `SCHEMAS` or `LARGE OBJECTS`, which no schema holds.
    DefaultPrivilegesInSchema {
        /// The object type, as PostgreSQL spells it.
        object_type: String,
    },
    #[error("Privilege `{privilege}` cannot be a default on {object_type}.")]
    /// Error indicating that `ALTER DEFAULT PRIVILEGES` names a privilege the
    /// object type does not have, such as `EXECUTE` on `TABLES`.
    InvalidDefaultPrivilege {
        /// The privilege, as written.
        privilege: String,
        /// The object type, as PostgreSQL spells it.
        object_type: String,
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
    /// Error indicating that a statement names a relation or a domain with a
    /// name something else in the same schema already holds.
    ///
    /// PostgreSQL keeps tables, views, materialized views, sequences and index
    /// names in one pool per schema, and a named `UNIQUE` or `PRIMARY KEY`
    /// constraint puts the name of the index behind it in that pool too. Types
    /// share a second pool, which holds every domain and the row type of every
    /// table, view and materialized view, and which a new sequence may not
    /// take a name from either.
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
    #[error("Domain `{domain_name}` not found for DROP DOMAIN statement.")]
    /// Error indicating that a `DROP DOMAIN` statement without `IF EXISTS`
    /// names a domain the input never creates.
    DropDomainNotFound {
        /// Name the statement wrote.
        domain_name: String,
    },
    #[error(
        "Domain `{domain_name}` cannot be dropped because {dependent_kind} `{dependent_name}` uses it."
    )]
    /// Error indicating that a statement drops a domain a column, another
    /// domain or a function's signature is declared with.
    ///
    /// PostgreSQL refuses without `CASCADE`, and `CASCADE` drops the columns,
    /// the domains and the functions along with it. A column of an array of
    /// the domain uses it too.
    DomainHasDependents {
        /// Name of the domain the statement drops.
        domain_name: String,
        /// [`ObjectKind::Column`], [`ObjectKind::Domain`] or
        /// [`ObjectKind::Function`].
        dependent_kind: ObjectKind,
        /// The column as `table.column`, or the name of the domain or the
        /// function.
        dependent_name: String,
    },
    #[error("`{object_name}` is a {actual_kind}, not a {expected_kind}.")]
    /// Error indicating that a statement names an existing relation of the
    /// wrong kind, which PostgreSQL refuses rather than treating the spellings
    /// as interchangeable: `DROP TABLE` will not drop a view or a sequence,
    /// `DROP VIEW` will not drop a table or a materialized view, `DROP
    /// MATERIALIZED VIEW` will not drop a plain view, `DROP SEQUENCE` will
    /// drop nothing but a sequence, and `DROP DOMAIN` will not drop the row
    /// type of a table or a view.
    RelationKindMismatch {
        /// Name the statement wrote.
        object_name: String,
        /// Kind the statement's spelling asked for.
        expected_kind: ObjectKind,
        /// Kind the name actually holds.
        actual_kind: ObjectKind,
    },
    #[error("Type `{type_name}` not found.")]
    /// Error indicating that a `DROP TYPE` statement without `IF EXISTS`, or
    /// an `ALTER TYPE` statement, names a type the input never creates, or a
    /// type of the database system, or that an operator statement names an
    /// operand type no stored type spells.
    TypeNotFound {
        /// Name the statement wrote.
        type_name: String,
    },
    #[error("Type `{type_name}` cannot be dropped: the database system requires it.")]
    /// Error indicating that a `DROP TYPE` statement names a type of the
    /// database system, which PostgreSQL refuses even with `IF EXISTS`.
    TypeRequiredBySystem {
        /// Name the statement wrote.
        type_name: String,
    },
    #[error("Type `{type_name}` is only a shell.")]
    /// Error indicating that a statement declares a value or a definition
    /// with a type `CREATE TYPE name;` recorded as a shell and never filled,
    /// or alters one.
    TypeIsOnlyShell {
        /// Name of the shell type.
        type_name: String,
    },
    #[error(
        "Type `{type_name}` cannot be dropped because {dependent_kind} `{dependent_name}` uses it."
    )]
    /// Error indicating that a statement drops a type a column, a composite
    /// attribute, a domain, a range or a function's signature is declared
    /// with.
    ///
    /// PostgreSQL refuses without `CASCADE`, and `CASCADE` drops the columns,
    /// the composite attributes, the domains, the ranges and the functions
    /// along with it.
    TypeHasDependents {
        /// Name of the type the statement drops.
        type_name: String,
        /// [`ObjectKind::Column`], [`ObjectKind::Domain`],
        /// [`ObjectKind::Type`], [`ObjectKind::Table`] or
        /// [`ObjectKind::Function`].
        dependent_kind: ObjectKind,
        /// The column as `table.column`, or the name of the other object.
        dependent_name: String,
    },
    #[error(
        "Type `{type_name}` cannot be dropped: {requiring_kind} `{requiring_name}` requires it."
    )]
    /// Error indicating that a `DROP TYPE` statement names the multirange of
    /// a range, the array of a base type, or the row type of a relation,
    /// which PostgreSQL removes only with the object that needs it, whatever
    /// the statement asks.
    TypeDropRequiredBy {
        /// Name the statement wrote.
        type_name: String,
        /// [`ObjectKind::Type`] for a range or a base type, or the kind of
        /// relation whose row type the name holds.
        requiring_kind: ObjectKind,
        /// Name of the object that requires the dropped one.
        requiring_name: String,
    },
    #[error("Type `{type_name}` is a {relation_kind}'s row type.")]
    /// Error indicating that an `ALTER TYPE` statement names the row type of
    /// a relation, which only the statement for the relation changes.
    TypeIsRowType {
        /// Name the statement wrote.
        type_name: String,
        /// Kind of relation that owns the row type.
        relation_kind: ObjectKind,
    },
    #[error("Cannot alter array type `{array_name}`.")]
    /// Error indicating that an `ALTER TYPE` statement names the array type
    /// of a type, which only a statement for the type it is the array of
    /// changes.
    CannotAlterArrayType {
        /// The type whose array type the statement names, spelled as `name[]`.
        array_name: String,
    },
    #[error("Enum label `{label}` of type `{type_name}` already exists.")]
    /// Error indicating that an `ALTER TYPE ... ADD VALUE` or `RENAME VALUE`
    /// statement writes a label the enum already holds.
    EnumLabelExists {
        /// Name of the enum.
        type_name: String,
        /// Label the statement wrote.
        label: String,
    },
    #[error("Label `{label}` is not an existing enum label of type `{type_name}`.")]
    /// Error indicating that an `ALTER TYPE ... ADD VALUE ... BEFORE/AFTER`
    /// or `RENAME VALUE` statement names a label the enum does not hold.
    EnumLabelNotFound {
        /// Name of the enum.
        type_name: String,
        /// Label the statement wrote.
        label: String,
    },
    #[error("Type `{type_name}` is not an enum.")]
    /// Error indicating that an `ALTER TYPE ... ADD VALUE` or `RENAME VALUE`
    /// statement names a type of another kind than enum.
    NotAnEnum {
        /// Name the statement wrote.
        type_name: String,
    },
    #[error("The type attribute `subtype` is required for type `{type_name}`.")]
    /// Error indicating that a `CREATE TYPE ... AS RANGE` statement names no
    /// `SUBTYPE`.
    RangeSubtypeRequired {
        /// Name the statement wrote.
        type_name: String,
    },
    #[error("Enum label `{label}` of type `{type_name}` is written more than once.")]
    /// Error indicating that a `CREATE TYPE ... AS ENUM` statement writes the
    /// same label twice.
    DuplicateEnumLabel {
        /// Name the statement wrote.
        type_name: String,
        /// Label the statement wrote twice.
        label: String,
    },
    #[error("Column `{attribute}` of type `{type_name}` is specified more than once.")]
    /// Error indicating that a `CREATE TYPE ... AS (...)` statement writes the
    /// same attribute name twice.
    DuplicateCompositeAttribute {
        /// Name the statement wrote.
        type_name: String,
        /// Attribute the statement wrote twice.
        attribute: String,
    },
    #[error("{constraint} constraints are not supported on foreign table `{table_name}`.")]
    /// Error indicating that a statement gives a foreign table a primary key,
    /// unique, exclusion or foreign key constraint, which PostgreSQL refuses.
    ConstraintUnsupportedOnForeignTable {
        /// Name of the foreign table.
        table_name: String,
        /// The kind of constraint refused.
        constraint: KeyConstraint,
    },
    #[error(
        "Column `{column_name}` of table `{table_name}` cannot take foreign-data wrapper options, because `{table_name}` is not a foreign table."
    )]
    /// Error indicating that a statement gives a column of an ordinary table
    /// `OPTIONS`, which only a column of a foreign table carries.
    ForeignOptionsOnTableColumn {
        /// Name of the ordinary table.
        table_name: String,
        /// Name of the column given the options.
        column_name: String,
    },
    #[error("Option `{option_name}` is given more than once to foreign table `{table_name}`.")]
    /// Error indicating that an `OPTIONS` list of a foreign table or of one of
    /// its columns names a key twice, or that an `ALTER` adds a key the list
    /// already holds, keys compared as PostgreSQL stores them.
    RepeatedForeignOption {
        /// Name of the foreign table.
        table_name: String,
        /// Name of the column whose list repeats the key, or `None` for the
        /// table's own list.
        column_name: Option<String>,
        /// The repeated key, as PostgreSQL stores it.
        option_name: String,
    },
    #[error("Option `{option_name}` is not set on foreign table `{table_name}`.")]
    /// Error indicating that an `ALTER` sets or drops an option the list of a
    /// foreign table or of one of its columns does not hold.
    ForeignOptionNotFound {
        /// Name of the foreign table.
        table_name: String,
        /// Name of the column whose list lacks the key, or `None` for the
        /// table's own list.
        column_name: Option<String>,
        /// The missing key, as PostgreSQL stores it.
        option_name: String,
    },
    #[error(
        "Partitioned table `{table_name}` cannot hold a unique index, because its partition `{partition_name}` is a foreign table."
    )]
    /// Error indicating that a unique index would reach a foreign table through
    /// the partitions of the table it is created on, which PostgreSQL refuses.
    UniqueIndexOverForeignPartition {
        /// Name of the partitioned table the unique index is created on.
        table_name: String,
        /// Name of the foreign table below it.
        partition_name: String,
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
    #[error("Table `{table_name}` specifies more column names than its query produces.")]
    /// Error indicating that the column list of a `CREATE TABLE … AS`
    /// statement is longer than what its query produces, which PostgreSQL
    /// refuses as specifying too many column names.
    ///
    /// Checked only when the query's output columns can be counted.
    TableColumnListTooLong {
        /// Name the statement wrote.
        table_name: String,
    },
    #[error("{object_kind} `{view_name}` takes no option `{option_name}`.")]
    /// Error indicating that a view's `WITH (...)` or an `ALTER TABLE ... SET
    /// (...)` naming it writes an option its kind does not take.
    ///
    /// A plain view takes only `check_option`, `security_barrier` and
    /// `security_invoker`. A materialized view takes storage parameters and
    /// none of those three.
    UnrecognizedViewOption {
        /// Kind of view the option is written on.
        object_kind: ObjectKind,
        /// Name the statement wrote.
        view_name: String,
        /// Option name the statement wrote.
        option_name: String,
    },
    #[error("Invalid value `{option_value}` for option `{option_name}` of view `{view_name}`.")]
    /// Error indicating that a view option is given a value PostgreSQL does
    /// not read as that option's type.
    InvalidViewOptionValue {
        /// Name the statement wrote.
        view_name: String,
        /// The option, as PostgreSQL names it.
        option_name: String,
        /// Value the statement supplied.
        option_value: String,
    },
    #[error("Option `{option_name}` of view `{view_name}` is written more than once.")]
    /// Error indicating that one option list names a view option twice.
    RepeatedViewOption {
        /// Name the statement wrote.
        view_name: String,
        /// The option, as PostgreSQL names it.
        option_name: String,
    },
    #[error(
        "{object_kind} `{object_name}` cannot be dropped because {dependent_kind} `{dependent_name}` uses it."
    )]
    /// Error indicating that a relation has dependents preventing its removal.
    RelationHasDependents {
        /// Kind of relation the statement drops.
        object_kind: ObjectKind,
        /// Name of the relation the statement drops.
        object_name: String,
        /// Kind of the dependent object.
        dependent_kind: ObjectKind,
        /// Name of the dependent object.
        dependent_name: String,
    },
    #[error(
        "ALTER action {operation} cannot be performed on {object_kind} `{relation_name}`: this operation is not supported for it."
    )]
    /// Error indicating that an `ALTER TABLE` asks a relation for an action
    /// its kind cannot take: a view for most actions, and a table that is not
    /// partitioned for `ATTACH PARTITION`.
    AlterActionUnsupportedOnRelation {
        /// Kind of relation the name holds.
        object_kind: ObjectKind,
        /// Name the statement wrote.
        relation_name: String,
        /// The action, rendered by the parser.
        operation: String,
    },
    #[error(
        "Constraint `{constraint_name}` not found on table `{table_name}` for ALTER TABLE RENAME CONSTRAINT statement."
    )]
    /// Error indicating that `RENAME CONSTRAINT` names a constraint the table
    /// holds under no such name, written or given by PostgreSQL.
    RenameConstraintNotFound {
        /// Name of the table.
        table_name: String,
        /// Name the statement renames.
        constraint_name: String,
    },
    #[error(
        "Cannot rename constraint `{constraint_name}` of table `{table_name}` because it is inherited."
    )]
    /// Error indicating that a table tried to rename a check or `NOT NULL`
    /// constraint it receives from a parent, which only the parent can rename,
    /// for every table below it at once.
    InheritedConstraintNotRenamable {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the inherited constraint.
        constraint_name: String,
    },
    #[error("Constraint `{constraint_name}` already exists on table `{table_name}`.")]
    /// Error indicating that `ADD CONSTRAINT` names a constraint the table
    /// already holds under that name, written or given by PostgreSQL.
    ConstraintAlreadyExists {
        /// Name of the table.
        table_name: String,
        /// Name the statement gives the constraint.
        constraint_name: String,
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
    #[error("Rule `{rule_name}` already exists on relation `{relation_name}`.")]
    /// Error indicating that a `CREATE RULE` statement names a rule the table
    /// or view already carries without `OR REPLACE`. A rule name is unique per
    /// relation, so the same name on another relation is fine.
    RuleAlreadyExists {
        /// Name of the rule that already exists.
        rule_name: String,
        /// Name of the relation carrying it.
        relation_name: String,
    },
    #[error("Extension `{extension_name}` already exists.")]
    /// Error indicating that a `CREATE EXTENSION` statement without `IF NOT
    /// EXISTS` names an extension the database already holds. An extension
    /// name is unique in the database, whatever schema each was created in.
    ExtensionAlreadyExists {
        /// Name of the extension that already exists.
        extension_name: String,
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
        "Permission denied for function `{function_name}`: role `{role_name}` creating it may not execute it."
    )]
    /// Error indicating that `CREATE FUNCTION` leaves the role running it
    /// without `EXECUTE` on the function, which PostgreSQL's `sql` and
    /// `plpgsql` validators refuse.
    ///
    /// Raised only for a role the input names and declares as no superuser,
    /// since the validator lets a superuser through and an unnamed or
    /// undeclared role may be one.
    FunctionNotExecutableByCreator {
        /// Name of the function.
        function_name: String,
        /// The role running the statement, as stored.
        role_name: String,
    },
    #[error(
        "Cannot change routine kind: `{function_name}` is an aggregate or window function, which `CREATE OR REPLACE FUNCTION` cannot replace."
    )]
    /// Error indicating that a `CREATE OR REPLACE FUNCTION` statement would
    /// replace an aggregate or a window function.
    ///
    /// The replacement keeps the function's `pg_proc` row, and PostgreSQL
    /// refuses one that would change the row's `prokind`.
    FunctionKindChange {
        /// Name of the function the statement would replace.
        function_name: String,
        /// The kind of the function the statement would replace.
        kind: FunctionKind,
    },
    #[error("Relation `{relation_name}` not found for function `{function_name}`.")]
    /// Error indicating that a function body names a table or view the
    /// database does not hold.
    ///
    /// PostgreSQL resolves the body's relation names at `CREATE FUNCTION`
    /// time, so a statement whose body names a relation it cannot reach is
    /// refused before it records or replaces anything.
    RelationNotFoundForFunctionBody {
        /// Name of the relation the body names.
        relation_name: String,
        /// Name of the function the statement declares.
        function_name: String,
    },
    #[error("`{statement}` is not supported in the body of function `{function_name}`.")]
    /// Error indicating that a statement of a function's `BEGIN ATOMIC` body
    /// is one PostgreSQL does not support in an unquoted SQL function body,
    /// such as `SET`.
    ///
    /// PostgreSQL refuses the `CREATE FUNCTION` at `CREATE FUNCTION` time,
    /// so the statement is refused before it records or replaces anything.
    FunctionBodyStatementUnsupported {
        /// The statement the body holds.
        statement: String,
        /// Name of the function the statement declares.
        function_name: String,
    },
    #[error(
        "Column `{column_name}` of `{relation_name}` cannot be renamed while function `{function_name}` writes to the relation in a statement that cannot follow the rename."
    )]
    /// Error indicating that a column rename reaches the write target of a
    /// `BEGIN ATOMIC` statement in a position the stored text cannot follow.
    ///
    /// A statement reading a view whose columns are unknown keeps its
    /// references as written, and the relations it reads take back the old
    /// column names through an alias column list. PostgreSQL accepts no such
    /// list on an `UPDATE`, `DELETE` or `MERGE` target. A whole-row reference
    /// to the target in `RETURNING` that the new column name would capture
    /// needs the form `t.*::t`, which the parser does not read. Either way the
    /// rename is refused before it changes anything.
    FunctionBodyBlocksColumnRename {
        /// Name of the column the statement renames.
        column_name: String,
        /// Name of the relation the function writes to.
        relation_name: String,
        /// Name of the function whose body writes to the relation.
        function_name: String,
    },
    #[error(
        "Function `{function_name}` declares a result, but its body's final statement produces none."
    )]
    /// Error indicating that a `BEGIN ATOMIC` function declares a result
    /// while its block is empty or ends in a data-modifying statement without
    /// `RETURNING`, which PostgreSQL refuses at creation.
    FunctionBodyResultMissing {
        /// Name of the function whose final statement produces no result.
        function_name: String,
    },
    #[error("Column `{column_name}` is ambiguous in the body of function `{function_name}`.")]
    /// Error indicating that a body statement references a column bare while
    /// more than one relation in its scope exposes that column, which
    /// PostgreSQL refuses at creation.
    FunctionBodyReferenceAmbiguous {
        /// Name of the column the statement references ambiguously.
        column_name: String,
        /// Name of the function whose body is ambiguous.
        function_name: String,
    },
    #[error("Function `{function_name}`'s `INSERT` has more expressions than target columns.")]
    /// Error indicating that a `BEGIN ATOMIC` function's `INSERT` offers
    /// more expressions than its target has columns, which PostgreSQL
    /// refuses at creation.
    FunctionBodyInsertTooWide {
        /// Name of the function whose body holds the `INSERT`.
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
    #[error("Extension `{extension_name}` not found for DROP EXTENSION statement.")]
    /// Error indicating that a `DROP EXTENSION` statement without `IF EXISTS`
    /// names an extension the database does not hold.
    DropExtensionNotFound {
        /// Name of the extension that was not found.
        extension_name: String,
    },
    #[error("Extension `{extension_name}` not found for function `{function_name}` to depend on.")]
    /// Error indicating that `ALTER FUNCTION ... [NO] DEPENDS ON EXTENSION`
    /// names an extension the database does not hold.
    AlterFunctionExtensionNotFound {
        /// Name of the extension that was not found.
        extension_name: String,
        /// Name of the function the statement alters.
        function_name: String,
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
    #[error(
        "Constraint `{constraint_name}` not found on table `{table_name}` for ALTER TABLE VALIDATE CONSTRAINT statement."
    )]
    /// Error indicating that an `ALTER TABLE ... VALIDATE CONSTRAINT`
    /// statement names a constraint the table does not declare. Constraints
    /// declared without a name have no name to validate them by.
    ValidateConstraintNotFound {
        /// Name of the table the constraint was looked up on.
        table_name: String,
        /// Name of the constraint that was not found.
        constraint_name: String,
    },
    #[error(
        "Constraint `{constraint_name}` of table `{table_name}` cannot be validated, because only a check, a foreign key or a `NOT NULL` constraint can be."
    )]
    /// Error indicating that an `ALTER TABLE ... VALIDATE CONSTRAINT`
    /// statement names a key, which is checked against every row the moment
    /// it is added and so has nothing left to validate.
    ConstraintNotValidatable {
        /// Name of the table the statement names.
        table_name: String,
        /// Name of the constraint the statement names.
        constraint_name: String,
    },
    #[error(
        "Constraint `{constraint}` added to table `{table_name}` cannot be marked `NOT VALID`, because only a check or a foreign key can be."
    )]
    /// Error indicating that `ALTER TABLE ... ADD ... NOT VALID` asked to skip
    /// checking existing rows against a key, which PostgreSQL refuses while
    /// reading the statement.
    NotValidRefused {
        /// Name of the table the statement names.
        table_name: String,
        /// The constraint as the statement wrote it.
        constraint: String,
    },
    #[error(
        "Constraint `{constraint}` arriving at table `{table_name}` from a parent conflicts with the `NOT VALID` copy the table holds."
    )]
    /// Error indicating that a parent added a validated check which a table
    /// below already holds `NOT VALID`, which PostgreSQL refuses because the
    /// merged check would claim rows nobody has checked.
    NotValidConstraintConflict {
        /// Name of the table holding the unvalidated copy.
        table_name: String,
        /// The constraint as the table holds it.
        constraint: String,
    },
    #[error("Schema `{schema_name}` not found for ALTER SCHEMA statement.")]
    /// Error indicating that an ALTER SCHEMA statement references a schema
    /// that does not exist.
    AlterSchemaNotFound {
        /// Name of the schema that was not found.
        schema_name: String,
    },
    #[error(
        "Function `{function_name}` cannot be moved or renamed while the call `{call}` may also reach a function that stays, since which one PostgreSQL bound is not known."
    )]
    /// Error indicating that `ALTER FUNCTION ... RENAME TO`, `ALTER FUNCTION
    /// ... SET SCHEMA` or `ALTER SCHEMA ... RENAME TO` moves a function a
    /// stored call may reach while another function that call may reach stays.
    /// Calls are bound by name and argument count, so the model cannot tell
    /// which of them PostgreSQL bound the call to, and respelling the call
    /// either way could point it at the wrong one.
    MovedFunctionCallAmbiguous {
        /// Name of the function being moved or renamed.
        function_name: String,
        /// The call as written.
        call: String,
    },
    #[error("Schema `{schema_name}` not found for moving function `{function_name}`.")]
    /// Error indicating that `ALTER FUNCTION ... SET SCHEMA` names a schema
    /// that does not exist.
    AlterFunctionSchemaNotFound {
        /// Name of the schema that was not found.
        schema_name: String,
        /// Name of the function being moved.
        function_name: String,
    },
    #[error(
        "Aggregate `{aggregate_name}` cannot be renamed or moved, as this model holds no aggregate."
    )]
    /// Error indicating that `ALTER AGGREGATE ... RENAME TO` or `SET SCHEMA`
    /// names an aggregate. No user aggregate can be in this model, and
    /// PostgreSQL refuses to reach a function through `ALTER AGGREGATE`, so
    /// the statement is refused rather than read as naming a function.
    AggregateAlterationUnsupported {
        /// Name of the aggregate the statement names.
        aggregate_name: String,
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
    #[error("`{statement}` is transaction control the engine behind this dialect does not have.")]
    /// A transaction control statement names a feature the engine lacks, such
    /// as a savepoint in DuckDB or any transaction in Spark.
    UnsupportedTransactionControl {
        /// The statement as written.
        statement: String,
    },
    #[error("`{statement}` runs code whose effects on the schema this model cannot represent.")]
    /// A PostgreSQL `DO` block ran. What it creates, alters or grants depends
    /// on the database it runs against, so the document is refused rather
    /// than modelled as though the block had not run.
    UnsupportedDoBlock {
        /// The statement as written.
        statement: String,
    },
    #[error("`{statement}` can only be used inside a transaction.")]
    /// A statement that ends or marks a transaction ran with none open, in an
    /// engine that refuses it there.
    NoOpenTransaction {
        /// The statement as written.
        statement: String,
    },
    #[error("`{statement}` cannot open a transaction inside another one.")]
    /// A statement opening a transaction ran inside one, in an engine that
    /// does not nest transactions.
    TransactionAlreadyOpen {
        /// The statement as written.
        statement: String,
    },
    #[error("Savepoint `{savepoint_name}` does not exist.")]
    /// A release of, or rollback to, a savepoint the open transaction does not
    /// hold.
    SavepointNotFound {
        /// Name of the savepoint as written.
        savepoint_name: String,
    },
    #[error(
        "`{statement}` defines a schema object inside a transaction, which this engine refuses."
    )]
    /// A schema definition ran inside a transaction, in an engine that only
    /// runs definitions outside one.
    DefinitionInTransaction {
        /// The statement as written.
        statement: String,
    },
    #[error(
        "`{statement}` follows a schema definition inside its transaction, where only the end of the transaction may."
    )]
    /// A statement other than the end of the transaction followed a schema
    /// definition, in an engine that requires the definition to come last.
    StatementAfterDefinition {
        /// The statement as written.
        statement: String,
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
        assert_eq!(InheritedChange::ValidateConstraint.to_string(), "validate a constraint of");
        assert_eq!(InheritedChange::DropColumn.to_string(), "drop a column of");
        assert_eq!(InheritedChange::SetNotNull.to_string(), "require a value in a column of");
        assert_eq!(RequiredValue::CoveredByKey.to_string(), "a key covers it");
        assert_eq!(RequiredValue::EnforcedByParent.to_string(), "a parent requires it");
        assert_eq!(FixedPersistence::Temporary.to_string(), "it is temporary");
        assert_eq!(FixedPersistence::Partitioned.to_string(), "it is partitioned");
        assert_eq!(KeyConstraint::PrimaryKey.to_string(), "Primary key");
        assert_eq!(KeyConstraint::Unique.to_string(), "Unique");
        assert_eq!(KeyConstraint::ForeignKey.to_string(), "Foreign key");
        assert_eq!(KeyConstraint::Exclusion.to_string(), "Exclusion");
        assert_eq!(ObjectKind::ForeignTable.to_string(), "Foreign table");
        assert_eq!(
            UnattachablePartition::ForeignTableUnderUniqueIndex.to_string(),
            "it is a foreign table and the partitioned table holds a unique index"
        );
        for (reason, text) in [
            (
                InvalidBound::StrategyMismatch { strategy: PartitionStrategy::Range },
                "it is not a range bound",
            ),
            (
                InvalidBound::StrategyMismatch { strategy: PartitionStrategy::List },
                "it is not a list bound",
            ),
            (
                InvalidBound::StrategyMismatch { strategy: PartitionStrategy::Hash },
                "it is not a hash bound",
            ),
            (
                InvalidBound::ValueCount { expected: 1, found: 2 },
                "it lists 2 values for 1 key columns",
            ),
            (InvalidBound::NullInRange, "a range bound cannot list NULL"),
            (
                InvalidBound::ValueAfterInfinity,
                "every value after MINVALUE or MAXVALUE must be the same",
            ),
            (InvalidBound::ColumnReference, "a bound cannot read a column"),
            (InvalidBound::DefaultOnHash, "a hash-partitioned table takes no default"),
            (
                InvalidBound::DefaultExists { default_partition: "d".into() },
                "`d` already is the default partition",
            ),
            (InvalidBound::ModulusNotPositive, "the modulus must be greater than zero"),
            (InvalidBound::RemainderNotBelowModulus, "the remainder must be less than the modulus"),
            (
                InvalidBound::ModulusNotFactor { other_partition: "h".into() },
                "its modulus and that of `h` do not divide one another",
            ),
            (InvalidBound::EmptyRange, "its range is empty"),
            (
                InvalidBound::ValueNotOfKeyType { value: "'x'".into(), type_name: "INT".into() },
                "`'x'` is not a value of type `INT`",
            ),
        ] {
            assert_eq!(reason.to_string(), text);
        }
    }

    #[test]
    fn parser_errors_keep_the_parser_cause() {
        let error = Error::from(ParserError::ParserError("broken SQL".to_string()));
        assert!(matches!(error, Error::SqlParserError { .. }));
    }
}
