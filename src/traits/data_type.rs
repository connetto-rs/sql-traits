//! Submodule providing a trait for describing declared SQL data types.

use alloc::{borrow::Cow, vec, vec::Vec};
use core::{fmt::Debug, num::NonZeroU32, ptr};

use crate::{
    traits::{ColumnCollation, DatabaseLike},
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

/// The type modifier a declared type applies, as `pg_attribute.atttypmod`
/// stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypeModifier {
    /// The declaration applies none, stored as `-1`.
    Unmodified,
    /// The declaration applies this modifier.
    Modified(i32),
}

impl TypeModifier {
    /// Returns the modifier, or `None` for none.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::traits::TypeModifier;
    ///
    /// assert_eq!(TypeModifier::Modified(9).value(), Some(9));
    /// assert_eq!(TypeModifier::Unmodified.value(), None);
    /// ```
    #[must_use]
    #[inline]
    pub const fn value(self) -> Option<i32> {
        match self {
            Self::Unmodified => None,
            Self::Modified(value) => Some(value),
        }
    }
}

/// What the declared length of a character type counts, as
/// [`DataTypeLike::character_length_unit`] answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CharacterLengthUnit {
    /// Characters, so `VARCHAR(10)` holds ten characters of any width.
    Characters,
    /// Bytes of the stored encoding, so `VARCHAR(10)` holds ten one-byte
    /// characters or fewer wider ones.
    Octets,
    /// UTF-16 code units, so a character outside the Basic Multilingual Plane
    /// takes two of them.
    Utf16CodeUnits,
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

/// Whether a value of an enumerated type holds one of its members or any
/// subset of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnumerationKind {
    /// `ENUM`, whose value is exactly one member.
    Enum,
    /// MySQL's `SET`, whose value is any subset of the members.
    Set,
}

/// The integer type ClickHouse stores the values of an enum's members in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnumWidth {
    /// `Enum8`, whose values are 8-bit signed integers.
    Enum8,
    /// `Enum16`, whose values are 16-bit signed integers.
    Enum16,
}

/// One member of an enumerated type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumerationMember<'db> {
    label: &'db str,
    value: Option<i16>,
}

impl<'db> EnumerationMember<'db> {
    /// Creates a member named `label` that the engine stores as `value`, or
    /// as its position when `value` is `None`.
    #[must_use]
    #[inline]
    pub const fn new(label: &'db str, value: Option<i16>) -> Self {
        Self { label, value }
    }

    /// Returns the label a value of the type spells the member with.
    #[must_use]
    #[inline]
    pub const fn label(self) -> &'db str {
        self.label
    }

    /// Returns the integer the engine stores for the member, or `None` where
    /// the engine identifies a member by its position.
    #[must_use]
    #[inline]
    pub const fn value(self) -> Option<i16> {
        self.value
    }
}

/// The members of an enumerated type, with the kind of its values and its
/// storage width, as [`DataTypeLike::enumeration`] answers them.
#[derive(Debug, Clone)]
pub struct Enumeration<I> {
    kind: EnumerationKind,
    width: Option<EnumWidth>,
    members: I,
}

impl<I> Enumeration<I> {
    /// Creates an enumeration of `kind` stored in `width`, holding `members`.
    #[must_use]
    #[inline]
    pub const fn new(kind: EnumerationKind, width: Option<EnumWidth>, members: I) -> Self {
        Self { kind, width, members }
    }

    /// Returns whether a value holds one member or a subset of them.
    #[must_use]
    #[inline]
    pub const fn kind(&self) -> EnumerationKind {
        self.kind
    }

    /// Returns the integer type ClickHouse stores the member values in, or
    /// `None` where the engine stores positions.
    #[must_use]
    #[inline]
    pub const fn width(&self) -> Option<EnumWidth> {
        self.width
    }

    /// Returns the members.
    #[must_use]
    #[inline]
    pub fn into_members(self) -> I {
        self.members
    }
}

/// What PostgreSQL's `pgoutput` does with a value of a type under the `binary`
/// option, as [`DataTypeLike::binary_send`] answers it.
#[derive(Debug)]
pub enum BinarySend<'db, T: ?Sized> {
    /// The type and every type its send function reaches have a send function,
    /// so the value travels in binary.
    Binary,
    /// The type has no send function of its own, so `pgoutput` sends the
    /// column as text.
    Text,
    /// The type has a send function, and that function reaches this type,
    /// which has none, so sending the value raises `no binary output function
    /// available for type` naming it.
    Fails(&'db T),
    /// The implementation cannot tell whether this type, reached on the way,
    /// has a send function, and no type found fails.
    Unknown(&'db T),
}

/// How a structured type holds its fields, as
/// [`DataTypeLike::structure_kind`] answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StructureKind {
    /// Every field holds a value at once, as in a `STRUCT` or a PostgreSQL
    /// composite type.
    Struct,
    /// One field at a time holds the value, as in DuckDB's `UNION`.
    Union,
    /// Snowflake's structured `OBJECT`.
    Object,
    /// ClickHouse's `Tuple`, whose fields may be unnamed.
    Tuple,
    /// ClickHouse's `Nested`, which stores each field as an array holding one
    /// element per nested row.
    Nested,
}

/// A field of a structured type, as [`DataTypeLike::structured_fields`]
/// answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredField<'db, T: ?Sized> {
    name: Option<&'db str>,
    name_is_quoted: bool,
    data_type: &'db T,
    nullable: bool,
    options: Vec<(Cow<'db, str>, Cow<'db, str>)>,
    collation: ColumnCollation<'db>,
}

impl<'db, T: ?Sized> StructuredField<'db, T> {
    /// Creates an unnamed field of `data_type`, admitting `NULL` when
    /// `nullable`, with no options and the database's default collation.
    #[must_use]
    #[inline]
    pub const fn new(data_type: &'db T, nullable: bool) -> Self {
        Self {
            name: None,
            name_is_quoted: false,
            data_type,
            nullable,
            options: Vec::new(),
            collation: ColumnCollation::DatabaseDefault,
        }
    }

    /// Names the field `name` as written, quoted when `quoted`.
    #[must_use]
    #[inline]
    pub const fn with_name(mut self, name: &'db str, quoted: bool) -> Self {
        self.name = Some(name);
        self.name_is_quoted = quoted;
        self
    }

    /// Gives the field its options, each a key and its value as text, in the
    /// order written.
    #[must_use]
    #[inline]
    pub fn with_options(mut self, options: Vec<(Cow<'db, str>, Cow<'db, str>)>) -> Self {
        self.options = options;
        self
    }

    /// Gives the field the collation it declares.
    #[must_use]
    #[inline]
    pub fn with_collation(mut self, collation: ColumnCollation<'db>) -> Self {
        self.collation = collation;
        self
    }

    /// Returns the field's name as written, or `None` for an unnamed field,
    /// such as one of ClickHouse's `Tuple(Int32, String)`.
    #[must_use]
    #[inline]
    pub const fn name(&self) -> Option<&'db str> {
        self.name
    }

    /// Returns whether the field's name is written quoted.
    #[must_use]
    #[inline]
    pub const fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    /// Returns the field's type, without the collation it declares.
    #[must_use]
    #[inline]
    pub const fn data_type(&self) -> &'db T {
        self.data_type
    }

    /// Returns whether the field admits `NULL`, which a written `NOT NULL`
    /// and a type that does not admit it refuse.
    #[must_use]
    #[inline]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// Returns the field's options, such as BigQuery's
    /// `OPTIONS(rounding_mode = 'ROUND_HALF_EVEN')`, each a key as written and
    /// its value as text, a string literal by its contents. An empty
    /// `OPTIONS()` answers none, as an omitted one does.
    #[must_use]
    #[inline]
    pub fn options(&self) -> &[(Cow<'db, str>, Cow<'db, str>)] {
        &self.options
    }

    /// Returns the collation the field declares. Its facts are those of the
    /// collation the name reached when the type was declared, as a column's
    /// are those of the collation it reached when its table was.
    #[must_use]
    #[inline]
    pub const fn collation(&self) -> &ColumnCollation<'db> {
        &self.collation
    }
}

/// The key and value types of a map type, as [`DataTypeLike::map_entry`]
/// answers them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry<'db, T: ?Sized> {
    key: &'db T,
    value: &'db T,
    value_nullable: bool,
}

impl<'db, T: ?Sized> MapEntry<'db, T> {
    /// Creates the entry of a map from `key` to `value`, whose value admits
    /// `NULL` when `value_nullable`.
    #[must_use]
    #[inline]
    pub const fn new(key: &'db T, value: &'db T, value_nullable: bool) -> Self {
        Self { key, value, value_nullable }
    }

    /// Returns the key type.
    #[must_use]
    #[inline]
    pub const fn key(&self) -> &'db T {
        self.key
    }

    /// Returns the value type.
    #[must_use]
    #[inline]
    pub const fn value(&self) -> &'db T {
        self.value
    }

    /// Returns whether a value admits `NULL`.
    #[must_use]
    #[inline]
    pub const fn value_is_nullable(&self) -> bool {
        self.value_nullable
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
    /// type, so it is owned. A name part PostgreSQL reads case-sensitively, or
    /// one that would otherwise read as a type keyword, is double-quoted, as
    /// `format_type` prints it.
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

    /// Returns the name of the type normalized by
    /// [`normalize_postgres_type`](crate::utils::normalize_postgres_type), so
    /// every spelling of one type answers one name. `SERIAL` answers `INT`,
    /// `BIGSERIAL` answers `BIGINT`, and PostgreSQL's print `integer[]`
    /// answers `INT[]` as a declared `INT4[]` does.
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
    /// The family of a domain's type is that of its [`Self::base_type`].
    /// The family is read from [`Self::normalized_name`], so every spelling of
    /// one type answers alike, `pg_catalog.timestamptz` as `timestamptz` does.
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
        classify_scalar_family(&self.normalized_name(database))
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
    /// MySQL binary log stores it, needs `n` to restore it. ClickHouse pads a
    /// `FixedString(n)` value the same way. `VARBINARY(n)` answers `None`, and
    /// so does `BINARY(n)` in a dialect where it bounds a varying length.
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

    /// Returns the most bytes a value of a binary type can hold, or `None`
    /// when the engine does not bound it by the declared type.
    ///
    /// `VARBINARY(n)`, `BINARY(n)`, BigQuery's `BYTES(n)` and ClickHouse's
    /// `FixedString(n)` answer `n` wherever the engine rejects or truncates a
    /// longer value. A bare type takes the engine's default length where it
    /// has one, which is 1 for `BINARY` and SQL Server's `VARBINARY`, 64000
    /// for Redshift's `VARBYTE` and 8388608 for Snowflake's `BINARY`, and
    /// answers `None` where it has none. MySQL creates `BLOB(n)` as the
    /// smallest of `TINYBLOB`, `BLOB`, `MEDIUMBLOB` and `LONGBLOB` holding `n`
    /// bytes, so it answers that type's 255, 65535, 16777215 or 4294967295.
    /// SQL Server's `VARBINARY(MAX)`, PostgreSQL's `BYTEA` and every binary
    /// type of an engine that stores any length, as SQLite, DuckDB, Hive and
    /// Spark do, answer `None`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::MsSqlDialect;
    ///
    /// let db = ParserDB::parse::<MsSqlDialect>(
    ///     "CREATE TABLE t (digest VARBINARY(32), flag VARBINARY, payload VARBINARY(MAX));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let lengths: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).max_binary_length(&db)).collect();
    /// assert_eq!(lengths, [Some(32), Some(1), None]);
    /// # Ok(())
    /// # }
    /// ```
    fn max_binary_length(&self, database: &Self::DB) -> Option<u64>;

    /// Returns the most bits a value of a bit-string type can hold, or `None`
    /// for a bit string of any length and for every other type.
    ///
    /// `BIT(n)` holds exactly `n` bits and `BIT VARYING(n)` or `VARBIT(n)` at
    /// most `n`, so each answers `n`, and a bare `BIT` is `BIT(1)`. A bare
    /// `BIT VARYING` holds any length and answers `None`. [`Self::name`] tells
    /// the fixed type from the varying one. SQL Server's `BIT`, which holds a
    /// boolean, and DuckDB's `BIT`, which holds a bit string of any length,
    /// answer `None`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (mask BIT(8), flag BIT, bits BIT VARYING(16), any_bits VARBIT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let lengths: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).max_bit_length(&db)).collect();
    /// assert_eq!(lengths, [Some(8), Some(1), Some(16), None]);
    /// # Ok(())
    /// # }
    /// ```
    fn max_bit_length(&self, database: &Self::DB) -> Option<u64>;

    /// Returns the most characters a value of the type can hold, or `None`
    /// when the engine does not bound it by the declared type.
    ///
    /// `VARCHAR(n)`, `CHARACTER VARYING(n)`, `CHAR(n)` and `CHARACTER(n)`
    /// answer `n` wherever the engine rejects or truncates a longer value, and
    /// so does `CLOB(n)` in the standard and in Teradata. A bare `CHAR` is
    /// `CHAR(1)`. A bare `VARCHAR` takes the engine's default length where it
    /// has one, as in SQL Server, and answers `None` where it is unbounded, as
    /// in PostgreSQL. `TEXT` and a length that is not an integer literal, such
    /// as `VARCHAR(MAX)`, answer `None`. So does every declared length in an
    /// engine that stores any length regardless, as SQLite, DuckDB and
    /// ClickHouse do. A length counted in bytes or in UTF-16 code units, as
    /// [`Self::character_length_unit`] tells, still bounds the characters,
    /// since every character takes at least one of them.
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

    /// Returns what the length [`Self::max_character_length`] answers
    /// counts, or `None` when it answers no length or the engine counts it in
    /// no single unit.
    ///
    /// A unit written in the declaration, as in `VARCHAR(10 OCTETS)`, answers
    /// as written. An unwritten unit is the engine's. PostgreSQL, MySQL,
    /// Snowflake, BigQuery and the standard count characters, Redshift counts
    /// bytes, SQL Server counts bytes in `VARCHAR(n)` and UTF-16 code units in
    /// `NVARCHAR(n)`, and Oracle counts bytes in `VARCHAR(n)` under its default
    /// length semantics and characters in `NCHAR(n)`. The generic dialect names
    /// no engine, so it answers only a written unit.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::MsSqlDialect;
    ///
    /// let db = ParserDB::parse::<MsSqlDialect>(
    ///     "CREATE TABLE notes (code VARCHAR(8), title NVARCHAR(100), body NVARCHAR(MAX));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)?.unwrap();
    /// let units: Vec<_> = table
    ///     .columns(&db)?
    ///     .map(|column| column.data_type(&db).character_length_unit(&db))
    ///     .collect();
    /// assert_eq!(
    ///     units,
    ///     [Some(CharacterLengthUnit::Octets), Some(CharacterLengthUnit::Utf16CodeUnits), None]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn character_length_unit(&self, database: &Self::DB) -> Option<CharacterLengthUnit>;

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

    /// Returns the time zone a temporal type names as a parameter, or `None`
    /// for a type that names none.
    ///
    /// ClickHouse's `DateTime64(p, 'zone')` answers `zone`, as written, apart
    /// from the fractional digits [`Self::fractional_seconds_precision`]
    /// answers. A type that names no zone, such as a bare `DateTime64(3)`
    /// reading values in the server's zone, answers `None`, and so does a
    /// zone-aware type such as `TIMESTAMP WITH TIME ZONE`, which names no zone
    /// of its own.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::ClickHouseDialect;
    ///
    /// let db = ParserDB::parse::<ClickHouseDialect>(
    ///     "CREATE TABLE events (at DateTime64(3, 'Europe/Rome'), seen DateTime64(3)) \
    ///      ENGINE = MergeTree ORDER BY at;",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("events", false), IdentifierCase::AsWritten)?.unwrap();
    /// let zones: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).time_zone(&db)).collect();
    /// assert_eq!(zones, [Some("Europe/Rome"), None]);
    /// # Ok(())
    /// # }
    /// ```
    fn time_zone<'db>(&'db self, database: &'db Self::DB) -> Option<&'db str>;

    /// Returns the declared number of decimal digits, including fractional
    /// digits, or `None` when the type fixes no integral decimal precision.
    ///
    /// `NUMERIC(p, s)` and `DECIMAL(p)` answer `p`, and a bare decimal takes
    /// its engine's default. MySQL's `FLOAT(p, s)` and `DOUBLE(p, s)` bound
    /// their values to `p` decimal digits and answer `p`. Integer, other
    /// floating-point and non-numeric types, unconstrained PostgreSQL
    /// `NUMERIC`, Oracle `NUMBER`, and every SQLite type answer `None`.
    /// BigQuery's fractional bare `BIGNUMERIC` precision is not modeled and
    /// answers `None`.
    fn numeric_precision(&self, database: &Self::DB) -> Option<u64>;

    /// Returns how many digits a value of a decimal type keeps to the right
    /// of the decimal point, or `None` when the type fixes no scale.
    ///
    /// `NUMERIC(p, s)` and `DECIMAL(p, s)` answer `s`, and `NUMERIC(p)` answers
    /// `0`. A bare type answers the engine's default, which is 0 in MySQL, SQL
    /// Server and the standard, 3 in DuckDB and 9 in BigQuery. A bare `NUMERIC`
    /// in PostgreSQL holds any scale and answers `None`, and so does a bare
    /// `NUMBER` in Oracle. A negative scale, which PostgreSQL accepts and which
    /// rounds a value to tens or hundreds, answers as written. MySQL rounds a
    /// value of `FLOAT(p, s)` or `DOUBLE(p, s)` to `s` decimal places, so these
    /// answer `s`. Integer, other floating-point and non-numeric types answer
    /// `None`. So does every declared type in SQLite, which stores a decimal
    /// value as an integer or a float whatever scale it declares.
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

    /// Returns whether a numeric type holds only values of zero and above, or
    /// `None` for a type outside the integer, floating-point and decimal
    /// families.
    ///
    /// MySQL's `UNSIGNED` integers, decimals and floating types answer
    /// `Some(true)`, and so does every numeric type MySQL declares with
    /// `ZEROFILL`, which implies `UNSIGNED`, or as `SERIAL`, which is a
    /// `BIGINT UNSIGNED`. So do DuckDB's `UTINYINT`, `USMALLINT`, `UBIGINT`
    /// and `UHUGEINT` and ClickHouse's `UInt8` through `UInt256`. Every other
    /// numeric type answers `Some(false)`, every type in SQLite included,
    /// since SQLite stores a negative value whatever the declared type says. A
    /// domain answers `None`, as [`Self::scalar_family`] does, and its
    /// signedness is that of its [`Self::base_type`].
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::MySqlDialect;
    ///
    /// let db = ParserDB::parse::<MySqlDialect>(
    ///     "CREATE TABLE t (id INT, hits INT UNSIGNED, price DECIMAL(10,2) UNSIGNED, label VARCHAR(10));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let unsigned: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).is_unsigned(&db)).collect();
    /// assert_eq!(unsigned, [Some(false), Some(true), Some(true), None]);
    /// # Ok(())
    /// # }
    /// ```
    fn is_unsigned(&self, database: &Self::DB) -> Option<bool>;

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

    /// Returns the type modifier the type applies, as
    /// `pg_attribute.atttypmod` stores it, or `None` when it is unknown.
    ///
    /// `VARCHAR(5)` applies `9`, the length and the four header bytes, and
    /// `TEXT` applies none. The default reads the modifier from the reported
    /// parameters where they tell it apart, and answers `None` for time and
    /// interval types, whose reported precision a declaration may leave
    /// implicit, and for types outside `pg_catalog`, so an implementation
    /// that stores the modifier should override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (code VARCHAR(5), stamp TIMESTAMP(3), note TEXT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let modifiers: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).typmod(&db)).collect();
    /// assert_eq!(
    ///     modifiers,
    ///     [
    ///         Some(TypeModifier::Modified(9)),
    ///         Some(TypeModifier::Modified(3)),
    ///         Some(TypeModifier::Unmodified),
    ///     ]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn typmod(&self, database: &Self::DB) -> Option<TypeModifier> {
        crate::utils::type_modifier::derived(self, database)
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

    /// Returns whether a value of the type itself may be `NULL`.
    ///
    /// ClickHouse admits `NULL` only in a type wrapped in `Nullable` and in
    /// `Variant` and `Dynamic`, which hold `NULL` of their own. So `Int32`
    /// answers `false`, while `Nullable(Int32)`,
    /// `LowCardinality(Nullable(String))` and `Variant(UInt64, String)` answer
    /// `true`. Every other engine lets a value of any type be `NULL` and keeps
    /// `NOT NULL` on the column, field or element that holds it, so every type
    /// answers `true` there.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::{ClickHouseDialect, PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<ClickHouseDialect>(
    ///     "CREATE TABLE t (id Int32, note Nullable(String), tag LowCardinality(Nullable(String))) \
    ///      ENGINE = MergeTree ORDER BY id;",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let admits: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).admits_null(&db)).collect();
    /// assert_eq!(admits, [false, true, true]);
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT NOT NULL);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let id = table.column("id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// assert!(id.data_type(&db).admits_null(&db));
    /// # Ok(())
    /// # }
    /// ```
    fn admits_null(&self, database: &Self::DB) -> bool;

    /// Returns the type a domain is defined over, followed through every
    /// domain in between, or `None` for a type that names no domain.
    ///
    /// PostgreSQL compares, sorts and indexes a value of a domain as a value
    /// of this base type, so a key over a domain over `interval` treats
    /// `'1 day'` and `'24 hours'` as one value as an `interval` key does. The
    /// base type answers every fact this trait answers for a column type. It
    /// is never itself a domain, and it may be an array, whose
    /// [`Self::array_element`] may name a domain in turn. An array of a domain
    /// names no domain, so it answers `None` here while its element answers
    /// the domain's base type.
    ///
    /// The default answers `None`, so an implementation over a backend with
    /// domains must override it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{prelude::*, utils::scalar_family::ScalarFamily};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE DOMAIN span AS interval;
    /// CREATE DOMAIN short_span AS span;
    /// CREATE TABLE t (took short_span, spans span[], n INT);
    /// ",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let took = table.column("took", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let spans = table.column("spans", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let n = table.column("n", &db, IdentifierCase::AsWritten)?.unwrap();
    ///
    /// let base = took.data_type(&db).base_type(&db).unwrap();
    /// assert_eq!(base.scalar_family(&db), Some(ScalarFamily::Interval));
    /// let element = spans.data_type(&db).array_element(&db).unwrap();
    /// assert_eq!(element.base_type(&db).unwrap().name(&db), "INTERVAL");
    /// assert!(n.data_type(&db).base_type(&db).is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn base_type<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Self> {
        None
    }

    /// Returns whether the type has a binary send function of its own, as
    /// PostgreSQL's `pg_type.typsend` records it, or `None` when the
    /// implementation cannot tell, as for a type an extension creates.
    ///
    /// A domain answers its base type's, since PostgreSQL copies the send
    /// function into the domain. Every array, range, multirange, composite and
    /// enumeration type has one, and so does every base type declared with
    /// `SEND`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE DOMAIN grant_item AS aclitem;
    ///      CREATE TABLE t (id INT, acl aclitem, acls aclitem[], item grant_item, shape box2d);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let sends: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).send_function(&db)).collect();
    /// assert_eq!(sends, [Some(true), Some(false), Some(true), Some(false), None]);
    /// # Ok(())
    /// # }
    /// ```
    fn send_function(&self, database: &Self::DB) -> Option<bool>;

    /// Returns the subtype of a range type, or of the ranges a multirange type
    /// holds, or `None` for every other type, a domain over a range included.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TYPE floatrange AS RANGE (subtype = float8);
    ///      CREATE TABLE t (span int4range, spans tstzmultirange, floats floatmultirange, n INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let subtypes: Vec<_> = table
    ///     .columns(&db)?
    ///     .map(|column| column.data_type(&db).range_subtype(&db).map(|subtype| subtype.name(&db)))
    ///     .collect();
    /// assert_eq!(
    ///     subtypes,
    ///     [Some("INT4".into()), Some("TIMESTAMPTZ".into()), Some("FLOAT8".into()), None]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn range_subtype<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Self>;

    /// Returns how a structured type holds its fields, or `None` for a type
    /// that holds none, a domain over a composite included.
    ///
    /// An inline `STRUCT` or ClickHouse `Tuple`, a DuckDB `UNION`, a Snowflake
    /// `OBJECT`, a ClickHouse `Nested` and a PostgreSQL composite type answer,
    /// and [`Self::structured_fields`] answers their fields.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::DuckDbDialect;
    ///
    /// let db = ParserDB::parse::<DuckDbDialect>(
    ///     "CREATE TABLE t (s STRUCT(code INTEGER), u UNION(code INTEGER, label VARCHAR), n INTEGER);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let kinds: Vec<_> =
    ///     table.columns(&db)?.map(|column| column.data_type(&db).structure_kind(&db)).collect();
    /// assert_eq!(kinds, [Some(StructureKind::Struct), Some(StructureKind::Union), None]);
    /// # Ok(())
    /// # }
    /// ```
    fn structure_kind(&self, database: &Self::DB) -> Option<StructureKind>;

    /// Returns the fields of a structured type in declaration order, or
    /// `None` for every type [`Self::structure_kind`] answers `None` for.
    ///
    /// Each field answers its name where one is written, its type, whether
    /// it admits `NULL`, its options and its collation. A field's type
    /// answers every fact this trait answers for a column type, so the
    /// precision of `STRUCT<amount NUMERIC(10, 2)>`'s field is that type's
    /// [`Self::numeric_precision`]. A PostgreSQL composite type answers its
    /// attributes the same way.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TYPE pair AS (left_side INT, right_side TEXT COLLATE \"C\");
    ///      CREATE TABLE t (p pair, n INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let pair = table.column("p", &db, IdentifierCase::AsWritten)?.unwrap().data_type(&db);
    /// let fields: Vec<_> = pair
    ///     .structured_fields(&db)
    ///     .unwrap()
    ///     .map(|field| (field.name(), field.data_type().name(&db), field.is_nullable()))
    ///     .collect();
    /// assert_eq!(
    ///     fields,
    ///     [(Some("left_side"), "INT".into(), true), (Some("right_side"), "TEXT".into(), true)]
    /// );
    /// let right_side = pair.structured_fields(&db).unwrap().nth(1).unwrap();
    /// let ColumnCollation::Named(collation) = right_side.collation() else {
    ///     panic!("a declared COLLATE names the collation");
    /// };
    /// assert_eq!(collation.name().name(), "C");
    /// let n = table.column("n", &db, IdentifierCase::AsWritten)?.unwrap().data_type(&db);
    /// assert!(n.structured_fields(&db).is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn structured_fields<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<impl Iterator<Item = StructuredField<'db, Self>>>;

    /// Returns the key and value types of a map type, or `None` for every
    /// other type.
    ///
    /// ClickHouse's `Map(K, V)`, Snowflake's `MAP(K, V)` and Hive's
    /// `MAP<K, V>` answer. The value answers whether it admits `NULL`, which
    /// Snowflake's `MAP(K, V NOT NULL)` and a ClickHouse value not wrapped in
    /// `Nullable` refuse.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::ClickHouseDialect;
    ///
    /// let db = ParserDB::parse::<ClickHouseDialect>(
    ///     "CREATE TABLE t (id UInt64, tags Map(String, Nullable(UInt64))) \
    ///      ENGINE = MergeTree ORDER BY id;",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let tags = table.column("tags", &db, IdentifierCase::AsWritten)?.unwrap().data_type(&db);
    /// let entry = tags.map_entry(&db).unwrap();
    /// assert_eq!(entry.key().name(&db), "STRING");
    /// assert_eq!(entry.value().name(&db), "UINT64");
    /// assert!(entry.value_is_nullable());
    /// let id = table.column("id", &db, IdentifierCase::AsWritten)?.unwrap().data_type(&db);
    /// assert!(id.map_entry(&db).is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn map_entry<'db>(&'db self, database: &'db Self::DB) -> Option<MapEntry<'db, Self>>;

    /// Returns the members of an enumerated type with the kind of its values
    /// and its storage width, or `None` for every other type, a domain over an
    /// enumerated type and an array of one included.
    ///
    /// MySQL's `ENUM` and `SET`, DuckDB's inline `ENUM` and a PostgreSQL enum
    /// type identify a member by its position, so they answer the members in
    /// declaration order without values. A PostgreSQL column answers the
    /// labels of the `CREATE TYPE ... AS ENUM` its type names, with every
    /// `ALTER TYPE ... ADD VALUE` applied. ClickHouse stores a member as its
    /// value, so a ClickHouse enum answers each member's value, written or
    /// assigned, in the order of the values, and its width. A member written
    /// without a value takes 1 when it is first and the value of the first
    /// member plus its position otherwise, and a bare `Enum` takes `Enum8`
    /// when every value fits it. So `Enum8('ready', 'busy')` and
    /// `Enum('busy' = 2, 'ready' = 1)` answer alike. A ClickHouse enum whose
    /// values ClickHouse refuses, one not an integer or out of the width's
    /// range or a mix of written and omitted values after the first member,
    /// answers `None`. A ClickHouse `Nullable` or `LowCardinality` answers the
    /// enumeration it wraps.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TYPE status AS ENUM ('pending', 'ready');
    ///      CREATE TABLE t (s status, n INT);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let status = table.column("s", &db, IdentifierCase::AsWritten)?.unwrap().data_type(&db);
    /// let enumeration = status.enumeration(&db).unwrap();
    /// assert_eq!(enumeration.kind(), EnumerationKind::Enum);
    /// assert_eq!(enumeration.width(), None);
    /// let labels: Vec<_> = enumeration.into_members().map(EnumerationMember::label).collect();
    /// assert_eq!(labels, ["pending", "ready"]);
    /// let n = table.column("n", &db, IdentifierCase::AsWritten)?.unwrap().data_type(&db);
    /// assert!(n.enumeration(&db).is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn enumeration<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<Enumeration<impl Iterator<Item = EnumerationMember<'db>>>>;

    /// Returns what `pgoutput` does with a value of the type when a logical
    /// replication stream asks for the `binary` format.
    ///
    /// `pgoutput` sends a column in binary when its type has a send function
    /// and as text otherwise. The send function of an array, a range, a
    /// multirange or a composite calls the send function of its element,
    /// subtype, range or attributes, at any depth and through domains, and
    /// raises an error on one that has none. The stream then fails at the
    /// same record on every reconnect. This walks [`Self::send_function`]
    /// through [`Self::base_type`], [`Self::array_element`],
    /// [`Self::range_subtype`] and [`Self::structured_fields`] in the order
    /// PostgreSQL does, so [`BinarySend::Fails`] names the type its error
    /// names.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE DOMAIN grant_item AS aclitem;
    ///      CREATE DOMAIN grant_items AS aclitem[];
    ///      CREATE TABLE t (id INT, acl aclitem, item grant_item, items grant_items, shape box2d);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let sends: Vec<String> = table
    ///     .columns(&db)?
    ///     .map(|column| {
    ///         match column.data_type(&db).binary_send(&db) {
    ///             BinarySend::Binary => "binary".to_owned(),
    ///             BinarySend::Text => "text".to_owned(),
    ///             BinarySend::Fails(failing) => format!("fails on {}", failing.name(&db)),
    ///             BinarySend::Unknown(unknown) => format!("unknown {}", unknown.name(&db)),
    ///         }
    ///     })
    ///     .collect();
    /// assert_eq!(sends, ["binary", "text", "text", "fails on aclitem", "unknown box2d"]);
    /// # Ok(())
    /// # }
    /// ```
    fn binary_send<'db>(&'db self, database: &'db Self::DB) -> BinarySend<'db, Self> {
        match self.send_function(database) {
            None => return BinarySend::Unknown(self),
            Some(false) => return BinarySend::Text,
            Some(true) => {}
        }
        // Pre-order and in attribute order, as PostgreSQL's send functions call
        // each other.
        let mut pending = sent_through(self, database);
        let mut expanded = vec![self];
        let mut unknown = None;
        while let Some(sent) = pending.pop() {
            match sent.send_function(database) {
                Some(false) => return BinarySend::Fails(sent),
                None => {
                    unknown.get_or_insert(sent);
                }
                Some(true) if !expanded.iter().any(|seen| ptr::eq(*seen, sent)) => {
                    expanded.push(sent);
                    pending.extend(sent_through(sent, database));
                }
                Some(true) => {}
            }
        }
        unknown.map_or(BinarySend::Binary, BinarySend::Unknown)
    }
}

/// The types the send function of `sent` calls the send functions of, last
/// first, so popping them visits them in order.
fn sent_through<'db, T: DataTypeLike + ?Sized>(sent: &'db T, database: &'db T::DB) -> Vec<&'db T> {
    let structure = sent.base_type(database).unwrap_or(sent);
    let mut reached: Vec<&'db T> = structure
        .array_element(database)
        .into_iter()
        .chain(structure.range_subtype(database))
        .chain(
            structure
                .structured_fields(database)
                .into_iter()
                .flatten()
                .map(|field| field.data_type()),
        )
        .collect();
    reached.reverse();
    reached
}
