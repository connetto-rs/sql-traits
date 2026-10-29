//! Submodule providing a trait for describing declared SQL data types.

use alloc::borrow::Cow;
use core::{fmt::Debug, num::NonZeroU32};

use crate::{
    traits::DatabaseLike,
    utils::{
        normalize_postgres_type_cow,
        scalar_family::{ScalarFamily, scalar_family as classify_scalar_family},
    },
};

/// The fields an interval type restricts its values to, as PostgreSQL spells
/// them after `INTERVAL`.
///
/// A value keeps only the fields named, so `INTERVAL YEAR TO MONTH` drops the
/// days and the time of `'1 year 2 months 3 days 04:05:06'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntervalFields {
    /// `YEAR`.
    Year,
    /// `MONTH`.
    Month,
    /// `DAY`.
    Day,
    /// `HOUR`.
    Hour,
    /// `MINUTE`.
    Minute,
    /// `SECOND`.
    Second,
    /// `YEAR TO MONTH`.
    YearToMonth,
    /// `DAY TO HOUR`.
    DayToHour,
    /// `DAY TO MINUTE`.
    DayToMinute,
    /// `DAY TO SECOND`.
    DayToSecond,
    /// `HOUR TO MINUTE`.
    HourToMinute,
    /// `HOUR TO SECOND`.
    HourToSecond,
    /// `MINUTE TO SECOND`.
    MinuteToSecond,
}

impl IntervalFields {
    /// Returns whether a value restricted to these fields keeps seconds, and
    /// so the fraction of a second a precision bounds.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::traits::IntervalFields;
    ///
    /// assert!(IntervalFields::DayToSecond.holds_seconds());
    /// assert!(!IntervalFields::YearToMonth.holds_seconds());
    /// ```
    #[must_use]
    #[inline]
    pub const fn holds_seconds(self) -> bool {
        matches!(self, Self::Second | Self::DayToSecond | Self::HourToSecond | Self::MinuteToSecond)
    }
}

/// Which of pgvector's vector types a column holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VectorKind {
    /// `vector`, whose elements are single-precision floats.
    Vector,
    /// `halfvec`, whose elements are half-precision floats.
    HalfVec,
    /// `sparsevec`, which stores only its nonzero elements with their
    /// positions.
    SparseVec,
}

/// A pgvector type with its declared dimension count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VectorType {
    kind: VectorKind,
    dimensions: Option<u32>,
}

impl VectorType {
    /// Creates a vector type of `kind` holding `dimensions` elements, or
    /// any number of them when `dimensions` is `None`.
    #[must_use]
    #[inline]
    pub const fn new(kind: VectorKind, dimensions: Option<u32>) -> Self {
        Self { kind, dimensions }
    }

    /// Returns which pgvector type this is.
    #[must_use]
    #[inline]
    pub const fn kind(self) -> VectorKind {
        self.kind
    }

    /// Returns the declared dimension count, `3` for `vector(3)`, or `None`
    /// for a bare `vector`, whose values may each have their own.
    #[must_use]
    #[inline]
    pub const fn dimensions(self) -> Option<u32> {
        self.dimensions
    }
}

/// A trait for declared SQL data types, the type of a column or of an array
/// element.
///
/// Every fact a type answers may depend on the database holding it, since an
/// engine gives a bare `NUMERIC` or `TIMESTAMP` its own default, and an
/// extension type exists only where the extension was created.
pub trait DataTypeLike: Debug + Send + Sync {
    /// The type of the database that this data type belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the type.
    ///
    /// The name is a borrowed canonical token whenever the implementation can
    /// name one literally. An array type has to be assembled from its element
    /// type, so it is owned.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, name TEXT, score REAL);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let names: Vec<_> = table.columns(&db)?.map(|column| column.data_type(&db).name(&db)).collect();
    /// assert_eq!(names, ["INT", "TEXT", "REAL"]);
    /// # Ok(())
    /// # }
    /// ```
    fn name<'db>(&'db self, database: &'db Self::DB) -> Cow<'db, str>;

    /// Returns the name of the type with PostgreSQL's aliases folded, so
    /// `SERIAL` answers `INT` and `BIGSERIAL` answers `BIGINT`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, serial_id SERIAL, big_id BIGSERIAL, small_id SMALLSERIAL);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let names: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).normalized_name(&db)).collect();
    /// assert_eq!(names, ["INT", "INT", "BIGINT", "SMALLINT"]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn normalized_name<'db>(&'db self, database: &'db Self::DB) -> Cow<'db, str> {
        normalize_postgres_type_cow(self.name(database))
    }

    /// Returns the builtin scalar family of the type, or `None` for a type
    /// outside every family, such as an enumeration, a domain or an array.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{prelude::*, utils::scalar_family::ScalarFamily};
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE measurements (id BIGINT, value NUMERIC(20, 4), label VARCHAR(255), span INTERVAL);",
    /// )?;
    /// let table = db
    ///     .table_by_target(TargetName::new("measurements", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let families: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).scalar_family(&db)).collect();
    /// assert_eq!(
    ///     families,
    ///     [
    ///         Some(ScalarFamily::Int),
    ///         Some(ScalarFamily::Decimal),
    ///         Some(ScalarFamily::String),
    ///         Some(ScalarFamily::Interval),
    ///     ]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn scalar_family(&self, database: &Self::DB) -> Option<ScalarFamily> {
        classify_scalar_family(self.name(database).as_ref())
    }

    /// Returns whether the type belongs to the string family.
    ///
    /// The family is the one the schema fingerprint records, so a type that
    /// fingerprints as a string reads as textual here and the two cannot
    /// drift apart.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_table (id INT, name TEXT, description VARCHAR, note CLOB);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("my_table", false), IdentifierCase::AsWritten)?.unwrap();
    /// let textual: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).is_textual(&db)).collect();
    /// assert_eq!(textual, [false, true, true, true]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_textual(&self, database: &Self::DB) -> bool {
        self.scalar_family(database) == Some(ScalarFamily::String)
    }

    /// Returns the width in bytes of a fixed-length binary type, or `None`
    /// when the type is not a fixed-length binary type.
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
    ///     table.columns(&db)?.map(|column| column.data_type(&db).fixed_binary_length(&db)).collect();
    /// assert_eq!(widths, [Some(16), Some(1), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn fixed_binary_length(&self, _database: &Self::DB) -> Option<u64> {
        None
    }

    /// Returns the most characters a value of the type can hold, or `None`
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
    ///     table.columns(&db)?.map(|column| column.data_type(&db).max_character_length(&db)).collect();
    /// assert_eq!(lengths, [Some(100), Some(8), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn max_character_length(&self, _database: &Self::DB) -> Option<u64> {
        None
    }

    /// Returns how many fractional digits of a second a value of a time,
    /// timestamp or interval type keeps, or `None` when the type holds no
    /// seconds.
    ///
    /// A written precision such as `TIMESTAMP(0)` or `TIME(3)` answers the
    /// digits the engine keeps for it, and a bare type answers the engine's
    /// default, which is 6 in PostgreSQL, 0 in MySQL and 9 in Snowflake. An
    /// engine with a fixed resolution answers that resolution whatever was
    /// written, as BigQuery does with microseconds. The engine rounds or
    /// truncates a value with more digits, so `Some(0)` marks a type that
    /// cannot hold the fraction of a second `now()` returns. A type without a
    /// time of day, such as a standard `DATE`, answers `None`. Oracle's `DATE`
    /// keeps whole seconds and answers `Some(0)`. An interval answers the
    /// digits it keeps of its seconds field, so `INTERVAL DAY TO SECOND(3)`
    /// answers `Some(3)`, while `INTERVAL YEAR TO MONTH`, which keeps no
    /// seconds, answers `None`.
    ///
    /// The default answers `None`, so an implementation over a backend with a
    /// time, timestamp or interval type must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE events (at timestamptz(0), seen timestamp, starts time(2), day date, \
    ///      took interval second(3), spans interval year to month);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("events", false), IdentifierCase::AsWritten)?.unwrap();
    /// let precisions: Vec<_> = table
    ///     .columns(&db)?
    ///     .map(|column| column.data_type(&db).fractional_seconds_precision(&db))
    ///     .collect();
    /// assert_eq!(precisions, [Some(0), Some(6), Some(2), None, Some(3), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn fractional_seconds_precision(&self, _database: &Self::DB) -> Option<u64> {
        None
    }

    /// Returns how many digits a value of a decimal type keeps to the right
    /// of the decimal point, or `None` when the type fixes no scale.
    ///
    /// `NUMERIC(p, s)` and `DECIMAL(p, s)` answer `s`, and `NUMERIC(p)` answers
    /// `0`. A bare type answers the engine's default, which is 0 in MySQL, SQL
    /// Server and the standard, 3 in DuckDB and 9 in BigQuery. A bare `NUMERIC`
    /// in PostgreSQL holds any scale and answers `None`, and so does a bare
    /// `NUMBER` in Oracle. A negative scale, which PostgreSQL accepts and which
    /// rounds a value to tens or hundreds, answers as written. Integer,
    /// floating-point and non-numeric types answer `None`. So does every
    /// declared type in SQLite, which stores a decimal value as an integer or a
    /// float whatever scale it declares.
    ///
    /// The default answers `None`, so an implementation over a backend with a
    /// decimal type must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE amounts (id INT, amount NUMERIC(10,2), rate DECIMAL(5,4), bare NUMERIC);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("amounts", false), IdentifierCase::AsWritten)?.unwrap();
    /// let scales: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).numeric_scale(&db)).collect();
    /// assert_eq!(scales, [None, Some(2), Some(4), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn numeric_scale(&self, _database: &Self::DB) -> Option<i64> {
        None
    }

    /// Returns the fields an interval type restricts its values to, or `None`
    /// for an interval that keeps every field and for every other type.
    ///
    /// [`Self::scalar_family`] answers [`ScalarFamily::Interval`] for every
    /// interval, so the two together tell an unrestricted interval from a
    /// type that is not one.
    ///
    /// The default answers `None`, so an implementation over a backend with
    /// restricted interval types must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (any_span INTERVAL, short_span INTERVAL DAY TO SECOND, n INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let fields: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).interval_fields(&db)).collect();
    /// assert_eq!(fields, [None, Some(IntervalFields::DayToSecond), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn interval_fields(&self, _database: &Self::DB) -> Option<IntervalFields> {
        None
    }

    /// Returns the pgvector type this type names, with its declared dimension
    /// count, or `None` for every other type.
    ///
    /// pgvector's `vector`, `halfvec` and `sparsevec` exist only in a database
    /// that holds the `vector` extension, so a type of one of these names
    /// answers only when the database holds `CREATE EXTENSION vector` and the
    /// name resolves to the schema the extension was created in. A schema
    /// that declares `vector(3)` without the extension answers `None`, and so
    /// does a type of the same name in another schema.
    ///
    /// The default answers `None`, so an implementation over a backend with
    /// pgvector must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE EXTENSION vector;
    ///      CREATE TABLE items (embedding vector(3), any_length vector, half halfvec(3));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("items", false), IdentifierCase::AsWritten)?.unwrap();
    /// let vectors: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).vector(&db)).collect();
    /// assert_eq!(
    ///     vectors,
    ///     [
    ///         Some(VectorType::new(VectorKind::Vector, Some(3))),
    ///         Some(VectorType::new(VectorKind::Vector, None)),
    ///         Some(VectorType::new(VectorKind::HalfVec, Some(3))),
    ///     ]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn vector(&self, _database: &Self::DB) -> Option<VectorType> {
        None
    }

    /// Returns the element type of an array type, or `None` for a type that
    /// is not an array and for an array declared without an element type,
    /// such as Snowflake's bare `ARRAY`.
    ///
    /// The element of a multidimensional array is the type its innermost
    /// values hold, so `INT[][]` answers `INT`, as PostgreSQL's catalog does.
    /// The element answers every fact this trait answers for a column type,
    /// so the scale of `NUMERIC(10,2)[]` is the element's
    /// [`Self::numeric_scale`].
    ///
    /// The default answers `None`, so an implementation over a backend with
    /// array types must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{prelude::*, utils::scalar_family::ScalarFamily};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (tags TEXT[], amounts NUMERIC(10,2)[], n INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let tags = table.column("tags", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let amounts = table.column("amounts", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let n = table.column("n", &db, IdentifierCase::AsWritten)?.unwrap();
    ///
    /// let tag = tags.data_type(&db).array_element(&db).unwrap();
    /// assert_eq!(tag.scalar_family(&db), Some(ScalarFamily::String));
    /// let amount = amounts.data_type(&db).array_element(&db).unwrap();
    /// assert_eq!(amount.numeric_scale(&db), Some(2));
    /// assert!(n.data_type(&db).array_element(&db).is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn array_element<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Self> {
        None
    }

    /// Returns how many dimensions an array type declares, or `None` for a
    /// type that is not an array.
    ///
    /// `INT[]` and `INT ARRAY` declare one dimension and `INT[][]` declares
    /// two. PostgreSQL enforces neither the count nor a declared size, and
    /// stores a value of any dimensions in any array column, so this is the
    /// count as written.
    ///
    /// The default answers `None`, so an implementation over a backend with
    /// array types must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use core::num::NonZeroU32;
    ///
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (a INT[], b TEXT ARRAY, grid INT[][], n INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let dimensions: Vec<_> = table
    ///     .columns(&db)?
    ///     .map(|column| column.data_type(&db).array_dimensions(&db).map(NonZeroU32::get))
    ///     .collect();
    /// assert_eq!(dimensions, [Some(1), Some(1), Some(2), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn array_dimensions(&self, _database: &Self::DB) -> Option<NonZeroU32> {
        None
    }
}
