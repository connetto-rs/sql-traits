//! Submodule providing a trait for describing SQL Column-like entities.

use alloc::{borrow::Cow, string::String, vec::Vec};
use core::{borrow::Borrow, fmt::Debug, hash::Hash};

use crate::{
    errors::LookupError,
    structs::TargetName,
    traits::{
        CheckConstraintLike, DatabaseLike, ForeignKeyLike, IndexLike, Metadata, TableLike,
        check_constraint::non_tautological,
    },
    utils::{
        identifier_resolution::normalize_identifier,
        normalize_postgres_type_cow,
        scalar_family::{ScalarFamily, scalar_family as classify_scalar_family},
    },
};

/// The padding rule MySQL exposes for a collation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MySqlCollationPadding {
    /// Comparisons ignore trailing spaces.
    PadSpace,
    /// Comparisons keep trailing spaces significant.
    NoPad,
}

/// Collation metadata for one named comparison rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NamedColumnCollation<'a> {
    name: TargetName<'a>,
    postgres_deterministic: Option<bool>,
    mysql_padding: Option<MySqlCollationPadding>,
}

impl<'a> NamedColumnCollation<'a> {
    /// Creates named collation metadata from the SQL name.
    #[must_use]
    pub fn new(name: TargetName<'a>) -> Self {
        Self { name, postgres_deterministic: None, mysql_padding: None }
    }

    /// Stores PostgreSQL determinism, or `None` when it is unknown.
    #[must_use]
    pub fn with_postgres_deterministic(mut self, deterministic: Option<bool>) -> Self {
        self.postgres_deterministic = deterministic;
        self
    }

    /// Stores MySQL padding, or `None` when it is unknown.
    #[must_use]
    pub fn with_mysql_padding(mut self, padding: Option<MySqlCollationPadding>) -> Self {
        self.mysql_padding = padding;
        self
    }

    /// Returns the collation name.
    ///
    /// The name is lent, so a reader comparing its text allocates nothing
    /// even when the collation owns its parts. Use [`Self::into_name`] to
    /// take the name out, or clone it when the facts beside it are still
    /// needed.
    #[must_use]
    pub fn name(&self) -> &TargetName<'a> {
        &self.name
    }

    /// Returns PostgreSQL determinism, or `None` when it is unknown.
    #[must_use]
    pub fn postgres_deterministic(&self) -> Option<bool> {
        self.postgres_deterministic
    }

    /// Returns MySQL padding, or `None` when it is unknown.
    #[must_use]
    pub fn mysql_padding(&self) -> Option<MySqlCollationPadding> {
        self.mysql_padding
    }

    /// Takes ownership of the collation name, so the metadata outlives the
    /// database it was resolved against.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{prelude::*, traits::NamedColumnCollation};
    ///
    /// let owned = {
    ///     let text = String::from("ci");
    ///     NamedColumnCollation::new(TargetName::new(&text, false)).into_owned()
    /// };
    /// assert_eq!(owned.name().name(), "ci");
    /// ```
    #[must_use]
    pub fn into_owned(self) -> NamedColumnCollation<'static> {
        NamedColumnCollation {
            name: self.name.into_owned(),
            postgres_deterministic: self.postgres_deterministic,
            mysql_padding: self.mysql_padding,
        }
    }

    /// Takes the collation name out, dropping the catalog facts beside it.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{prelude::*, traits::NamedColumnCollation};
    ///
    /// let name = NamedColumnCollation::new(TargetName::new("ci", false)).into_name();
    /// assert_eq!(name.name(), "ci");
    /// ```
    #[must_use]
    pub fn into_name(self) -> TargetName<'a> {
        self.name
    }
}

/// How a column resolves text comparison rules.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColumnCollation<'a> {
    /// No declared rule is present, so the database default applies.
    DatabaseDefault,
    /// A declared `COLLATE` names the rule, with catalog facts when known.
    Named(NamedColumnCollation<'a>),
    /// The source changes comparison rules without a resolved collation name.
    Unknown,
}

impl ColumnCollation<'_> {
    /// Takes ownership of the declared name, so a resolved collation outlives
    /// the database borrow it came from.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let collation = {
    ///     let db = ParserDB::parse::<PostgreSqlDialect>(
    ///         "CREATE TABLE t (name TEXT COLLATE \"und-x-icu\");",
    ///     )?;
    ///     let table =
    ///         db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    ///     let column = table.column("name", &db, IdentifierCase::AsWritten)?.unwrap();
    ///     column.collation(&db)?.into_owned()
    /// };
    /// let ColumnCollation::Named(named) = collation else {
    ///     panic!("a declared COLLATE resolves to a named collation");
    /// };
    /// assert_eq!(named.name().name(), "und-x-icu");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn into_owned(self) -> ColumnCollation<'static> {
        match self {
            Self::DatabaseDefault => ColumnCollation::DatabaseDefault,
            Self::Named(named) => ColumnCollation::Named(named.into_owned()),
            Self::Unknown => ColumnCollation::Unknown,
        }
    }
}

/// Which values of an identity column the database supplies.
///
/// PostgreSQL records the mode in `pg_attribute.attidentity`, as `a` for
/// [`Self::Always`] and `d` for [`Self::ByDefault`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityGeneration {
    /// `GENERATED ALWAYS AS IDENTITY`. The database supplies every value and
    /// refuses one an insert gives unless the insert overrides the identity.
    Always,
    /// `GENERATED BY DEFAULT AS IDENTITY`. The database supplies a value only
    /// when an insert gives none.
    ByDefault,
}

/// A trait for types that can be treated as SQL columns.
pub trait ColumnLike:
    Debug
    + Clone
    + Send
    + Sync
    + Hash
    + Eq
    + Ord
    + Metadata
    + Borrow<<<Self as ColumnLike>::DB as DatabaseLike>::Column>
{
    /// The type of the database that this column belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the column.
    ///
    /// The name is the stored spelling, without the quotes a quoted
    /// identifier was written with.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(r#"CREATE TABLE t ("Id" INT);"#)?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column = table.columns(&db)?.next().unwrap();
    /// assert_eq!(column.column_name(), "Id");
    /// assert!(column.column_name_is_quoted());
    /// # Ok(())
    /// # }
    /// ```
    fn column_name(&self) -> &str;

    /// Returns whether the column identifier was quoted in SQL.
    ///
    /// Quoted identifiers are resolved case-sensitively in PostgreSQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn column_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this column: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    ///
    /// Prefer this over [`Self::column_name`] whenever the name is emitted into
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
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (ID INT, \"Name\" TEXT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let stored: Vec<_> = table.columns(&db)?.map(|col| col.stored_column_name()).collect();
    /// assert_eq!(stored, vec!["id", "Name"]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn stored_column_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.column_name(), self.column_name_is_quoted())
    }

    /// Returns documentation from standalone comments above the column name.
    /// A blank line detaches the preceding comment group.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the column
    ///   documentation from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (
    ///     -- the id of the table_row
    ///     id INT,
    ///     name TEXT
    /// );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let column_name =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// assert_eq!(column.column_doc(&db)?, Some("the id of the table_row"));
    /// assert!(column_name.column_doc(&db)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn column_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db;

    /// Returns the data type of the column as a string.
    ///
    /// The type is a borrowed canonical token whenever the implementation can
    /// name one literally. An array type has to be assembled from its element
    /// type, so it is owned.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, name TEXT, score REAL);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let score_column = table
    ///     .column("score", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'score' should exist");
    /// assert_eq!(id_column.data_type(&db), "INT");
    /// assert_eq!(name_column.data_type(&db), "TEXT");
    /// assert_eq!(score_column.data_type(&db), "REAL");
    /// # Ok(())
    /// # }
    /// ```
    fn data_type<'db>(&'db self, database: &'db Self::DB) -> Cow<'db, str>;

    /// Returns the width in bytes of a fixed-length binary column, or `None`
    /// when the declared type is not a fixed-length binary type.
    ///
    /// MySQL, SQL Server and ANSI SQL pad every `BINARY(n)` value with zero
    /// bytes to exactly `n` bytes, and read a bare `BINARY` as `BINARY(1)`. A
    /// consumer receiving such a value without its trailing zero bytes, as the
    /// MySQL binary log stores it, needs `n` to restore it. `VARBINARY(n)`
    /// answers `None`, and so does `BINARY(n)` in a dialect where it bounds a
    /// varying length.
    ///
    /// The default answers `None`, so an implementation over a backend with a
    /// fixed-length binary type must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::MySqlDialect;
    ///
    /// let db = ParserDB::parse::<MySqlDialect>(
    ///     "CREATE TABLE t (token BINARY(16), flag BINARY, payload VARBINARY(8));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let widths: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.fixed_binary_length(&db)).collect();
    /// assert_eq!(widths, [Some(16), Some(1), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn fixed_binary_length(&self, _database: &Self::DB) -> Option<u64> {
        None
    }

    /// Returns the most characters a value of the column can hold, or `None`
    /// when the engine does not bound it by the declared type.
    ///
    /// `VARCHAR(n)`, `CHARACTER VARYING(n)`, `CHAR(n)` and `CHARACTER(n)`
    /// answer `n` wherever the engine rejects or truncates a longer value. A
    /// bare `CHAR` is `CHAR(1)`. A bare `VARCHAR` takes the engine's default
    /// length where it has one, as in SQL Server, and answers `None` where it
    /// is unbounded, as in PostgreSQL. `TEXT` and a length that is not an
    /// integer literal, such as `VARCHAR(MAX)`, answer `None`. So does every
    /// declared length in an engine that stores any length regardless, as
    /// SQLite, DuckDB and ClickHouse do. A length counted in bytes or in UTF-16
    /// code units still bounds the characters, since every character takes at
    /// least one of them.
    ///
    /// The default answers `None`, so an implementation over a backend with a
    /// bounded character type must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE notes (title VARCHAR(100), code CHAR(8), body TEXT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)?.unwrap();
    /// let lengths: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.max_character_length(&db)).collect();
    /// assert_eq!(lengths, [Some(100), Some(8), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn max_character_length(&self, _database: &Self::DB) -> Option<u64> {
        None
    }

    /// Returns how many fractional digits of a second a value of a time or
    /// timestamp column keeps, or `None` when the declared type holds no time
    /// of day.
    ///
    /// A written precision such as `TIMESTAMP(0)` or `TIME(3)` answers the
    /// digits the engine keeps for it, and a bare type answers the engine's
    /// default, which is 6 in PostgreSQL, 0 in MySQL and 9 in Snowflake. An
    /// engine with a fixed resolution answers that resolution whatever was
    /// written, as BigQuery does with microseconds. The engine rounds or
    /// truncates a value with more digits, so `Some(0)` marks a column that
    /// cannot hold the fraction of a second `now()` returns. A type without a
    /// time of day, such as a standard `DATE`, answers `None`, and so does an
    /// interval, which measures a duration. Oracle's `DATE` keeps whole seconds
    /// and answers `Some(0)`.
    ///
    /// The default answers `None`, so an implementation over a backend with a
    /// time or timestamp type must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE events (at timestamptz(0), seen timestamp, starts time(2), day date);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("events", false), IdentifierCase::AsWritten)?.unwrap();
    /// let precisions: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.fractional_seconds_precision(&db)).collect();
    /// assert_eq!(precisions, [Some(0), Some(6), Some(2), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn fractional_seconds_precision(&self, _database: &Self::DB) -> Option<u64> {
        None
    }

    /// Returns the builtin scalar family of the column's declared type.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{prelude::*, utils::scalar_family::ScalarFamily};
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE measurements (id BIGINT, value NUMERIC(20, 4), label VARCHAR(255));",
    /// )?;
    /// let table = db
    ///     .table_by_target(TargetName::new("measurements", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let id = table.column("id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let value = table.column("value", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let label = table.column("label", &db, IdentifierCase::AsWritten)?.unwrap();
    /// assert_eq!(id.scalar_family(&db), Some(ScalarFamily::Int));
    /// assert_eq!(value.scalar_family(&db), Some(ScalarFamily::Decimal));
    /// assert_eq!(label.scalar_family(&db), Some(ScalarFamily::String));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn scalar_family(&self, database: &Self::DB) -> Option<ScalarFamily> {
        classify_scalar_family(self.data_type(database).as_ref())
    }

    /// Returns the collation rule used for string comparison.
    ///
    /// A returned `Named` value always carries the declared `COLLATE` name. The
    /// backend-specific facts are `None` when the implementation has no catalog
    /// source for them. `ParserDB` fills PostgreSQL determinism from
    /// `CREATE COLLATION`, but it does not infer MySQL padding from a name.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    ///     CREATE COLLATION ci (
    ///         provider = icu,
    ///         locale = 'und-u-ks-level2',
    ///         deterministic = false
    ///     );
    ///     CREATE TABLE t (name TEXT COLLATE ci);
    ///     ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column = table.column("name", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let ColumnCollation::Named(collation) = column.collation(&db)? else {
    ///     panic!("expected a named collation");
    /// };
    /// assert_eq!(collation.name().name(), "ci");
    /// assert_eq!(collation.postgres_deterministic(), Some(false));
    /// assert_eq!(collation.mysql_padding(), None);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this column.
    fn collation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<ColumnCollation<'db>, LookupError>;

    /// Returns which values the database supplies when the column is an
    /// identity column, or `None` when it is not one.
    ///
    /// A spelling that writes no mode, such as SQL Server's `IDENTITY(1, 1)`
    /// or Snowflake's `AUTOINCREMENT`, reports the mode its engine enforces.
    /// A `serial` column is not an identity, since its values come from a
    /// sequence named in its default (see [`Self::is_serial`]).
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance the column belongs
    ///   to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (
    ///          always INT GENERATED ALWAYS AS IDENTITY,
    ///          by_default INT GENERATED BY DEFAULT AS IDENTITY,
    ///          sequenced SERIAL,
    ///          plain INT
    ///      );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let mode = |name| {
    ///     table
    ///         .column(name, &db, IdentifierCase::AsWritten)
    ///         .map(|column| column.expect("column exists").identity_generation(&db))
    /// };
    /// assert_eq!(mode("always")?, Some(IdentityGeneration::Always));
    /// assert_eq!(mode("by_default")?, Some(IdentityGeneration::ByDefault));
    /// assert_eq!(mode("sequenced")?, None);
    /// assert_eq!(mode("plain")?, None);
    /// # Ok(())
    /// # }
    /// ```
    fn identity_generation(&self, database: &Self::DB) -> Option<IdentityGeneration>;

    /// Returns whether the column was declared with one of PostgreSQL's
    /// serial pseudo-types, `smallserial`, `serial` or `bigserial`, or their
    /// aliases `serial2`, `serial4` and `serial8`.
    ///
    /// The declared type reads as the integer type the pseudo-type stands
    /// for (see [`Self::normalized_data_type`]), and this reader keeps the
    /// declaration apart from a plain integer column.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id serial, big BIGSERIAL, plain INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let serial = |name| {
    ///     table
    ///         .column(name, &db, IdentifierCase::AsWritten)
    ///         .map(|column| column.expect("column exists").is_serial())
    /// };
    /// assert!(serial("id")?);
    /// assert!(serial("big")?);
    /// assert!(!serial("plain")?);
    /// # Ok(())
    /// # }
    /// ```
    fn is_serial(&self) -> bool;

    /// Returns whether the database supplies the values of the column, which
    /// it does for a serial column and for an identity column.
    ///
    /// A computed `GENERATED ALWAYS AS (expression)` column is not generated
    /// in this sense, since its value is derived from the rest of its row.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance the column belongs
    ///   to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (
    ///          sequenced serial,
    ///          identity INT GENERATED ALWAYS AS IDENTITY,
    ///          computed INT GENERATED ALWAYS AS (1) STORED,
    ///          plain INT
    ///      );",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let generated = |name| {
    ///     table
    ///         .column(name, &db, IdentifierCase::AsWritten)
    ///         .map(|column| column.expect("column exists").is_generated(&db))
    /// };
    /// assert!(generated("sequenced")?);
    /// assert!(generated("identity")?);
    /// assert!(!generated("computed")?);
    /// assert!(!generated("plain")?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_generated(&self, database: &Self::DB) -> bool {
        self.is_serial() || self.identity_generation(database).is_some()
    }

    /// Returns whether the column is the primary key of its table (not just
    /// part of it).
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT PRIMARY KEY, name TEXT, age INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let age_column =
    ///     table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// assert!(id_column.is_primary_key(&db)?, "id column should be primary key");
    /// assert!(!name_column.is_primary_key(&db)?, "name column should not be primary key");
    /// assert!(!age_column.is_primary_key(&db)?, "age column should not be primary key");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_primary_key(&self, database: &Self::DB) -> Result<bool, LookupError> {
        let table: &<Self::DB as DatabaseLike>::Table = ColumnLike::table(self, database);
        table.is_primary_key_column(database, self.borrow())
    }

    #[inline]
    /// Returns whether the column is a surrogate key.
    ///
    /// A surrogate key is defined as a primary key that is also generative.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id SERIAL PRIMARY KEY, name TEXT, age INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let age_column =
    ///     table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// assert!(id_column.is_surrogate_key(&db)?, "id column should be a surrogate key");
    /// assert!(!name_column.is_surrogate_key(&db)?, "name column should not be a surrogate key");
    /// assert!(!age_column.is_surrogate_key(&db)?, "age column should not be a surrogate key");
    /// # Ok(())
    /// # }
    /// ```
    fn is_surrogate_key(&self, database: &Self::DB) -> Result<bool, LookupError> {
        let table: &<Self::DB as DatabaseLike>::Table = ColumnLike::table(self, database);
        Ok(table.has_surrogate_primary_key(database)? && self.is_primary_key(database)?)
    }

    /// Returns the normalized data type of the column as a string.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, serial_id SERIAL, bigg_id BIGSERIAL, small_id SMALLSERIAL, name TEXT);")?;
    /// let table = db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column = table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let serial_id_column = table.column("serial_id", &db, IdentifierCase::AsWritten)?.expect("Column 'serial_id' should exist");
    /// let bigg_id_column = table.column("bigg_id", &db, IdentifierCase::AsWritten)?.expect("Column 'bigg_id' should exist");
    /// let small_id_column = table.column("small_id", &db, IdentifierCase::AsWritten)?.expect("Column 'small_id' should exist");
    /// let name_column = table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// assert_eq!(id_column.normalized_data_type(&db), "INT");
    /// assert_eq!(serial_id_column.normalized_data_type(&db), "INT");
    /// assert_eq!(bigg_id_column.normalized_data_type(&db), "BIGINT");
    /// assert_eq!(small_id_column.normalized_data_type(&db), "SMALLINT");
    /// assert_eq!(name_column.normalized_data_type(&db), "TEXT");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn normalized_data_type<'db>(&'db self, database: &'db Self::DB) -> Cow<'db, str> {
        normalize_postgres_type_cow(self.data_type(database))
    }

    /// Returns whether the column type belongs to the string family.
    ///
    /// The family is the one the schema fingerprint records, so a type that
    /// fingerprints as a string reads as textual here and the two cannot
    /// drift apart.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the column
    ///   data type from.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, name TEXT, description VARCHAR, note CLOB);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let description_column = table
    ///     .column("description", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'description' should exist");
    /// let note_column =
    ///     table.column("note", &db, IdentifierCase::AsWritten)?.expect("Column 'note' should exist");
    /// assert!(!id_column.is_textual(&db), "id column should not be textual");
    /// assert!(name_column.is_textual(&db), "name column should be textual");
    /// assert!(description_column.is_textual(&db), "description column should be textual");
    /// assert!(note_column.is_textual(&db), "note (CLOB) column should be textual");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_textual(&self, database: &Self::DB) -> bool {
        self.scalar_family(database) == Some(ScalarFamily::String)
    }

    /// Returns whether the column is nullable.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT NOT NULL, name TEXT, optional_field INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let optional_column = table
    ///     .column("optional_field", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'optional_field' should exist");
    /// assert!(!id_column.is_nullable(&db)?, "id column should not be nullable");
    /// assert!(name_column.is_nullable(&db)?, "name column should be nullable by default");
    /// assert!(
    ///     optional_column.is_nullable(&db)?,
    ///     "optional_field column should be nullable by default"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn is_nullable(&self, database: &Self::DB) -> Result<bool, LookupError>;

    /// Returns the SQL default value of the column, if any.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT DEFAULT 0, name TEXT, created_at TIMESTAMP DEFAULT NOW());",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let created_at_column = table
    ///     .column("created_at", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'created_at' should exist");
    /// assert_eq!(
    ///     id_column.default_value(),
    ///     Some("0".to_string()),
    ///     "id column should have default value 0"
    /// );
    /// assert_eq!(name_column.default_value(), None, "name column should not have a default value");
    /// assert_eq!(
    ///     created_at_column.default_value(),
    ///     Some("NOW()".to_string()),
    ///     "created_at column should have default value NOW()"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn default_value(&self) -> Option<String>;

    /// Returns whether the column has a default value.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT DEFAULT 0, name TEXT, created_at TIMESTAMP DEFAULT NOW());",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let created_at_column = table
    ///     .column("created_at", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'created_at' should exist");
    /// assert!(id_column.has_default(), "id column should have a default value");
    /// assert!(!name_column.has_default(), "name column should not have a default value");
    /// assert!(created_at_column.has_default(), "created_at column should have a default value");
    /// # Ok(())
    /// # }
    /// ```
    fn has_default(&self) -> bool {
        self.default_value().is_some()
    }

    /// Returns the table that this column belongs to.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query the table
    ///   from.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, name TEXT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let column_table = ColumnLike::table(id_column, &db);
    /// assert_eq!(column_table.table_name(), "my_table");
    /// # Ok(())
    /// # }
    /// ```
    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db;

    /// Returns the column ID according to its position in the table's column
    /// iterator.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
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
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let age_column =
    ///     table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// assert_eq!(id_column.column_id(&db)?, Some(0));
    /// assert_eq!(name_column.column_id(&db)?, Some(1));
    /// assert_eq!(age_column.column_id(&db)?, Some(2));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn column_id(&self, database: &Self::DB) -> Result<Option<usize>, LookupError> {
        let table = ColumnLike::table(self, database);
        Ok(table.columns(database)?.position(|column| column == self.borrow()))
    }

    /// Returns the foreign keys associated with this column.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query foreign
    ///   keys from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to, and
    /// [`LookupError::TableNotFound`] or [`LookupError::ColumnNotFound`] when a
    /// foreign key reached from it names a table or a column `database` does
    /// not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY);
    /// CREATE TABLE host_table (
    ///     id INT,
    ///     name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id)
    /// );
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let id_column =
    ///     host_table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column = host_table
    ///     .column("name", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'name' should exist");
    /// let id_fks = id_column.foreign_keys(&db)?.collect::<Vec<_>>();
    /// let name_fks = name_column.foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(id_fks.len(), 1);
    /// assert_eq!(name_fks.len(), 0);
    /// # Ok(())
    /// # }
    /// ```
    fn foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        let borrowed = self.borrow();
        let mut foreign_keys = Vec::new();
        for foreign_key in ColumnLike::table(self, database).foreign_keys(database)? {
            if foreign_key.host_columns(database)?.any(|col| col == borrowed) {
                foreign_keys.push(foreign_key);
            }
        }

        Ok(foreign_keys.into_iter())
    }

    /// Returns whether the column references the given table or any of its
    /// descendants.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query foreign
    ///   keys from.
    /// * `table` - A reference to the table to check references against.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to, and
    /// [`LookupError::TableNotFound`] or [`LookupError::ColumnNotFound`] when a
    /// foreign key reached from it names a table or a column `database` does
    /// not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent (id INT PRIMARY KEY);
    /// CREATE TABLE child (
    ///     id INT PRIMARY KEY REFERENCES parent(id)
    /// );
    /// CREATE TABLE other (
    ///     id INT PRIMARY KEY,
    ///     child_id INT REFERENCES child(id)
    /// );
    /// ",
    /// )?;
    /// let parent_table =
    ///     db.table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)?.unwrap();
    /// let child_table =
    ///     db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)?.unwrap();
    /// let other_table =
    ///     db.table_by_target(TargetName::new("other", false), IdentifierCase::AsWritten)?.unwrap();
    /// let child_id_column = other_table.column("child_id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// assert!(
    ///     child_id_column.references_table_pk_or_descendant(&db, parent_table)?,
    ///     "child_id should reference parent or its descendant"
    /// );
    /// assert!(
    ///     child_id_column.references_table_pk_or_descendant(&db, child_table)?,
    ///     "child_id should reference child or its descendant"
    /// );
    /// assert!(
    ///     !child_id_column.references_table_pk_or_descendant(&db, other_table)?,
    ///     "child_id should not reference other or its descendant"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn references_table_pk_or_descendant(
        &self,
        database: &Self::DB,
        table: &<Self::DB as DatabaseLike>::Table,
    ) -> Result<bool, LookupError> {
        for foreign_key in self.foreign_keys(database)? {
            if !foreign_key.is_referenced_primary_key(database)?
                || foreign_key.is_composite(database)?
            {
                continue;
            }
            let referenced_table = foreign_key.referenced_table(database)?;
            if referenced_table == table || referenced_table.is_descendant_of(database, table)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Returns the extension foreign keys associated with this column.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query foreign
    ///   keys from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to, and
    /// [`LookupError::TableNotFound`] or [`LookupError::ColumnNotFound`] when a
    /// foreign key reached from it names a table or a column `database` does
    /// not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE parent (id INT PRIMARY KEY);
    /// CREATE TABLE child (
    ///     parent_id INT PRIMARY KEY REFERENCES parent(id)
    /// );
    /// ",
    /// )?;
    /// let parent_table =
    ///     db.table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)?.unwrap();
    /// let child_table =
    ///     db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)?.unwrap();
    /// let parent_id_column = child_table
    ///     .column("parent_id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'parent_id' should exist");
    /// let ext_fks = parent_id_column.extension_foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(ext_fks.len(), 1);
    /// let id_column = parent_table
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// let id_fks = id_column.extension_foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(id_fks.len(), 0);
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

    /// Returns whether the column is a foreign key, i.e. it is part of any
    /// foreign key constraint.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query foreign
    ///   keys from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to, and
    /// [`LookupError::TableNotFound`] or [`LookupError::ColumnNotFound`] when a
    /// foreign key reached from it names a table or a column `database` does
    /// not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY);
    /// CREATE TABLE host_table (
    ///    id INT REFERENCES referenced_table(id),
    ///    name TEXT
    /// );
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let id_column = host_table.column("id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let name_column = host_table.column("name", &db, IdentifierCase::AsWritten)?.unwrap();
    /// assert!(id_column.is_part_of_foreign_key(&db)?, "id column should be a foreign key");
    /// assert!(!name_column.is_part_of_foreign_key(&db)?, "name column should not be a foreign key");
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_part_of_foreign_key(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.foreign_keys(database)?.next().is_some())
    }

    /// Returns the non-composite foreign keys associated with this column.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query foreign
    ///   keys from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to, and
    /// [`LookupError::TableNotFound`] or [`LookupError::ColumnNotFound`] when a
    /// foreign key reached from it names a table or a column `database` does
    /// not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY);
    /// CREATE TABLE host_table (
    ///    id INT,
    ///    name TEXT,
    ///    FOREIGN KEY (id) REFERENCES referenced_table(id)
    /// );
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let id_column =
    ///     host_table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let name_column = host_table
    ///     .column("name", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'name' should exist");
    /// let id_fks = id_column.non_composite_foreign_keys(&db)?.collect::<Vec<_>>();
    /// let name_fks = name_column.non_composite_foreign_keys(&db)?.collect::<Vec<_>>();
    /// assert_eq!(id_fks.len(), 1);
    /// assert_eq!(name_fks.len(), 0);
    /// # Ok(())
    /// # }
    /// ```
    fn non_composite_foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        let mut foreign_keys = Vec::new();
        for foreign_key in self.foreign_keys(database)? {
            if !foreign_key.is_composite(database)? {
                foreign_keys.push(foreign_key);
            }
        }

        Ok(foreign_keys.into_iter())
    }

    /// Returns whether the column is compatible with another column.
    ///
    /// # Implementation Note
    /// Two columns are considered compatible if:
    /// - They have the same data type.
    /// - If they are foreign keys, they reference the same table or share an
    ///   ancestor table.
    ///
    /// If both columns are not foreign keys, they are considered compatible if
    /// they have the same data type.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to which the table
    ///   belongs.
    /// * `other` - The column in the other table to check.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to, and
    /// [`LookupError::TableNotFound`] or [`LookupError::ColumnNotFound`] when a
    /// foreign key reached from it names a table or a column `database` does
    /// not hold. The same applies to the table `other` belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE another_referenced_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE host_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// CREATE TABLE another_host_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES another_referenced_table(id));
    /// CREATE TABLE compatible_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES referenced_table(id));
    /// CREATE TABLE incompatible_table (id INT PRIMARY KEY, name TEXT,
    ///     FOREIGN KEY (id) REFERENCES another_referenced_table(id));
    /// CREATE TABLE non_fk_table (id INT PRIMARY KEY, name TEXT);
    /// CREATE TABLE serial_table_one (id SERIAL PRIMARY KEY, name TEXT);
    /// CREATE TABLE serial_table_two (id SERIAL PRIMARY KEY, name TEXT);
    /// ",
    /// )?;
    /// let host_table = db
    ///     .table_by_target(TargetName::new("host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let id_column =
    ///     host_table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let compatible_table = db
    ///     .table_by_target(TargetName::new("compatible_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let serial_table_one = db
    ///     .table_by_target(TargetName::new("serial_table_one", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let serial_id_column = serial_table_one
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// let serial_table_two = db
    ///     .table_by_target(TargetName::new("serial_table_two", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let serial_id_column_two = serial_table_two
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// let compatible_id_column = compatible_table
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// let incompatible_table = db
    ///     .table_by_target(TargetName::new("incompatible_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let incompatible_id_column = incompatible_table
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// let another_host_table = db
    ///     .table_by_target(TargetName::new("another_host_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let another_id_column = another_host_table
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// let non_fk_table = db
    ///     .table_by_target(TargetName::new("non_fk_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let non_fk_id_column = non_fk_table
    ///     .column("id", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'id' should exist");
    /// assert!(
    ///     id_column.is_compatible_with(&db, compatible_id_column)?,
    ///     "Columns should be compatible as they reference the same table"
    /// );
    /// assert!(
    ///     !id_column.is_compatible_with(&db, incompatible_id_column)?,
    ///     "Columns should not be compatible as they reference different tables"
    /// );
    /// assert!(
    ///     !id_column.is_compatible_with(&db, another_id_column)?,
    ///     "Columns should not be compatible as they reference different tables"
    /// );
    /// assert!(
    ///     !id_column.is_compatible_with(&db, non_fk_id_column)?,
    ///     "Columns should not be compatible as one of them is not a foreign key"
    /// );
    /// assert!(
    ///     !serial_id_column.is_compatible_with(&db, serial_id_column_two)?,
    ///     "Columns should not be compatible as both are generative"
    /// );
    /// assert!(
    ///     serial_id_column.is_compatible_with(&db, non_fk_id_column)?,
    ///     "Columns should be compatible as only one is generative and they have the same data type"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn is_compatible_with(&self, database: &Self::DB, other: &Self) -> Result<bool, LookupError> {
        let host_table: &<Self::DB as DatabaseLike>::Table = ColumnLike::table(self, database);
        let other_table: &<Self::DB as DatabaseLike>::Table = ColumnLike::table(other, database);

        // If the two columns are the same, they are compatible.
        if host_table == other_table && self == other {
            return Ok(true);
        }

        // When the database supplies the values of both columns, the two
        // never hold the same value.
        if self.is_generated(database) && other.is_generated(database) {
            return Ok(false);
        }

        if self.normalized_data_type(database) != other.normalized_data_type(database) {
            return Ok(false);
        }

        let mut local_referenced_tables =
            host_table.referenced_tables_via_column(database, self.borrow())?;
        let mut other_referenced_tables =
            other_table.referenced_tables_via_column(database, other.borrow())?;

        if local_referenced_tables.is_empty() && other_referenced_tables.is_empty() {
            // If both columns are not foreign keys, they are compatible.
            return Ok(true);
        }

        // If the columns are primary keys, we include the local table as a
        // referenced table.
        if self.is_primary_key(database)? {
            local_referenced_tables.push(host_table);
        }
        if other.is_primary_key(database)? {
            other_referenced_tables.push(other_table);
        }

        // We determine the set of ancestors of the referenced tables.
        let mut local_referenced_ancestors = Vec::new();
        for &table in &local_referenced_tables {
            local_referenced_ancestors.extend(table.ancestral_extended_tables(database)?);
        }
        let mut other_referenced_ancestors = Vec::new();
        for &table in &other_referenced_tables {
            other_referenced_ancestors.extend(table.ancestral_extended_tables(database)?);
        }

        // We extend the referenced tables with their ancestors.
        local_referenced_tables.extend(local_referenced_ancestors);
        other_referenced_tables.extend(other_referenced_ancestors);

        Ok(local_referenced_tables.iter().any(|table| other_referenced_tables.contains(table)))
    }

    /// Iterates over the
    /// [`CheckConstraintLike`]s
    /// that involve this column within the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query check
    ///   constraints from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, age INT CHECK (age >= 0), score INT CHECK (score BETWEEN 0 AND 100));")?;
    ///
    /// let table = db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let age_column = table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// let score_column = table.column("score", &db, IdentifierCase::AsWritten)?.expect("Column 'score' should exist");
    ///
    /// let age_checks = age_column.check_constraints(&db)?.collect::<Vec<_>>();
    /// let score_checks = score_column.check_constraints(&db)?.collect::<Vec<_>>();
    ///
    /// assert_eq!(age_checks.len(), 1, "age column should have one check constraint");
    /// assert_eq!(score_checks.len(), 1, "score column should have one check constraint");
    /// # Ok(())
    /// # }
    /// ```
    fn check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint> + 'db,
        LookupError,
    > {
        let table: &<Self::DB as DatabaseLike>::Table = ColumnLike::table(self, database);
        let mut check_constraints = Vec::new();
        for check in table.check_constraints(database)? {
            if check.involves_column(database, self.borrow())? {
                check_constraints.push(check);
            }
        }

        Ok(check_constraints.into_iter())
    }

    /// Returns an iterator over the indices that involve this column.
    ///
    /// # Arguments
    ///
    /// * `database` - The database containing the column.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, name TEXT);CREATE INDEX idx_name ON my_table(name);",
    /// )?;
    ///
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let name_column =
    ///     table.column("name", &db, IdentifierCase::AsWritten)?.expect("Column 'name' should exist");
    /// let id_column =
    ///     table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    ///
    /// let name_indices: Vec<_> = name_column.indices(&db)?.collect();
    /// let id_indices: Vec<_> = id_column.indices(&db)?.collect();
    ///
    /// assert_eq!(name_indices.len(), 1, "name column should have one index");
    /// assert_eq!(id_indices.len(), 0, "id column should have no indices");
    /// # Ok(())
    /// # }
    /// ```
    fn indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Index>, LookupError>
    where
        Self: 'db,
    {
        let table = self.table(database);
        let mut indices = Vec::new();
        for index in table.indices(database)? {
            if index.columns(database)?.any(|col| col == self.borrow()) {
                indices.push(index);
            }
        }

        Ok(indices.into_iter())
    }

    /// Returns whether the column has any check constraints.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query check
    ///   constraints from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, age INT CHECK (age >= 0), score INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let age_column =
    ///     table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// let score_column = table
    ///     .column("score", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'score' should exist");
    /// assert!(age_column.has_check_constraints(&db)?, "age column should have check constraints");
    /// assert!(
    ///     !score_column.has_check_constraints(&db)?,
    ///     "score column should not have check constraints"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_check_constraints(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.check_constraints(database)?.next().is_some())
    }

    #[inline]
    /// Returns an iterator over the non-tautological
    /// [`CheckConstraintLike`]s
    /// that involve this column within the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query check
    ///   constraints from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT, age INT CHECK (age >= 0), score INT CHECK (score BETWEEN 0 AND 100));")?;
    /// let table = db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let age_column = table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// let score_column = table.column("score", &db, IdentifierCase::AsWritten)?.expect("Column 'score' should exist");
    /// let age_non_tauto = age_column.non_tautological_check_constraints(&db)?.collect::<Vec<_>>();
    /// let score_non_tauto = score_column.non_tautological_check_constraints(&db)?.collect::<Vec<_>>();
    /// assert_eq!(age_non_tauto.len(), 1, "age column should have one non-tautological check constraint");
    /// assert_eq!(score_non_tauto.len(), 1, "score column should have one non-tautological check constraint");
    /// # Ok(())
    /// # }
    /// ```
    fn non_tautological_check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<
        impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint> + 'db,
        LookupError,
    > {
        Ok(non_tautological(self.check_constraints(database)?, database)?.into_iter())
    }

    #[inline]
    /// Returns whether the column has non-tautological check constraints.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query check
    ///   constraints from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, age INT CHECK (age >= 0), score INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let age_column =
    ///     table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// let score_column = table
    ///     .column("score", &db, IdentifierCase::AsWritten)?
    ///     .expect("Column 'score' should exist");
    /// assert!(
    ///     age_column.has_non_tautological_check_constraints(&db)?,
    ///     "age column should have non-tautological check constraints"
    /// );
    /// assert!(
    ///     !score_column.has_non_tautological_check_constraints(&db)?,
    ///     "score column should not have non-tautological check constraints"
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn has_non_tautological_check_constraints(
        &self,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        Ok(self.non_tautological_check_constraints(database)?.next().is_some())
    }

    /// Iterates over the
    /// [`UniqueIndexLike`](crate::traits::UniqueIndexLike)s
    /// that involve this column within the table.
    ///
    /// # Arguments
    ///
    /// * `database` - A reference to the database instance to query unique
    ///   indices from.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold the table this column belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE my_table (id INT PRIMARY KEY, other TEXT, age INT, score INT, UNIQUE (age, score));")?;
    ///
    /// let table = db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id_column = table.column("id", &db, IdentifierCase::AsWritten)?.expect("Column 'id' should exist");
    /// let age_column = table.column("age", &db, IdentifierCase::AsWritten)?.expect("Column 'age' should exist");
    /// let score_column = table.column("score", &db, IdentifierCase::AsWritten)?.expect("Column 'score' should exist");
    /// let other_column = table.column("other", &db, IdentifierCase::AsWritten)?.expect("Column 'other' should exist");
    ///
    /// let id_unique = id_column.unique_indices(&db)?.collect::<Vec<_>>();
    /// let age_unique = age_column.unique_indices(&db)?.collect::<Vec<_>>();
    /// let score_unique = score_column.unique_indices(&db)?.collect::<Vec<_>>();
    /// let other_unique = other_column.unique_indices(&db)?.collect::<Vec<_>>();
    ///
    /// assert_eq!(id_unique.len(), 1, "id column should have one unique index");
    /// assert_eq!(age_unique.len(), 1, "age column should have one unique index");
    /// assert_eq!(score_unique.len(), 1, "score column should have one unique index");
    /// assert_eq!(other_unique.len(), 0, "other column should have no unique indices");
    /// # Ok(())
    /// # }
    /// ```
    fn unique_indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::UniqueIndex>, LookupError>
    where
        Self: 'db,
    {
        let table = self.table(database);
        let mut unique_indices = Vec::new();
        for unique_index in table.unique_indices(database)? {
            if unique_index.columns(database)?.any(|col| col == self.borrow()) {
                unique_indices.push(unique_index);
            }
        }

        Ok(unique_indices.into_iter())
    }
}

impl<C> ColumnLike for &C
where
    C: ColumnLike,
    Self: Borrow<<<C as ColumnLike>::DB as DatabaseLike>::Column>,
{
    type DB = C::DB;

    #[inline]
    fn column_name(&self) -> &str {
        (*self).column_name()
    }

    #[inline]
    fn column_name_is_quoted(&self) -> bool {
        (*self).column_name_is_quoted()
    }

    #[inline]
    fn column_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        (*self).column_doc(database)
    }

    #[inline]
    fn data_type<'db>(&'db self, database: &'db Self::DB) -> Cow<'db, str> {
        (*self).data_type(database)
    }

    #[inline]
    fn fixed_binary_length(&self, database: &Self::DB) -> Option<u64> {
        (*self).fixed_binary_length(database)
    }

    #[inline]
    fn max_character_length(&self, database: &Self::DB) -> Option<u64> {
        (*self).max_character_length(database)
    }

    #[inline]
    fn fractional_seconds_precision(&self, database: &Self::DB) -> Option<u64> {
        (*self).fractional_seconds_precision(database)
    }

    #[inline]
    fn collation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<ColumnCollation<'db>, LookupError> {
        (*self).collation(database)
    }

    #[inline]
    fn identity_generation(&self, database: &Self::DB) -> Option<IdentityGeneration> {
        (*self).identity_generation(database)
    }

    #[inline]
    fn is_serial(&self) -> bool {
        (*self).is_serial()
    }

    #[inline]
    fn is_generated(&self, database: &Self::DB) -> bool {
        (*self).is_generated(database)
    }

    #[inline]
    fn is_nullable(&self, database: &Self::DB) -> Result<bool, LookupError> {
        (*self).is_nullable(database)
    }

    #[inline]
    fn default_value(&self) -> Option<String> {
        (*self).default_value()
    }

    #[inline]
    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        (*self).table(database)
    }
}

impl<C> ColumnLike for alloc::sync::Arc<C>
where
    C: ColumnLike<DB: DatabaseLike>,
    Self: Borrow<<<C as ColumnLike>::DB as DatabaseLike>::Column>,
{
    type DB = C::DB;

    #[inline]
    fn column_name(&self) -> &str {
        (**self).column_name()
    }

    #[inline]
    fn column_name_is_quoted(&self) -> bool {
        (**self).column_name_is_quoted()
    }

    #[inline]
    fn column_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        (**self).column_doc(database)
    }

    #[inline]
    fn data_type<'db>(&'db self, database: &'db Self::DB) -> Cow<'db, str> {
        (**self).data_type(database)
    }

    #[inline]
    fn fixed_binary_length(&self, database: &Self::DB) -> Option<u64> {
        (**self).fixed_binary_length(database)
    }

    #[inline]
    fn max_character_length(&self, database: &Self::DB) -> Option<u64> {
        (**self).max_character_length(database)
    }

    #[inline]
    fn fractional_seconds_precision(&self, database: &Self::DB) -> Option<u64> {
        (**self).fractional_seconds_precision(database)
    }

    #[inline]
    fn collation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<ColumnCollation<'db>, LookupError> {
        (**self).collation(database)
    }

    #[inline]
    fn identity_generation(&self, database: &Self::DB) -> Option<IdentityGeneration> {
        (**self).identity_generation(database)
    }

    #[inline]
    fn is_serial(&self) -> bool {
        (**self).is_serial()
    }

    #[inline]
    fn is_generated(&self, database: &Self::DB) -> bool {
        (**self).is_generated(database)
    }

    #[inline]
    fn is_nullable(&self, database: &Self::DB) -> Result<bool, LookupError> {
        (**self).is_nullable(database)
    }

    #[inline]
    fn default_value(&self) -> Option<String> {
        (**self).default_value()
    }

    #[inline]
    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        (**self).table(database)
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use sqlparser::dialect::GenericDialect;

    use super::*;
    use crate::prelude::*;

    mod reference_impl {
        use super::*;

        #[test]
        fn test_all_methods() {
            let sql = "CREATE TABLE users (id INT PRIMARY KEY, name TEXT DEFAULT 'val');";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
            let table = db
                .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table not found");
            let column = table
                .column("name", &db, IdentifierCase::AsWritten)
                .expect("table lookup")
                .expect("Column not found");

            let col_ref = column;

            assert_eq!(<&_ as ColumnLike>::column_name(&col_ref), "name");
            assert_eq!(<&_ as ColumnLike>::data_type(&col_ref, &db), "TEXT");
            assert!(<&_ as ColumnLike>::is_nullable(&col_ref, &db).expect("is_nullable failed"));
            assert_eq!(<&_ as ColumnLike>::default_value(&col_ref).as_deref(), Some("'val'"));
            assert!(!<&_ as ColumnLike>::is_generated(&col_ref, &db));
            assert_eq!(<&_ as ColumnLike>::table(&col_ref, &db).table_name(), "users");
            assert_eq!(
                <&_ as ColumnLike>::column_id(&col_ref, &db).expect("column_id failed"),
                Some(1)
            );
            assert_eq!(
                <&_ as ColumnLike>::column_doc(&col_ref, &db).expect("column_doc failed"),
                None
            );
        }
    }

    mod arc_impl {
        use super::*;

        #[test]
        fn test_all_methods() {
            let sql = "CREATE TABLE products (id INT PRIMARY KEY, price INT DEFAULT 0);";
            let db = ParserDB::parse::<GenericDialect>(sql).expect("Failed to parse SQL");
            let table = db
                .table_by_target(TargetName::new("products", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("Table not found");
            let column = table
                .column("price", &db, IdentifierCase::AsWritten)
                .expect("table lookup")
                .expect("Column not found");

            let col_arc = Arc::new(column.clone());

            assert_eq!(<Arc<_> as ColumnLike>::column_name(&col_arc), "price");
            assert_eq!(<Arc<_> as ColumnLike>::data_type(&col_arc, &db), "INT");
            assert!(
                <Arc<_> as ColumnLike>::is_nullable(&col_arc, &db).expect("is_nullable failed")
            );
            assert_eq!(<Arc<_> as ColumnLike>::default_value(&col_arc).as_deref(), Some("0"));
            assert!(!<Arc<_> as ColumnLike>::is_generated(&col_arc, &db));
            assert_eq!(<Arc<_> as ColumnLike>::table(&col_arc, &db).table_name(), "products");
            assert_eq!(
                <Arc<_> as ColumnLike>::column_id(&col_arc, &db).expect("column_id failed"),
                Some(1)
            );
            assert_eq!(
                <Arc<_> as ColumnLike>::column_doc(&col_arc, &db).expect("column_doc failed"),
                None
            );
        }
    }

    mod is_textual_tests {
        use super::*;

        fn make_db(sql: &str) -> ParserDB {
            ParserDB::parse::<GenericDialect>(sql).expect("parse")
        }

        #[test]
        fn textual_for_text_and_varchar() {
            let db = make_db("CREATE TABLE t (a TEXT, b VARCHAR, c CHAR);");
            let t = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();
            for col_name in &["a", "b", "c"] {
                let col = t.column(col_name, &db, IdentifierCase::AsWritten).unwrap().unwrap();
                assert!(col.is_textual(&db), "{col_name} should be textual");
            }
        }

        #[test]
        fn textual_for_newly_classified_string_types() {
            let db =
                make_db("CREATE TABLE t (a TINYTEXT, b LONGTEXT, c CLOB, d NVARCHAR, e STRING);");
            let t = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();
            for col_name in &["a", "b", "c", "d", "e"] {
                let col = t.column(col_name, &db, IdentifierCase::AsWritten).unwrap().unwrap();
                assert!(col.is_textual(&db), "{col_name} should be textual");
            }
        }

        #[test]
        fn not_textual_for_int_bytea_interval() {
            let db = make_db("CREATE TABLE t (a INT, b BYTEA, c INTERVAL);");
            let t = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .unwrap();
            for col_name in &["a", "b", "c"] {
                let col = t.column(col_name, &db, IdentifierCase::AsWritten).unwrap().unwrap();
                assert!(!col.is_textual(&db), "{col_name} should not be textual");
            }
        }
    }
}
