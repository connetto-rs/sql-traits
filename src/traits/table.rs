//! Submodule providing a trait for describing SQL Table-like entities.

use alloc::{borrow::Cow, string::ToString, vec::Vec};
use core::{borrow::Borrow, fmt::Debug, hash::Hash};

use crate::{
    errors::{Error, LookupError, ObjectKind},
    structs::{
        IdentifierCase, SchemaFingerprint,
        fingerprint::{FingerprintError, compute_persistence_v1},
    },
    traits::{
        ColumnLike, DatabaseLike, DocumentationMetadata, ForeignKeyLike, GrantLike, Metadata,
        PolicyLike, TableGrantLike, TriggerLike, check_constraint::CheckConstraintLike,
    },
    utils::{
        identifier_resolution::{identifiers_match, normalize_identifier},
        object_name::is_temporary_schema,
    },
};

/// How a partitioned table routes a row to one of its partitions.
///
/// PostgreSQL requires one of these three words after `PARTITION BY`. A table
/// that declares one holds no rows of its own, even before a partition is
/// attached to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionStrategy {
    /// Each partition takes a contiguous span of the key.
    Range,
    /// Each partition takes an enumerated set of key values.
    List,
    /// Each partition takes a remainder class of the key.
    Hash,
}

/// A trait for types that can be treated as SQL tables.
pub trait TableLike:
    Debug
    + Clone
    + Send
    + Sync
    + Hash
    + Ord
    + Eq
    + Metadata
    + DocumentationMetadata
    + Borrow<<<Self as TableLike>::DB as DatabaseLike>::Table>
{
    /// The database type the table belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE mytable (id INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("mytable", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(table.table_name(), "mytable");
    /// # Ok(())
    /// # }
    /// ```
    fn table_name(&self) -> &str;

    /// Returns whether the table identifier was quoted in SQL.
    ///
    /// Quoted identifiers are resolved case-sensitively in PostgreSQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn table_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this table: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// Prefer this over [`Self::table_name`] whenever the name is emitted into
    /// SQL or compared against a catalog, since the raw name alone is only
    /// correct for identifiers that were already lowercase.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE Docs (id INT); CREATE TABLE \"Docs2\" (id INT);",
    /// )?;
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .stored_table_name(),
    ///     "docs"
    /// );
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("Docs2", true), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .stored_table_name(),
    ///     "Docs2"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_table_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.table_name(), self.table_name_is_quoted())
    }

    /// Returns whether the table has a snake_case name.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TABLE MyTable (id INT);
    /// ",
    /// )?;
    /// let snake_case_table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(snake_case_table.is_snake_case());
    /// let non_snake_case_table =
    ///     db.table_by_target(TargetName::new("MyTable", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(!non_snake_case_table.is_snake_case());
    /// # Ok(())
    /// # }
    /// ```
    fn is_snake_case(&self) -> bool {
        let name = self.table_name();
        name.chars().all(|c| c.is_lowercase() || c == '_')
    }

    /// Returns the triggers associated with the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   triggers from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT, name TEXT);
    /// CREATE FUNCTION my_func() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger BEFORE INSERT ON my_table FOR EACH ROW EXECUTE FUNCTION my_func();
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let triggers: Vec<&str> = table.triggers(&db)?.map(|t| t.name()).collect();
    /// assert_eq!(triggers, vec!["my_trigger"]);
    /// # Ok(())
    /// # }
    /// ```
    fn triggers<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Trigger>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;

        Ok(database.triggers().filter(move |trigger| {
            trigger.table(database).is_ok_and(|t| t.borrow() == self.borrow())
        }))
    }

    /// Returns the documentation of the table, if any.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   documentation from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE TABLE my_table (id INT);
    ///     -- the next table to create
    ///     CREATE TABLE my_next_table (id INT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let table_next = db
    ///     .table_by_target(TargetName::new("my_next_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert_eq!(table.table_doc(&db)?, None); // No documentation available
    /// assert_eq!(table_next.table_doc(&db)?, Some("the next table to create"));
    /// # Ok(())
    /// # }
    /// ```
    fn table_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db;

    /// The schema name of the table, if it has one.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE SCHEMA my_schema;
    /// CREATE TABLE my_schema.my_table_with_schema (id INT);
    /// CREATE TABLE my_table (id INT);",
    /// )?;
    /// let table_no_schema =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(table_no_schema.table_schema(), None);
    /// let table_with_schema = db
    ///     .table_by_target(
    ///         TargetName::new("my_table_with_schema", false).with_schema("my_schema", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// assert_eq!(table_with_schema.table_schema(), Some("my_schema"));
    /// # Ok(())
    /// # }
    /// ```
    fn table_schema(&self) -> Option<&str>;

    /// Returns whether the schema identifier of this table was quoted in SQL.
    ///
    /// This only matters when [`Self::table_schema`] returns `Some`.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn table_schema_is_quoted(&self) -> bool {
        false
    }

    /// Returns the schema name PostgreSQL stores for this table, or `None` when
    /// the table declares no schema.
    ///
    /// An unquoted identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA My_Schema;
    /// CREATE SCHEMA \"Other\";
    /// CREATE TABLE My_Schema.t (id INT);
    /// CREATE TABLE \"Other\".u (id INT);
    /// CREATE TABLE v (id INT);",
    /// )?;
    /// assert_eq!(
    ///     db.table_by_target(
    ///         TargetName::new("t", false).with_schema("my_schema", false),
    ///         IdentifierCase::AsWritten
    ///     )?
    ///     .unwrap()
    ///     .stored_table_schema()
    ///     .as_deref(),
    ///     Some("my_schema")
    /// );
    /// assert_eq!(
    ///     db.table_by_target(
    ///         TargetName::new("u", false).with_schema("Other", true),
    ///         IdentifierCase::AsWritten
    ///     )?
    ///     .unwrap()
    ///     .stored_table_schema()
    ///     .as_deref(),
    ///     Some("Other")
    /// );
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .stored_table_schema(),
    ///     None
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_table_schema(&self) -> Option<Cow<'_, str>> {
        self.table_schema()
            .map(|schema| normalize_identifier(schema, self.table_schema_is_quoted()))
    }

    /// Returns whether this table lives in the session's temporary schema.
    ///
    /// PostgreSQL keeps every temporary relation in `pg_temp`, whether the
    /// statement wrote `TEMP`, qualified the name with `pg_temp`, or created
    /// it while the search path began there. The model records each of those
    /// in `pg_temp`, so this reads the stored schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id INT); CREATE TEMP TABLE docs (id INT, draft TEXT);",
    /// )?;
    /// let lookup = |target| db.table_by_target(target, IdentifierCase::AsWritten);
    /// let permanent = lookup(TargetName::new("docs", false))?.unwrap();
    /// let temporary = lookup(TargetName::new("docs", false).with_schema("pg_temp", false))?.unwrap();
    /// assert!(!permanent.is_temporary());
    /// assert!(temporary.is_temporary());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_temporary(&self) -> bool {
        self.table_schema()
            .is_some_and(|schema| is_temporary_schema(schema, self.table_schema_is_quoted()))
    }

    /// Returns the table ID according to its position in the database's table
    /// iterator.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT);
    /// CREATE TABLE table2 (name TEXT);
    /// CREATE TABLE table3 (score DECIMAL);
    /// ",
    /// )?;
    /// let table2 = db
    ///     .table_by_target(TargetName::new("table2", false), IdentifierCase::AsWritten)?
    ///     .expect("Table 'table2' should exist");
    /// assert_eq!(table2.table_id(&db), Some(1));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn table_id(&self, database: &Self::DB) -> Option<usize> {
        database.table_id(self.borrow())
    }

    /// Confirms that `database` holds this table.
    ///
    /// Most accessors resolve the table's own metadata and so report an absent
    /// receiver as a side effect. The ones that answer from the tables
    /// `database` holds, comparing this table by identity, would otherwise
    /// answer as though it were simply unrelated to everything. They call this
    /// first so that an absent receiver is reported rather than answered.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(table.require_in_database(&db).is_ok());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn require_in_database(&self, database: &Self::DB) -> Result<(), LookupError> {
        // The iterator is the probe, not the answer: resolving it is what
        // establishes that `database` holds this table.
        drop(TableLike::columns(self, database)?);
        Ok(())
    }

    /// Iterates over the columns of the table using the provided schema.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, name TEXT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column_names: Vec<&str> = table.columns(&db)?.map(|col| col.column_name()).collect();
    /// assert_eq!(column_names, vec!["id", "name"]);
    /// # Ok(())
    /// # }
    /// ```
    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db;

    /// Iterates over the columns the table declares itself.
    ///
    /// A table that inherits carries its parents' columns beside its own, and
    /// this answers only those written in its own definition. A column both
    /// the table and a parent declare counts as the table's own, which is
    /// what PostgreSQL records.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE docs (id INT, owner_id TEXT);
    ///      CREATE TABLE secret_docs (classification TEXT) INHERITS (docs);",
    /// )?;
    /// let child = db
    ///     .table_by_target(TargetName::new("secret_docs", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let all: Vec<&str> = child.columns(&db)?.map(|column| column.column_name()).collect();
    /// assert_eq!(all, vec!["id", "owner_id", "classification"]);
    /// let own: Vec<&str> = child.local_columns(&db)?.map(|column| column.column_name()).collect();
    /// assert_eq!(own, vec!["classification"]);
    /// # Ok(())
    /// # }
    /// ```
    fn local_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db;

    /// Iterates over the tables this one inherits from.
    ///
    /// Only `INHERITS` parents, and only direct ones, so a grandparent is
    /// answered by the parent rather than here. A partition answers nothing
    /// here: the table it belongs to is [`TableLike::partition_root`], which
    /// PostgreSQL records the same way but which passes down its keys as well
    /// as its columns and which holds no rows of its own.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE docs (id INT);
    ///      CREATE TABLE secret_docs (classification TEXT) INHERITS (docs);
    ///      CREATE TABLE evt (id INT) PARTITION BY RANGE (id);
    ///      CREATE TABLE evt_low PARTITION OF evt FOR VALUES FROM (1) TO (9);",
    /// )?;
    /// let child = db
    ///     .table_by_target(TargetName::new("secret_docs", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parents: Vec<&str> = child.inherits_from(&db)?.map(|p| p.table_name()).collect();
    /// assert_eq!(parents, vec!["docs"]);
    ///
    /// let parent =
    ///     db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(parent.inherits_from(&db)?.count(), 0);
    ///
    /// // A partition belongs to its root, it does not inherit from it.
    /// let part =
    ///     db.table_by_target(TargetName::new("evt_low", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(part.inherits_from(&db)?.count(), 0);
    /// # Ok(())
    /// # }
    /// ```
    fn inherits_from<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db;

    /// Iterates over the tables that inherit from this one.
    ///
    /// The inverse of [`TableLike::inherits_from`], so a partition is answered
    /// by [`TableLike::partitions`] instead, and likewise only one step deep,
    /// so a grandchild is answered by the child rather than here.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table or one of the tables it checks.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE docs (id INT);
    ///      CREATE TABLE secret_docs (classification TEXT) INHERITS (docs);
    ///      CREATE TABLE evt (id INT) PARTITION BY RANGE (id);
    ///      CREATE TABLE evt_low PARTITION OF evt FOR VALUES FROM (1) TO (9);",
    /// )?;
    /// let parent =
    ///     db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?.unwrap();
    /// let children: Vec<&str> = parent.inheritors(&db)?.map(|c| c.table_name()).collect();
    /// assert_eq!(children, vec!["secret_docs"]);
    ///
    /// // A root has partitions, not inheritors.
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("evt", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .inheritors(&db)?
    ///         .count(),
    ///     0
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn inheritors<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;
        let schema = self.table_schema();
        let name = self.table_name();
        let mut children = Vec::new();
        for candidate in database.tables() {
            let mut parents = candidate.inherits_from(database)?;
            if parents.any(|parent| parent.table_schema() == schema && parent.table_name() == name)
            {
                children.push(candidate);
            }
        }
        Ok(children.into_iter())
    }

    /// Returns the partitioned table this one is a partition of.
    ///
    /// A partition is written `PARTITION OF` and receives the root's keys,
    /// unique constraints and foreign keys along with its columns, none of
    /// which an `INHERITS` child receives. Only the direct root is answered,
    /// so a partition of a partition answers the one it names.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE evt (id INT) PARTITION BY RANGE (id);
    ///      CREATE TABLE evt_low PARTITION OF evt FOR VALUES FROM (1) TO (9);",
    /// )?;
    /// let part =
    ///     db.table_by_target(TargetName::new("evt_low", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(part.partition_root(&db)?.map(|root| root.table_name()), Some("evt"));
    /// assert!(
    ///     db.table_by_target(TargetName::new("evt", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .partition_root(&db)?
    ///         .is_none()
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn partition_root<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db;

    /// Returns how this table routes rows to its partitions, or [`None`] when
    /// it is not partitioned.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE evt (id INT, region TEXT) PARTITION BY LIST (region);
    ///      CREATE TABLE docs (id INT);",
    /// )?;
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("evt", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .partition_strategy(),
    ///     Some(PartitionStrategy::List)
    /// );
    /// assert_eq!(
    ///     db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .partition_strategy(),
    ///     None
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn partition_strategy(&self) -> Option<PartitionStrategy>;

    /// Iterates over the partitions of this table.
    ///
    /// The inverse of [`TableLike::partition_root`], one step deep, so a
    /// partition that is itself partitioned answers its own.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table or one of the tables it checks.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE evt (id INT) PARTITION BY RANGE (id);
    ///      CREATE TABLE evt_low PARTITION OF evt FOR VALUES FROM (1) TO (9);",
    /// )?;
    /// let root =
    ///     db.table_by_target(TargetName::new("evt", false), IdentifierCase::AsWritten)?.unwrap();
    /// let parts: Vec<&str> = root.partitions(&db)?.map(|p| p.table_name()).collect();
    /// assert_eq!(parts, vec!["evt_low"]);
    /// # Ok(())
    /// # }
    /// ```
    fn partitions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;
        let schema = self.table_schema();
        let name = self.table_name();
        let mut partitions = Vec::new();
        for candidate in database.tables() {
            if candidate
                .partition_root(database)?
                .is_some_and(|root| root.table_schema() == schema && root.table_name() == name)
            {
                partitions.push(candidate);
            }
        }
        Ok(partitions.into_iter())
    }

    /// Whether this table is a partition of another.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    fn is_partition(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.partition_root(database)?.is_some())
    }

    /// Whether this table is partitioned, and so holds no rows of its own.
    ///
    /// True from the moment `PARTITION BY` is written, which is also when
    /// PostgreSQL starts refusing rows into the table, so a root with no
    /// partitions yet still answers `true`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE evt (id INT) PARTITION BY RANGE (id);
    ///      CREATE TABLE docs (id INT);",
    /// )?;
    /// let root =
    ///     db.table_by_target(TargetName::new("evt", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(root.is_partitioned());
    /// assert_eq!(root.partitions(&db)?.count(), 0);
    /// assert!(
    ///     !db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?
    ///         .unwrap()
    ///         .is_partitioned()
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn is_partitioned(&self) -> bool {
        self.partition_strategy().is_some()
    }

    /// Returns a deterministic SHA-256 fingerprint of the table's schema.
    ///
    /// The fingerprint is stable across Rust versions and suitable for
    /// persistence. It encodes the schema name, table name, columns
    /// (ordinal, name, canonical type token, nullability, generated flag),
    /// and primary-key ordinals using a versioned binary format.
    ///
    /// Returns [`FingerprintError`] if the canonical model is malformed
    /// (non-contiguous column ordinals, duplicate primary-key ordinals, or
    /// primary-key ordinals out of range). See FINGERPRINT_SPEC §10.2.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`FingerprintError`], and produces no digest, when the input
    /// table fails validation or when `database` does not hold it.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE users_archive (id INT PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    ///
    /// let users = db
    ///     .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?
    ///     .expect("users table should exist");
    /// let users_archive = db
    ///     .table_by_target(TargetName::new("users_archive", false), IdentifierCase::AsWritten)?
    ///     .expect("users_archive table should exist");
    ///
    /// // Different table names produce different fingerprints.
    /// assert_ne!(users.schema_fingerprint(&db)?, users_archive.schema_fingerprint(&db)?);
    ///
    /// // Same SQL produces the same fingerprint (deterministic).
    /// let db2 =
    ///     ParserDB::parse::<GenericDialect>("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")?;
    /// let users2 =
    ///     db2.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(users.schema_fingerprint(&db)?, users2.schema_fingerprint(&db2)?);
    /// # Ok(())
    /// # }
    /// ```
    fn schema_fingerprint(
        &self,
        database: &Self::DB,
    ) -> Result<SchemaFingerprint, FingerprintError> {
        compute_persistence_v1(self, database)
    }

    /// Returns whether any of the columns of the table are generated.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id SERIAL PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_other_table (id INT PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    ///
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(table.has_generated_columns(&db)?);
    /// let other_table = db
    ///     .table_by_target(TargetName::new("my_other_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!other_table.has_generated_columns(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_generated_columns(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.columns(database)?.any(ColumnLike::is_generated))
    }

    /// Returns the number of columns in the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db =
    ///     ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, name TEXT, age INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(table.number_of_columns(&db)?, 3);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn number_of_columns(&self, database: &Self::DB) -> Result<usize, LookupError> {
        Ok(self.columns(database)?.count())
    }

    /// Returns the corresponding column by name, if it exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the column to retrieve.
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, name TEXT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// assert_eq!(id_column.column_name(), "id");
    /// let non_existent_column = table.column("non_existent", &db, IdentifierCase::AsWritten)?;
    /// assert!(non_existent_column.is_none());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// PostgreSQL-style identifier resolution is applied:
    ///
    /// - Unquoted lookup names are case-insensitive.
    /// - Quoted lookup names are case-sensitive.
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     r#"
    ///     CREATE TABLE t (
    ///         Foo INT,
    ///         "ColA" INT
    ///     );
    ///     "#,
    /// )?;
    /// let table = db
    ///     .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
    ///     .expect("Table should exist");
    ///
    /// assert!(table.column("foo", &db, IdentifierCase::AsWritten)?.is_some());
    /// assert!(table.column("\"foo\"", &db, IdentifierCase::AsWritten)?.is_some());
    /// assert!(table.column("\"Foo\"", &db, IdentifierCase::AsWritten)?.is_none());
    ///
    /// assert!(table.column("\"ColA\"", &db, IdentifierCase::AsWritten)?.is_some());
    /// assert!(table.column("cola", &db, IdentifierCase::AsWritten)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn column<'db>(
        &'db self,
        name: &str,
        database: &'db Self::DB,
        case: IdentifierCase,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        Ok(TableLike::columns(self, database)?
            .find(|col| case.names_stored(col.column_name(), col.column_name_is_quoted(), name)))
    }

    /// Returns the position of the named column in the table's column
    /// iterator, if the table declares it.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the column to locate, quoted as SQL quotes it.
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (\"ID\" INT, id INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    ///
    /// assert_eq!(table.column_id_by_name("\"ID\"", &db, IdentifierCase::AsWritten)?, Some(0));
    /// assert_eq!(table.column_id_by_name("ID", &db, IdentifierCase::AsWritten)?, Some(1));
    /// assert_eq!(table.column_id_by_name("absent", &db, IdentifierCase::AsWritten)?, None);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn column_id_by_name(
        &self,
        name: &str,
        database: &Self::DB,
        case: IdentifierCase,
    ) -> Result<Option<usize>, LookupError> {
        Ok(TableLike::columns(self, database)?.position(|column| {
            case.names_stored(column.column_name(), column.column_name_is_quoted(), name)
        }))
    }

    /// Returns the corresponding column by ID position in the table's column
    /// iterator, if it exists.
    ///
    /// # Arguments
    ///
    /// * `column_id` - The position of the column to retrieve.
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db =
    ///     ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, name TEXT, age INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    ///
    /// let name_column = table.column_by_id(1, &db)?.expect("Column at position 1 should exist");
    /// assert_eq!(name_column.column_name(), "name");
    /// assert!(table.column_by_id(3, &db)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn column_by_id<'db>(
        &'db self,
        column_id: usize,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        Ok(TableLike::columns(self, database)?.nth(column_id))
    }

    /// Returns the name of the column at the given position in the table's
    /// column iterator, if the position names one.
    ///
    /// # Arguments
    ///
    /// * `column_id` - The position of the column to name.
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT, name TEXT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    ///
    /// assert_eq!(table.column_name_by_id(1, &db)?, Some("name"));
    /// assert_eq!(table.column_name_by_id(2, &db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn column_name_by_id<'db>(
        &'db self,
        column_id: usize,
        database: &'db Self::DB,
    ) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(TableLike::column_by_id(self, column_id, database)?.map(ColumnLike::column_name))
    }

    /// Returns whether the provided column belongs to this table.
    ///
    /// # Arguments
    ///
    /// * `column` - The column to check.
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT, name TEXT);
    /// CREATE TABLE table2 (id INT, description TEXT);
    /// ",
    /// )?;
    /// let table1 =
    ///     db.table_by_target(TargetName::new("table1", false), IdentifierCase::AsWritten)?.unwrap();
    /// let table2 =
    ///     db.table_by_target(TargetName::new("table2", false), IdentifierCase::AsWritten)?.unwrap();
    /// let table1_id = table1
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist in table1");
    /// let table2_id = table2
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist in table2");
    /// assert!(table1.has_column(table1_id, &db)?);
    /// assert!(!table1.has_column(table2_id, &db)?);
    /// assert!(table2.has_column(table2_id, &db)?);
    /// assert!(!table2.has_column(table1_id, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_column(
        &self,
        column: &<Self::DB as DatabaseLike>::Column,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(TableLike::columns(self, database)?.any(|col| col == column))
    }

    /// Iterates over the primary key columns of the table using the provided
    /// schema.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_composite_pk_table (id1 INT, id2 INT, name TEXT, PRIMARY KEY (id1, id2));
    /// CREATE TABLE my_no_pk_table (id INT, name TEXT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let pk_columns: Vec<&str> =
    ///     table.primary_key_columns(&db)?.map(|col| col.column_name()).collect();
    /// assert_eq!(pk_columns, vec!["id"]);
    /// let composite_pk_table = db
    ///     .table_by_target(
    ///         TargetName::new("my_composite_pk_table", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// let composite_pk_columns: Vec<&str> =
    ///     composite_pk_table.primary_key_columns(&db)?.map(|col| col.column_name()).collect();
    /// assert_eq!(composite_pk_columns, vec!["id1", "id2"]);
    /// let no_pk_table = db
    ///     .table_by_target(TargetName::new("my_no_pk_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let no_pk_columns: Vec<&str> =
    ///     no_pk_table.primary_key_columns(&db)?.map(|col| col.column_name()).collect();
    /// assert_eq!(no_pk_columns, Vec::<&str>::new());
    /// # Ok(())
    /// # }
    /// ```
    fn primary_key_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db;

    /// Returns the positions of the primary key columns in the table's column
    /// iterator, in the order the key declares them.
    ///
    /// The columns are walked once for the whole key, so the cost is flat from
    /// a key of two columns upward, and a single column key stores nothing of
    /// its own and costs less still.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::ColumnNotFound`] when a key column
    /// is not among the table's own columns, which a shorter key would hide.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE composite (a INT, b INT, c INT, PRIMARY KEY (c, a));
    /// CREATE TABLE keyless (a INT);
    /// ",
    /// )?;
    /// let composite = db
    ///     .table_by_target(TargetName::new("composite", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert_eq!(composite.primary_key_column_ids(&db)?, vec![2, 0]);
    ///
    /// let keyless =
    ///     db.table_by_target(TargetName::new("keyless", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(keyless.primary_key_column_ids(&db)?, Vec::<usize>::new());
    /// # Ok(())
    /// # }
    /// ```
    fn primary_key_column_ids(&self, database: &Self::DB) -> Result<Vec<usize>, LookupError> {
        let mut declared = TableLike::primary_key_columns(self, database)?;
        let Some(first_key_column) = declared.next() else {
            return Ok(Vec::new());
        };
        // A single column key leaves this empty, and collecting an exhausted
        // iterator allocates nothing.
        let remaining_key_columns: Vec<&<Self::DB as DatabaseLike>::Column> = declared.collect();
        let key_columns =
            || core::iter::once(first_key_column).chain(remaining_key_columns.iter().copied());

        // No ordinal can be `usize::MAX`, so it stands for a key column the
        // walk below has not reached yet.
        let mut column_ids = alloc::vec![usize::MAX; remaining_key_columns.len() + 1];
        for (position, column) in TableLike::columns(self, database)?.enumerate() {
            for (ordinal, key_column) in column_ids.iter_mut().zip(key_columns()) {
                if *ordinal == usize::MAX
                    && identifiers_match(
                        column.column_name(),
                        column.column_name_is_quoted(),
                        key_column.column_name(),
                        key_column.column_name_is_quoted(),
                    )
                {
                    *ordinal = position;
                }
            }
        }

        if let Some(unresolved) = column_ids.iter().position(|ordinal| *ordinal == usize::MAX) {
            let key_column = if unresolved == 0 {
                first_key_column
            } else {
                remaining_key_columns[unresolved - 1]
            };
            return Err(LookupError::ColumnNotFound {
                table_name: self.table_name().to_string(),
                column_name: key_column.column_name().to_string(),
            });
        }
        Ok(column_ids)
    }

    /// Returns the single primary key column of the table, if it exists and is
    /// non-composite.
    ///
    /// Returns `None` if the table has no primary key or if the primary key is
    /// composite.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE composite_pk (id1 INT, id2 INT, PRIMARY KEY (id1, id2));
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let pk_column = table.primary_key_column(&db)?.unwrap();
    /// assert_eq!(pk_column.column_name(), "id");
    ///
    /// let composite_table = db
    ///     .table_by_target(TargetName::new("composite_pk", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(composite_table.primary_key_column(&db)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn primary_key_column<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Column>, LookupError> {
        let mut pk_columns = self.primary_key_columns(database)?;
        let Some(pk_column) = pk_columns.next() else {
            return Ok(None);
        };
        if pk_columns.next().is_some() {
            // Composite primary key
            return Ok(None);
        }
        Ok(Some(pk_column))
    }

    /// Returns whether the primary key of the table is generated (i.e.,
    /// auto-incrementing).
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id SERIAL PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_no_gen_pk_table (id INT PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(table.has_surrogate_primary_key(&db)?);
    /// let no_gen_pk_table = db
    ///     .table_by_target(TargetName::new("my_no_gen_pk_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!no_gen_pk_table.has_surrogate_primary_key(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_surrogate_primary_key(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.primary_key_columns(database)?.all(ColumnLike::is_generated)
            && self.has_primary_key(database)?)
    }

    /// Returns a vector with the normalized data types of the primary key
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id SERIAL PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_composite_pk_table (id1 INT, id2 BIGSERIAL, name TEXT, PRIMARY KEY (id1, id2));
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let pk_types = table.primary_key_type(&db)?;
    /// assert_eq!(pk_types, vec!["INT"]);
    /// let composite_pk_table = db
    ///     .table_by_target(
    ///         TargetName::new("my_composite_pk_table", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// let composite_pk_types = composite_pk_table.primary_key_type(&db)?;
    /// assert_eq!(composite_pk_types, vec!["INT", "BIGINT"]);
    /// # Ok(())
    /// # }
    /// ```
    fn primary_key_type<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Vec<Cow<'db, str>>, LookupError> {
        Ok(self
            .primary_key_columns(database)?
            .map(|col| col.normalized_data_type(database))
            .collect())
    }

    /// Returns whether the provided column is the primary key column of the
    /// table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `column` - A reference to the column to check.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// assert!(table.is_primary_key_column(&db, id_column)?);
    /// assert!(!table.is_primary_key_column(&db, name_column)?);
    ///
    /// // Every column of a composite key is one of its columns.
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE pair (a INT, b INT, c INT, PRIMARY KEY (a, b));
    /// ",
    /// )?;
    /// let pair =
    ///     db.table_by_target(TargetName::new("pair", false), IdentifierCase::AsWritten)?.unwrap();
    /// for name in ["a", "b"] {
    ///     let column = pair.column(name, &db, IdentifierCase::AsWritten)?.expect("column exists");
    ///     assert!(pair.is_primary_key_column(&db, column)?);
    /// }
    /// let c = pair.column("c", &db, IdentifierCase::AsWritten)?.expect("column exists");
    /// assert!(!pair.is_primary_key_column(&db, c)?);
    /// # Ok(())
    /// # }
    /// ```
    fn is_primary_key_column(
        &self,
        database: &Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Result<bool, LookupError> {
        // Any of the key's columns, not all of them: asking `all` answered
        // false for every column of a composite key, and true for a table with
        // no key at all, which is what the emptiness guard was covering up.
        Ok(self.primary_key_columns(database)?.any(|col| col == column))
    }

    /// Returns whether the table has a primary key.
    ///
    /// # Arguments
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_no_pk_table (id INT, name TEXT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(table.has_primary_key(&db)?);
    /// let no_pk_table = db
    ///     .table_by_target(TargetName::new("my_no_pk_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!no_pk_table.has_primary_key(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_primary_key(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.primary_key_columns(database)?.next().is_some())
    }

    /// Returns an iterator over the non-primary key columns of the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT, age INT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let non_pk_columns: Vec<&str> =
    ///     table.non_primary_key_columns(&db)?.map(|col| col.column_name()).collect();
    /// assert_eq!(non_pk_columns, vec!["name", "age"]);
    /// # Ok(())
    /// # }
    /// ```
    fn non_primary_key_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        let primary_key_column_names: Vec<&<Self::DB as DatabaseLike>::Column> =
            self.primary_key_columns(database)?.collect();

        Ok(self.columns(database)?.filter(move |col| !primary_key_column_names.contains(col)))
    }

    /// Returns whether the table has non-primary key columns.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_pk_only_table (id INT PRIMARY KEY);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(table.has_non_primary_key_columns(&db)?);
    /// let pk_only_table = db
    ///     .table_by_target(TargetName::new("my_pk_only_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!pk_only_table.has_non_primary_key_columns(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_non_primary_key_columns(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.non_primary_key_columns(database)?.next().is_some())
    }

    /// Returns whether the table has a composite primary key.
    ///
    /// # Arguments
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE my_composite_pk_table (id1 INT, id2 INT, name TEXT, PRIMARY KEY (id1, id2));
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(!table.has_composite_primary_key(&db)?);
    /// let composite_pk_table = db
    ///     .table_by_target(
    ///         TargetName::new("my_composite_pk_table", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// assert!(composite_pk_table.has_composite_primary_key(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_composite_primary_key(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.primary_key_columns(database)?.nth(1).is_some())
    }

    /// Iterates over the check constraints of the table using the provided
    /// schema.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT CHECK (id > 0), name TEXT, CHECK (length(name) > 0));
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check_constraints: Vec<_> =
    ///     table.check_constraints(&db)?.map(|cc| cc.expression(&db).to_string()).collect();
    /// assert_eq!(check_constraints, vec!["id > 0", "length(name) > 0"]);
    /// # Ok(())
    /// # }
    /// ```
    fn check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint>, LookupError>
    where
        Self: 'db;

    /// Iterates over the non-tautological check constraints of the table using
    /// the provided schema.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT CHECK (TRUE), name TEXT, CHECK (length(name) > 0));
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let non_tautological_ccs: Vec<_> = table
    ///     .non_tautological_check_constraints(&db)?
    ///     .map(|cc| cc.expression(&db).to_string())
    ///     .collect();
    /// assert_eq!(non_tautological_ccs, vec!["length(name) > 0"]);
    /// # Ok(())
    /// # }
    /// ```
    fn non_tautological_check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint>, LookupError>
    where
        Self: 'db,
    {
        let mut check_constraints = Vec::new();
        for check_constraint in self.check_constraints(database)? {
            if !check_constraint.is_tautology(database)? {
                check_constraints.push(check_constraint);
            }
        }

        Ok(check_constraints.into_iter())
    }

    /// Returns whether the table has any check constraints.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table_with_cc (id INT CHECK (id > 0), name TEXT);
    /// CREATE TABLE my_table_without_cc (id INT, name TEXT);
    /// ",
    /// )?;
    /// let table_with_cc = db
    ///     .table_by_target(TargetName::new("my_table_with_cc", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(table_with_cc.has_check_constraints(&db)?);
    /// let table_without_cc = db
    ///     .table_by_target(TargetName::new("my_table_without_cc", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!table_without_cc.has_check_constraints(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_check_constraints(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.check_constraints(database)?.next().is_some())
    }

    /// Returns whether the table has any non-tautological check constraints.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table_with_non_tautological_cc (id INT CHECK (id > 0), name TEXT);
    /// CREATE TABLE my_table_with_only_tautological_cc (id INT CHECK (TRUE), name TEXT);
    /// ",
    /// )?;
    /// let table_with_non_tautological_cc = db
    ///     .table_by_target(
    ///         TargetName::new("my_table_with_non_tautological_cc", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// assert!(table_with_non_tautological_cc.has_non_tautological_check_constraints(&db)?);
    /// let table_with_only_tautological_cc = db
    ///     .table_by_target(
    ///         TargetName::new("my_table_with_only_tautological_cc", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// assert!(!table_with_only_tautological_cc.has_non_tautological_check_constraints(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_non_tautological_check_constraints(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(self.non_tautological_check_constraints(database)?.next().is_some())
    }

    /// Returns whether the table or any of its ancestral extended tables have
    /// non-tautological check constraints.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, age INT CHECK (age > 0));
    /// CREATE TABLE child_table (id INT PRIMARY KEY REFERENCES parent_table(id), salary INT);
    /// CREATE TABLE another_table (id INT PRIMARY KEY, value INT CHECK (TRUE));
    /// ",
    /// )?;
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(parent_table.has_non_tautological_check_constraints_in_hierarchy(&db)?);
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(child_table.has_non_tautological_check_constraints_in_hierarchy(&db)?);
    /// let another_table = db
    ///     .table_by_target(TargetName::new("another_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!another_table.has_non_tautological_check_constraints_in_hierarchy(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_non_tautological_check_constraints_in_hierarchy(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        if self.has_non_tautological_check_constraints(database)? {
            return Ok(true);
        }
        for table in self.ancestral_extended_tables(database)? {
            if table.has_non_tautological_check_constraints(database)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Iterates over the indices associated with the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT, name TEXT);
    /// CREATE INDEX my_index ON my_table (name);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let indices: Vec<_> = table.indices(&db)?.collect();
    /// assert_eq!(indices.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Index>, LookupError>
    where
        Self: 'db;

    /// Iterates over the unique indices of the table using the provided schema.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT UNIQUE, name TEXT, UNIQUE (name));
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let unique_indices: Vec<_> = table
    ///     .unique_indices(&db)?
    ///     .map(|ui| ui.columns(&db).map(|iter| iter.map(|col| col.column_name()).collect::<Vec<_>>()))
    ///     .collect::<Result<Vec<_>, _>>()?;
    /// assert_eq!(unique_indices, vec![vec!["id"], vec!["name"]]);
    /// # Ok(())
    /// # }
    /// ```
    fn unique_indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::UniqueIndex>, LookupError>
    where
        Self: 'db;

    /// Iterates over the foreign keys of the table using the provided schema.
    ///
    /// # Arguments
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    /// ```
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT, name TEXT, FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let foreign_keys = host_table.foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(foreign_keys.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db;

    /// Returns whether the table has any foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table_with_fk (id INT, name TEXT, FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// CREATE TABLE host_table_without_fk (id INT, name TEXT);
    /// ",
    /// )?;
    /// let referenced_table = db
    ///     .table_by_target(TargetName::new("referenced_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!referenced_table.has_foreign_keys(&db)?);
    /// let host_table_with_fk = db
    ///     .table_by_target(TargetName::new("host_table_with_fk", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(host_table_with_fk.has_foreign_keys(&db)?);
    /// let host_table_without_fk = db
    ///     .table_by_target(
    ///         TargetName::new("host_table_without_fk", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    /// assert!(!host_table_without_fk.has_foreign_keys(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_foreign_keys(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.foreign_keys(database)?.next().is_some())
    }

    /// Returns whether the table has non-self-referential foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY);
    /// CREATE TABLE child_table (id INT PRIMARY KEY, parent_id INT REFERENCES parent_table(id));
    /// CREATE TABLE self_ref_table (id INT PRIMARY KEY, parent_id INT REFERENCES self_ref_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(child_table.has_non_self_referential_foreign_keys(&db)?);
    /// let self_ref_table = db
    ///     .table_by_target(TargetName::new("self_ref_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!self_ref_table.has_non_self_referential_foreign_keys(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_non_self_referential_foreign_keys(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        for foreign_key in self.foreign_keys(database)? {
            if !foreign_key.is_self_referential(database)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Iterates over the foreign keys in the current table which refer to
    /// ancestors of the provided table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `table` - A reference to the table whose ancestors are to be
    ///   considered.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold. The same applies to
    /// `table`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY REFERENCES parent_table(id));
    /// CREATE TABLE other_table (id INT PRIMARY KEY);
    /// CREATE TABLE host_table (
    ///     id INT PRIMARY KEY,
    ///     parent_id INT REFERENCES parent_table(id),
    ///     grandparent_id INT REFERENCES grandparent_table(id),
    ///     other_id INT REFERENCES other_table(id)
    /// );
    /// ",
    /// )?;
    ///
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let fks_to_ancestors = host_table.foreign_keys_to_ancestors_of(&db, child_table)?;
    /// assert_eq!(fks_to_ancestors.count(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn foreign_keys_to_ancestors_of<'db>(
        &'db self,
        database: &'db Self::DB,
        table: &'db Self,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        let ancestors = table.ancestral_extended_tables(database)?;
        let mut foreign_keys = Vec::new();
        for foreign_key in self.foreign_keys(database)? {
            let referenced_table = foreign_key.referenced_table(database)?;
            if ancestors.iter().any(|ancestor| (*ancestor).borrow() == referenced_table)
                && foreign_key.is_referenced_primary_key(database)?
            {
                foreign_keys.push(foreign_key);
            }
        }

        Ok(foreign_keys.into_iter())
    }

    /// Returns a vector with the (deduplicated) tables which are referenced by
    /// the current table via foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table1 (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE referenced_table2 (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table1(id),
    ///     FOREIGN KEY (id) REFERENCES referenced_table2(id));
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let referenced_tables = host_table.referenced_tables(&db)?;
    /// assert_eq!(referenced_tables.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn referenced_tables<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let mut referenced_tables = Vec::new();

        for foreign_key in self.foreign_keys(database)? {
            referenced_tables.push(foreign_key.referenced_table(database)?);
        }

        referenced_tables.sort_unstable();
        referenced_tables.dedup();

        Ok(referenced_tables)
    }

    /// Returns a vector with the (deduplicated) tables which are referenced by
    /// the current table via foreign keys, and which are not the table itself.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (
    ///     id INT PRIMARY KEY,
    ///     parent_id INT REFERENCES host_table(id),
    ///     other_id INT REFERENCES referenced_table(id)
    /// );
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let non_self_refs = host_table.non_self_referenced_tables(&db)?;
    /// assert_eq!(non_self_refs.len(), 1);
    /// assert_eq!(non_self_refs[0].table_name(), "referenced_table");
    /// # Ok(())
    /// # }
    /// ```
    fn non_self_referenced_tables<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let mut referenced_tables = self.referenced_tables(database)?;
        referenced_tables.retain(|&table| table != self.borrow());
        Ok(referenced_tables)
    }

    /// Returns the foreign keys which are used to define extensions.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT PRIMARY KEY REFERENCES referenced_table(id), name TEXT);
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let extension_fks = host_table.extension_foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(extension_fks.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn extension_foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        let mut foreign_keys = Vec::new();
        for foreign_key in self.foreign_keys(database)? {
            if foreign_key.is_extension_foreign_key(database)? {
                foreign_keys.push(foreign_key);
            }
        }

        Ok(foreign_keys.into_iter())
    }

    /// Returns the tables which are extended by the current table via foreign
    /// keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE extended_table (id INT PRIMARY KEY);
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY);
    /// CREATE TABLE host_table (
    ///     id INT PRIMARY KEY,
    ///     other_id INT,
    ///     FOREIGN KEY (other_id) REFERENCES referenced_table(id),
    ///     FOREIGN KEY (id) REFERENCES extended_table(id)
    /// );
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let extended_tables = host_table.extended_tables(&db)?;
    /// assert_eq!(extended_tables.count(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn extended_tables<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let mut extended_tables = Vec::new();
        for foreign_key in self.extension_foreign_keys(database)? {
            extended_tables.push(foreign_key.referenced_table(database)?);
        }

        Ok(extended_tables.into_iter())
    }

    /// Returns the root table of the extension hierarchy for the current
    /// table, if any. If the table is not extending any other table, returns
    /// `None`.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let root_table = child_table.extension_root_table(&db)?.unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert_eq!(root_table, grandparent_table);
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_root_table = parent_table.extension_root_table(&db)?.unwrap();
    /// assert_eq!(parent_root_table, grandparent_table);
    /// let grandparent_root_table = grandparent_table.extension_root_table(&db)?;
    /// assert!(grandparent_root_table.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn extension_root_table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let Some(extension) = self.extended_tables(database)?.next() else {
            return Ok(None);
        };

        Ok(extension.extension_root_table(database)?.or(Some(extension)))
    }

    /// Returns the tables which extend the current table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY);
    /// CREATE TABLE child_table (id INT PRIMARY KEY REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let extending_tables = parent_table.extending_tables(&db)?;
    /// assert_eq!(extending_tables.count(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn extending_tables<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;

        let mut extending_tables = Vec::new();
        for table in database.tables() {
            if table.is_descendant_of(database, self.borrow())? {
                extending_tables.push(table);
            }
        }

        Ok(extending_tables.into_iter())
    }

    /// Returns whether the current table is extended by any other table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY);
    /// CREATE TABLE child_table (id INT PRIMARY KEY REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(parent_table.is_extended(&db)?);
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!child_table.is_extended(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_extended(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.extending_tables(database)?.next().is_some())
    }

    /// Returns the first extension foreign key found in the current table which
    /// references to the provided table or any of its descendants.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `table` - A reference to the table to check for extensions.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY);
    /// CREATE TABLE father_table (id INT PRIMARY KEY REFERENCES grandparent_table(id));
    /// CREATE TABLE mother_table (id INT PRIMARY KEY REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (
    ///     id INT PRIMARY KEY,
    ///     FOREIGN KEY (id) REFERENCES father_table(id),
    ///     FOREIGN KEY (id) REFERENCES mother_table(id)
    /// );
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let father_table = db
    ///     .table_by_target(TargetName::new("father_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let mother_table = db
    ///     .table_by_target(TargetName::new("mother_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let extension_fks = child_table.extension_foreign_keys(&db)?.collect::<Vec<_>>();
    /// let [father_extension_fk, mother_extension_fk] = extension_fks.as_slice() else {
    ///     panic!("Expected two extension foreign keys");
    /// };
    /// let extension_fk_to_father = child_table.extension_foreign_key_to(&db, father_table)?;
    /// assert_eq!(extension_fk_to_father, Some(*father_extension_fk));
    /// let extension_fk_to_mother = child_table.extension_foreign_key_to(&db, mother_table)?;
    /// assert_eq!(extension_fk_to_mother, Some(*mother_extension_fk));
    /// let extension_fk_to_grandparent =
    ///     child_table.extension_foreign_key_to(&db, grandparent_table)?;
    /// assert_eq!(extension_fk_to_grandparent, Some(*father_extension_fk));
    /// assert!(child_table.extension_foreign_key_to(&db, child_table)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn extension_foreign_key_to<'db>(
        &'db self,
        database: &'db Self::DB,
        table: &'db <Self::DB as DatabaseLike>::Table,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        for foreign_key in self.extension_foreign_keys(database)? {
            let referenced_table: &<Self::DB as DatabaseLike>::Table =
                foreign_key.referenced_table(database)?;
            if referenced_table == table
                || referenced_table.extension_foreign_key_to(database, table)?.is_some()
            {
                return Ok(Some(foreign_key));
            }
        }

        Ok(None)
    }

    /// Returns the first extended table found in the current table which
    /// matches the provided table or is its descendants.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `table` - A reference to the table to check for extensions.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY);
    /// CREATE TABLE father_table (id INT PRIMARY KEY REFERENCES grandparent_table(id));
    /// CREATE TABLE mother_table (id INT PRIMARY KEY REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (
    ///     id INT PRIMARY KEY,
    ///     FOREIGN KEY (id) REFERENCES father_table(id),
    ///     FOREIGN KEY (id) REFERENCES mother_table(id)
    /// );
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let father_table = db
    ///     .table_by_target(TargetName::new("father_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let mother_table = db
    ///     .table_by_target(TargetName::new("mother_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let extended_table_to_father = child_table.extended_table_to(&db, father_table)?;
    /// assert_eq!(extended_table_to_father, Some(father_table));
    /// let extended_table_to_mother = child_table.extended_table_to(&db, mother_table)?;
    /// assert_eq!(extended_table_to_mother, Some(mother_table));
    /// let extended_table_to_grandparent = child_table.extended_table_to(&db, grandparent_table)?;
    /// assert_eq!(extended_table_to_grandparent, Some(father_table));
    /// assert!(child_table.extended_table_to(&db, child_table)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn extended_table_to<'db>(
        &'db self,
        database: &'db Self::DB,
        table: &'db <Self::DB as DatabaseLike>::Table,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        match self.extension_foreign_key_to(database, table)? {
            Some(foreign_key) => Ok(Some(foreign_key.referenced_table(database)?)),
            None => Ok(None),
        }
    }

    /// Returns the unique tables which are extended by either the current
    /// table or any of the tables it extends via foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let ancestral_tables = child_table.ancestral_extended_tables(&db)?;
    /// assert_eq!(ancestral_tables.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn ancestral_extended_tables<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let extension_tables =
            self.extended_tables(database)?.collect::<Vec<&<Self::DB as DatabaseLike>::Table>>();
        let mut ancestral_tables = extension_tables.clone();

        for table in extension_tables {
            let mut parent_ancestral_tables = table.ancestral_extended_tables(database)?;
            ancestral_tables.append(&mut parent_ancestral_tables);
        }

        ancestral_tables.sort_unstable();
        ancestral_tables.dedup();

        Ok(ancestral_tables)
    }

    /// Returns the unique tables which are extended by either the current
    /// table or any of the tables it extends via foreign keys, sorted by
    /// topological order.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let ancestral_tables = child_table.ancestral_extended_tables_topological(&db)?;
    /// assert_eq!(ancestral_tables, vec![grandparent_table, parent_table]);
    /// # Ok(())
    /// # }
    /// ```
    fn ancestral_extended_tables_topological<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Table>, Error>
    where
        Self: 'db,
    {
        let ancestral_extended_tables = self.ancestral_extended_tables(database)?;

        if ancestral_extended_tables.len() <= 1 {
            return Ok(ancestral_extended_tables);
        }

        let sorted_dag = database.table_dag()?;

        let mut ordered = Vec::with_capacity(ancestral_extended_tables.len());
        for table in ancestral_extended_tables {
            let position = sorted_dag
                .iter()
                .position(|candidate| *candidate == table)
                .ok_or_else(|| ObjectKind::Table.not_in_database(table.table_name()))?;
            ordered.push((position, table));
        }
        ordered.sort_unstable_by_key(|&(position, _)| position);

        Ok(ordered.into_iter().map(|(_, table)| table).collect())
    }

    /// Returns the tables referenced in foreign keys of the current table via
    /// the provided column.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `column` - A reference to the column in the current table.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let id_column =
    ///     host_table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let referenced_tables = host_table.referenced_tables_via_column(&db, id_column)?;
    /// assert_eq!(referenced_tables.len(), 1);
    /// let name_column = host_table
    ///     .column("name", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'name' should exist");
    /// let no_referenced_tables = host_table.referenced_tables_via_column(&db, name_column)?;
    /// assert_eq!(no_referenced_tables.len(), 0);
    /// # Ok(())
    /// # }
    /// ```
    fn referenced_tables_via_column<'db>(
        &'db self,
        database: &'db Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Result<Vec<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let mut referenced_tables = Vec::new();

        for foreign_key in self.foreign_keys(database)? {
            if foreign_key.host_columns(database)?.all(|col| col == column)
                && foreign_key.is_referenced_primary_key(database)?
            {
                referenced_tables.push(foreign_key.referenced_table(database)?);
            }
        }

        referenced_tables.sort_unstable();
        referenced_tables.dedup();

        Ok(referenced_tables)
    }

    /// Returns whether the table extends any other table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(child_table.is_extension(&db)?);
    /// assert!(!parent_table.is_extension(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn is_extension(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.extension_foreign_keys(database)?.next().is_some())
    }

    /// Returns whether the table is a descendant of another table, i.e., if it
    /// extends the other table either directly or some other table which
    /// extends the other table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `other` - The other table to check against.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(child_table.is_descendant_of(&db, parent_table)?);
    /// assert!(!parent_table.is_descendant_of(&db, child_table)?);
    /// # Ok(())
    /// # }
    /// ```
    fn is_descendant_of(
        &self,
        database: &Self::DB,
        other: &<Self::DB as DatabaseLike>::Table,
    ) -> Result<bool, LookupError> {
        Ok(self.ancestral_extended_tables(database)?.contains(&other))
    }

    /// Returns whether the table shares any ancestor with the given table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `other` - The other table to check against.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold. The same applies to
    /// `other`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// CREATE TABLE unrelated_table (id INT PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let unrelated_table = db
    ///     .table_by_target(TargetName::new("unrelated_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(
    ///     child_table.shares_ancestors_with(&db, parent_table)?,
    ///     "Child should share ancestors with parent"
    /// );
    /// assert!(
    ///     child_table.shares_ancestors_with(&db, grandparent_table)?,
    ///     "Child should share ancestors with grandparent"
    /// );
    /// assert!(
    ///     !child_table.shares_ancestors_with(&db, unrelated_table)?,
    ///     "Child should not share ancestors with unrelated"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn shares_ancestors_with(
        &self,
        database: &Self::DB,
        other: &<Self::DB as DatabaseLike>::Table,
    ) -> Result<bool, LookupError> {
        let self_ancestors = self.ancestral_extended_tables(database)?;
        let other_ancestors = other.ancestral_extended_tables(database)?;

        Ok(self_ancestors.iter().any(|table| other_ancestors.contains(table))
            || self.borrow() == other
            || self_ancestors.contains(&other)
            || other_ancestors.contains(&self.borrow()))
    }

    /// Returns the table singleton foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT UNIQUE, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let singleton_fks = host_table.singleton_foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(singleton_fks.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn singleton_foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        let mut foreign_keys = Vec::new();
        for foreign_key in self.foreign_keys(database)? {
            if foreign_key.is_singleton(database)? {
                foreign_keys.push(foreign_key);
            }
        }

        Ok(foreign_keys.into_iter())
    }

    /// Returns the table singleton foreign keys which are not self-referential.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (
    ///     id INT PRIMARY KEY,
    ///     name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id),
    ///     FOREIGN KEY (id) REFERENCES host_table(id)
    /// );
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let non_self_referential_singleton_fks =
    ///     host_table.non_self_referential_singleton_foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(non_self_referential_singleton_fks.len(), 1);
    /// assert!(non_self_referential_singleton_fks[0].is_singleton(&db)?);
    /// assert!(!non_self_referential_singleton_fks[0].is_self_referential(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn non_self_referential_singleton_foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        let mut foreign_keys = Vec::new();
        for foreign_key in self.singleton_foreign_keys(database)? {
            if !foreign_key.is_self_referential(database)? {
                foreign_keys.push(foreign_key);
            }
        }

        Ok(foreign_keys.into_iter())
    }

    /// Returns whether the table has singleton foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT UNIQUE, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(host_table.has_singleton_foreign_keys(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_singleton_foreign_keys(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.singleton_foreign_keys(database)?.next().is_some())
    }

    /// Returns whether the table has non-self-referential singleton foreign
    /// keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (
    ///     id INT PRIMARY KEY,
    ///     name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id),
    ///     FOREIGN KEY (id) REFERENCES host_table(id)
    /// );
    /// ",
    /// )?;
    /// let referenced_table = db
    ///     .table_by_target(TargetName::new("referenced_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!referenced_table.has_non_self_referential_singleton_foreign_keys(&db)?);
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(host_table.has_non_self_referential_singleton_foreign_keys(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_non_self_referential_singleton_foreign_keys(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(self.non_self_referential_singleton_foreign_keys(database)?.next().is_some())
    }

    /// Returns whether the table depends directly or indirectly on another
    /// table via foreign keys.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   and the other table belong.
    /// * `other` - The other table to check against.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(child_table.depends_on(&db, parent_table)?);
    /// assert!(child_table.depends_on(&db, grandparent_table)?);
    /// assert!(!parent_table.depends_on(&db, child_table)?);
    /// assert!(!grandparent_table.depends_on(&db, child_table)?);
    /// assert!(parent_table.depends_on(&db, grandparent_table)?);
    /// assert!(!grandparent_table.depends_on(&db, parent_table)?);
    /// assert!(child_table.depends_on(&db, child_table)?);
    /// assert!(parent_table.depends_on(&db, parent_table)?);
    /// assert!(grandparent_table.depends_on(&db, grandparent_table)?);
    /// # Ok(())
    /// # }
    /// ```
    fn depends_on(
        &self,
        database: &Self::DB,
        other: &<Self::DB as DatabaseLike>::Table,
    ) -> Result<bool, LookupError> {
        if self.borrow() == other {
            return Ok(true);
        }
        for foreign_key in self.foreign_keys(database)? {
            let referenced_table = foreign_key.referenced_table(database)?;
            if referenced_table == other
                || referenced_table != self.borrow()
                    && referenced_table.depends_on(database, other)?
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns whether the table contains a foreign key referring to
    /// the provided table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   and the other table belong.
    /// * `other` - The other table to check against.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT, ancestor_id INT REFERENCES grandparent_table(id));
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(child_table.refers_to(&db, parent_table)?);
    /// assert!(!child_table.refers_to(&db, grandparent_table)?);
    /// assert!(!parent_table.refers_to(&db, child_table)?);
    /// assert!(!grandparent_table.refers_to(&db, child_table)?);
    /// assert!(parent_table.refers_to(&db, grandparent_table)?);
    /// assert!(!grandparent_table.refers_to(&db, parent_table)?);
    /// assert!(!child_table.refers_to(&db, child_table)?);
    /// assert!(!parent_table.refers_to(&db, parent_table)?);
    /// assert!(grandparent_table.refers_to(&db, grandparent_table)?);
    /// # Ok(())
    /// # }
    /// ```
    fn refers_to(
        &self,
        database: &Self::DB,
        other: &<Self::DB as DatabaseLike>::Table,
    ) -> Result<bool, LookupError> {
        for foreign_key in self.foreign_keys(database)? {
            if foreign_key.referenced_table(database)? == other {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns an iterator over all tables that depend directly or indirectly
    /// via foreign keys (including extensions) on the current table, excluding
    /// itself.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let dependent_tables: Vec<&str> =
    ///     grandparent_table.dependent_tables(&db)?.map(|t| t.table_name()).collect();
    /// assert_eq!(dependent_tables, vec!["child_table", "parent_table"]);
    /// # Ok(())
    /// # }
    /// ```
    fn dependent_tables<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;

        let mut dependent_tables = Vec::new();
        for table in database.tables() {
            if table != self.borrow() && table.depends_on(database, self.borrow())? {
                dependent_tables.push(table);
            }
        }

        Ok(dependent_tables.into_iter())
    }

    /// Returns whether the table has any dependent tables.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE child_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// ",
    /// )?;
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(parent_table.has_dependent_tables(&db)?);
    /// assert!(!child_table.has_dependent_tables(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_dependent_tables(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.dependent_tables(database)?.next().is_some())
    }

    /// Returns the most recent common ancestor table between the current table
    /// and all of the provided tables, if any.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `others` - A slice of other tables to check against.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold. The same applies to each
    /// entry of `others`.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE grandparent_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE parent_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES grandparent_table(id));
    /// CREATE TABLE child_table1 (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// CREATE TABLE child_table2 (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES parent_table(id));
    /// CREATE TABLE unrelated_table (id INT PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    /// let child_table1 = db
    ///     .table_by_target(TargetName::new("child_table1", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let child_table2 = db
    ///     .table_by_target(TargetName::new("child_table2", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let parent_table = db
    ///     .table_by_target(TargetName::new("parent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let grandparent_table = db
    ///     .table_by_target(TargetName::new("grandparent_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let unrelated_table = db
    ///     .table_by_target(TargetName::new("unrelated_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert_eq!(child_table1.most_recent_common_ancestor(&db, &[child_table2])?, Some(parent_table));
    /// assert_eq!(child_table1.most_recent_common_ancestor(&db, &[parent_table])?, Some(parent_table));
    /// assert_eq!(
    ///     child_table1.most_recent_common_ancestor(&db, &[grandparent_table])?,
    ///     Some(grandparent_table)
    /// );
    /// assert_eq!(child_table1.most_recent_common_ancestor(&db, &[unrelated_table])?, None);
    /// assert_eq!(
    ///     child_table1.most_recent_common_ancestor(&db, &[child_table2, parent_table])?,
    ///     Some(parent_table)
    /// );
    /// assert_eq!(
    ///     child_table1.most_recent_common_ancestor(&db, &[child_table2, grandparent_table])?,
    ///     Some(grandparent_table)
    /// );
    /// assert_eq!(child_table1.most_recent_common_ancestor(&db, &[])?, Some(child_table1));
    /// # Ok(())
    /// # }
    /// ```
    fn most_recent_common_ancestor<'db>(
        &'db self,
        database: &'db Self::DB,
        others: &[&'db <Self::DB as DatabaseLike>::Table],
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let mut covers_every_other = true;
        for &other in others {
            if other != self.borrow() && !other.is_descendant_of(database, self.borrow())? {
                covers_every_other = false;
                break;
            }
        }
        if covers_every_other {
            return Ok(Some(self.borrow()));
        }

        for extended_table in self.extended_tables(database)? {
            if let Some(common_ancestor) =
                extended_table.most_recent_common_ancestor(database, others)?
            {
                return Ok(Some(common_ancestor));
            }
        }

        Ok(None)
    }

    /// Returns a sorted vector of the table's spouses.
    ///
    /// The spouses of a table `t` are the set of tables which are:
    /// 1. NOT ancestors of `t`.
    /// 2. NOT equal to `t`.
    /// 3. Ancestors of some table that descends from `t`.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table, and [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key reached from it names
    /// a table or a column `database` does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE root (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE spouse_table (
    ///     id INT PRIMARY KEY REFERENCES root(id),
    ///     name TEXT
    /// );
    /// CREATE TABLE my_table (
    ///     id INT PRIMARY KEY REFERENCES root(id),
    ///     name TEXT
    /// );
    /// CREATE TABLE child_table (
    ///     id INT PRIMARY KEY,
    ///     FOREIGN KEY (id) REFERENCES my_table(id),
    ///     FOREIGN KEY (id) REFERENCES spouse_table(id)
    /// );
    /// ",
    /// )?;
    /// let root =
    ///     db.table_by_target(TargetName::new("root", false), IdentifierCase::AsWritten)?.unwrap();
    /// let child_table = db
    ///     .table_by_target(TargetName::new("child_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let my_table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let spouse_table = db
    ///     .table_by_target(TargetName::new("spouse_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert_eq!(my_table.spouses(&db)?.next(), Some(spouse_table));
    /// assert_eq!(spouse_table.spouses(&db)?.next(), Some(my_table));
    /// assert!(root.spouses(&db)?.next().is_none());
    /// assert!(child_table.spouses(&db)?.next().is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn spouses<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        let descendants: Vec<&<Self::DB as DatabaseLike>::Table> =
            self.extending_tables(database)?.collect();

        let mut spouses = Vec::new();
        for candidate in database.tables() {
            if candidate == self.borrow()
                || descendants.contains(&candidate)
                || self.is_descendant_of(database, candidate)?
            {
                continue;
            }
            let mut shares_descendant = false;
            for descendant in &descendants {
                if descendant.is_descendant_of(database, candidate)? {
                    shares_descendant = true;
                    break;
                }
            }
            if shares_descendant {
                spouses.push(candidate);
            }
        }

        Ok(spouses.into_iter())
    }

    /// Returns whether the table's columns share a snake_case prefix.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (user_id INT, user_name TEXT, user_email TEXT);
    ///      CREATE TABLE no_snake_prefix_table (user_id INT, username TEXT);",
    /// )?;
    ///
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let no_snake_prefix_table = db
    ///     .table_by_target(
    ///         TargetName::new("no_snake_prefix_table", false),
    ///         IdentifierCase::AsWritten,
    ///     )?
    ///     .unwrap();
    ///
    /// assert!(table.has_common_column_name_snake_prefix(&db)?);
    /// assert!(!no_snake_prefix_table.has_common_column_name_snake_prefix(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_common_column_name_snake_prefix(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(self.common_column_name_snake_prefix(database)?.is_some())
    }

    /// Returns the shared snake_case prefix across the table's columns.
    ///
    /// The returned prefix ends at the last `_` boundary within the common
    /// prefix. If no `_` boundary exists in the common prefix, returns
    /// `None`.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (user_id INT, user_name TEXT, user_email TEXT);
    ///      CREATE TABLE other_table (id INT, name TEXT);
    ///      CREATE TABLE another_table (user_id INT, username TEXT, email TEXT);",
    /// )?;
    ///
    /// let my_table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let other_table = db
    ///     .table_by_target(TargetName::new("other_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let another_table = db
    ///     .table_by_target(TargetName::new("another_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    ///
    /// assert_eq!(my_table.common_column_name_snake_prefix(&db)?, Some("user_"));
    /// assert_eq!(other_table.common_column_name_snake_prefix(&db)?, None);
    /// assert_eq!(another_table.common_column_name_snake_prefix(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn common_column_name_snake_prefix<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(crate::utils::common_column_name_snake_prefix(
            self.columns(database)?.map(ColumnLike::column_name),
        ))
    }

    /// Returns whether the table's columns share a snake_case suffix.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (user_id INT, group_id INT, team_id INT);
    ///      CREATE TABLE other_table (userid INT, group_id INT, id_team INT);",
    /// )?;
    ///
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let other_table = db
    ///     .table_by_target(TargetName::new("other_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    ///
    /// assert!(table.has_common_column_name_snake_suffix(&db)?);
    /// assert!(!other_table.has_common_column_name_snake_suffix(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_common_column_name_snake_suffix(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(self.common_column_name_snake_suffix(database)?.is_some())
    }

    /// Returns the shared snake_case suffix across the table's columns.
    ///
    /// The returned suffix starts at the first `_` boundary within the common
    /// suffix. If no `_` boundary exists in the common suffix, returns
    /// `None`.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (user_id INT, group_id INT, team_id INT);
    ///      CREATE TABLE other_table (user_id INT, groupid INT, teamid INT);
    ///      CREATE TABLE another_table (id INT, name TEXT);",
    /// )?;
    ///
    /// let my_table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let other_table = db
    ///     .table_by_target(TargetName::new("other_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let another_table = db
    ///     .table_by_target(TargetName::new("another_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    ///
    /// assert_eq!(my_table.common_column_name_snake_suffix(&db)?, Some("_id"));
    /// assert_eq!(other_table.common_column_name_snake_suffix(&db)?, None);
    /// assert_eq!(another_table.common_column_name_snake_suffix(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn common_column_name_snake_suffix<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(crate::utils::common_column_name_snake_suffix(
            self.columns(database)?.map(ColumnLike::column_name),
        ))
    }
    /// Returns whether the table has Row Level Security (RLS) enabled.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// ALTER TABLE my_table ENABLE ROW LEVEL SECURITY;
    /// CREATE TABLE my_other_table (id INT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert!(table.has_row_level_security(&db)?);
    /// let other_table = db
    ///     .table_by_target(TargetName::new("my_other_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!other_table.has_row_level_security(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_row_level_security(&self, database: &Self::DB) -> Result<bool, LookupError>;

    /// Returns whether the table has forced Row Level Security (RLS).
    ///
    /// When RLS is forced, the security policies apply even to the table owner,
    /// who would normally bypass RLS policies. This is useful when the table
    /// owner should also be subject to the same row-level restrictions as
    /// other users.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE forced_table (id INT);
    /// ALTER TABLE forced_table ENABLE ROW LEVEL SECURITY;
    /// ALTER TABLE forced_table FORCE ROW LEVEL SECURITY;
    /// CREATE TABLE normal_rls_table (id INT);
    /// ALTER TABLE normal_rls_table ENABLE ROW LEVEL SECURITY;
    /// CREATE TABLE no_rls_table (id INT);
    /// ",
    /// )?;
    /// let forced = db
    ///     .table_by_target(TargetName::new("forced_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(forced.has_row_level_security(&db)?);
    /// assert!(forced.has_forced_row_level_security(&db)?);
    ///
    /// let normal_rls = db
    ///     .table_by_target(TargetName::new("normal_rls_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(normal_rls.has_row_level_security(&db)?);
    /// assert!(!normal_rls.has_forced_row_level_security(&db)?);
    ///
    /// let no_rls = db
    ///     .table_by_target(TargetName::new("no_rls_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(!no_rls.has_row_level_security(&db)?);
    /// assert!(!no_rls.has_forced_row_level_security(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn has_forced_row_level_security(&self, database: &Self::DB) -> Result<bool, LookupError>;

    /// Returns the role the input names as the table's owner.
    ///
    /// A table owner bypasses every policy on the table unless the table also
    /// has forced Row Level Security, so a caller reporting that exemption
    /// needs the role's name to say who is exempt.
    ///
    /// Only `ALTER TABLE ... OWNER TO <role>` names an owner. A table no such
    /// statement altered has none, and neither has one handed to
    /// `CURRENT_ROLE`, `CURRENT_USER` or `SESSION_USER`, which name whoever
    /// runs the statement rather than a role the input declares.
    ///
    /// The role is reported as the statement spelled it, with no case folding,
    /// which is how [`DatabaseLike::role`] stores the names it matches against.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE app_owner;
    /// CREATE TABLE docs (id INT);
    /// ALTER TABLE docs OWNER TO app_owner;
    /// CREATE TABLE notes (id INT);
    /// ",
    /// )?;
    /// let docs =
    ///     db.table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(docs.owner(&db)?, Some("app_owner"));
    ///
    /// let notes =
    ///     db.table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(notes.owner(&db)?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>;

    /// Iterates over the policies associated with the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the
    ///   policies from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::CreatePolicyCommand;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE POLICY select_policy ON my_table FOR SELECT USING (id > 0);
    /// CREATE POLICY insert_policy ON my_table FOR INSERT WITH CHECK (id > 0);
    /// CREATE POLICY all_policy ON my_table TO public USING (true);
    /// CREATE TABLE other_table (id INT);
    /// CREATE POLICY other_policy ON other_table USING (true);
    /// ",
    /// )?;
    ///
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let mut policies: Vec<_> = table.policies(&db)?.collect();
    /// policies.sort_by(|a, b| a.name().cmp(b.name()));
    ///
    /// assert_eq!(policies.len(), 3);
    /// assert_eq!(policies[0].name(), "all_policy");
    /// assert_eq!(policies[1].name(), "insert_policy");
    /// assert_eq!(policies[2].name(), "select_policy");
    ///
    /// assert_eq!(policies[0].command(), CreatePolicyCommand::All);
    /// assert_eq!(policies[1].command(), CreatePolicyCommand::Insert);
    /// assert_eq!(policies[2].command(), CreatePolicyCommand::Select);
    ///
    /// let other_table = db
    ///     .table_by_target(TargetName::new("other_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let other_policies: Vec<_> = other_table.policies(&db)?.collect();
    /// assert_eq!(other_policies.len(), 1);
    /// assert_eq!(other_policies[0].name(), "other_policy");
    /// # Ok(())
    /// # }
    /// ```
    fn policies<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Policy>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;

        Ok(database.policies().filter(move |policy| {
            policy.table(database).is_ok_and(|t| t.borrow() == self.borrow())
        }))
    }

    /// Returns an iterator over the grants that apply to this table.
    ///
    /// This includes both direct table grants (`GRANT ... ON table_name`)
    /// and schema-wide grants (`GRANT ... ON ALL TABLES IN SCHEMA`).
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE TABLE other_table (id INT);
    /// CREATE ROLE reader;
    /// CREATE ROLE writer;
    /// CREATE ROLE admin;
    /// GRANT SELECT ON my_table TO reader;
    /// GRANT INSERT ON my_table TO writer;
    /// GRANT DELETE ON other_table TO admin;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let grants: Vec<_> = table.grants(&db)?.collect();
    /// assert_eq!(grants.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn grants<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::TableGrant>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;

        Ok(database
            .table_grants()
            .filter(move |grant| grant.applies_to_table(self.borrow(), database)))
    }

    /// Returns whether the given role can read (SELECT) from this table.
    ///
    /// A role can read if there's a grant that:
    /// - Applies to this table (directly or via ALL TABLES IN SCHEMA)
    /// - Applies to this role as a grantee
    /// - Includes SELECT privilege or ALL PRIVILEGES
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE reader;
    /// CREATE ROLE writer;
    /// GRANT SELECT ON my_table TO reader;
    /// GRANT INSERT ON my_table TO writer;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let reader = db.role("reader").unwrap();
    /// let writer = db.role("writer").unwrap();
    ///
    /// assert!(table.can_select(reader, &db)?);
    /// assert!(!table.can_select(writer, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn can_select(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        use sqlparser::ast::Action;
        Ok(self.grants(database)?.any(|grant| {
            grant.applies_to_role(role)
                && (grant.is_all_privileges()
                    || grant.privileges(database).any(|p| matches!(p, Action::Select { .. })))
        }))
    }

    /// Returns whether the given role can insert into this table.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE writer;
    /// GRANT INSERT ON my_table TO writer;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let writer = db.role("writer").unwrap();
    ///
    /// assert!(table.can_insert(writer, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn can_insert(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        use sqlparser::ast::Action;
        Ok(self.grants(database)?.any(|grant| {
            grant.applies_to_role(role)
                && (grant.is_all_privileges()
                    || grant.privileges(database).any(|p| matches!(p, Action::Insert { .. })))
        }))
    }

    /// Returns whether the given role can update this table.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE updater;
    /// GRANT UPDATE ON my_table TO updater;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let updater = db.role("updater").unwrap();
    ///
    /// assert!(table.can_update(updater, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn can_update(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        use sqlparser::ast::Action;
        Ok(self.grants(database)?.any(|grant| {
            grant.applies_to_role(role)
                && (grant.is_all_privileges()
                    || grant.privileges(database).any(|p| matches!(p, Action::Update { .. })))
        }))
    }

    /// Returns whether the given role can delete from this table.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE deleter;
    /// GRANT DELETE ON my_table TO deleter;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let deleter = db.role("deleter").unwrap();
    ///
    /// assert!(table.can_delete(deleter, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn can_delete(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        use sqlparser::ast::Action;
        Ok(self.grants(database)?.any(|grant| {
            grant.applies_to_role(role)
                && (grant.is_all_privileges()
                    || grant.privileges(database).any(|p| matches!(p, Action::Delete)))
        }))
    }

    /// Returns whether the given role can write to this table (INSERT, UPDATE,
    /// or DELETE).
    ///
    /// This is a convenience method that checks if the role has any write
    /// permission.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE reader;
    /// CREATE ROLE writer;
    /// GRANT SELECT ON my_table TO reader;
    /// GRANT INSERT ON my_table TO writer;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let reader = db.role("reader").unwrap();
    /// let writer = db.role("writer").unwrap();
    ///
    /// assert!(!table.can_write(reader, &db)?);
    /// assert!(table.can_write(writer, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn can_write(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(self.can_insert(role, database)?
            || self.can_update(role, database)?
            || self.can_delete(role, database)?)
    }

    /// Returns whether the given role can truncate this table.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE my_table (id INT);
    /// CREATE ROLE truncator;
    /// GRANT TRUNCATE ON my_table TO truncator;
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let truncator = db.role("truncator").unwrap();
    ///
    /// assert!(table.can_truncate(truncator, &db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn can_truncate(
        &self,
        role: &<Self::DB as DatabaseLike>::Role,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        use sqlparser::ast::Action;
        Ok(self.grants(database)?.any(|grant| {
            grant.applies_to_role(role)
                && (grant.is_all_privileges()
                    || grant.privileges(database).any(|p| matches!(p, Action::Truncate)))
        }))
    }
}

impl<T: TableLike> TableLike for &T
where
    Self: Borrow<<<T as TableLike>::DB as DatabaseLike>::Table>,
{
    type DB = T::DB;

    fn table_name(&self) -> &str {
        T::table_name(self)
    }

    fn table_name_is_quoted(&self) -> bool {
        T::table_name_is_quoted(self)
    }

    fn table_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        T::table_doc(self, database)
    }

    fn table_schema(&self) -> Option<&str> {
        T::table_schema(self)
    }

    fn table_schema_is_quoted(&self) -> bool {
        T::table_schema_is_quoted(self)
    }

    fn table_id(&self, database: &Self::DB) -> Option<usize> {
        T::table_id(self, database)
    }

    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        T::columns(self, database)
    }

    fn local_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        T::local_columns(self, database)
    }

    fn inherits_from<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        T::inherits_from(self, database)
    }

    fn inheritors<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        T::inheritors(self, database)
    }

    fn partition_root<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        T::partition_root(self, database)
    }

    fn partition_strategy(&self) -> Option<PartitionStrategy> {
        T::partition_strategy(self)
    }

    fn partitions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        T::partitions(self, database)
    }

    fn is_partition(&self, database: &Self::DB) -> Result<bool, LookupError> {
        T::is_partition(self, database)
    }

    fn is_partitioned(&self) -> bool {
        T::is_partitioned(self)
    }

    fn column_by_id<'db>(
        &'db self,
        column_id: usize,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        T::column_by_id(self, column_id, database)
    }

    fn column_id_by_name(
        &self,
        name: &str,
        database: &Self::DB,
        case: IdentifierCase,
    ) -> Result<Option<usize>, LookupError> {
        T::column_id_by_name(self, name, database, case)
    }

    fn column_name_by_id<'db>(
        &'db self,
        column_id: usize,
        database: &'db Self::DB,
    ) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        T::column_name_by_id(self, column_id, database)
    }

    fn has_row_level_security(&self, database: &Self::DB) -> Result<bool, LookupError> {
        T::has_row_level_security(self, database)
    }

    fn has_forced_row_level_security(&self, database: &Self::DB) -> Result<bool, LookupError> {
        T::has_forced_row_level_security(self, database)
    }

    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        T::owner(self, database)
    }

    fn primary_key_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        T::primary_key_columns(self, database)
    }

    fn primary_key_column_ids(&self, database: &Self::DB) -> Result<Vec<usize>, LookupError> {
        T::primary_key_column_ids(self, database)
    }

    fn check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint>, LookupError>
    where
        Self: 'db,
    {
        T::check_constraints(self, database)
    }

    fn unique_indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::UniqueIndex>, LookupError>
    where
        Self: 'db,
    {
        T::unique_indices(self, database)
    }

    fn indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Index>, LookupError>
    where
        Self: 'db,
    {
        T::indices(self, database)
    }

    fn foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        T::foreign_keys(self, database)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::GenericDialect;

    use super::*;
    use crate::{
        prelude::*,
        structs::{IdentifierCase, TargetName},
    };

    mod identifier_resolution {
        use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

        use super::*;

        fn parse_postgres(sql: &str) -> Result<ParserDB, crate::errors::Error> {
            let dialect = PostgreSqlDialect {};
            let statements = Parser::parse_sql(&dialect, sql)?;
            ParserDB::from_statements(statements, "test".to_string())
        }

        #[test]
        fn test_table_lookup_unquoted_identifier_is_case_insensitive() {
            let db = parse_postgres("CREATE TABLE Foo (id INT);").expect("Failed to parse SQL");

            assert!(
                db.table_by_target(TargetName::new("foo", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("FOO", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("foo", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("Foo", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_table_lookup_quoted_identifier_is_case_sensitive() {
            let db = parse_postgres("CREATE TABLE \"Foo\" (id INT);").expect("Failed to parse SQL");

            assert!(
                db.table_by_target(TargetName::new("Foo", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("foo", true), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("foo", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("FOO", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_column_lookup_respects_quoted_and_unquoted_rules() {
            let db = parse_postgres(
                "
                CREATE TABLE t (
                    Foo INT,
                    \"ColA\" INT
                );
                ",
            )
            .expect("Failed to parse SQL");
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table 't' should exist");

            // Unquoted column created as Foo resolves as lowercase identifier.
            assert!(
                table
                    .column("foo", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_some()
            );
            assert!(
                table
                    .column("FOO", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_some()
            );
            assert!(
                table
                    .column("\"foo\"", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_some()
            );
            assert!(
                table
                    .column("\"Foo\"", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_none()
            );

            // Quoted column keeps exact case.
            assert!(
                table
                    .column("\"ColA\"", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_some()
            );
            assert!(
                table
                    .column("\"cola\"", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_none()
            );
            assert!(
                table
                    .column("cola", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_none()
            );
            assert!(
                table
                    .column("COLA", &db, IdentifierCase::AsWritten)
                    .expect("column lookup")
                    .is_none()
            );
        }
    }

    mod reference_impl {
        use super::*;

        #[test]
        fn test_all_methods() {
            let sql = "
                CREATE TABLE users (
                    id INT PRIMARY KEY,
                    name TEXT CHECK (length(name) > 0),
                    UNIQUE(name)
                );
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
            let table = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table not found");

            let table_ref = &table;

            assert_eq!(<&_ as TableLike>::table_name(table_ref), table.table_name());
            assert_eq!(<&_ as TableLike>::table_doc(table_ref, &db), table.table_doc(&db));
            assert_eq!(<&_ as TableLike>::table_schema(table_ref), table.table_schema());
            assert_eq!(<&_ as TableLike>::table_id(table_ref, &db), table.table_id(&db));

            assert_eq!(
                <&_ as TableLike>::columns(table_ref, &db).expect("columns on &T").count(),
                table.columns(&db).expect("columns").count()
            );
            assert_eq!(
                <&_ as TableLike>::column_by_id(table_ref, 0, &db)
                    .expect("column_by_id on &T")
                    .map(ColumnLike::column_name),
                table.column_by_id(0, &db).expect("column_by_id").map(ColumnLike::column_name)
            );
            assert_eq!(
                <&_ as TableLike>::primary_key_columns(table_ref, &db)
                    .expect("pk_columns on &T")
                    .count(),
                table.primary_key_columns(&db).expect("pk_columns").count()
            );
            assert_eq!(
                <&_ as TableLike>::check_constraints(table_ref, &db)
                    .expect("check_constraints on &T")
                    .count(),
                table.check_constraints(&db).expect("check_constraints").count()
            );
            assert_eq!(
                <&_ as TableLike>::unique_indices(table_ref, &db)
                    .expect("unique_indices on &T")
                    .count(),
                table.unique_indices(&db).expect("unique_indices").count()
            );
            assert_eq!(
                <&_ as TableLike>::foreign_keys(table_ref, &db)
                    .expect("foreign_keys on &T")
                    .count(),
                table.foreign_keys(&db).expect("foreign_keys").count()
            );
            assert_eq!(
                <&_ as TableLike>::schema_fingerprint(table_ref, &db),
                table.schema_fingerprint(&db)
            );
        }

        #[test]
        fn test_dependent_tables() {
            let sql = "
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES parent(id));
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
            let parent = db
                .table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Parent not found");
            let child = db
                .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Child not found");

            let parent_ref = &parent;

            // calling dependent_tables on &parent
            // parent has 'child' depending on it
            let deps: Vec<_> = <&_ as TableLike>::dependent_tables(parent_ref, &db)
                .expect("dependent_tables on &T")
                .map(TableLike::table_name)
                .collect();
            assert!(deps.contains(&"child"));
            assert!(!deps.contains(&"parent"));

            // calling on &child - nothing depends on child
            let child_ref = &child;
            let child_deps: Vec<_> = <&_ as TableLike>::dependent_tables(child_ref, &db)
                .expect("dependent_tables on &T")
                .map(TableLike::table_name)
                .collect();
            assert_eq!(child_deps, Vec::<&str>::new());
        }

        #[test]
        fn test_table_id_matches_global_table_ordering() {
            let sql = "
                CREATE SCHEMA z_schema;
                CREATE SCHEMA a_schema;
                CREATE TABLE z_schema.table_z (id INT PRIMARY KEY);
                CREATE TABLE table_without_schema (id INT PRIMARY KEY);
                CREATE TABLE a_schema.table_a (id INT PRIMARY KEY);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            for (expected_id, table) in db.tables().enumerate() {
                assert_eq!(table.table_id(&db), Some(expected_id));
                assert_eq!(table.table_id(&db), db.table_id(table));
                assert_eq!(db.table_by_id(expected_id), Some(table));
            }

            assert_eq!(db.table_by_id(db.number_of_tables()), None);
        }

        #[test]
        fn test_column_id_matches_table_column_ordering() {
            let sql = "
                CREATE TABLE users (
                    id INT PRIMARY KEY,
                    name TEXT,
                    age INT
                );
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
            let table = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table not found");

            for (expected_id, column) in table.columns(&db).expect("columns").enumerate() {
                assert_eq!(column.column_id(&db).expect("column_id"), Some(expected_id));
                assert_eq!(
                    table.column_by_id(expected_id, &db).expect("column_by_id"),
                    Some(column)
                );
            }

            assert_eq!(
                table
                    .column_by_id(table.number_of_columns(&db).expect("number_of_columns"), &db)
                    .expect("column_by_id"),
                None
            );
        }
    }

    mod schema_fingerprint {
        use super::*;

        #[test]
        fn test_determinism() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db_a = ParserDB::parse::<GenericDialect>(sql).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql).expect("parse B");

            let table_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();
            let table_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();

            assert_eq!(table_a.schema_fingerprint(&db_a), table_b.schema_fingerprint(&db_b));
        }

        #[test]
        fn test_sensitivity_column_order() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let sql_b = "CREATE TABLE users (name TEXT, id INT PRIMARY KEY);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b);
        }

        #[test]
        fn test_sensitivity_column_type() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, score INT);";
            let sql_b = "CREATE TABLE users (id INT PRIMARY KEY, score TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b);
        }

        #[test]
        fn test_sensitivity_pk_membership() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, ext_id INT, name TEXT);";
            let sql_b = "CREATE TABLE users (id INT, ext_id INT PRIMARY KEY, name TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b);
        }

        #[test]
        fn test_sensitivity_table_name() {
            let sql = "
                CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
                CREATE TABLE users_archive (id INT PRIMARY KEY, name TEXT);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");

            let fp_users = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");
            let fp_archive = db
                .table_by_target(TargetName::new("users_archive", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            assert_ne!(fp_users, fp_archive);
        }

        #[test]
        fn test_sensitivity_nullability() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL);";
            let sql_b = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b);
        }

        #[test]
        fn test_truncation_fingerprint128() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            let full = fp.fingerprint256();
            let truncated = fp.fingerprint128();
            assert_eq!(&full[..16], &truncated);
        }

        #[test]
        fn test_truncation_fingerprint64() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            let full = fp.fingerprint256();
            let expected = u64::from_be_bytes(full[..8].try_into().unwrap());
            assert_eq!(fp.fingerprint64(), expected);
        }

        #[test]
        fn test_comparability() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");

            let fp_a = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");
            let fp_b = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            assert!(fp_a.is_comparable_to(&fp_b));
        }

        #[test]
        fn test_hex_length() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            let hex = fp.to_hex();
            assert_eq!(hex.len(), 64);
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn test_version() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            assert_eq!(fp.canonicalization_version(), 1);
        }

        #[test]
        fn test_golden_vector() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            // Pin the hex digest so any encoding change is caught.
            let hex = fp.to_hex();
            assert_eq!(
                hex, "961dddeb22561e74e0e58c55b43afd875c5eb6b1b030facb48ec1237acf6f9d3",
                "Golden vector mismatch — the fingerprint encoding has changed!"
            );
        }

        #[test]
        fn test_display_format() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            let display = format!("{fp}");
            // Format: "<algorithm>:v<canonicalization_version>:p<profile_id>:
            // <hex>"
            assert!(display.starts_with("sha2-256:v1:p1:"));
            assert!(display.ends_with(&fp.to_hex()));
            // "sha2-256" (8) + ":" + "v1" (2) + ":" + "p1" (2) + ":" + 64 hex
            // chars
            assert_eq!(display.len(), 8 + 1 + 2 + 1 + 2 + 1 + 64);
        }

        #[test]
        fn test_sensitivity_schema_name() {
            let sql = "
                CREATE SCHEMA schema_a;
                CREATE SCHEMA schema_b;
                CREATE TABLE schema_a.users (id INT PRIMARY KEY, name TEXT);
                CREATE TABLE schema_b.users (id INT PRIMARY KEY, name TEXT);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");

            let fp_a = db
                .table_by_target(
                    TargetName::new("users", false).with_schema("schema_a", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");
            let fp_b = db
                .table_by_target(
                    TargetName::new("users", false).with_schema("schema_b", false),
                    IdentifierCase::AsWritten,
                )
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b);
        }

        #[test]
        fn test_type_canonicalization() {
            // VARCHAR and TEXT both map to STRING, so these tables should
            // produce the same fingerprint.
            let sql_a = "CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR);";
            let sql_b = "CREATE TABLE t (id INT PRIMARY KEY, val TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_eq!(fp_a, fp_b);
        }

        #[test]
        fn test_no_primary_key() {
            let sql = "CREATE TABLE t (id INT, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            // Should produce a valid fingerprint with 64 hex chars.
            assert_eq!(fp.to_hex().len(), 64);
            assert_eq!(fp.canonicalization_version(), 1);
        }

        #[test]
        fn test_eq_hash_contract() {
            use std::hash::{DefaultHasher, Hash, Hasher};

            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");

            let fp_a = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");
            let fp_b = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            assert_eq!(fp_a, fp_b);

            let hash_of = |fp: &crate::structs::SchemaFingerprint| {
                let mut h = DefaultHasher::new();
                fp.hash(&mut h);
                h.finish()
            };

            assert_eq!(hash_of(&fp_a), hash_of(&fp_b));
        }

        // ---------------------------------------------------------------
        // Spec §10.1 envelope-layout tests (audit §3, P-04 / P-05).
        //
        // These tests pin the canonical byte layout of the v1 envelope so
        // that any future encoding drift is caught before SHA-256 hides it.
        // ---------------------------------------------------------------

        /// Inline-construct the canonical bytes for `users(id INT PRIMARY KEY,
        /// name TEXT)` per FINGERPRINT_SPEC §10.1 and assert that
        /// SHA-256 of those bytes equals `schema_fingerprint(...).
        /// fingerprint256()`.
        ///
        /// Envelope:
        ///   bytes[4]  magic = "SFP1"
        ///   u16       canonicalization_version (big-endian)
        ///   u16       profile_id               (big-endian)
        ///   str       schema_name_norm         (u32 BE length, then bytes)
        ///   str       table_name_norm
        ///   u32       column_count
        ///   repeat:
        ///     u32   ordinal
        ///     str   column_name_norm
        ///     str   canonical_type_token
        ///     u8    nullable_flag
        ///     u8    generated_flag
        ///   u32       pk_count
        ///   repeat:
        ///     u32   pk_ordinal
        #[test]
        fn test_envelope_layout_spec_10_1() {
            use sha2::{Digest, Sha256};

            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(b"SFP1");
            buf.extend_from_slice(&1u16.to_be_bytes()); // canonicalization_version
            buf.extend_from_slice(&1u16.to_be_bytes()); // profile_id

            // schema_name_norm = "" (no schema qualifier)
            buf.extend_from_slice(&0u32.to_be_bytes());

            // table_name_norm = "users"
            buf.extend_from_slice(&5u32.to_be_bytes());
            buf.extend_from_slice(b"users");

            // column_count = 2
            buf.extend_from_slice(&2u32.to_be_bytes());

            // col 0: id INT PRIMARY KEY (nullable=0, generated=0)
            buf.extend_from_slice(&0u32.to_be_bytes());
            buf.extend_from_slice(&2u32.to_be_bytes());
            buf.extend_from_slice(b"id");
            buf.extend_from_slice(&3u32.to_be_bytes());
            buf.extend_from_slice(b"INT");
            buf.push(0); // nullable
            buf.push(0); // generated

            // col 1: name TEXT (nullable=1, generated=0)
            buf.extend_from_slice(&1u32.to_be_bytes());
            buf.extend_from_slice(&4u32.to_be_bytes());
            buf.extend_from_slice(b"name");
            buf.extend_from_slice(&6u32.to_be_bytes());
            buf.extend_from_slice(b"STRING");
            buf.push(1); // nullable
            buf.push(0); // generated

            // pk_count = 1, pk_ordinal = 0
            buf.extend_from_slice(&1u32.to_be_bytes());
            buf.extend_from_slice(&0u32.to_be_bytes());

            let expected: [u8; 32] = Sha256::digest(&buf).into();
            assert_eq!(
                fp.fingerprint256(),
                expected,
                "fingerprint encoding diverges from FINGERPRINT_SPEC §10.1"
            );
        }

        /// `is_generated` participates in the envelope: SERIAL and plain INT
        /// produce the same canonical type token (`INT`) and the same
        /// nullability (NOT NULL — both are primary keys), so the only field
        /// distinguishing them is `generated_flag`. Their fingerprints MUST
        /// therefore differ. (Audit §3.3.)
        #[test]
        fn test_sensitivity_generated_flag() {
            let sql_serial = "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT);";
            let sql_plain = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";

            let db_serial = ParserDB::parse::<GenericDialect>(sql_serial).expect("parse serial");
            let db_plain = ParserDB::parse::<GenericDialect>(sql_plain).expect("parse plain");

            let fp_serial = db_serial
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_serial)
                .expect("fingerprint");
            let fp_plain = db_plain
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_plain)
                .expect("fingerprint");

            assert_ne!(fp_serial, fp_plain, "generated/identity flag must affect the fingerprint");
        }

        // ---------------------------------------------------------------
        // Spec §12 envelope-aware comparability tests (audit §7, P-13).
        //
        // Two fingerprints are comparable only if their (algorithm_id,
        // canonicalization_version, profile_id) triple matches. Equality
        // requires comparability AND digest match — comparing across
        // envelopes is a category error.
        // ---------------------------------------------------------------

        /// MIG-001: identical digest under different `canonicalization_version`
        /// values is not comparable and not equal.
        #[test]
        fn test_mig_001_canonicalization_version_mismatch() {
            use crate::structs::{SchemaFingerprint, fingerprint::AlgorithmId};

            let digest = [0u8; 32];
            let fp_v1 = SchemaFingerprint::new(AlgorithmId::Sha2_256, 1, 1, digest);
            let fp_v2 = SchemaFingerprint::new(AlgorithmId::Sha2_256, 2, 1, digest);

            assert!(
                !fp_v1.is_comparable_to(&fp_v2),
                "different canonicalization_version must not be comparable",
            );
            assert_ne!(
                fp_v1, fp_v2,
                "PartialEq must require comparability — cross-version equality is a category error",
            );
        }

        /// MIG-002: identical digest under different `profile_id` values is
        /// not comparable and not equal.
        #[test]
        fn test_mig_002_profile_id_mismatch() {
            use crate::structs::{SchemaFingerprint, fingerprint::AlgorithmId};

            let digest = [0u8; 32];
            let fp_p1 = SchemaFingerprint::new(AlgorithmId::Sha2_256, 1, 1, digest);
            let fp_p2 = SchemaFingerprint::new(AlgorithmId::Sha2_256, 1, 2, digest);

            assert!(!fp_p1.is_comparable_to(&fp_p2), "different profile_id must not be comparable");
            assert_ne!(
                fp_p1, fp_p2,
                "PartialEq must require comparability — cross-profile equality is a category error",
            );
        }

        /// MIG-003: identical digest under different `algorithm_id` values is
        /// not comparable and not equal.
        #[test]
        fn test_mig_003_algorithm_id_mismatch() {
            use crate::structs::{SchemaFingerprint, fingerprint::AlgorithmId};

            let digest = [0u8; 32];
            let fp_sha2 = SchemaFingerprint::new(AlgorithmId::Sha2_256, 1, 1, digest);
            let fp_sha3 = SchemaFingerprint::new(AlgorithmId::Sha3_256, 1, 1, digest);

            assert!(
                !fp_sha2.is_comparable_to(&fp_sha3),
                "different algorithm_id must not be comparable",
            );
            assert_ne!(
                fp_sha2, fp_sha3,
                "PartialEq must require comparability — cross-algorithm equality is a category error",
            );
        }

        /// Envelope accessors expose the metadata used for comparability.
        #[test]
        fn test_envelope_accessors() {
            use crate::structs::fingerprint::AlgorithmId;

            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let fp = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db)
                .expect("fingerprint");

            assert_eq!(fp.algorithm_id(), AlgorithmId::Sha2_256);
            assert_eq!(fp.canonicalization_version(), 1);
            assert_eq!(fp.profile_id(), 1);
        }

        /// `TableLike::schema_fingerprint` must propagate the validator's
        /// errors (audit §4, P-12). This test pins the API shape: the
        /// return type is `Result<SchemaFingerprint, FingerprintError>`.
        /// For a well-formed table it must return `Ok`.
        #[test]
        fn test_schema_fingerprint_is_fallible() {
            use crate::structs::{SchemaFingerprint, fingerprint::FingerprintError};

            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("parse");
            let table = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();

            let fp_result: Result<SchemaFingerprint, FingerprintError> =
                table.schema_fingerprint(&db);
            assert!(fp_result.is_ok(), "well-formed table must produce a fingerprint");
        }

        /// SEN-007 (audit §8): composite-PK column order matters. Two
        /// tables with identical columns but PK declared as `(a, b)` vs
        /// `(b, a)` produce PK-ordinal lists `[0, 1]` vs `[1, 0]` and so
        /// must yield distinct fingerprints. This regression-guards any
        /// future "sort PK ordinals for stability" refactor that would
        /// silently collapse the two.
        #[test]
        fn test_sen_007_pk_order_sensitivity() {
            let sql_ab = "CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b));";
            let sql_ba = "CREATE TABLE t (a INT, b INT, PRIMARY KEY (b, a));";

            let db_ab = ParserDB::parse::<GenericDialect>(sql_ab).expect("parse ab");
            let db_ba = ParserDB::parse::<GenericDialect>(sql_ba).expect("parse ba");

            let fp_ab = db_ab
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_ab)
                .expect("fingerprint");
            let fp_ba = db_ba
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_ba)
                .expect("fingerprint");

            assert_ne!(fp_ab, fp_ba, "composite-PK declaration order must affect the fingerprint");
        }

        // ---------------------------------------------------------------
        // Audit §10 conformance gap fill (Step 7).
        //
        // These tests close the §13 conformance matrix entries listed
        // in the audit as "No" or "Partial" and that don't require
        // a CI-matrix harness or a second `DatabaseLike` impl. With the
        // earlier steps in place they all pass against the current
        // encoding; their role is regression coverage.
        //
        // Out-of-scope here (require external infrastructure — see
        // TODO block at the end of this module):
        //   • DET-002 (cross-process)
        //   • DET-003 (cross-platform)
        //   • DET-004 (cross-toolchain)
        //   • DIFF-001 (parser vs DB-introspection parity)
        //   • COL-001 (1 M random corpus collision smoke)
        // ---------------------------------------------------------------

        /// INV-003: source-statement order does not affect any individual
        /// table's fingerprint. Reordering `CREATE TABLE` statements
        /// rearranges the database's table iteration but each per-table
        /// digest depends only on that table's own canonical model.
        #[test]
        fn test_inv_003_source_ordering_invariance() {
            let sql_a = "
                CREATE TABLE t1 (id INT PRIMARY KEY, name TEXT);
                CREATE TABLE t2 (id INT PRIMARY KEY, value INT);
                CREATE TABLE t3 (id INT, x TEXT);
            ";
            let sql_b = "
                CREATE TABLE t3 (id INT, x TEXT);
                CREATE TABLE t1 (id INT PRIMARY KEY, name TEXT);
                CREATE TABLE t2 (id INT PRIMARY KEY, value INT);
            ";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            for name in ["t1", "t2", "t3"] {
                let fp_a = db_a
                    .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .unwrap()
                    .schema_fingerprint(&db_a)
                    .expect("fingerprint");
                let fp_b = db_b
                    .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .unwrap()
                    .schema_fingerprint(&db_b)
                    .expect("fingerprint");
                assert_eq!(fp_a, fp_b, "fingerprint for `{name}` must be source-order-invariant");
            }
        }

        /// SEN-001a: adding a column changes the fingerprint.
        #[test]
        fn test_sen_001_column_add_changes_fingerprint() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let sql_b = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, email TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b, "adding a column must change the fingerprint");
        }

        /// SEN-001b: dropping a column changes the fingerprint.
        #[test]
        fn test_sen_001_column_drop_changes_fingerprint() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, email TEXT);";
            let sql_b = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b, "dropping a column must change the fingerprint");
        }

        /// SEN-002: renaming a column (with type and position unchanged)
        /// changes the fingerprint, because column names participate in
        /// the canonical encoding.
        #[test]
        fn test_sen_002_column_rename_changes_fingerprint() {
            let sql_a = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let sql_b = "CREATE TABLE users (id INT PRIMARY KEY, full_name TEXT);";

            let db_a = ParserDB::parse::<GenericDialect>(sql_a).expect("parse A");
            let db_b = ParserDB::parse::<GenericDialect>(sql_b).expect("parse B");

            let fp_a = db_a
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_a)
                .expect("fingerprint");
            let fp_b = db_b
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_b)
                .expect("fingerprint");

            assert_ne!(fp_a, fp_b, "renaming a column must change the fingerprint");
        }

        /// DIFF-002: a SQL identifier wrapped in matching-case double
        /// quotes is canonically equivalent to its unquoted form
        /// (post-NFC, post-lowercase-fold). Their fingerprints must
        /// match. (Quoted identifiers whose case differs from the
        /// folded form are NOT equivalent — that's the Postgres
        /// semantics already covered by other tests.)
        #[test]
        fn test_diff_002_quoted_unquoted_equivalence() {
            let sql_unq = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);";
            let sql_qtd = "CREATE TABLE \"users\" (\"id\" INT PRIMARY KEY, \"name\" TEXT);";

            let db_unq = ParserDB::parse::<GenericDialect>(sql_unq).expect("parse unquoted");
            let db_qtd = ParserDB::parse::<GenericDialect>(sql_qtd).expect("parse quoted");

            let fp_unq = db_unq
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_unq)
                .expect("fingerprint");
            let fp_qtd = db_qtd
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&db_qtd)
                .expect("fingerprint");

            assert_eq!(
                fp_unq, fp_qtd,
                "quoted-lowercase identifiers must fingerprint identically to unquoted",
            );
        }

        /// DET-001: in-process repeatability. Bumped from the legacy
        /// 2-iteration smoke (`test_determinism`) to the spec-mandated
        /// 1000 iterations.
        #[test]
        fn test_det_001_repeatability_in_process() {
            let sql = "
                CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
                CREATE TABLE products (id INT PRIMARY KEY, price INT, sku TEXT);
            ";

            let baseline = {
                let db = ParserDB::parse::<GenericDialect>(sql).expect("baseline parse");
                (
                    db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                        .expect("unambiguous lookup")
                        .unwrap()
                        .schema_fingerprint(&db)
                        .expect("fingerprint"),
                    db.table_by_target(
                        TargetName::new("products", false),
                        IdentifierCase::AsWritten,
                    )
                    .expect("unambiguous lookup")
                    .unwrap()
                    .schema_fingerprint(&db)
                    .expect("fingerprint"),
                )
            };

            for i in 0..1000 {
                let db = ParserDB::parse::<GenericDialect>(sql).expect("iter parse");
                let users_fp = db
                    .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .unwrap()
                    .schema_fingerprint(&db)
                    .expect("fingerprint");
                let products_fp = db
                    .table_by_target(TargetName::new("products", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .unwrap()
                    .schema_fingerprint(&db)
                    .expect("fingerprint");
                assert_eq!(users_fp, baseline.0, "iter {i}: users fingerprint diverged");
                assert_eq!(products_fp, baseline.1, "iter {i}: products fingerprint diverged");
            }
        }

        /// COL-002: per-field near-neighbor adversarial coverage. For each
        /// mutable field of the v1 canonical envelope, mutate ONLY that
        /// field while holding the others identical, and assert the
        /// resulting fingerprint differs from the baseline. SHA-256
        /// avalanche guarantees any one-bit canonical-byte difference
        /// propagates to the digest, so this proves the encoding has no
        /// "absorbing" field whose mutation is silently lost.
        ///
        /// Field coverage:
        /// - `schema_name`     → `schema_name_norm`
        /// - `table_name`      → `table_name_norm`
        /// - `column_count_*`  → `column_count`
        /// - `column_name`     → per-column name
        /// - `column_type`     → per-column canonical type token
        /// - `nullable_flag`   → per-column nullable byte
        /// - `generated_flag`  → per-column generated byte
        /// - `column_ordinal`  → per-column ordinal (column reorder)
        /// - `pk_count`        → `pk_count`
        /// - `pk_order`        → `pk_ordinal` slot ordering
        ///
        /// `magic`, `canonicalization_version`, and `profile_id` are
        /// constants in v1 — already covered by MIG-001/002.
        #[test]
        fn test_col_002_per_field_adversarial() {
            let baseline_sql =
                "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, score INT NOT NULL);";

            // (mutated_field_label, mutated_sql, lookup_schema,
            // lookup_table_name)
            let mutations: &[(&str, &str, Option<&str>, &str)] = &[
                (
                    "schema_name",
                    "CREATE SCHEMA myschema; CREATE TABLE myschema.users (id INT PRIMARY KEY, name TEXT, score INT NOT NULL);",
                    Some("myschema"),
                    "users",
                ),
                (
                    "table_name",
                    "CREATE TABLE u (id INT PRIMARY KEY, name TEXT, score INT NOT NULL);",
                    None,
                    "u",
                ),
                (
                    "column_count_drop",
                    "CREATE TABLE users (id INT PRIMARY KEY, name TEXT);",
                    None,
                    "users",
                ),
                (
                    "column_count_add",
                    "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, score INT NOT NULL, extra INT);",
                    None,
                    "users",
                ),
                (
                    "column_name",
                    "CREATE TABLE users (id INT PRIMARY KEY, full_name TEXT, score INT NOT NULL);",
                    None,
                    "users",
                ),
                (
                    "column_type",
                    "CREATE TABLE users (id INT PRIMARY KEY, name INT, score INT NOT NULL);",
                    None,
                    "users",
                ),
                (
                    "nullable_flag",
                    "CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, score INT NOT NULL);",
                    None,
                    "users",
                ),
                (
                    "generated_flag",
                    "CREATE TABLE users (id SERIAL PRIMARY KEY, name TEXT, score INT NOT NULL);",
                    None,
                    "users",
                ),
                (
                    "column_ordinal",
                    "CREATE TABLE users (name TEXT, id INT PRIMARY KEY, score INT NOT NULL);",
                    None,
                    "users",
                ),
                (
                    "pk_count",
                    "CREATE TABLE users (id INT, name TEXT, score INT NOT NULL, PRIMARY KEY (id, name));",
                    None,
                    "users",
                ),
                (
                    "pk_order",
                    "CREATE TABLE users (id INT, name TEXT, score INT NOT NULL, PRIMARY KEY (name, id));",
                    None,
                    "users",
                ),
            ];

            let baseline_db =
                ParserDB::parse::<GenericDialect>(baseline_sql).expect("baseline parse");
            let baseline_fp = baseline_db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap()
                .schema_fingerprint(&baseline_db)
                .expect("baseline fingerprint");

            for (field, sql, schema, name) in mutations {
                let db = ParserDB::parse::<GenericDialect>(sql)
                    .unwrap_or_else(|e| panic!("[{field}] parse failed: {e:?}"));
                let target = match schema {
                    Some(schema) => TargetName::new(name, false).with_schema(schema, false),
                    None => TargetName::new(name, false),
                };
                let fp = db
                    .table_by_target(target, IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .unwrap_or_else(|| panic!("[{field}] table `{name}` not found"))
                    .schema_fingerprint(&db)
                    .expect("fingerprint");
                assert_ne!(fp, baseline_fp, "mutation of `{field}` left fingerprint unchanged");
            }
        }

        // ---------------------------------------------------------------
        // Out-of-scope conformance gates (audit §10).
        //
        // The following test gates from FINGERPRINT_SPEC §13 are NOT
        // covered by in-process unit tests because they require
        // external infrastructure beyond this crate's scope:
        //
        //   • DET-002 (cross-process determinism): needs a small CLI
        //     harness that emits hex digests; verify two separate
        //     processes agree on the digest for a fixed fixture.
        //
        //   • DET-003 (cross-platform determinism): CI matrix gate —
        //     run the existing fingerprint tests on Linux + macOS +
        //     Windows and compare golden hex.
        //
        //   • DET-004 (cross-toolchain determinism): CI matrix gate —
        //     stable / beta / nightly / MSRV.
        //
        //   • DIFF-001 (parser vs DB-introspection parity): requires a
        //     second `DatabaseLike` implementation (e.g. live PostgreSQL
        //     introspection) to compare against `ParserDB` output for
        //     the same schema. Tracked downstream in `pg_diesel`.
        //
        //   • COL-001 (1 M random-corpus collision smoke): scheduled CI
        //     job, not a per-PR unit test. Generates 1 M synthetic
        //     schemas via `arbitrary` and verifies that
        //     `fingerprint64` collisions remain below the birthday
        //     bound for SHA-256-truncated digests.
        // ---------------------------------------------------------------
    }

    mod drop_table_tests {
        use sqlparser::dialect::GenericDialect;

        use crate::{
            structs::{IdentifierCase, ParserDB, TargetName},
            traits::{DatabaseLike, TableLike},
        };

        #[test]
        fn test_drop_table_basic() {
            let sql = r"
                CREATE TABLE my_table (id INT PRIMARY KEY);
                DROP TABLE my_table;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Table should be removed
            assert!(
                db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert_eq!(db.tables().count(), 0);
        }

        #[test]
        fn test_drop_table_if_exists_when_exists() {
            let sql = r"
                CREATE TABLE my_table (id INT PRIMARY KEY);
                DROP TABLE IF EXISTS my_table;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Table should be removed
            assert!(
                db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_drop_table_if_exists_when_not_exists() {
            let sql = r"
                DROP TABLE IF EXISTS nonexistent_table;
            ";
            let db =
                ParserDB::parse::<GenericDialect>(sql).expect("Should not error with IF EXISTS");

            // No tables
            assert_eq!(db.tables().count(), 0);
        }

        #[test]
        fn test_drop_table_not_found() {
            let sql = r"
                DROP TABLE nonexistent_table;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            // Should fail because table doesn't exist
            assert!(result.is_err());
            if let Err(e) = result {
                let error_msg = format!("{e}");
                assert!(error_msg.contains("nonexistent_table"));
            }
        }

        #[test]
        fn test_drop_table_referenced_by_foreign_key_fails() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                DROP TABLE parent;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            // Should fail because parent is referenced by child
            assert!(result.is_err());
            if let Err(e) = result {
                let error_msg = format!("{e}");
                assert!(error_msg.contains("parent"));
                assert!(error_msg.contains("referenced"));
            }
        }

        #[test]
        fn test_drop_table_with_cascade_succeeds() {
            let sql = r"
                CREATE TABLE parent (id INT PRIMARY KEY);
                CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id));
                DROP TABLE parent CASCADE;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("CASCADE should allow drop");

            // Parent should be removed
            assert!(
                db.table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );

            // Child should still exist
            assert!(
                db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_drop_table_self_referential_succeeds() {
            let sql = r"
                CREATE TABLE tree (id INT PRIMARY KEY, parent_id INT REFERENCES tree(id));
                DROP TABLE tree;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql)
                .expect("Self-referential table should be droppable");

            // Table should be removed
            assert!(
                db.table_by_target(TargetName::new("tree", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_drop_table_removes_associated_objects() {
            let sql = r"
                CREATE TABLE my_table (
                    id INT PRIMARY KEY,
                    name TEXT CHECK (length(name) > 0)
                );
                CREATE INDEX my_idx ON my_table (name);
                DROP TABLE my_table;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Table should be removed
            assert!(
                db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );

            // No tables, columns, indices should remain
            assert_eq!(db.tables().count(), 0);
        }

        #[test]
        fn test_drop_table_keeps_other_tables() {
            let sql = r"
                CREATE TABLE table1 (id INT PRIMARY KEY);
                CREATE TABLE table2 (id INT PRIMARY KEY);
                CREATE TABLE table3 (id INT PRIMARY KEY);
                DROP TABLE table2;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // table2 should be removed
            assert!(
                db.table_by_target(TargetName::new("table2", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );

            // table1 and table3 should still exist
            assert!(
                db.table_by_target(TargetName::new("table1", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("table3", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert_eq!(db.tables().count(), 2);
        }

        #[test]
        fn test_drop_table_then_recreate() {
            let sql = r"
                CREATE TABLE my_table (id INT PRIMARY KEY);
                DROP TABLE my_table;
                CREATE TABLE my_table (id INT, name TEXT);
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            // Table should exist with new schema
            let table = db
                .table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.columns(&db).expect("columns").count(), 2);
        }

        #[test]
        fn test_drop_table_unquoted_name_is_case_insensitive() {
            let sql = r"
                CREATE TABLE Foo (id INT PRIMARY KEY);
                DROP TABLE foo;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
            assert!(
                db.table_by_target(TargetName::new("foo", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_drop_table_quoted_name_is_case_sensitive() {
            let sql = r#"
                CREATE TABLE Foo (id INT PRIMARY KEY);
                DROP TABLE "Foo";
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                matches!(err, crate::errors::Error::DropTableNotFound { table_name } if table_name == "Foo")
            );
        }
    }

    mod rename_table {
        use super::*;

        #[test]
        fn test_rename_table() {
            let sql = r"
                CREATE TABLE old_name (id INT PRIMARY KEY);
                RENAME TABLE old_name TO new_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert!(
                db.table_by_target(TargetName::new("old_name", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            let table = db
                .table_by_target(TargetName::new("new_name", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("new_name should exist");
            assert_eq!(table.table_name(), "new_name");
        }

        #[test]
        fn test_rename_nonexistent_table_fails() {
            let sql = r"RENAME TABLE nonexistent TO other;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                matches!(err, crate::errors::Error::RenameTableNotFound { table_name } if table_name == "nonexistent")
            );
        }

        #[test]
        fn test_rename_multiple_tables() {
            let sql = r"
                CREATE TABLE table_a (id INT);
                CREATE TABLE table_b (id INT);
                RENAME TABLE table_a TO new_a, table_b TO new_b;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert!(
                db.table_by_target(TargetName::new("table_a", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("table_b", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            assert!(
                db.table_by_target(TargetName::new("new_a", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("new_b", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
        }

        #[test]
        fn test_rename_preserves_columns() {
            let sql = r"
                CREATE TABLE old_name (id INT PRIMARY KEY, name TEXT);
                RENAME TABLE old_name TO new_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            let table = db
                .table_by_target(TargetName::new("new_name", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table should exist");
            assert_eq!(table.columns(&db).expect("columns").count(), 2);
        }

        #[test]
        fn test_rename_to_existing_semantic_name_fails() {
            let sql = r"
                CREATE TABLE foo (id INT);
                CREATE TABLE bar (id INT);
                RENAME TABLE bar TO foo;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(crate::errors::Error::IdentifierLookupError(
                    crate::errors::LookupError::TableLookupConflict {
                        table,
                        conflicting_table,
                        ..
                    }
                )) if table == "foo" && conflicting_table == "foo"
            ));
        }

        #[test]
        fn test_rename_introducing_unqualified_public_ambiguity_fails() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE TABLE other (id INT);
                RENAME TABLE other TO public.t;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(crate::errors::Error::IdentifierLookupError(
                    crate::errors::LookupError::TableLookupConflict {
                        table,
                        conflicting_table,
                        ..
                    }
                )) if table == "public.t" && conflicting_table == "t"
            ));
        }

        #[test]
        fn test_rename_quoted_unquoted_equivalent_fails() {
            let sql = r#"
                CREATE TABLE foo (id INT);
                CREATE TABLE bar (id INT);
                RENAME TABLE bar TO "foo";
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(crate::errors::Error::IdentifierLookupError(
                    crate::errors::LookupError::TableLookupConflict {
                        table,
                        conflicting_table,
                        ..
                    }
                )) if table == "\"foo\"" && conflicting_table == "foo"
            ));
        }

        #[test]
        fn test_alter_table_rename_table() {
            let sql = r"
                CREATE TABLE old_name (id INT PRIMARY KEY);
                ALTER TABLE old_name RENAME TO new_name;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert!(
                db.table_by_target(TargetName::new("old_name", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
            let table = db
                .table_by_target(TargetName::new("new_name", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("new_name should exist");
            assert_eq!(table.table_name(), "new_name");
        }

        #[test]
        fn test_alter_table_rename_nonexistent_table_fails() {
            let sql = r"ALTER TABLE nonexistent RENAME TO other;";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                matches!(err, crate::errors::Error::RenameTableNotFound { table_name } if table_name == "nonexistent")
            );
        }

        #[test]
        fn test_alter_table_if_exists_rename_nonexistent_table_is_noop() {
            let sql = r"
                CREATE TABLE existing (id INT);
                ALTER TABLE IF EXISTS nonexistent RENAME TO other;
            ";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");

            assert!(
                db.table_by_target(TargetName::new("existing", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_some()
            );
            assert!(
                db.table_by_target(TargetName::new("other", false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .is_none()
            );
        }

        #[test]
        fn test_alter_table_rename_to_existing_semantic_name_fails() {
            let sql = r"
                CREATE TABLE foo (id INT);
                CREATE TABLE bar (id INT);
                ALTER TABLE bar RENAME TO foo;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(crate::errors::Error::IdentifierLookupError(
                    crate::errors::LookupError::TableLookupConflict {
                        table,
                        conflicting_table,
                        ..
                    }
                )) if table == "foo" && conflicting_table == "foo"
            ));
        }

        #[test]
        fn test_alter_table_rename_introducing_unqualified_public_ambiguity_fails() {
            let sql = r"
                CREATE TABLE t (id INT);
                CREATE TABLE other (id INT);
                ALTER TABLE other RENAME TO public.t;
            ";
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(crate::errors::Error::IdentifierLookupError(
                    crate::errors::LookupError::TableLookupConflict {
                        table,
                        conflicting_table,
                        ..
                    }
                )) if table == "public.t" && conflicting_table == "t"
            ));
        }

        #[test]
        fn test_alter_table_rename_quoted_unquoted_equivalent_fails() {
            let sql = r#"
                CREATE TABLE foo (id INT);
                CREATE TABLE bar (id INT);
                ALTER TABLE bar RENAME TO "foo";
            "#;
            let result = ParserDB::parse::<GenericDialect>(sql);

            assert!(matches!(
                result,
                Err(crate::errors::Error::IdentifierLookupError(
                    crate::errors::LookupError::TableLookupConflict {
                        table,
                        conflicting_table,
                        ..
                    }
                )) if table == "\"foo\"" && conflicting_table == "foo"
            ));
        }
    }
}
