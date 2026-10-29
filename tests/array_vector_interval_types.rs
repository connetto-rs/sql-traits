//! Tests that an array, a pgvector and an interval column answer their element
//! type, dimensions and fields through the declared type rather than only as a
//! type name.
#![allow(clippy::expect_used)]

use core::num::NonZeroU32;

use sql_traits::{prelude::*, utils::scalar_family::ScalarFamily};
use sqlparser::dialect::{PostgreSqlDialect, SnowflakeDialect};

/// The declared type of column `name` of table `t`.
fn data_type<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::DataType {
    database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t is declared")
        .column(name, database, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("column is declared")
        .data_type(database)
}

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

/// The columns of the original report, each answering its family, its array
/// dimensions and element, its vector and its interval fields.
#[test]
fn every_reported_column_answers_its_structure() {
    let db = postgres(
        "CREATE EXTENSION vector;
         CREATE TABLE t (id INT PRIMARY KEY, a INT[], aa INT[][], b TEXT ARRAY, c TIMESTAMPTZ[], \
         v vector(3), vv vector, h halfvec(3), i INTERVAL, i2 INTERVAL DAY TO SECOND, \
         n NUMERIC(10,2)[]);",
    );
    let dimensions = |name| data_type(&db, name).array_dimensions(&db).map(NonZeroU32::get);
    let element_family =
        |name| data_type(&db, name).array_element(&db).map(|element| element.scalar_family(&db));

    for (name, family, dims, element) in [
        ("id", Some(ScalarFamily::Int), None, None),
        ("a", None, Some(1), Some(Some(ScalarFamily::Int))),
        ("aa", None, Some(2), Some(Some(ScalarFamily::Int))),
        ("b", None, Some(1), Some(Some(ScalarFamily::String))),
        ("c", None, Some(1), Some(Some(ScalarFamily::TimestampTz))),
        ("v", None, None, None),
        ("i", Some(ScalarFamily::Interval), None, None),
        ("i2", Some(ScalarFamily::Interval), None, None),
        ("n", None, Some(1), Some(Some(ScalarFamily::Decimal))),
    ] {
        assert_eq!(data_type(&db, name).scalar_family(&db), family, "{name}");
        assert_eq!(dimensions(name), dims, "{name}");
        assert_eq!(element_family(name), element, "{name}");
    }

    let numeric = data_type(&db, "n").array_element(&db).expect("n is an array");
    assert_eq!(numeric.numeric_scale(&db), Some(2));
    assert_eq!(data_type(&db, "n").numeric_scale(&db), None, "an array has no scale of its own");

    let vector = |name| data_type(&db, name).vector(&db).expect("column is a vector");
    assert_eq!(vector("v").kind(), VectorKind::Vector);
    assert_eq!(vector("v").dimensions(), Some(3));
    assert_eq!(vector("vv").dimensions(), None, "a bare vector fixes no dimension count");
    assert_eq!(vector("h").kind(), VectorKind::HalfVec, "a consumer can tell a halfvec apart");
    assert_eq!(vector("h").dimensions(), Some(3));
    assert_eq!(data_type(&db, "a").vector(&db), None);

    assert_eq!(data_type(&db, "i").interval_fields(&db), None);
    assert_eq!(data_type(&db, "i2").interval_fields(&db), Some(IntervalFields::DayToSecond));
    assert_eq!(data_type(&db, "i").fractional_seconds_precision(&db), Some(6));
    assert_eq!(data_type(&db, "c").interval_fields(&db), None);
}

/// Every field restriction PostgreSQL accepts answers as written, and keeps
/// fractional seconds exactly when it reaches the seconds field.
#[test]
fn every_interval_field_restriction_answers_its_fields_and_seconds() {
    let cases = [
        ("YEAR", IntervalFields::Year, None),
        ("MONTH", IntervalFields::Month, None),
        ("DAY", IntervalFields::Day, None),
        ("HOUR", IntervalFields::Hour, None),
        ("MINUTE", IntervalFields::Minute, None),
        ("SECOND", IntervalFields::Second, Some(6)),
        ("YEAR TO MONTH", IntervalFields::YearToMonth, None),
        ("DAY TO HOUR", IntervalFields::DayToHour, None),
        ("DAY TO MINUTE", IntervalFields::DayToMinute, None),
        ("DAY TO SECOND", IntervalFields::DayToSecond, Some(6)),
        ("HOUR TO MINUTE", IntervalFields::HourToMinute, None),
        ("HOUR TO SECOND", IntervalFields::HourToSecond, Some(6)),
        ("MINUTE TO SECOND", IntervalFields::MinuteToSecond, Some(6)),
    ];
    let columns: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(index, (fields, _, _))| format!("c{index} INTERVAL {fields}"))
        .collect();
    let db = postgres(&format!("CREATE TABLE t ({});", columns.join(", ")));
    for (index, (written, fields, precision)) in cases.into_iter().enumerate() {
        let column = data_type(&db, &format!("c{index}"));
        assert_eq!(column.interval_fields(&db), Some(fields), "{written}");
        assert_eq!(fields.holds_seconds(), precision.is_some(), "{written}");
        assert_eq!(column.fractional_seconds_precision(&db), precision, "{written}");
    }
}

/// A type spelled like a pgvector type that PostgreSQL would not resolve to
/// it answers no vector.
#[test]
fn a_malformed_vector_spelling_answers_no_vector() {
    let db = postgres(
        "CREATE EXTENSION vector;
         CREATE TABLE t (pair vector(3, 4), far other_db.public.vector(3), \
         named vector(dims), huge vector(99999999999));",
    );
    for name in ["pair", "far", "named", "huge"] {
        assert_eq!(data_type(&db, name).vector(&db), None, "{name}");
    }

    let run_time = ParserDB::parse::<SnowflakeDialect>(
        "CREATE EXTENSION vector; CREATE TABLE t (x IDENTIFIER('public').vector(3));",
    )
    .expect("schema builds");
    assert_eq!(
        data_type(&run_time, "x").vector(&run_time),
        None,
        "a schema named only when the statement runs resolves to no known schema"
    );
}

/// An array element answers every fact a column of its type answers.
#[test]
fn an_array_element_answers_like_a_column_type() {
    let db = postgres(
        "CREATE EXTENSION vector;
         CREATE TABLE t (stamps TIMESTAMP(3)[], spans INTERVAL YEAR TO MONTH[], \
         codes VARCHAR(8)[], embeddings vector(4)[], grid NUMERIC(6,3)[][]);",
    );
    let element = |name| data_type(&db, name).array_element(&db).expect("column is an array");

    assert_eq!(element("stamps").fractional_seconds_precision(&db), Some(3));
    assert_eq!(element("spans").scalar_family(&db), Some(ScalarFamily::Interval));
    assert_eq!(element("spans").interval_fields(&db), Some(IntervalFields::YearToMonth));
    assert_eq!(element("codes").max_character_length(&db), Some(8));
    assert_eq!(
        element("embeddings").vector(&db),
        Some(VectorType::new(VectorKind::Vector, Some(4)))
    );
    assert_eq!(element("grid").numeric_scale(&db), Some(3));
    assert!(element("grid").array_element(&db).is_none(), "the element is the innermost type");
}

/// An array declared without an element type is still an array.
#[test]
fn an_array_without_an_element_type_answers_only_its_dimensions() {
    let db = ParserDB::parse::<SnowflakeDialect>("CREATE TABLE t (items ARRAY);")
        .expect("schema builds");
    let items = data_type(&db, "items");
    assert_eq!(items.array_dimensions(&db).map(NonZeroU32::get), Some(1));
    assert!(items.array_element(&db).is_none());
}

/// pgvector's types exist only where the extension was created, and only in
/// the schema it was created in.
#[test]
fn a_vector_type_answers_only_where_the_extension_puts_it() {
    let column_types = "plain vector(3), half halfvec(2), sparse sparsevec(5)";

    let without = postgres(&format!("CREATE TABLE t ({column_types});"));
    for name in ["plain", "half", "sparse"] {
        assert_eq!(data_type(&without, name).vector(&without), None, "{name} without extension");
    }

    let with = postgres(&format!("CREATE EXTENSION vector; CREATE TABLE t ({column_types});"));
    assert_eq!(
        data_type(&with, "sparse").vector(&with),
        Some(VectorType::new(VectorKind::SparseVec, Some(5)))
    );

    let dropped = postgres(&format!(
        "CREATE EXTENSION vector; CREATE TABLE t ({column_types}); DROP EXTENSION vector;"
    ));
    assert_eq!(data_type(&dropped, "plain").vector(&dropped), None);

    let elsewhere = postgres(
        "CREATE SCHEMA ext;
         CREATE EXTENSION vector WITH SCHEMA ext;
         CREATE TABLE t (bare vector(3), qualified ext.vector(3), wrong public.vector(3));",
    );
    assert_eq!(data_type(&elsewhere, "bare").vector(&elsewhere), None, "ext is off the path");
    assert_eq!(
        data_type(&elsewhere, "qualified").vector(&elsewhere),
        Some(VectorType::new(VectorKind::Vector, Some(3)))
    );
    assert_eq!(data_type(&elsewhere, "wrong").vector(&elsewhere), None);

    let on_path = postgres(
        "CREATE SCHEMA ext;
         CREATE EXTENSION vector WITH SCHEMA ext;
         SET search_path = public, ext;
         CREATE TABLE public.t (bare vector(3), qualified public.vector(3));",
    );
    assert_eq!(
        data_type(&on_path, "bare").vector(&on_path),
        Some(VectorType::new(VectorKind::Vector, Some(3)))
    );
    assert_eq!(data_type(&on_path, "qualified").vector(&on_path), None);

    let in_catalog = postgres(
        "SET search_path = pg_catalog;
         CREATE EXTENSION vector;
         SET search_path = public;
         CREATE TABLE t (bare vector(3));",
    );
    assert_eq!(
        data_type(&in_catalog, "bare").vector(&in_catalog),
        Some(VectorType::new(VectorKind::Vector, Some(3))),
        "pg_catalog is searched for a type whatever the path says"
    );

    let quoted = postgres(
        "CREATE EXTENSION vector; CREATE TABLE t (exact \"vector\"(3), other \"Vector\"(3));",
    );
    assert_eq!(
        data_type(&quoted, "exact").vector(&quoted),
        Some(VectorType::new(VectorKind::Vector, Some(3)))
    );
    assert_eq!(data_type(&quoted, "other").vector(&quoted), None, "a quoted name keeps its case");
}
