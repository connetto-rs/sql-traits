//! [`DialectLike`] implementation for the `sqlparser`-backed
//! [`ParserDB`].
//!
//! This module exposes the closed [`SqlparserDialect`] enum plus its
//! [`DialectLike`] impl. Type predicates (`is_bool`, `is_uuid`) match on the
//! raw [`sqlparser::ast::DataType`] and return a
//! [`TypeMatch`], distinguishing an unambiguous
//! DDL declaration from a plausible-under-this-dialect one (e.g. SQLite
//! `INTEGER` used as a boolean by convention, MySQL `CHAR(36)` used to hold
//! a UUID).

use core::any::TypeId;

use sqlparser::{
    ast::{
        CharacterLength, ColumnDef, CreateTable, DataType, ObjectName, ObjectNamePart,
        TimezoneInfo, TypeModifier,
    },
    dialect::{
        AnsiDialect, BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect,
        HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect, PostgreSqlDialect,
        RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect, SparkSqlDialect, TeradataDialect,
    },
};

use crate::{
    structs::{ParserDB, TableAttribute},
    traits::{DialectLike, TypeMatch},
};

/// Closed enumeration of every stock `sqlparser` dialect.
///
/// [`ParserDB`] stores one of these
/// variants so that dialect-conditional predicates on
/// [`crate::traits::DatabaseLike`] can
/// dispatch through a single concrete `Dialect` type. The alternative,
/// a generic on `ParserDB<D>`, is blocked by Rust coherence rules on the
/// AST-node impls. Unknown or custom `sqlparser::dialect::Dialect`
/// implementations classify as [`SqlparserDialect::Generic`], which behaves
/// permissively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum SqlparserDialect {
    /// ANSI SQL dialect.
    Ansi,
    /// BigQuery dialect.
    BigQuery,
    /// ClickHouse dialect.
    ClickHouse,
    /// Databricks dialect.
    Databricks,
    /// DuckDB dialect.
    DuckDb,
    /// Permissive generic dialect (accepts a superset of SQL vocabularies).
    #[default]
    Generic,
    /// Hive dialect.
    Hive,
    /// Microsoft SQL Server dialect.
    MsSql,
    /// MySQL and MariaDB dialect.
    MySql,
    /// Oracle dialect.
    Oracle,
    /// PostgreSQL dialect.
    PostgreSql,
    /// Amazon Redshift dialect.
    Redshift,
    /// Snowflake dialect.
    Snowflake,
    /// Apache Spark SQL dialect.
    Spark,
    /// SQLite dialect.
    SQLite,
    /// Teradata dialect.
    Teradata,
}

impl SqlparserDialect {
    /// Classifies a `sqlparser::dialect::Dialect` type into its
    /// [`SqlparserDialect`] variant via [`TypeId`] equality.
    ///
    /// Unknown dialect types fall back to [`SqlparserDialect::Generic`],
    /// which is the permissive superset behavior.
    #[must_use]
    #[inline]
    pub fn of<D: Dialect + 'static>() -> Self {
        let tid = TypeId::of::<D>();
        if tid == TypeId::of::<AnsiDialect>() {
            Self::Ansi
        } else if tid == TypeId::of::<BigQueryDialect>() {
            Self::BigQuery
        } else if tid == TypeId::of::<ClickHouseDialect>() {
            Self::ClickHouse
        } else if tid == TypeId::of::<DatabricksDialect>() {
            Self::Databricks
        } else if tid == TypeId::of::<DuckDbDialect>() {
            Self::DuckDb
        } else if tid == TypeId::of::<HiveDialect>() {
            Self::Hive
        } else if tid == TypeId::of::<MsSqlDialect>() {
            Self::MsSql
        } else if tid == TypeId::of::<MySqlDialect>() {
            Self::MySql
        } else if tid == TypeId::of::<OracleDialect>() {
            Self::Oracle
        } else if tid == TypeId::of::<PostgreSqlDialect>() {
            Self::PostgreSql
        } else if tid == TypeId::of::<RedshiftSqlDialect>() {
            Self::Redshift
        } else if tid == TypeId::of::<SnowflakeDialect>() {
            Self::Snowflake
        } else if tid == TypeId::of::<SparkSqlDialect>() {
            Self::Spark
        } else if tid == TypeId::of::<SQLiteDialect>() {
            Self::SQLite
        } else if tid == TypeId::of::<TeradataDialect>() {
            Self::Teradata
        } else {
            Self::Generic
        }
    }

    /// Returns whether this dialect belongs to the MySQL / MariaDB family
    /// (i.e. treats `TINYINT(1)` as a synonym for `BOOLEAN`).
    ///
    /// [`SqlparserDialect::Generic`] counts as MySQL-family for classification
    /// purposes because it is intentionally permissive.
    #[must_use]
    #[inline]
    pub fn is_mysql_family(self) -> bool {
        matches!(self, Self::MySql | Self::Generic)
    }

    /// Returns whether this dialect is SQLite, which stores booleans as
    /// integer affinity by convention and thus produces `Maybe` for integer
    /// scalars declared without a boolean keyword.
    #[must_use]
    #[inline]
    pub fn is_sqlite(self) -> bool {
        matches!(self, Self::SQLite)
    }
}

/// Returns true when the [`ObjectName`] segments spell a single identifier
/// case-insensitively equal to `expected`.
fn custom_type_is(name: &ObjectName, expected: &str) -> bool {
    matches!(
        name.0.as_slice(),
        [ObjectNamePart::Identifier(ident)] if ident.value.eq_ignore_ascii_case(expected)
    )
}

/// Extracts the character-length argument of a `CHAR(...)` / `VARCHAR(...)`
/// declaration when it is a plain integer literal, else `None`.
fn char_length(spec: Option<&sqlparser::ast::CharacterLength>) -> Option<u64> {
    match spec {
        Some(sqlparser::ast::CharacterLength::IntegerLength { length, .. }) => Some(*length),
        _ => None,
    }
}

/// Boolean classifier operating on the raw [`DataType`].
fn classify_bool(dialect: SqlparserDialect, ty: &DataType) -> TypeMatch {
    match ty {
        // Every dialect that parses `BOOL` / `BOOLEAN` at all treats the
        // author's intent as boolean.
        DataType::Bool | DataType::Boolean => TypeMatch::Yes,

        // MySQL / MariaDB: `TINYINT(1)` is the idiomatic boolean spelling
        // and `BOOL` / `BOOLEAN` expand to it at DDL and driver level.
        // The unsigned variant is also accepted for the same purpose.
        DataType::TinyInt(Some(1)) | DataType::TinyIntUnsigned(Some(1))
            if dialect.is_mysql_family() =>
        {
            TypeMatch::Yes
        }

        // SQLite: no native boolean, integer-affine columns commonly carry
        // 0/1 booleans by application convention. Report Maybe so callers
        // can distinguish this from a Yes.
        DataType::Integer(_)
        | DataType::IntegerUnsigned(_)
        | DataType::Int(_)
        | DataType::IntUnsigned(_)
        | DataType::Int2(_)
        | DataType::Int2Unsigned(_)
        | DataType::Int4(_)
        | DataType::Int4Unsigned(_)
        | DataType::Int8(_)
        | DataType::Int8Unsigned(_)
        | DataType::SmallInt(_)
        | DataType::SmallIntUnsigned(_)
        | DataType::BigInt(_)
        | DataType::BigIntUnsigned(_)
        | DataType::TinyInt(_)
        | DataType::TinyIntUnsigned(_)
        | DataType::MediumInt(_)
        | DataType::MediumIntUnsigned(_)
        | DataType::Numeric(_)
        | DataType::Decimal(_)
        | DataType::Dec(_)
            if dialect.is_sqlite() =>
        {
            TypeMatch::Maybe
        }

        _ => TypeMatch::No,
    }
}

/// UUID classifier operating on the raw [`DataType`].
fn classify_uuid(dialect: SqlparserDialect, ty: &DataType) -> TypeMatch {
    match ty {
        // Native UUID token: Postgres, DuckDB, ClickHouse and every other
        // dialect that surfaces `UUID` as a keyword produces this variant.
        DataType::Uuid => TypeMatch::Yes,

        // SQL Server surfaces `UNIQUEIDENTIFIER` as a `Custom` type.
        DataType::Custom(name, _) if custom_type_is(name, "UNIQUEIDENTIFIER") => {
            if matches!(dialect, SqlparserDialect::MsSql | SqlparserDialect::Generic) {
                TypeMatch::Yes
            } else {
                TypeMatch::No
            }
        }

        // MySQL: no native UUID type. CHAR(36), CHARACTER(36), VARCHAR(36),
        // CHARACTER VARYING(36), and CHAR VARYING(36) are all widespread
        // but ambiguous conventions. BINARY(16) is also common. Report Maybe.
        DataType::Char(spec)
        | DataType::Character(spec)
        | DataType::Varchar(spec)
        | DataType::CharacterVarying(spec)
        | DataType::CharVarying(spec)
            if dialect.is_mysql_family() && char_length(spec.as_ref()) == Some(36) =>
        {
            TypeMatch::Maybe
        }
        DataType::Binary(Some(16)) if dialect.is_mysql_family() => TypeMatch::Maybe,

        // SQLite: no native UUID. `TEXT` or `BLOB` are the common carriers,
        // both broadly ambiguous. Report Maybe.
        DataType::Text | DataType::Blob(_) if dialect.is_sqlite() => TypeMatch::Maybe,

        _ => TypeMatch::No,
    }
}

/// Width in bytes of a fixed-length binary type.
///
/// `BINARY(n)` holds exactly `n` zero-padded bytes in the MySQL family, SQL
/// Server and ANSI SQL, where a bare `BINARY` is one byte. Snowflake reads `n`
/// as a maximum length, and DuckDB, Hive, Spark and Databricks alias `BINARY`
/// to a varying type.
pub(super) fn fixed_binary_length(dialect: SqlparserDialect, ty: &DataType) -> Option<u64> {
    let DataType::Binary(length) = ty else {
        return None;
    };
    (dialect.is_mysql_family()
        || matches!(dialect, SqlparserDialect::MsSql | SqlparserDialect::Ansi))
    .then(|| length.unwrap_or(1))
}

/// Most characters a value of a character type can hold, where the engine
/// enforces the declared length.
///
/// SQLite and DuckDB store a string of any length in a `VARCHAR(n)` column,
/// and ClickHouse aliases it to an unbounded `String`. Every other engine
/// rejects or truncates a longer value. A bare `CHAR` is `CHAR(1)`, and a bare
/// varying type is one character in SQL Server and 256 bytes in Redshift.
pub(super) fn max_character_length(dialect: SqlparserDialect, ty: &DataType) -> Option<u64> {
    if matches!(
        dialect,
        SqlparserDialect::ClickHouse | SqlparserDialect::DuckDb | SqlparserDialect::SQLite
    ) {
        return None;
    }
    let bare_varying = match dialect {
        SqlparserDialect::MsSql => Some(1),
        SqlparserDialect::Redshift => Some(256),
        _ => None,
    };
    let (spec, bare) = match ty {
        DataType::Char(spec) | DataType::Character(spec) => (spec, Some(1)),
        DataType::Varchar(spec)
        | DataType::CharacterVarying(spec)
        | DataType::CharVarying(spec)
        | DataType::Nvarchar(spec) => (spec, bare_varying),
        DataType::String(length) => return *length,
        _ => return None,
    };
    match spec {
        None => bare,
        Some(CharacterLength::IntegerLength { length, .. }) => Some(*length),
        Some(CharacterLength::Max) => None,
    }
}

/// Fractional digits of a second a value of a time or timestamp type keeps.
///
/// A bare type takes the engine default. PostgreSQL keeps microseconds and
/// reduces a larger written precision to 6, and Redshift, which documents
/// microsecond values, is read the same way. MySQL keeps none, Oracle and
/// Teradata keep 6, and the standard keeps 0 in `TIME` and 6 in `TIMESTAMP`.
/// Oracle's `DATE` holds whole seconds. BigQuery, Spark and Databricks keep
/// microseconds and Hive keeps nanoseconds, of which only a Spark `TIME` takes
/// a written precision. SQLite has no temporal type. The generic dialect names
/// no engine, so it answers only a written precision.
pub(super) fn fractional_seconds_precision(
    dialect: SqlparserDialect,
    ty: &DataType,
) -> Option<u64> {
    match dialect {
        SqlparserDialect::Generic => written_seconds_precision(ty),
        SqlparserDialect::MsSql => sql_server_seconds_precision(ty),
        SqlparserDialect::Snowflake => snowflake_seconds_precision(ty),
        SqlparserDialect::DuckDb => duckdb_seconds_precision(ty),
        SqlparserDialect::ClickHouse => clickhouse_seconds_precision(ty),
        SqlparserDialect::Ansi => {
            match ty {
                DataType::Time(p, _) => Some(p.unwrap_or(0)),
                DataType::Timestamp(p, _) => Some(p.unwrap_or(6)),
                _ => None,
            }
        }
        SqlparserDialect::Teradata => {
            match ty {
                DataType::Time(p, _) | DataType::Timestamp(p, _) => Some(p.unwrap_or(6)),
                _ => None,
            }
        }
        SqlparserDialect::PostgreSql | SqlparserDialect::Redshift => {
            match ty {
                DataType::Time(p, _) | DataType::Timestamp(p, _) => Some(p.map_or(6, |p| p.min(6))),
                _ => None,
            }
        }
        SqlparserDialect::MySql => {
            match ty {
                DataType::Time(p, _) | DataType::Timestamp(p, _) | DataType::Datetime(p) => {
                    Some(p.unwrap_or(0))
                }
                _ => None,
            }
        }
        SqlparserDialect::Oracle => {
            match ty {
                DataType::Timestamp(p, _) => Some(p.unwrap_or(6)),
                DataType::Date => Some(0),
                _ => None,
            }
        }
        SqlparserDialect::BigQuery => {
            match ty {
                DataType::Time(..) | DataType::Timestamp(..) | DataType::Datetime(_) => Some(6),
                _ => None,
            }
        }
        SqlparserDialect::Hive => {
            match ty {
                DataType::Timestamp(..) => Some(9),
                _ => None,
            }
        }
        SqlparserDialect::Spark | SqlparserDialect::Databricks => {
            match ty {
                DataType::Time(p, _) => Some(p.unwrap_or(6)),
                DataType::Timestamp(..) | DataType::TimestampNtz(_) => Some(6),
                DataType::Custom(name, _) if custom_type_is(name, "TIMESTAMP_LTZ") => Some(6),
                _ => None,
            }
        }
        SqlparserDialect::SQLite => None,
    }
}

/// SQL Server keeps 7 digits in `TIME`, `DATETIME2` and `DATETIMEOFFSET`, 3
/// in `DATETIME` and none in `SMALLDATETIME`. Its `TIMESTAMP` is a row
/// version.
fn sql_server_seconds_precision(ty: &DataType) -> Option<u64> {
    match ty {
        DataType::Time(p, _) => Some(p.unwrap_or(7)),
        DataType::Datetime(_) => Some(3),
        DataType::Custom(name, modifiers)
            if custom_type_is(name, "DATETIME2") || custom_type_is(name, "DATETIMEOFFSET") =>
        {
            modifier_precision(modifiers, 7)
        }
        DataType::Custom(name, modifiers) if custom_type_is(name, "SMALLDATETIME") => {
            modifiers.is_empty().then_some(0)
        }
        _ => None,
    }
}

/// Snowflake keeps 9 digits in every time and timestamp type.
fn snowflake_seconds_precision(ty: &DataType) -> Option<u64> {
    match ty {
        DataType::Time(p, _)
        | DataType::Timestamp(p, _)
        | DataType::Datetime(p)
        | DataType::TimestampNtz(p) => Some(p.unwrap_or(9)),
        DataType::Custom(name, modifiers)
            if custom_type_is(name, "TIMESTAMP_LTZ") || custom_type_is(name, "TIMESTAMP_TZ") =>
        {
            modifier_precision(modifiers, 9)
        }
        _ => None,
    }
}

/// DuckDB stores whole seconds, milliseconds, microseconds or nanoseconds, so
/// a written timestamp precision rises to the next of them. Its other types
/// reject a written precision and keep microseconds.
fn duckdb_seconds_precision(ty: &DataType) -> Option<u64> {
    match ty {
        DataType::Timestamp(Some(p), TimezoneInfo::None | TimezoneInfo::WithoutTimeZone)
        | DataType::Datetime(Some(p)) => {
            Some(match p {
                0 => 0,
                1..=3 => 3,
                4..=6 => 6,
                _ => 9,
            })
        }
        DataType::Time(..) | DataType::Timestamp(..) | DataType::Datetime(None) => Some(6),
        DataType::Custom(name, modifiers) if modifiers.is_empty() => {
            [("TIMESTAMP_S", 0), ("TIMESTAMP_MS", 3), ("TIMESTAMP_NS", 9), ("TIME_NS", 9)]
                .into_iter()
                .find_map(|(spelling, digits)| custom_type_is(name, spelling).then_some(digits))
        }
        _ => None,
    }
}

/// ClickHouse keeps whole seconds in `DateTime` and `Time`, widens
/// `DateTime(p)` to `DateTime64(p)` and `Time(p)` to `Time64(p)`, and keeps
/// milliseconds in a bare `Time64`.
fn clickhouse_seconds_precision(ty: &DataType) -> Option<u64> {
    match ty {
        DataType::Time(p, _) | DataType::Timestamp(p, _) | DataType::Datetime(p) => {
            Some(p.unwrap_or(0))
        }
        DataType::Datetime64(p, _) => Some(*p),
        DataType::Custom(name, modifiers) if custom_type_is(name, "TIME64") => {
            modifier_precision(modifiers, 3)
        }
        _ => None,
    }
}

/// Precision written on a time or timestamp type, in any dialect's spelling.
fn written_seconds_precision(ty: &DataType) -> Option<u64> {
    match ty {
        DataType::Time(p, _)
        | DataType::Timestamp(p, _)
        | DataType::Datetime(p)
        | DataType::TimestampNtz(p) => *p,
        DataType::Datetime64(p, _) => Some(*p),
        DataType::Custom(name, modifiers)
            if ["DATETIME2", "DATETIMEOFFSET", "TIMESTAMP_LTZ", "TIMESTAMP_TZ", "TIME64"]
                .into_iter()
                .any(|spelling| custom_type_is(name, spelling)) =>
        {
            written_modifier(modifiers)
        }
        _ => None,
    }
}

/// Precision a custom type spells as its single integer modifier, or `bare`
/// when it has none.
fn modifier_precision(modifiers: &[TypeModifier], bare: u64) -> Option<u64> {
    if modifiers.is_empty() { Some(bare) } else { written_modifier(modifiers) }
}

/// Value of a single integer modifier written on a custom type.
fn written_modifier(modifiers: &[TypeModifier]) -> Option<u64> {
    match modifiers {
        [TypeModifier::Number(digits, false)] => digits.parse().ok(),
        _ => None,
    }
}

impl DialectLike for SqlparserDialect {
    type DB = ParserDB;
    type Match = TypeMatch;

    #[inline]
    fn is_bool(
        &self,
        _database: &Self::DB,
        column: &TableAttribute<CreateTable, ColumnDef>,
    ) -> TypeMatch {
        classify_bool(*self, &column.attribute().data_type)
    }

    #[inline]
    fn is_uuid(
        &self,
        _database: &Self::DB,
        column: &TableAttribute<CreateTable, ColumnDef>,
    ) -> TypeMatch {
        classify_uuid(*self, &column.attribute().data_type)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{CharacterLength, ExactNumberInfo},
        dialect::GenericDialect,
    };

    use super::*;
    use crate::traits::DatabaseLike;

    fn char_of(len: u64) -> DataType {
        DataType::Char(Some(CharacterLength::IntegerLength { length: len, unit: None }))
    }

    #[test]
    fn classify_stock_dialects() {
        assert_eq!(SqlparserDialect::of::<AnsiDialect>(), SqlparserDialect::Ansi);
        assert_eq!(SqlparserDialect::of::<BigQueryDialect>(), SqlparserDialect::BigQuery);
        assert_eq!(SqlparserDialect::of::<ClickHouseDialect>(), SqlparserDialect::ClickHouse);
        assert_eq!(SqlparserDialect::of::<DatabricksDialect>(), SqlparserDialect::Databricks);
        assert_eq!(SqlparserDialect::of::<DuckDbDialect>(), SqlparserDialect::DuckDb);
        assert_eq!(SqlparserDialect::of::<GenericDialect>(), SqlparserDialect::Generic);
        assert_eq!(SqlparserDialect::of::<HiveDialect>(), SqlparserDialect::Hive);
        assert_eq!(SqlparserDialect::of::<MsSqlDialect>(), SqlparserDialect::MsSql);
        assert_eq!(SqlparserDialect::of::<MySqlDialect>(), SqlparserDialect::MySql);
        assert_eq!(SqlparserDialect::of::<OracleDialect>(), SqlparserDialect::Oracle);
        assert_eq!(SqlparserDialect::of::<PostgreSqlDialect>(), SqlparserDialect::PostgreSql);
        assert_eq!(SqlparserDialect::of::<RedshiftSqlDialect>(), SqlparserDialect::Redshift);
        assert_eq!(SqlparserDialect::of::<SnowflakeDialect>(), SqlparserDialect::Snowflake);
        assert_eq!(SqlparserDialect::of::<SparkSqlDialect>(), SqlparserDialect::Spark);
        assert_eq!(SqlparserDialect::of::<SQLiteDialect>(), SqlparserDialect::SQLite);
        assert_eq!(SqlparserDialect::of::<TeradataDialect>(), SqlparserDialect::Teradata);
    }

    #[test]
    fn classify_unknown_dialect_falls_back_to_generic() {
        #[derive(Debug, Default)]
        struct MyOwnDialect;
        impl Dialect for MyOwnDialect {
            fn is_identifier_start(&self, _: char) -> bool {
                false
            }
            fn is_identifier_part(&self, _: char) -> bool {
                false
            }
        }
        assert_eq!(SqlparserDialect::of::<MyOwnDialect>(), SqlparserDialect::Generic);
    }

    // ----- is_bool -------------------------------------------------------

    #[test]
    fn classify_bool_mysql_tinyint_1_is_yes() {
        assert_eq!(
            classify_bool(SqlparserDialect::MySql, &DataType::TinyInt(Some(1))),
            TypeMatch::Yes,
        );
    }

    #[test]
    fn classify_bool_mysql_wider_tinyint_is_no() {
        assert_eq!(
            classify_bool(SqlparserDialect::MySql, &DataType::TinyInt(Some(2))),
            TypeMatch::No,
        );
        assert_eq!(classify_bool(SqlparserDialect::MySql, &DataType::TinyInt(None)), TypeMatch::No,);
    }

    #[test]
    fn classify_bool_postgres_bool_keyword_is_yes() {
        assert_eq!(classify_bool(SqlparserDialect::PostgreSql, &DataType::Boolean), TypeMatch::Yes);
        assert_eq!(classify_bool(SqlparserDialect::PostgreSql, &DataType::Bool), TypeMatch::Yes);
    }

    #[test]
    fn classify_bool_postgres_tinyint_is_no() {
        assert_eq!(
            classify_bool(SqlparserDialect::PostgreSql, &DataType::TinyInt(Some(1))),
            TypeMatch::No,
        );
    }

    #[test]
    fn classify_bool_sqlite_integer_is_maybe() {
        // SQLite stores booleans as integer affinity by convention.
        assert_eq!(
            classify_bool(SqlparserDialect::SQLite, &DataType::Integer(None)),
            TypeMatch::Maybe,
        );
        assert_eq!(
            classify_bool(SqlparserDialect::SQLite, &DataType::TinyInt(None)),
            TypeMatch::Maybe,
        );
        assert_eq!(
            classify_bool(SqlparserDialect::SQLite, &DataType::TinyInt(Some(1))),
            TypeMatch::Maybe,
        );
        assert_eq!(
            classify_bool(SqlparserDialect::SQLite, &DataType::Numeric(ExactNumberInfo::None)),
            TypeMatch::Maybe,
        );
    }

    #[test]
    fn classify_bool_sqlite_bool_keyword_is_yes() {
        // Even though SQLite has no boolean at the storage layer, sqlparser
        // parses the `BOOLEAN` keyword into `DataType::Boolean` and the
        // author's declared intent is unambiguous.
        assert_eq!(classify_bool(SqlparserDialect::SQLite, &DataType::Boolean), TypeMatch::Yes);
    }

    #[test]
    fn classify_bool_sqlite_text_is_no() {
        assert_eq!(classify_bool(SqlparserDialect::SQLite, &DataType::Text), TypeMatch::No);
    }

    #[test]
    fn classify_bool_generic_accepts_tinyint_1() {
        assert_eq!(
            classify_bool(SqlparserDialect::Generic, &DataType::TinyInt(Some(1))),
            TypeMatch::Yes,
        );
    }

    #[test]
    fn classify_bool_mysql_tinyint_unsigned_1_is_yes() {
        assert_eq!(
            classify_bool(SqlparserDialect::MySql, &DataType::TinyIntUnsigned(Some(1))),
            TypeMatch::Yes,
        );
        assert_eq!(
            classify_bool(SqlparserDialect::Generic, &DataType::TinyIntUnsigned(Some(1))),
            TypeMatch::Yes,
        );
    }

    #[test]
    fn classify_bool_mysql_tinyint_unsigned_wider_is_no() {
        assert_eq!(
            classify_bool(SqlparserDialect::MySql, &DataType::TinyIntUnsigned(Some(2))),
            TypeMatch::No,
        );
        assert_eq!(
            classify_bool(SqlparserDialect::MySql, &DataType::TinyIntUnsigned(None)),
            TypeMatch::No,
        );
    }

    #[test]
    fn classify_bool_sqlite_unsigned_integers_are_maybe() {
        for ty in &[
            DataType::IntegerUnsigned(None),
            DataType::IntUnsigned(None),
            DataType::Int2Unsigned(None),
            DataType::Int4Unsigned(None),
            DataType::Int8Unsigned(None),
            DataType::SmallIntUnsigned(None),
            DataType::BigIntUnsigned(None),
            DataType::TinyIntUnsigned(None),
            DataType::MediumIntUnsigned(None),
        ] {
            assert_eq!(
                classify_bool(SqlparserDialect::SQLite, ty),
                TypeMatch::Maybe,
                "expected Maybe for {ty:?}",
            );
        }
    }

    #[test]
    fn classify_bool_sqlite_decimal_and_dec_are_maybe() {
        assert_eq!(
            classify_bool(SqlparserDialect::SQLite, &DataType::Decimal(ExactNumberInfo::None)),
            TypeMatch::Maybe,
        );
        assert_eq!(
            classify_bool(SqlparserDialect::SQLite, &DataType::Dec(ExactNumberInfo::None)),
            TypeMatch::Maybe,
        );
    }

    // ----- is_uuid -------------------------------------------------------

    fn custom(name: &str) -> DataType {
        DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(sqlparser::ast::Ident::new(name))]),
            vec![],
        )
    }

    #[test]
    fn classify_uuid_uuid_keyword_is_yes_everywhere() {
        for d in [
            SqlparserDialect::PostgreSql,
            SqlparserDialect::MySql,
            SqlparserDialect::SQLite,
            SqlparserDialect::MsSql,
            SqlparserDialect::Generic,
        ] {
            assert_eq!(classify_uuid(d, &DataType::Uuid), TypeMatch::Yes, "{d:?}");
        }
    }

    #[test]
    fn classify_uuid_mssql_uniqueidentifier_is_yes() {
        assert_eq!(
            classify_uuid(SqlparserDialect::MsSql, &custom("UNIQUEIDENTIFIER")),
            TypeMatch::Yes,
        );
        assert_eq!(
            classify_uuid(SqlparserDialect::MsSql, &custom("uniqueidentifier")),
            TypeMatch::Yes,
        );
    }

    #[test]
    fn classify_uuid_mssql_uniqueidentifier_is_no_under_postgres() {
        // Case-preserved routing: `UNIQUEIDENTIFIER` is not a Postgres type.
        assert_eq!(
            classify_uuid(SqlparserDialect::PostgreSql, &custom("UNIQUEIDENTIFIER")),
            TypeMatch::No,
        );
    }

    #[test]
    fn classify_uuid_mysql_char_36_is_maybe() {
        // Cover both AST alternatives of the OR-pattern arm.
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &char_of(36)), TypeMatch::Maybe);
        assert_eq!(
            classify_uuid(
                SqlparserDialect::MySql,
                &DataType::Character(Some(CharacterLength::IntegerLength {
                    length: 36,
                    unit: None,
                })),
            ),
            TypeMatch::Maybe,
        );
    }

    #[test]
    fn classify_uuid_mysql_char_other_length_is_no() {
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &char_of(10)), TypeMatch::No);
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &DataType::Char(None)), TypeMatch::No,);
    }

    #[test]
    fn classify_uuid_mysql_binary_16_is_maybe() {
        assert_eq!(
            classify_uuid(SqlparserDialect::MySql, &DataType::Binary(Some(16))),
            TypeMatch::Maybe,
        );
    }

    #[test]
    fn classify_uuid_postgres_char_36_is_no() {
        // Postgres has native UUID; CHAR(36) is not a UUID convention there.
        assert_eq!(classify_uuid(SqlparserDialect::PostgreSql, &char_of(36)), TypeMatch::No);
    }

    #[test]
    fn classify_uuid_sqlite_text_is_maybe() {
        assert_eq!(classify_uuid(SqlparserDialect::SQLite, &DataType::Text), TypeMatch::Maybe);
        assert_eq!(
            classify_uuid(SqlparserDialect::SQLite, &DataType::Blob(None)),
            TypeMatch::Maybe,
        );
    }

    #[test]
    fn classify_uuid_mysql_varchar_36_is_maybe() {
        let varchar_36 =
            DataType::Varchar(Some(CharacterLength::IntegerLength { length: 36, unit: None }));
        let char_varying_36 = DataType::CharacterVarying(Some(CharacterLength::IntegerLength {
            length: 36,
            unit: None,
        }));
        let short_spelling_36 =
            DataType::CharVarying(Some(CharacterLength::IntegerLength { length: 36, unit: None }));
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &varchar_36), TypeMatch::Maybe);
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &char_varying_36), TypeMatch::Maybe);
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &short_spelling_36), TypeMatch::Maybe);
    }

    #[test]
    fn classify_uuid_mysql_varchar_other_length_is_no() {
        let varchar_10 =
            DataType::Varchar(Some(CharacterLength::IntegerLength { length: 10, unit: None }));
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &varchar_10), TypeMatch::No);
        assert_eq!(classify_uuid(SqlparserDialect::MySql, &DataType::Varchar(None)), TypeMatch::No);
    }

    // --- GenericDB<..., SqlparserDialect>: Debug / Clone / ::new coverage ---

    #[test]
    fn parser_db_debug_and_clone_round_trip() {
        let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT);").expect("parse");

        // Exercise the manual `Debug` impl on `GenericDB<...,
        // SqlparserDialect>`.
        let rendered = alloc::format!("{db:?}");
        assert!(rendered.contains("GenericDB"), "debug output shape: {rendered}");
        assert!(rendered.contains("dialect"), "debug output must surface the dialect field");

        // Exercise the manual `Clone` impl and confirm the dialect propagates.
        let cloned = db.clone();
        assert_eq!(cloned.dialect(), db.dialect());
        assert_eq!(cloned.catalog_name(), db.catalog_name());
    }

    #[test]
    fn parser_db_new_returns_builder_with_dialect() {
        // `GenericDB::new` is the public factory; call it via the `ParserDB`
        // alias with an explicit dialect and confirm the builder carries it.
        let builder = ParserDB::new("cat".to_string(), SqlparserDialect::MySql);
        let db: ParserDB = builder.into();
        assert_eq!(*db.dialect(), SqlparserDialect::MySql);
        assert_eq!(db.catalog_name(), "cat");
    }
}
