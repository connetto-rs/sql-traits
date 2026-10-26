//! Submodule implementing the [`ColumnLike`] trait for `sqlparser`'s
//! [`ColumnDef`] struct.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
};

use sqlparser::ast::{ColumnDef, ColumnOption, CreateTable};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ColumnMetadata, ParserDB, TableAttribute},
    traits::{ColumnCollation, ColumnLike, DatabaseLike, Metadata, NamedColumnCollation},
    utils::{is_identity, normalize_sqlparser_type, object_name::target_name_of_object_name},
};

const GENERATED_TYPES: &[&str] = &["SERIAL", "BIGSERIAL", "SMALLSERIAL"];

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
    fn data_type<'db>(&'db self, _database: &'db Self::DB) -> Cow<'db, str> {
        normalize_sqlparser_type(&self.attribute().data_type)
    }

    #[inline]
    fn fixed_binary_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::fixed_binary_length(*database.dialect(), &self.attribute().data_type)
    }

    #[inline]
    fn max_character_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::max_character_length(*database.dialect(), &self.attribute().data_type)
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
    fn is_generated(&self) -> bool {
        GENERATED_TYPES.contains(&self.attribute().data_type.to_string().as_str())
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
            ColumnCollation, ColumnLike, DatabaseLike, DialectLike, TableLike, TypeMatch,
            TypeMatchLike,
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
            .map(|name| column_named(&db, name).fixed_binary_length(&db))
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

    #[test]
    fn fixed_binary_length_forwards_through_reference_and_arc() {
        let db = parse_with::<MySqlDialect>("CREATE TABLE t (token BINARY(16));");
        let column = column_named(&db, "token");
        assert_eq!(<&_ as ColumnLike>::fixed_binary_length(&column, &db), Some(16));
        assert_eq!(Arc::new(column.clone()).fixed_binary_length(&db), Some(16));
    }

    mod max_character_length {
        use sqlparser::dialect::{
            AnsiDialect, BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect,
            DuckDbDialect, GenericDialect, HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect,
            PostgreSqlDialect, RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect,
            SparkSqlDialect, TeradataDialect,
        };

        use super::{Arc, ColumnLike, column_named, parse_with};

        fn lengths<D: Dialect + Default + 'static, const N: usize>(
            columns: &str,
            names: [&str; N],
        ) -> [Option<u64>; N] {
            let db = parse_with::<D>(&format!("CREATE TABLE t ({columns});"));
            names.map(|name| column_named(&db, name).max_character_length(&db))
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

        #[test]
        fn forwards_through_reference_and_arc() {
            let db = parse_with::<PostgreSqlDialect>("CREATE TABLE t (title VARCHAR(100));");
            let column = column_named(&db, "title");
            assert_eq!(<&_ as ColumnLike>::max_character_length(&column, &db), Some(100));
            assert_eq!(Arc::new(column.clone()).max_character_length(&db), Some(100));
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
