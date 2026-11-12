//! Tests that inline structured types, maps and ClickHouse's type wrappers
//! answer their fields, entries and wrapped facts, so two declarations that
//! differ in any of them answer differently.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use core::fmt::Write as _;

use sql_traits::prelude::*;
use sqlparser::{
    ast::DataType,
    dialect::{
        BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect,
        GenericDialect, PostgreSqlDialect, SnowflakeDialect,
    },
};

/// A database holding table `t` with one column `c` of `declared`.
fn declaring<D: Dialect + Default>(declared: &str) -> ParserDB {
    let engine = if core::any::TypeId::of::<D>() == core::any::TypeId::of::<ClickHouseDialect>() {
        " ENGINE = Memory"
    } else {
        ""
    };
    ParserDB::parse::<D>(&format!("CREATE TABLE t (c {declared}){engine};"))
        .unwrap_or_else(|error| panic!("{declared} builds: {error}"))
}

/// The declared type of column `c` of table `t`.
fn column_type(database: &ParserDB) -> &DataType {
    database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t is declared")
        .column("c", database, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("c is declared")
        .data_type(database)
}

/// Every fact the trait answers for `data_type`, through its fields and map
/// entry, as text two declarations can be compared by.
fn facts(data_type: &DataType, database: &ParserDB) -> String {
    let mut out = format!(
        "{} fixbin={:?} chars={:?} fsp={:?} precision={:?} scale={:?} null={} kind={:?}",
        data_type.normalized_name(database),
        data_type.fixed_binary_length(database),
        data_type.max_character_length(database),
        data_type.fractional_seconds_precision(database),
        data_type.numeric_precision(database),
        data_type.numeric_scale(database),
        data_type.admits_null(database),
        data_type.structure_kind(database),
    );
    for field in data_type.structured_fields(database).into_iter().flatten() {
        write!(
            out,
            " [{:?} quoted={} null={} options={:?} collation={:?} {}]",
            field.name(),
            field.name_is_quoted(),
            field.is_nullable(),
            field.options(),
            field.collation(),
            facts(field.data_type(), database),
        )
        .expect("writing to a string");
    }
    if let Some(entry) = data_type.map_entry(database) {
        write!(
            out,
            " map<{} -> {} null={}>",
            facts(entry.key(), database),
            facts(entry.value(), database),
            entry.value_is_nullable(),
        )
        .expect("writing to a string");
    }
    out
}

/// The facts column `c` of `declared` answers.
fn facts_of<D: Dialect + Default>(declared: &str) -> String {
    let database = declaring::<D>(declared);
    facts(column_type(&database), &database)
}

/// Two declarations of one dialect, with how that dialect answers each.
type Pair = (fn(&str) -> String, &'static str, &'static str);

#[test]
fn renamed_retyped_reordered_reoptioned_or_recollated_fields_answer_differently() {
    let distinct: [Pair; 13] = [
        (facts_of::<BigQueryDialect>, "STRUCT<code INT64>", "STRUCT<identifier INT64>"),
        (facts_of::<BigQueryDialect>, "STRUCT<code INT64>", "STRUCT<code STRING>"),
        (
            facts_of::<BigQueryDialect>,
            "STRUCT<code INT64, label STRING>",
            "STRUCT<label STRING, code INT64>",
        ),
        (
            facts_of::<BigQueryDialect>,
            "STRUCT<amount NUMERIC(10, 2)>",
            "STRUCT<amount NUMERIC(10, 2) OPTIONS(rounding_mode = 'ROUND_HALF_EVEN')>",
        ),
        (
            facts_of::<BigQueryDialect>,
            "STRUCT<amount NUMERIC(10, 2) OPTIONS(rounding_mode = 'ROUND_HALF_EVEN')>",
            "STRUCT<amount NUMERIC(10, 2) OPTIONS(rounding_mode = 'ROUND_HALF_AWAY_FROM_ZERO')>",
        ),
        (
            facts_of::<DuckDbDialect>,
            "UNION(code INTEGER, label VARCHAR)",
            "UNION(identifier INTEGER, label VARCHAR)",
        ),
        (
            facts_of::<DuckDbDialect>,
            "UNION(code INTEGER, label VARCHAR)",
            "UNION(code BIGINT, label VARCHAR)",
        ),
        (
            facts_of::<DuckDbDialect>,
            "UNION(code INTEGER, label VARCHAR)",
            "UNION(label VARCHAR, code INTEGER)",
        ),
        (facts_of::<SnowflakeDialect>, "OBJECT(city VARCHAR)", "OBJECT(city VARCHAR NOT NULL)"),
        (
            facts_of::<DatabricksDialect>,
            "STRUCT<label VARCHAR(8) COLLATE UTF8_LCASE>",
            "STRUCT<label VARCHAR(16) COLLATE UTF8_LCASE>",
        ),
        (
            facts_of::<ClickHouseDialect>,
            "Nested(amount Decimal(10, 2))",
            "Nested(amount Decimal(12, 2))",
        ),
        (facts_of::<ClickHouseDialect>, "Tuple(Int32, String)", "Tuple(String, Int32)"),
        (facts_of::<ClickHouseDialect>, "Tuple(a Int32)", "Tuple(a Nullable(Int32))"),
    ];
    for (facts_of, left, right) in distinct {
        assert_ne!(facts_of(left), facts_of(right), "{left} against {right}");
    }
}

#[test]
fn equivalent_spellings_answer_alike() {
    let same: [Pair; 4] = [
        (facts_of::<BigQueryDialect>, "STRUCT<amount DEC(10, 2)>", "STRUCT<amount DECIMAL(10, 2)>"),
        (
            facts_of::<BigQueryDialect>,
            "STRUCT<amount NUMERIC(10, 2)>",
            "STRUCT<amount NUMERIC(10, 2) OPTIONS()>",
        ),
        (
            facts_of::<DuckDbDialect>,
            "UNION(code INTEGER, label VARCHAR)",
            "UNION(code INT, label VARCHAR)",
        ),
        (facts_of::<ClickHouseDialect>, "FixedString(8)", "LowCardinality(FixedString(8))"),
    ];
    for (facts_of, left, right) in same {
        assert_eq!(facts_of(left), facts_of(right), "{left} against {right}");
    }
}

#[test]
fn map_keys_values_and_value_nullability_answer() {
    for (left, right) in [
        ("Map(UInt64, Decimal(10, 2))", "Map(String, Decimal(10, 2))"),
        ("Map(UInt64, Decimal(10, 2))", "Map(UInt64, Decimal(12, 2))"),
        ("Map(UInt64, Int32)", "Map(UInt64, Nullable(Int32))"),
    ] {
        assert_ne!(
            facts_of::<ClickHouseDialect>(left),
            facts_of::<ClickHouseDialect>(right),
            "{left} against {right}"
        );
    }
    assert_ne!(
        facts_of::<SnowflakeDialect>("MAP(VARCHAR, NUMBER)"),
        facts_of::<SnowflakeDialect>("MAP(VARCHAR, NUMBER NOT NULL)"),
    );

    let database = declaring::<SnowflakeDialect>("MAP(VARCHAR, NUMBER NOT NULL)");
    let entry = column_type(&database).map_entry(&database).expect("a map answers its entry");
    assert_eq!(entry.key().normalized_name(&database), "VARCHAR");
    assert!(!entry.value_is_nullable());
}

#[test]
fn clickhouse_wrappers_answer_the_wrapped_facts() {
    let database = declaring::<ClickHouseDialect>("LowCardinality(Nullable(Decimal(12, 3)))");
    let data_type = column_type(&database);
    assert_eq!(data_type.numeric_precision(&database), Some(12));
    assert_eq!(data_type.numeric_scale(&database), Some(3));
    assert!(data_type.admits_null(&database));

    let database = declaring::<ClickHouseDialect>("LowCardinality(String)");
    assert!(!column_type(&database).admits_null(&database));

    for declared in ["Variant(UInt64, String)", "Dynamic"] {
        let database = declaring::<ClickHouseDialect>(declared);
        assert!(column_type(&database).admits_null(&database), "{declared}");
    }

    let database = declaring::<ClickHouseDialect>("Array(Nullable(Tuple(a Int32)))");
    let element = column_type(&database).array_element(&database).expect("an array");
    assert_eq!(element.structure_kind(&database), Some(StructureKind::Tuple));
}

#[test]
fn fields_answer_names_types_nullability_options_and_collations() {
    let database = declaring::<BigQueryDialect>(
        "STRUCT<`Code` INT64, amount NUMERIC(10, 2) OPTIONS(rounding_mode = 'ROUND_HALF_EVEN')>",
    );
    let data_type = column_type(&database);
    assert_eq!(data_type.structure_kind(&database), Some(StructureKind::Struct));
    let fields: Vec<_> = data_type.structured_fields(&database).expect("a struct").collect();
    assert_eq!(fields.len(), 2);
    assert_eq!((fields[0].name(), fields[0].name_is_quoted()), (Some("Code"), true));
    assert_eq!(fields[0].options(), []);
    assert_eq!(fields[1].data_type().numeric_precision(&database), Some(10));
    assert_eq!(fields[1].options(), [("rounding_mode".into(), "ROUND_HALF_EVEN".into())]);
    assert!(fields.iter().all(StructuredField::is_nullable));

    let database =
        declaring::<GenericDialect>("STRUCT<flagged INT OPTIONS(heap, policy.mode = 2)>");
    let field = column_type(&database)
        .structured_fields(&database)
        .expect("a struct")
        .next()
        .expect("one field");
    assert_eq!(field.options(), [("heap".into(), "".into()), ("policy.mode".into(), "2".into())]);

    let database = declaring::<DuckDbDialect>("UNION(code INTEGER, label VARCHAR)");
    let names: Vec<_> = column_type(&database)
        .structured_fields(&database)
        .expect("a union")
        .map(|field| field.name())
        .collect();
    assert_eq!(names, [Some("code"), Some("label")]);

    let database = declaring::<SnowflakeDialect>("OBJECT(city VARCHAR NOT NULL, zip VARCHAR)");
    let nullable: Vec<_> = column_type(&database)
        .structured_fields(&database)
        .expect("an object")
        .map(|field| (field.name(), field.is_nullable()))
        .collect();
    assert_eq!(nullable, [(Some("city"), false), (Some("zip"), true)]);

    let database = declaring::<ClickHouseDialect>("Tuple(Int32, Nullable(String))");
    let fields: Vec<_> = column_type(&database)
        .structured_fields(&database)
        .expect("a tuple")
        .map(|field| (field.name(), field.is_nullable()))
        .collect();
    assert_eq!(fields, [(None, false), (None, true)]);

    let database = declaring::<DatabricksDialect>("STRUCT<label VARCHAR(8) COLLATE UTF8_LCASE>");
    let field = column_type(&database)
        .structured_fields(&database)
        .expect("a struct")
        .next()
        .expect("one field");
    assert_eq!(field.data_type().max_character_length(&database), Some(8));
    let ColumnCollation::Named(collation) = field.collation() else {
        panic!("a COLLATE after the field type names its collation");
    };
    assert_eq!(collation.name().name(), "UTF8_LCASE");
}

#[test]
fn composite_attributes_answer_as_fields() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE pair AS (\"Left\" INT, right_side TEXT COLLATE \"C\", note TEXT COLLATE missing);
         CREATE TABLE t (c pair);",
    )
    .expect("schema builds");
    let data_type = column_type(&database);
    assert_eq!(data_type.structure_kind(&database), Some(StructureKind::Struct));
    assert!(data_type.map_entry(&database).is_none());
    let fields: Vec<_> = data_type.structured_fields(&database).expect("a composite").collect();
    assert_eq!((fields[0].name(), fields[0].name_is_quoted()), (Some("Left"), true));
    assert_eq!(fields[0].collation(), &ColumnCollation::DatabaseDefault);
    let ColumnCollation::Named(collation) = fields[1].collation() else {
        panic!("a declared COLLATE names the collation");
    };
    assert_eq!(collation.name().name(), "C");
    assert_eq!(collation.postgres_deterministic(), Some(true));
    let ColumnCollation::Named(unresolved) = fields[2].collation() else {
        panic!("a declared COLLATE names the collation");
    };
    assert_eq!(unresolved.name().name(), "missing");
    assert_eq!(unresolved.postgres_deterministic(), None);
}

#[test]
fn composite_collation_facts_follow_the_search_path_at_creation() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA a;
         CREATE SCHEMA b;
         CREATE COLLATION a.folded (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE COLLATION b.folded (provider = icu, locale = 'und-u-ks-level2', deterministic = true);
         SET search_path TO a;
         CREATE TYPE pair AS (label TEXT COLLATE folded);
         SET search_path TO b;
         CREATE TABLE public.t (c a.pair);",
    )
    .expect("schema builds");
    let field = column_type(&database)
        .structured_fields(&database)
        .expect("a composite")
        .next()
        .expect("one attribute");
    let ColumnCollation::Named(collation) = field.collation() else {
        panic!("a declared COLLATE names the collation");
    };
    assert_eq!(collation.name().name(), "folded");
    assert_eq!(collation.postgres_deterministic(), Some(false));
}
