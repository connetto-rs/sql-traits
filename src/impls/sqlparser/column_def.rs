//! Submodule implementing the [`ColumnLike`] trait for `sqlparser`'s
//! [`ColumnDef`] struct.

use alloc::string::{String, ToString};

use sqlparser::ast::{ColumnDef, ColumnOption, CreateTable, DataType, ObjectNamePart};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ColumnMetadata, ParserDB, TableAttribute},
    traits::{
        ColumnCollation, ColumnLike, CreatedFromQuery, DatabaseLike, IdentityGeneration, Metadata,
        NamedColumnCollation,
    },
    utils::{identity_generation, is_identity, object_name::target_name_of_object_name},
};

/// PostgreSQL's serial pseudo-types and their aliases, as the catalog spells
/// them.
const SERIAL_TYPES: [&str; 6] =
    ["smallserial", "serial", "bigserial", "serial2", "serial4", "serial8"];

/// The integer type behind a serial pseudo-type, which is also the type of
/// the sequence PostgreSQL creates for it, or `None` for any other type.
/// PostgreSQL folds an unquoted name to lower case before comparing it, and a
/// quoted name is compared as written.
pub(crate) fn serial_sequence_type(data_type: &DataType) -> Option<DataType> {
    let DataType::Custom(name, _) = data_type else {
        return None;
    };
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return None;
    };
    let serial = SERIAL_TYPES.iter().find(|serial| {
        if ident.quote_style.is_some() {
            ident.value == **serial
        } else {
            ident.value.eq_ignore_ascii_case(serial)
        }
    })?;
    Some(match *serial {
        "smallserial" | "serial2" => DataType::SmallInt(None),
        "bigserial" | "serial8" => DataType::BigInt(None),
        _ => DataType::Integer(None),
    })
}

/// Whether the declared type names a serial pseudo-type.
fn is_serial_type(data_type: &DataType) -> bool {
    serial_sequence_type(data_type).is_some()
}

impl Metadata for TableAttribute<CreateTable, ColumnDef> {
    type Meta = ColumnMetadata;
}

impl ColumnLike for TableAttribute<CreateTable, ColumnDef> {
    type DB = ParserDB;

    #[inline]
    fn column_name(&self) -> &str {
        self.attribute().name.value.as_str()
    }

    #[inline]
    fn column_name_is_quoted(&self) -> bool {
        self.attribute().name.quote_style.is_some()
    }

    #[inline]
    fn column_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(database
            .column_metadata(self)
            .ok_or_else(|| ObjectKind::Table.not_in_database(&self.table().name.to_string()))?
            .documentation())
    }

    #[inline]
    fn data_type<'db>(&'db self, _database: &'db Self::DB) -> &'db DataType {
        &self.attribute().data_type
    }

    #[inline]
    fn collation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<ColumnCollation<'db>, LookupError> {
        let metadata = database.column_metadata(self).ok_or_else(|| {
            let mut name = self.table().name.to_string();
            name.push('.');
            name.push_str(self.column_name());
            ObjectKind::Column.not_in_database(&name)
        })?;
        let mut unknown = false;
        for option in &self.attribute().options {
            match &option.option {
                ColumnOption::Collation(name) => {
                    let named = NamedColumnCollation::new(target_name_of_object_name(name))
                        .with_postgres_deterministic(metadata.postgres_deterministic())
                        .with_mysql_padding(metadata.mysql_padding());
                    return Ok(ColumnCollation::Named(named));
                }
                ColumnOption::CharacterSet(_) => unknown = true,
                _ => {}
            }
        }

        Ok(if unknown { ColumnCollation::Unknown } else { ColumnCollation::DatabaseDefault })
    }

    #[inline]
    fn created_from_query<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<CreatedFromQuery<'db, Self::DB>> {
        if self.attribute().data_type != DataType::Unspecified {
            return None;
        }
        let creation = database.column_metadata(self)?.creation()?;
        Some(CreatedFromQuery::new(
            creation.query(),
            creation.output_names(),
            creation.catalog(),
            creation.ordinal(),
        ))
    }

    #[inline]
    fn identity_generation(&self, database: &Self::DB) -> Option<IdentityGeneration> {
        self.attribute()
            .options
            .iter()
            .find_map(|option| identity_generation(&option.option, *database.dialect()))
    }

    #[inline]
    fn is_serial(&self) -> bool {
        is_serial_type(&self.attribute().data_type)
    }

    /// # Errors
    ///
    /// Returns an error when the table the column belongs to is not held by
    /// `database`.
    ///
    /// A stored node carries the `NOT NULL` a key or an identity implies, so
    /// the option alone answers for anything this crate built. The key and the
    /// identity are still consulted, so a node assembled by hand rather than
    /// parsed answers alike.
    #[inline]
    fn is_nullable(&self, database: &Self::DB) -> Result<bool, LookupError> {
        let declared = self.attribute().options.iter().any(|option| {
            matches!(option.option, sqlparser::ast::ColumnOption::NotNull)
                || is_identity(&option.option)
        });
        Ok(!declared && !self.is_primary_key(database)?)
    }

    #[inline]
    fn default_value(&self) -> Option<String> {
        self.attribute().options.iter().find_map(|opt| {
            if let sqlparser::ast::ColumnOption::Default(expr) = &opt.option {
                Some(expr.to_string())
            } else {
                None
            }
        })
    }

    #[inline]
    fn table<'a>(&'a self, _database: &'a Self::DB) -> &'a <Self::DB as DatabaseLike>::Table
    where
        Self: 'a,
    {
        self.table()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, sync::Arc};

    use sqlparser::dialect::{
        AnsiDialect, DatabricksDialect, DuckDbDialect, GenericDialect, HiveDialect, MsSqlDialect,
        MySqlDialect, PostgreSqlDialect, SQLiteDialect, SnowflakeDialect, SparkSqlDialect,
    };

    use crate::{
        errors::Error,
        impls::SqlparserDialect,
        prelude::{
            ParseOptions, ParserDB, PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType,
        },
        structs::{IdentifierCase, TargetName},
        traits::{
            ColumnCollation, ColumnLike, DataTypeLike, DatabaseLike, DialectLike, TableLike,
            TypeMatch, TypeMatchLike,
        },
    };

    fn parse_with<D: sqlparser::dialect::Dialect + Default + 'static>(sql: &str) -> ParserDB {
        ParserDB::parse::<D>(sql).expect("parse")
    }

    fn bool_of(db: &ParserDB, col: &str) -> TypeMatch {
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table t exists");
        let column = table
            .column(col, db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        db.dialect().is_bool(db, column)
    }

    fn uuid_of(db: &ParserDB, col: &str) -> TypeMatch {
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table t exists");
        let column = table
            .column(col, db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        db.dialect().is_uuid(db, column)
    }

    // ---------------- is_bool ----------------

    #[test]
    fn is_bool_mysql_tinyint_1_is_yes() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (flag TINYINT(1));");
        assert_eq!(bool_of(&db, "flag"), TypeMatch::Yes);
    }

    #[test]
    fn is_bool_mysql_wider_tinyint_is_no() {
        for width in ["TINYINT", "TINYINT(2)", "TINYINT(4)"] {
            let sql = format!("CREATE TABLE t (flag {width});");
            let db = parse_with::<MySqlDialect>(&sql);
            assert_eq!(bool_of(&db, "flag"), TypeMatch::No, "MySQL {width}");
        }
    }

    #[test]
    fn is_bool_mysql_bool_keyword_is_yes() {
        for spelling in ["BOOL", "BOOLEAN"] {
            let sql = format!("CREATE TABLE t (flag {spelling});");
            let db = parse_with::<MySqlDialect>(&sql);
            assert_eq!(bool_of(&db, "flag"), TypeMatch::Yes, "MySQL {spelling}");
        }
    }

    #[test]
    fn is_bool_postgres_tinyint_1_is_no() {
        let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (flag TINYINT(1));");
        assert_eq!(bool_of(&db, "flag"), TypeMatch::No);
    }

    #[test]
    fn is_bool_postgres_and_mysql_agree_on_boolean_keyword() {
        let sql = "CREATE TABLE t (flag BOOLEAN);";
        assert_eq!(bool_of(&parse_with::<PostgreSqlDialect>(sql), "flag"), TypeMatch::Yes);
        assert_eq!(bool_of(&parse_with::<MySqlDialect>(sql), "flag"), TypeMatch::Yes);
    }

    #[test]
    fn is_bool_sqlite_integer_is_maybe() {
        // SQLite has no boolean; INTEGER carries 0/1 booleans by convention.
        // Users must call `.is_yes()` vs `.is_maybe()` explicitly.
        let db = parse_with::<SQLiteDialect>("CREATE TABLE t (flag INTEGER);");
        let m = bool_of(&db, "flag");
        assert!(m.is_maybe(), "expected Maybe, got {m:?}");
        assert!(!m.is_yes());
        assert!(!m.is_no());
    }

    #[test]
    fn is_bool_sqlite_boolean_keyword_is_yes() {
        // sqlparser's SQLiteDialect still parses `BOOLEAN` into
        // DataType::Boolean, and the author's declared intent is
        // unambiguous even though SQLite stores it as numeric affinity.
        let db = parse_with::<SQLiteDialect>("CREATE TABLE t (flag BOOLEAN);");
        assert_eq!(bool_of(&db, "flag"), TypeMatch::Yes);
    }

    #[test]
    fn is_bool_sqlite_text_is_no() {
        let db = parse_with::<SQLiteDialect>("CREATE TABLE t (flag TEXT);");
        assert_eq!(bool_of(&db, "flag"), TypeMatch::No);
    }

    #[test]
    fn is_bool_generic_tinyint_1_is_yes() {
        let db = parse_with::<GenericDialect>("CREATE TABLE t (flag TINYINT(1));");
        assert_eq!(bool_of(&db, "flag"), TypeMatch::Yes);
    }

    // ---------------- is_uuid ----------------

    #[test]
    fn is_uuid_postgres_uuid_keyword_is_yes() {
        let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (id UUID);");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::Yes);
    }

    #[test]
    fn is_uuid_mssql_uniqueidentifier_is_yes() {
        let db = parse_with::<MsSqlDialect>("CREATE TABLE t (id UNIQUEIDENTIFIER);");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::Yes);
    }

    #[test]
    fn is_uuid_mysql_char_36_is_maybe() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (id CHAR(36));");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::Maybe);
    }

    #[test]
    fn is_uuid_mysql_binary_16_is_maybe() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (id BINARY(16));");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::Maybe);
    }

    #[test]
    fn is_uuid_mysql_char_other_length_is_no() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (id CHAR(10));");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::No);
    }

    #[test]
    fn is_uuid_postgres_char_36_is_no() {
        // Postgres has native UUID; CHAR(36) is not a UUID convention.
        let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (id CHAR(36));");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::No);
    }

    #[test]
    fn is_uuid_sqlite_text_is_maybe() {
        let db = parse_with::<SQLiteDialect>("CREATE TABLE t (id TEXT);");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::Maybe);
    }

    #[test]
    fn is_uuid_sqlite_integer_is_no() {
        let db = parse_with::<SQLiteDialect>("CREATE TABLE t (id INTEGER);");
        assert_eq!(uuid_of(&db, "id"), TypeMatch::No);
    }

    // ---------------- fixed_binary_length ----------------

    fn fixed_binary_lengths<D: sqlparser::dialect::Dialect + Default + 'static>() -> [Option<u64>; 5]
    {
        let db = parse_with::<D>(
            "CREATE TABLE t (fixed BINARY(4), bare BINARY, varying VARBINARY(8), blob BLOB, num INT);",
        );
        ["fixed", "bare", "varying", "blob", "num"]
            .map(|name| column_named(&db, name).data_type(&db).fixed_binary_length(&db))
    }

    #[test]
    fn fixed_binary_length_reads_declared_and_implicit_width() {
        let widths = [Some(4), Some(1), None, None, None];
        assert_eq!(fixed_binary_lengths::<MySqlDialect>(), widths);
        assert_eq!(fixed_binary_lengths::<GenericDialect>(), widths);
        assert_eq!(fixed_binary_lengths::<MsSqlDialect>(), widths);
        assert_eq!(fixed_binary_lengths::<AnsiDialect>(), widths);
    }

    #[test]
    fn fixed_binary_length_is_none_outside_fixed_width_dialects() {
        assert_eq!(fixed_binary_lengths::<SnowflakeDialect>(), [None; 5]);
        assert_eq!(fixed_binary_lengths::<SparkSqlDialect>(), [None; 5]);
        assert_eq!(fixed_binary_lengths::<DatabricksDialect>(), [None; 5]);
        assert_eq!(fixed_binary_lengths::<HiveDialect>(), [None; 5]);
        assert_eq!(fixed_binary_lengths::<DuckDbDialect>(), [None; 5]);
        assert_eq!(fixed_binary_lengths::<PostgreSqlDialect>(), [None; 5]);
        assert_eq!(fixed_binary_lengths::<SQLiteDialect>(), [None; 5]);
    }

    mod max_character_length {
        use sqlparser::dialect::{
            AnsiDialect, BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect,
            DuckDbDialect, GenericDialect, HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect,
            PostgreSqlDialect, RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect,
            SparkSqlDialect, TeradataDialect,
        };

        use super::{ColumnLike, DataTypeLike, column_named, parse_with};

        fn lengths<D: Dialect + Default + 'static, const N: usize>(
            columns: &str,
            names: [&str; N],
        ) -> [Option<u64>; N] {
            let db = parse_with::<D>(&format!("CREATE TABLE t ({columns});"));
            names.map(|name| column_named(&db, name).data_type(&db).max_character_length(&db))
        }

        fn common<D: Dialect + Default + 'static>() -> [Option<u64>; 8] {
            lengths::<D, 8>(
                "vc VARCHAR(100), cv CHARACTER VARYING(50), c CHAR(8), ch CHARACTER(4), \
                 bare_char CHAR, bare_vc VARCHAR, body TEXT, num INT",
                ["vc", "cv", "c", "ch", "bare_char", "bare_vc", "body", "num"],
            )
        }

        #[test]
        fn declared_lengths_bound_where_the_engine_enforces_them() {
            let bounded = [Some(100), Some(50), Some(8), Some(4), Some(1), None, None, None];
            assert_eq!(common::<PostgreSqlDialect>(), bounded);
            assert_eq!(common::<MySqlDialect>(), bounded);
            assert_eq!(common::<GenericDialect>(), bounded);
            assert_eq!(common::<AnsiDialect>(), bounded);
            assert_eq!(common::<SnowflakeDialect>(), bounded);
            assert_eq!(common::<OracleDialect>(), bounded);
            assert_eq!(common::<TeradataDialect>(), bounded);
            assert_eq!(common::<BigQueryDialect>(), bounded);
            assert_eq!(common::<HiveDialect>(), bounded);
            assert_eq!(common::<SparkSqlDialect>(), bounded);
            assert_eq!(common::<DatabricksDialect>(), bounded);
        }

        #[test]
        fn bare_varying_type_takes_the_engine_default() {
            let one = [Some(100), Some(50), Some(8), Some(4), Some(1), Some(1), None, None];
            assert_eq!(common::<MsSqlDialect>(), one);
            let redshift = [Some(100), Some(50), Some(8), Some(4), Some(1), Some(256), None, None];
            assert_eq!(common::<RedshiftSqlDialect>(), redshift);
        }

        #[test]
        fn declared_lengths_do_not_bound_where_the_engine_ignores_them() {
            assert_eq!(common::<SQLiteDialect>(), [None; 8]);
            assert_eq!(common::<DuckDbDialect>(), [None; 8]);
            assert_eq!(common::<ClickHouseDialect>(), [None; 8]);
        }

        #[test]
        fn national_and_max_lengths() {
            assert_eq!(
                lengths::<MsSqlDialect, 4>(
                    "n NVARCHAR(30), bare_n NVARCHAR, vc_max VARCHAR(MAX), n_max NVARCHAR(MAX)",
                    ["n", "bare_n", "vc_max", "n_max"],
                ),
                [Some(30), Some(1), None, None]
            );
        }

        #[test]
        fn octet_lengths_bound_characters_too() {
            assert_eq!(
                lengths::<AnsiDialect, 2>(
                    "o VARCHAR(10 OCTETS), c VARCHAR(12 CHARACTERS)",
                    ["o", "c"],
                ),
                [Some(10), Some(12)]
            );
        }

        #[test]
        fn parameterized_string_is_bounded() {
            assert_eq!(
                lengths::<BigQueryDialect, 2>("s STRING(10), bare STRING", ["s", "bare"]),
                [Some(10), None]
            );
        }
    }

    mod fractional_seconds_precision {
        use sqlparser::dialect::{
            AnsiDialect, BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect,
            DuckDbDialect, GenericDialect, HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect,
            PostgreSqlDialect, RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect,
            SparkSqlDialect, TeradataDialect,
        };

        use super::{ColumnLike, DataTypeLike, column_named, parse_with};

        /// Asserts the precision of a column declared with each type.
        fn assert_precisions<D: Dialect + Default + 'static>(cases: &[(&str, Option<u64>)]) {
            let columns: Vec<String> =
                cases.iter().enumerate().map(|(index, (ty, _))| format!("c{index} {ty}")).collect();
            let db = parse_with::<D>(&format!("CREATE TABLE t ({});", columns.join(", ")));
            for (index, (ty, expected)) in cases.iter().enumerate() {
                let column = column_named(&db, &format!("c{index}"));
                assert_eq!(
                    column.data_type(&db).fractional_seconds_precision(&db),
                    *expected,
                    "{ty}"
                );
            }
        }

        #[test]
        fn postgres_keeps_microseconds_by_default_and_clamps_to_them() {
            let cases = [
                ("TIMESTAMP", Some(6)),
                ("TIMESTAMP(0)", Some(0)),
                ("TIMESTAMPTZ(0)", Some(0)),
                ("TIMESTAMP(3) WITH TIME ZONE", Some(3)),
                ("TIME", Some(6)),
                ("TIME(2)", Some(2)),
                ("TIMETZ(3)", Some(3)),
                ("TIMESTAMP(9)", Some(6)),
                ("DATE", None),
                ("INTERVAL", Some(6)),
                ("INT", None),
            ];
            assert_precisions::<PostgreSqlDialect>(&cases);
            assert_precisions::<RedshiftSqlDialect>(&cases);
        }

        /// An interval keeps the digits of its seconds field, and one
        /// restricted to coarser fields keeps no seconds at all.
        #[test]
        fn postgres_interval_keeps_the_digits_of_its_seconds() {
            assert_precisions::<PostgreSqlDialect>(&[
                ("INTERVAL(2)", Some(2)),
                ("INTERVAL(9)", Some(6)),
                ("INTERVAL DAY TO SECOND(1)", Some(1)),
                ("INTERVAL HOUR TO SECOND", Some(6)),
                ("INTERVAL SECOND", Some(6)),
                ("INTERVAL YEAR TO MONTH", None),
                ("INTERVAL DAY", None),
                ("INTERVAL HOUR TO MINUTE", None),
            ]);
        }

        #[test]
        fn mysql_drops_the_fraction_by_default() {
            assert_precisions::<MySqlDialect>(&[
                ("DATETIME", Some(0)),
                ("DATETIME(3)", Some(3)),
                ("TIMESTAMP", Some(0)),
                ("TIMESTAMP(0)", Some(0)),
                ("TIME", Some(0)),
                ("TIME(2)", Some(2)),
                ("DATE", None),
            ]);
        }

        #[test]
        fn sql_server_types_and_rowversion() {
            assert_precisions::<MsSqlDialect>(&[
                ("DATETIME", Some(3)),
                ("DATETIME2", Some(7)),
                ("DATETIME2(0)", Some(0)),
                ("DATETIMEOFFSET", Some(7)),
                ("DATETIMEOFFSET(3)", Some(3)),
                ("DATETIME2(MAX)", None),
                ("TIME", Some(7)),
                ("TIME(2)", Some(2)),
                ("SMALLDATETIME", Some(0)),
                ("DATE", None),
                ("TIMESTAMP", None),
            ]);
        }

        #[test]
        fn oracle_dates_keep_whole_seconds() {
            assert_precisions::<OracleDialect>(&[
                ("TIMESTAMP", Some(6)),
                ("TIMESTAMP(0)", Some(0)),
                ("TIMESTAMP(9) WITH TIME ZONE", Some(9)),
                ("DATE", Some(0)),
                ("NUMBER(10)", None),
            ]);
        }

        #[test]
        fn snowflake_keeps_nanoseconds_by_default() {
            assert_precisions::<SnowflakeDialect>(&[
                ("TIMESTAMP", Some(9)),
                ("TIMESTAMP_NTZ(3)", Some(3)),
                ("TIMESTAMP_LTZ", Some(9)),
                ("TIMESTAMP_TZ(0)", Some(0)),
                ("TIME", Some(9)),
                ("DATETIME(6)", Some(6)),
                ("DATE", None),
            ]);
        }

        #[test]
        fn duckdb_raises_written_precision_to_a_stored_unit() {
            assert_precisions::<DuckDbDialect>(&[
                ("TIMESTAMP", Some(6)),
                ("TIMESTAMP(0)", Some(0)),
                ("TIMESTAMP(1)", Some(3)),
                ("TIMESTAMP(3)", Some(3)),
                ("TIMESTAMP(4)", Some(6)),
                ("TIMESTAMP(6)", Some(6)),
                ("TIMESTAMP(7)", Some(9)),
                ("DATETIME(0)", Some(0)),
                ("TIMESTAMPTZ", Some(6)),
                ("TIME", Some(6)),
                ("TIMESTAMP_S", Some(0)),
                ("TIMESTAMP_MS", Some(3)),
                ("TIMESTAMP_NS", Some(9)),
                ("TIME_NS", Some(9)),
                ("DATE", None),
            ]);
        }

        #[test]
        fn clickhouse_counts_whole_seconds_unless_widened() {
            assert_precisions::<ClickHouseDialect>(&[
                ("DateTime", Some(0)),
                ("DateTime(3)", Some(3)),
                ("DateTime64(9, 'UTC')", Some(9)),
                ("TIMESTAMP", Some(0)),
                ("Time", Some(0)),
                ("Time64(3)", Some(3)),
                ("Time64", Some(3)),
                ("Date", None),
            ]);
        }

        #[test]
        fn fixed_resolution_engines_ignore_written_precision() {
            assert_precisions::<BigQueryDialect>(&[
                ("TIMESTAMP", Some(6)),
                ("DATETIME", Some(6)),
                ("TIME", Some(6)),
                ("DATE", None),
            ]);
            assert_precisions::<HiveDialect>(&[("TIMESTAMP", Some(9)), ("DATE", None)]);
        }

        #[test]
        fn spark_keeps_microseconds_and_a_written_time_precision() {
            let cases = [
                ("TIMESTAMP", Some(6)),
                ("TIMESTAMP_NTZ", Some(6)),
                ("TIMESTAMP_LTZ", Some(6)),
                ("TIME", Some(6)),
                ("TIME(0)", Some(0)),
                ("DATE", None),
            ];
            assert_precisions::<SparkSqlDialect>(&cases);
            assert_precisions::<DatabricksDialect>(&cases);
        }

        #[test]
        fn standard_and_teradata_defaults() {
            assert_precisions::<AnsiDialect>(&[
                ("TIMESTAMP", Some(6)),
                ("TIMESTAMP(0)", Some(0)),
                ("TIME", Some(0)),
                ("TIME(2)", Some(2)),
                ("DATE", None),
            ]);
            assert_precisions::<TeradataDialect>(&[
                ("TIMESTAMP", Some(6)),
                ("TIMESTAMP(0)", Some(0)),
                ("TIME", Some(6)),
                ("TIME(2)", Some(2)),
                ("DATE", None),
            ]);
        }

        #[test]
        fn generic_answers_only_a_written_precision() {
            assert_precisions::<GenericDialect>(&[
                ("TIMESTAMP", None),
                ("TIMESTAMP(0)", Some(0)),
                ("TIME(2)", Some(2)),
                ("DATETIME(3)", Some(3)),
                ("DATETIME2(4)", Some(4)),
                ("DATETIME2", None),
                ("DateTime64(5)", Some(5)),
                ("TIMESTAMP_NTZ(1)", Some(1)),
                ("INT", None),
            ]);
        }

        #[test]
        fn sqlite_has_no_temporal_type() {
            assert_precisions::<SQLiteDialect>(&[
                ("TIMESTAMP(3)", None),
                ("DATETIME", None),
                ("TIME", None),
            ]);
        }
    }

    mod numeric_scale {
        use sqlparser::dialect::{
            AnsiDialect, BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect,
            DuckDbDialect, GenericDialect, HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect,
            PostgreSqlDialect, RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect,
            SparkSqlDialect, TeradataDialect,
        };

        use super::{ColumnLike, DataTypeLike, column_named, parse_with};

        /// Asserts the scale of a column declared with each type.
        fn assert_scales<D: Dialect + Default + 'static>(cases: &[(&str, Option<i64>)]) {
            let columns: Vec<String> =
                cases.iter().enumerate().map(|(index, (ty, _))| format!("c{index} {ty}")).collect();
            let db = parse_with::<D>(&format!("CREATE TABLE t ({});", columns.join(", ")));
            for (index, (ty, expected)) in cases.iter().enumerate() {
                let column = column_named(&db, &format!("c{index}"));
                assert_eq!(column.data_type(&db).numeric_scale(&db), *expected, "{ty}");
            }
        }

        /// Written scales and precisions, which every engine but SQLite reads
        /// alike.
        const WRITTEN: [(&str, Option<i64>); 8] = [
            ("NUMERIC(10,2)", Some(2)),
            ("DECIMAL(5,4)", Some(4)),
            ("NUMERIC(10,0)", Some(0)),
            ("NUMERIC(10)", Some(0)),
            ("DECIMAL(12)", Some(0)),
            ("INT", None),
            ("DOUBLE PRECISION", None),
            ("VARCHAR(10)", None),
        ];

        #[test]
        fn a_written_scale_answers_as_written_and_a_precision_alone_means_zero() {
            assert_scales::<PostgreSqlDialect>(&WRITTEN);
            assert_scales::<RedshiftSqlDialect>(&WRITTEN);
            assert_scales::<MySqlDialect>(&WRITTEN);
            assert_scales::<MsSqlDialect>(&WRITTEN);
            assert_scales::<OracleDialect>(&WRITTEN);
            assert_scales::<SnowflakeDialect>(&WRITTEN);
            assert_scales::<TeradataDialect>(&WRITTEN);
            assert_scales::<BigQueryDialect>(&WRITTEN);
            assert_scales::<DuckDbDialect>(&WRITTEN);
            assert_scales::<ClickHouseDialect>(&WRITTEN);
            assert_scales::<HiveDialect>(&WRITTEN);
            assert_scales::<SparkSqlDialect>(&WRITTEN);
            assert_scales::<DatabricksDialect>(&WRITTEN);
            assert_scales::<AnsiDialect>(&WRITTEN);
            assert_scales::<GenericDialect>(&WRITTEN);
        }

        #[test]
        fn postgres_bare_numeric_holds_any_scale_and_a_negative_scale_rounds() {
            assert_scales::<PostgreSqlDialect>(&[
                ("NUMERIC", None),
                ("DECIMAL", None),
                ("DEC(5,2)", Some(2)),
                ("NUMERIC(5,-2)", Some(-2)),
                ("NUMERIC(10,2)[]", None),
            ]);
        }

        #[test]
        fn bare_decimal_keeps_no_fraction_where_the_engine_defaults_scale_to_zero() {
            let cases = [("NUMERIC", Some(0)), ("DECIMAL", Some(0))];
            assert_scales::<RedshiftSqlDialect>(&cases);
            assert_scales::<MySqlDialect>(&cases);
            assert_scales::<MsSqlDialect>(&cases);
            assert_scales::<OracleDialect>(&cases);
            assert_scales::<SnowflakeDialect>(&cases);
            assert_scales::<TeradataDialect>(&cases);
            assert_scales::<ClickHouseDialect>(&cases);
            assert_scales::<HiveDialect>(&cases);
            assert_scales::<SparkSqlDialect>(&cases);
            assert_scales::<DatabricksDialect>(&cases);
            assert_scales::<AnsiDialect>(&cases);
        }

        #[test]
        fn mysql_unsigned_zerofill_and_fixed_keep_their_scale() {
            assert_scales::<MySqlDialect>(&[
                ("DECIMAL(10,2) UNSIGNED", Some(2)),
                ("DEC UNSIGNED", Some(0)),
                ("DECIMAL(10,2) ZEROFILL", Some(2)),
                ("FIXED(10,2)", Some(2)),
                ("FIXED", Some(0)),
            ]);
        }

        #[test]
        fn number_is_floating_when_bare_in_oracle_and_teradata() {
            let cases = [("NUMBER", None), ("NUMBER(10)", Some(0)), ("NUMBER(10,2)", Some(2))];
            assert_scales::<OracleDialect>(&cases);
            assert_scales::<TeradataDialect>(&cases);
        }

        #[test]
        fn snowflake_number_defaults_to_whole_numbers() {
            assert_scales::<SnowflakeDialect>(&[
                ("NUMBER", Some(0)),
                ("NUMBER(10)", Some(0)),
                ("NUMBER(38,6)", Some(6)),
            ]);
        }

        #[test]
        fn bigquery_bare_types_keep_nine_and_thirty_eight_digits() {
            assert_scales::<BigQueryDialect>(&[
                ("NUMERIC", Some(9)),
                ("DECIMAL", Some(9)),
                ("BIGNUMERIC", Some(38)),
                ("BIGDECIMAL", Some(38)),
                ("BIGNUMERIC(40)", Some(0)),
                ("BIGNUMERIC(50,20)", Some(20)),
            ]);
        }

        #[test]
        fn duckdb_bare_decimal_keeps_three_digits() {
            assert_scales::<DuckDbDialect>(&[("DECIMAL", Some(3)), ("NUMERIC", Some(3))]);
        }

        #[test]
        fn clickhouse_sized_decimals_take_the_scale_alone() {
            assert_scales::<ClickHouseDialect>(&[
                ("Decimal32(4)", Some(4)),
                ("Decimal64(6)", Some(6)),
                ("Decimal128(10)", Some(10)),
                ("Decimal256(20)", Some(20)),
                ("Nullable(Decimal(10,2))", Some(2)),
            ]);
        }

        #[test]
        fn generic_answers_only_a_written_scale() {
            assert_scales::<GenericDialect>(&[
                ("NUMERIC", None),
                ("DECIMAL", None),
                ("BIGNUMERIC(40,20)", Some(20)),
                ("NUMBER", None),
                ("NUMBER(10,2)", Some(2)),
                ("NUMBER(10)", Some(0)),
                ("Decimal64(4)", Some(4)),
            ]);
        }

        #[test]
        fn a_decimal_spelling_answers_only_in_the_engines_that_own_it() {
            assert_scales::<PostgreSqlDialect>(&[
                ("NUMBER(10,2)", None),
                ("FIXED(10,2)", None),
                ("Decimal64(4)", None),
                ("money_amount", None),
            ]);
            assert_scales::<MySqlDialect>(&[("NUMBER(10,2)", None), ("Decimal64(4)", None)]);
            assert_scales::<OracleDialect>(&[("FIXED(10,2)", None)]);
        }

        #[test]
        fn a_number_with_modifiers_other_than_precision_and_scale_has_no_scale() {
            assert_scales::<OracleDialect>(&[
                ("NUMBER(10,2,1)", None),
                ("NUMBER(p,2)", None),
                ("NUMBER('10')", None),
            ]);
        }

        #[test]
        fn sqlite_stores_decimals_regardless_of_the_declared_scale() {
            assert_scales::<SQLiteDialect>(&[
                ("NUMERIC(10,2)", None),
                ("DECIMAL(5,4)", None),
                ("NUMERIC", None),
            ]);
        }
    }

    mod identity_and_serial {
        use sqlparser::dialect::{
            Dialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect,
            SnowflakeDialect,
        };

        use super::{Arc, ColumnLike, TableLike, column_named, parse_with};
        use crate::{
            prelude::ParserDB,
            structs::{IdentifierCase, TargetName},
            traits::{DatabaseLike, IdentityGeneration},
        };

        fn identities<D: Dialect + Default + 'static, const N: usize>(
            sql: &str,
            names: [&str; N],
        ) -> [Option<IdentityGeneration>; N] {
            let db = parse_with::<D>(sql);
            names.map(|name| column_named(&db, name).identity_generation(&db))
        }

        fn serials<D: Dialect + Default + 'static, const N: usize>(
            columns: &str,
            names: [&str; N],
        ) -> [bool; N] {
            let db = parse_with::<D>(&format!("CREATE TABLE t ({columns});"));
            names.map(|name| column_named(&db, name).is_serial())
        }

        #[test]
        fn identity_reads_the_written_mode() {
            assert_eq!(
                identities::<PostgreSqlDialect, 5>(
                    "CREATE TABLE t (
                         always INT GENERATED ALWAYS AS IDENTITY,
                         by_default BIGINT GENERATED BY DEFAULT AS IDENTITY (START WITH 10),
                         computed INT GENERATED ALWAYS AS (1) STORED,
                         sequenced SERIAL,
                         plain INT
                     );",
                    ["always", "by_default", "computed", "sequenced", "plain"],
                ),
                [
                    Some(IdentityGeneration::Always),
                    Some(IdentityGeneration::ByDefault),
                    None,
                    None,
                    None
                ]
            );
        }

        #[test]
        fn identity_added_by_alter_table_is_read() {
            assert_eq!(
                identities::<PostgreSqlDialect, 2>(
                    "CREATE TABLE t (always INT NOT NULL, by_default INT NOT NULL);
                     ALTER TABLE t ALTER COLUMN always ADD GENERATED ALWAYS AS IDENTITY;
                     ALTER TABLE t ALTER COLUMN by_default ADD GENERATED BY DEFAULT AS IDENTITY;",
                    ["always", "by_default"],
                ),
                [Some(IdentityGeneration::Always), Some(IdentityGeneration::ByDefault)]
            );
        }

        /// SQL Server refuses an explicit value unless `IDENTITY_INSERT` is
        /// on, while Snowflake takes one whenever it is given.
        #[test]
        fn identity_without_written_mode_follows_the_dialect() {
            assert_eq!(
                identities::<MsSqlDialect, 1>("CREATE TABLE t (id INT IDENTITY(1, 1));", ["id"]),
                [Some(IdentityGeneration::Always)]
            );
            assert_eq!(
                identities::<SnowflakeDialect, 2>(
                    "CREATE TABLE t (id INT IDENTITY(1, 1), counter INT AUTOINCREMENT);",
                    ["id", "counter"],
                ),
                [Some(IdentityGeneration::ByDefault); 2]
            );
        }

        #[test]
        fn serial_reads_every_pseudo_type_in_any_case() {
            let columns = "a smallserial, b serial, c bigserial, d SERIAL2, e Serial4, \
                           f SERIAL8, g \"serial\", h INT, i INT GENERATED ALWAYS AS IDENTITY";
            let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
            let expected = [true, true, true, true, true, true, true, false, false];
            assert_eq!(serials::<PostgreSqlDialect, 9>(columns, names), expected);
            assert_eq!(serials::<GenericDialect, 9>(columns, names), expected);
        }

        /// PostgreSQL folds only unquoted names, so a quoted name names the
        /// pseudo-type only when written in lower case, and a qualified name
        /// never does.
        #[test]
        fn serial_follows_postgres_name_folding() {
            assert_eq!(
                serials::<PostgreSqlDialect, 2>(
                    "upper \"SERIAL\", qualified pg_catalog.serial",
                    ["upper", "qualified"],
                ),
                [false, false]
            );
        }

        #[test]
        fn serial_reads_the_mysql_shorthand() {
            assert_eq!(serials::<MySqlDialect, 2>("id SERIAL, n INT", ["id", "n"]), [true, false]);
        }

        /// The database supplies the value of a serial and of an identity, and
        /// of nothing else, a computed column being derived from its row.
        #[test]
        fn generated_covers_serials_and_identities() {
            let db = parse_with::<PostgreSqlDialect>(
                "CREATE TABLE t (
                     lower_serial serial,
                     upper_serial BIGSERIAL,
                     always INT GENERATED ALWAYS AS IDENTITY,
                     by_default INT GENERATED BY DEFAULT AS IDENTITY,
                     computed INT GENERATED ALWAYS AS (1) STORED,
                     plain INT
                 );",
            );
            let generated =
                ["lower_serial", "upper_serial", "always", "by_default", "computed", "plain"]
                    .map(|name| column_named(&db, name).is_generated(&db));
            assert_eq!(generated, [true, true, true, true, false, false]);
        }

        #[test]
        fn identity_primary_key_is_surrogate() {
            let db: ParserDB = parse_with::<PostgreSqlDialect>(
                "CREATE TABLE t (id INT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT);",
            );
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("table t exists");
            assert!(table.has_surrogate_primary_key(&db).expect("in database"));
            assert!(column_named(&db, "id").is_surrogate_key(&db).expect("in database"));
        }

        #[test]
        fn readers_forward_through_reference_and_arc() {
            let db = parse_with::<PostgreSqlDialect>(
                "CREATE TABLE t (id INT GENERATED BY DEFAULT AS IDENTITY, seq serial);",
            );
            let id = column_named(&db, "id");
            let seq = column_named(&db, "seq");
            assert_eq!(
                <&_ as ColumnLike>::identity_generation(&id, &db),
                Some(IdentityGeneration::ByDefault)
            );
            assert_eq!(
                Arc::new(id.clone()).identity_generation(&db),
                Some(IdentityGeneration::ByDefault)
            );
            assert!(<&_ as ColumnLike>::is_serial(&seq));
            assert!(Arc::new(seq.clone()).is_serial());
        }
    }

    fn column_named<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Column {
        let table = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table t exists");
        table
            .column(name, db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists")
    }

    #[test]
    fn collation_reads_database_default() {
        let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (name TEXT);");
        let column = column_named(&db, "name");
        assert_eq!(
            column.collation(&db).expect("collation metadata"),
            ColumnCollation::DatabaseDefault
        );
    }

    #[test]
    fn collation_reads_explicit_name() {
        let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (name TEXT COLLATE \"C\");");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.name().name(), "C");
        assert!(collation.name().name_is_quoted());
        assert_eq!(collation.name().schema(), None);
        assert_eq!(collation.postgres_deterministic(), Some(true));
        assert_eq!(collation.mysql_padding(), None);
    }

    #[test]
    fn collation_generic_keeps_explicit_name_without_postgres_metadata() {
        let db = parse_with::<GenericDialect>("CREATE TABLE t (name TEXT COLLATE \"C\");");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.name().name(), "C");
        assert_eq!(collation.postgres_deterministic(), None);
    }

    #[test]
    fn collation_generic_create_collation_skips_postgres_validation() {
        let db = parse_with::<GenericDialect>(
            "
            CREATE COLLATION app.ci FROM missing_ci;
            CREATE COLLATION ci FROM missing_ci;
            CREATE COLLATION ci FROM missing_ci;
            CREATE TABLE t (name TEXT COLLATE ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.name().name(), "ci");
        assert_eq!(collation.postgres_deterministic(), None);
    }

    #[test]
    fn collation_uses_supplied_postgres_catalog() {
        let catalog = PostgresCatalog::empty()
            .with_collation(
                PostgresCatalogCollation::new("en_US.utf8", true).with_deterministic(false),
            )
            .with_collatable_type(PostgresCatalogType::new("text", false));
        let db = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>("CREATE TABLE t (name TEXT COLLATE \"en_US.utf8\");")
            .expect("parse");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_postgres_rejects_noncollatable_builtin_type() {
        let result =
            ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id integer COLLATE \"C\");");
        assert!(matches!(result, Err(Error::NonCollatableColumnType { .. })));
    }

    #[test]
    fn collation_postgres_rejects_repeated_column_clause() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (name TEXT COLLATE \"C\" COLLATE \"POSIX\");",
        );
        assert!(matches!(result, Err(Error::RepeatedColumnCollation { .. })));
    }

    #[test]
    fn collation_postgres_accepts_collatable_builtin_types() {
        for ty in ["TEXT", "VARCHAR(12)", "CHAR(3)", "TEXT[]"] {
            let sql = format!("CREATE TABLE t (name {ty} COLLATE \"C\");");
            let db = parse_with::<PostgreSqlDialect>(&sql);
            let column = column_named(&db, "name");
            let ColumnCollation::Named(collation) =
                column.collation(&db).expect("collation metadata")
            else {
                panic!("expected a named collation");
            };
            assert_eq!(collation.postgres_deterministic(), Some(true), "{ty}");
        }
    }

    #[test]
    fn collation_postgres_rejects_custom_type_without_catalog_fact() {
        let catalog =
            PostgresCatalog::empty().with_collation(PostgresCatalogCollation::new("ci", false));
        let result = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>("CREATE TABLE t (name citext COLLATE ci);");
        assert!(matches!(result, Err(Error::ColumnTypeCollatabilityNotInCatalog { .. })));
    }

    #[test]
    fn collation_catalog_default_includes_new_postgres_18_builtins() {
        for name in ["pg_c_utf8", "pg_unicode_fast"] {
            let sql = format!("CREATE TABLE t (name TEXT COLLATE {name});");
            let db = parse_with::<PostgreSqlDialect>(&sql);
            let column = column_named(&db, "name");
            let ColumnCollation::Named(collation) =
                column.collation(&db).expect("collation metadata")
            else {
                panic!("expected a named collation");
            };
            assert_eq!(collation.postgres_deterministic(), Some(true), "{name}");
        }
    }

    #[test]
    fn collation_supplied_catalog_replaces_default() {
        let result = ParseOptions::default()
            .with_postgres_catalog(PostgresCatalog::empty())
            .parse::<PostgreSqlDialect>("CREATE TABLE t (name TEXT COLLATE \"C\");");
        assert!(matches!(result, Err(Error::CollationNotFound { .. })));
    }

    #[test]
    fn collation_catalog_records_custom_collatable_type() {
        let catalog = PostgresCatalog::empty()
            .with_collation(PostgresCatalogCollation::new("ci", false))
            .with_collatable_type(PostgresCatalogType::new("citext", false));
        let db = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>("CREATE TABLE t (name citext COLLATE ci);")
            .expect("parse");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(true));
    }

    #[test]
    fn collation_supplied_fact_replaces_built_in_of_same_identity() {
        let catalog = PostgresCatalog::postgres_18()
            .with_collation(PostgresCatalogCollation::new("C", true).with_deterministic(false));
        let built_ins = PostgresCatalog::postgres_18().collations().count();
        assert_eq!(catalog.collations().count(), built_ins);
        let is_c = |collation: &&PostgresCatalogCollation| {
            collation.schema() == Some("pg_catalog") && collation.name() == "C"
        };
        assert_eq!(catalog.collations().filter(is_c).count(), 1);
        let newest = catalog.collations().next_back().expect("the catalog holds collations");
        assert!(is_c(&newest) && !newest.deterministic(), "the replacement is the newest fact");

        let db = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>("CREATE TABLE t (name TEXT COLLATE \"C\");")
            .expect("parse");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_supplied_type_replaces_built_in_of_same_identity() {
        let catalog = PostgresCatalog::postgres_18()
            .with_collatable_type(PostgresCatalogType::new("text", false));
        let built_ins = PostgresCatalog::postgres_18().collatable_types().count();
        assert_eq!(catalog.collatable_types().count(), built_ins);
        let newest = catalog.collatable_types().last().expect("the catalog holds types");
        assert_eq!((newest.schema(), newest.name()), (Some("pg_catalog"), "text"));
    }

    #[test]
    fn collation_catalog_extends_built_ins_with_schema_qualified_type() {
        let catalog = PostgresCatalog::postgres_18().with_collatable_type(
            PostgresCatalogType::new("citext", false).with_schema("app", false),
        );
        let sql = "CREATE SCHEMA app; CREATE TABLE t (name app.citext COLLATE \"C\");";
        let db = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>(sql)
            .expect("parse");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(true));
        assert!(matches!(
            ParserDB::parse::<PostgreSqlDialect>(sql),
            Err(Error::ColumnTypeCollatabilityNotInCatalog { .. })
        ));
    }

    #[test]
    fn collation_catalog_reaches_parsed_statement_constructors() {
        let catalog = PostgresCatalog::empty()
            .with_collation(
                PostgresCatalogCollation::new("en_US.utf8", true).with_deterministic(false),
            )
            .with_collatable_type(PostgresCatalogType::new("text", false));
        let statements = sqlparser::parser::Parser::parse_sql(
            &PostgreSqlDialect {},
            "CREATE TABLE t (name TEXT COLLATE \"en_US.utf8\");",
        )
        .expect("parse statements");
        let db = ParserDB::from_statements_with_dialect(
            statements,
            String::from("unknown_catalog"),
            SqlparserDialect::PostgreSql,
        )
        .expect_err("plain constructor lacks the supplied catalog");
        assert!(matches!(db, Error::CollationNotFound { .. }));

        let statements = sqlparser::parser::Parser::parse_sql(
            &PostgreSqlDialect {},
            "CREATE TABLE t (name TEXT COLLATE \"en_US.utf8\");",
        )
        .expect("parse statements");
        let db = ParseOptions::default()
            .with_postgres_catalog(catalog.clone())
            .from_statements_with_dialect(
                statements,
                String::from("unknown_catalog"),
                SqlparserDialect::PostgreSql,
            )
            .expect("catalog reaches statements");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));

        let mut path = std::env::temp_dir();
        path.push(format!("sql_traits_catalog_{}.sql", std::process::id()));
        std::fs::write(&path, "CREATE TABLE t (name TEXT COLLATE \"en_US.utf8\");")
            .expect("write sql");
        let db = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .from_path::<PostgreSqlDialect>(&path)
            .expect("catalog reaches path");
        std::fs::remove_file(&path).expect("remove sql");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_builtin_name_ignores_public_shadow() {
        let db = parse_with::<PostgreSqlDialect>(
            r#"
            CREATE COLLATION public."C" (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE "C");
            "#,
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(true));
    }

    #[test]
    fn collation_resolution_respects_explicit_pg_catalog_position() {
        let db = parse_with::<PostgreSqlDialect>(
            r#"
            CREATE SCHEMA app;
            CREATE COLLATION app."C" (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            SET search_path = app, pg_catalog;
            CREATE COLLATION child_ci FROM "C";
            CREATE TABLE t (name TEXT COLLATE "C", child_name TEXT COLLATE child_ci);
            "#,
        );
        let table = db
            .table_by_target(
                TargetName::new("t", false).with_schema("app", false),
                IdentifierCase::AsWritten,
            )
            .expect("unambiguous lookup")
            .expect("table app.t exists");
        let column = table
            .column("name", &db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
        let column = table
            .column("child_name", &db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_reads_postgres_determinism_from_created_collation() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE app.ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.name().name(), "ci");
        assert_eq!(collation.name().schema(), Some("app"));
        assert_eq!(collation.postgres_deterministic(), Some(false));
        assert_eq!(collation.mysql_padding(), None);
    }

    #[test]
    fn collation_reads_created_pg_catalog_collation_metadata() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION pg_catalog.custom_ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE pg_catalog.custom_ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_metadata_survives_table_rebuild() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            ALTER TABLE t ADD COLUMN id INT;
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_metadata_keeps_original_resolution_after_path_change() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            SET search_path = app, public;
            ALTER TABLE public.t ADD COLUMN id INT;
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_metadata_survives_create_time_inheritance() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE p (name TEXT COLLATE ci);
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            SET search_path = app, public;
            CREATE TABLE public.t () INHERITS (public.p);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_metadata_rejects_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            CREATE TABLE p (name TEXT COLLATE ci);
            SET search_path = app, public;
            CREATE TABLE public.t (name TEXT COLLATE ci) INHERITS (public.p);
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_default_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE p (name TEXT);
            CREATE TABLE t (name TEXT COLLATE ci) INHERITS (p);
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_catalog_default_named_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            r#"
            CREATE COLLATION ci_false (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE p (name TEXT COLLATE "default");
            CREATE TABLE t (name TEXT COLLATE ci_false) INHERITS (p);
            "#,
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_accepts_catalog_default_alias() {
        let db = parse_with::<PostgreSqlDialect>(
            r#"
            CREATE TABLE p (name TEXT);
            CREATE TABLE t (name TEXT COLLATE "default") INHERITS (p);
            "#,
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(true));
    }

    #[test]
    fn collation_metadata_rejects_catalog_ucs_basic_default_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE TABLE p (name TEXT);
            CREATE TABLE t (name TEXT COLLATE ucs_basic) INHERITS (p);
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_catalog_unicode_default_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE TABLE p (name TEXT);
            CREATE TABLE t (name TEXT COLLATE unicode) INHERITS (p);
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_alter_add_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci_false (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION ci_true (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            CREATE TABLE p (id INT);
            CREATE TABLE t (name TEXT COLLATE ci_true) INHERITS (p);
            ALTER TABLE p ADD COLUMN name TEXT COLLATE ci_false;
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_alter_add_default_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci_false (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE p (id INT);
            CREATE TABLE t (name TEXT COLLATE ci_false) INHERITS (p);
            ALTER TABLE p ADD COLUMN name TEXT;
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_multi_parent_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            CREATE TABLE p1 (name TEXT COLLATE ci);
            SET search_path = app, public;
            CREATE TABLE p2 (name TEXT COLLATE ci);
            CREATE TABLE public.t () INHERITS (public.p1, p2);
            ",
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_builtin_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            r#"
            CREATE TABLE p (name TEXT COLLATE "C");
            CREATE TABLE t (name TEXT COLLATE "POSIX") INHERITS (p);
            "#,
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_c_utf8_builtin_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            r#"
            CREATE TABLE p (name TEXT COLLATE "C.utf8");
            CREATE TABLE t (name TEXT COLLATE "POSIX") INHERITS (p);
            "#,
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_icu_builtin_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            r#"
            CREATE TABLE p (name TEXT COLLATE "af-x-icu");
            CREATE TABLE t (name TEXT COLLATE "POSIX") INHERITS (p);
            "#,
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_rejects_en_icu_builtin_inherited_conflict() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            r#"
            CREATE TABLE p (name TEXT COLLATE "en-x-icu");
            CREATE TABLE t (name TEXT COLLATE "POSIX") INHERITS (p);
            "#,
        );
        assert!(matches!(result, Err(Error::InheritedColumnCollationConflict { .. })));
    }

    #[test]
    fn collation_metadata_attaches_to_altered_column() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (id INT);
            ALTER TABLE t ADD COLUMN name TEXT COLLATE ci;
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_metadata_survives_column_rename() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            ALTER TABLE t RENAME COLUMN name TO label;
            ",
        );
        let column = column_named(&db, "label");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_metadata_resolves_unqualified_name_on_search_path() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            SET search_path = app;
            CREATE TABLE t (name TEXT COLLATE ci);
            ",
        );
        let table = db
            .table_by_target(
                TargetName::new("t", false).with_schema("app", false),
                IdentifierCase::AsWritten,
            )
            .expect("unambiguous lookup")
            .expect("table app.t exists");
        let column = table
            .column("name", &db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.name().name(), "ci");
        assert_eq!(collation.name().schema(), None);
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_from_copies_source_metadata() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION base_ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION child_ci FROM base_ci;
            CREATE TABLE t (name TEXT COLLATE child_ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_from_created_pg_catalog_collation() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION pg_catalog.custom_ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION child_ci FROM pg_catalog.custom_ci;
            CREATE TABLE t (name TEXT COLLATE child_ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_resolves_created_pg_catalog_collation_through_implicit_path() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            SET search_path = public;
            CREATE COLLATION pg_catalog.custom_ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE custom_ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_from_resolves_created_pg_catalog_source_through_implicit_path() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            SET search_path = public;
            CREATE COLLATION pg_catalog.custom_ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION child_ci FROM custom_ci;
            CREATE TABLE t (name TEXT COLLATE child_ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_from_builtin_ignores_public_shadow() {
        let db = parse_with::<PostgreSqlDialect>(
            r#"
            CREATE COLLATION public."C" (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION child_ci FROM "C";
            CREATE TABLE t (name TEXT COLLATE child_ci);
            "#,
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(true));
    }

    #[test]
    fn collation_from_rejects_catalog_default() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            r#"
            CREATE COLLATION child_ci FROM "default";
            CREATE TABLE t (name TEXT COLLATE child_ci);
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn collation_from_rejects_missing_source() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION child_ci FROM missing_ci;
            CREATE TABLE t (name TEXT COLLATE child_ci);
            ",
        );
        assert!(matches!(result, Err(Error::CollationNotFound { .. })));
    }

    #[test]
    fn collation_from_uses_explicit_path_default_shadow() {
        let db = parse_with::<PostgreSqlDialect>(
            r#"
            CREATE COLLATION public."default" (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            SET search_path = public, pg_catalog;
            CREATE COLLATION child_ci FROM "default";
            CREATE TABLE t (name TEXT COLLATE child_ci);
            "#,
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_create_defaults_deterministic_true() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2'
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(true));
    }

    #[test]
    fn collation_create_accepts_quoted_deterministic_key() {
        let db = parse_with::<PostgreSqlDialect>(
            r#"
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                "deterministic" = false
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            "#,
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_create_rejects_invalid_boolean_option() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = maybe
            );
            ",
        );
        assert!(matches!(result, Err(Error::InvalidCollationOption { .. })));
    }

    #[test]
    fn collation_create_rejects_repeated_option_keys() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true,
                DETERMINISTIC = false
            );
            ",
        );
        assert!(matches!(result, Err(Error::RepeatedCollationOption { .. })));
    }

    #[test]
    fn collation_create_rejects_namespaced_option_keys() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                x.provider = icu,
                locale = 'und-u-ks-level2'
            );
            ",
        );
        assert!(
            matches!(&result, Err(Error::InvalidCollationOption { option_name, .. }) if &**option_name == "x.provider"),
            "{result:?}"
        );
    }

    #[test]
    fn collation_create_rejects_invalid_provider() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = invalid,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ",
        );
        assert!(matches!(result, Err(Error::InvalidCollationOption { .. })));
    }

    #[test]
    fn collation_create_if_not_exists_validates_before_skip() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION IF NOT EXISTS ci (
                provider = invalid,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ",
        );
        assert!(matches!(result, Err(Error::InvalidCollationOption { .. })));
    }

    #[test]
    fn collation_rejects_duplicate_created_name() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION public.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            ",
        );
        assert!(matches!(result, Err(Error::CollationAlreadyExists { .. })));
    }

    #[test]
    fn collation_rejects_pg_catalog_builtin_collision() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION pg_catalog.ucs_basic (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ",
        );
        assert!(matches!(result, Err(Error::CollationAlreadyExists { .. })));
    }

    #[test]
    fn collation_create_if_not_exists_keeps_original_metadata() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION IF NOT EXISTS ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = true
            );
            CREATE TABLE t (name TEXT COLLATE ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_duplicate_from_rejects_missing_source_before_duplicate() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION ci FROM missing_ci;
            ",
        );
        assert!(matches!(result, Err(Error::CollationNotFound { .. })));
    }

    #[test]
    fn collation_if_not_exists_from_rejects_missing_source() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE COLLATION IF NOT EXISTS ci FROM missing_ci;
            ",
        );
        assert!(matches!(result, Err(Error::CollationNotFound { .. })));
    }

    #[test]
    fn collation_rejects_missing_created_schema() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ",
        );
        assert!(matches!(result, Err(Error::SchemaNotFoundForCollation { .. })));
    }

    #[test]
    fn collation_rejects_missing_column_collation() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE TABLE t (name TEXT COLLATE missing_ci);
            ",
        );
        assert!(matches!(result, Err(Error::CollationNotFound { .. })));
    }

    #[test]
    fn collation_create_skips_missing_search_path_schema() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            SET search_path = missing, app;
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE app.t (name TEXT COLLATE app.ci);
            ",
        );
        let table = db
            .table_by_target(
                TargetName::new("t", false).with_schema("app", false),
                IdentifierCase::AsWritten,
            )
            .expect("unambiguous lookup")
            .expect("table app.t exists");
        let column = table
            .column("name", &db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_create_rejects_path_without_creatable_schema() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            SET search_path = missing;
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ",
        );
        assert!(matches!(result, Err(Error::NoSchemaSelectedForCollation { .. })));
    }

    #[test]
    fn collation_rejects_stale_column_node() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE TABLE t (name TEXT COLLATE \"C\");
            ",
        );
        let stale = column_named(&db, "name").clone();
        let renamed = parse_with::<PostgreSqlDialect>(
            "
            CREATE TABLE t (renamed TEXT COLLATE \"C\");
            ",
        );
        assert!(stale.collation(&renamed).is_err());
    }

    #[test]
    fn collation_forwards_through_reference_and_arc() {
        let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (name TEXT COLLATE \"C\");");
        let column = column_named(&db, "name");
        let reference = &column;
        let arc = Arc::new(column.clone());
        assert!(matches!(
            reference.collation(&db).expect("collation metadata"),
            ColumnCollation::Named(_)
        ));
        assert!(matches!(
            arc.collation(&db).expect("collation metadata"),
            ColumnCollation::Named(_)
        ));
    }

    #[test]
    fn collation_reads_unknown_for_character_set() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (name TEXT CHARACTER SET utf8mb4);");
        let column = column_named(&db, "name");
        assert_eq!(column.collation(&db).expect("collation metadata"), ColumnCollation::Unknown);
    }

    #[test]
    fn collation_reads_mysql_explicit_name_with_unknown_metadata() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (name TEXT COLLATE utf8mb4_bin);");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.name().name(), "utf8mb4_bin");
        assert_eq!(collation.postgres_deterministic(), None);
        assert_eq!(collation.mysql_padding(), None);
    }

    #[test]
    fn collation_schema_rename_updates_created_collation_resolution() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ALTER SCHEMA app RENAME TO renamed;
            CREATE TABLE t (name TEXT COLLATE renamed.ci);
            ",
        );
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_schema_rename_updates_configured_collation_resolution() {
        let catalog = PostgresCatalog::empty()
            .with_collation(
                PostgresCatalogCollation::new("ci", false)
                    .with_schema("app", false)
                    .with_deterministic(false),
            )
            .with_collatable_type(PostgresCatalogType::new("text", false));
        let db = ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>(
                "
                CREATE SCHEMA app;
                ALTER SCHEMA app RENAME TO renamed;
                CREATE TABLE t (name TEXT COLLATE renamed.ci);
                ",
            )
            .expect("parse");
        let column = column_named(&db, "name");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_schema_rename_uses_current_search_path() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ALTER SCHEMA app RENAME TO renamed;
            SET search_path = renamed;
            CREATE TABLE t (name TEXT COLLATE ci);
            ",
        );
        let table = db
            .table_by_target(
                TargetName::new("t", false).with_schema("renamed", false),
                IdentifierCase::AsWritten,
            )
            .expect("unambiguous lookup")
            .expect("table exists");
        let column = table
            .column("name", &db, IdentifierCase::AsWritten)
            .expect("column lookup")
            .expect("column exists");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }

    #[test]
    fn collation_schema_rename_removes_old_created_collation_resolution() {
        let result = ParserDB::parse::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            ALTER SCHEMA app RENAME TO renamed;
            CREATE TABLE t (name TEXT COLLATE app.ci);
            ",
        );
        assert!(matches!(result, Err(Error::CollationNotFound { .. })));
    }

    #[test]
    fn collation_schema_rename_keeps_existing_metadata_across_rebuilds() {
        let db = parse_with::<PostgreSqlDialect>(
            "
            CREATE SCHEMA app;
            CREATE COLLATION app.ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (name TEXT COLLATE app.ci);
            ALTER SCHEMA app RENAME TO renamed;
            ALTER TABLE t RENAME COLUMN name TO title;
            ",
        );
        let column = column_named(&db, "title");
        let ColumnCollation::Named(collation) = column.collation(&db).expect("collation metadata")
        else {
            panic!("expected a named collation");
        };
        assert_eq!(collation.postgres_deterministic(), Some(false));
    }
}
