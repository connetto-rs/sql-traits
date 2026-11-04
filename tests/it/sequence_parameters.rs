//! Tests that a sequence answers the values `pg_sequence` records for it,
//! every option a declaration leaves out filled with PostgreSQL's default,
//! and that parameters PostgreSQL refuses are refused.
//!
//! Every expectation was measured against PostgreSQL 18.6.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, SequenceParameterError},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The sequence `name` as `pg_sequence` lists it: type, start, increment,
/// minimum, maximum, cache and cycle.
fn row(database: &ParserDB, name: &str) -> String {
    let parameters = database
        .sequence_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("the name is well formed")
        .expect("the sequence exists")
        .parameters();
    format!(
        "{} {} {} {} {} {} {}",
        parameters.data_type(),
        parameters.start(),
        parameters.increment(),
        parameters.min_value(),
        parameters.max_value(),
        parameters.cache(),
        if parameters.cycles() { "t" } else { "f" },
    )
}

/// Why the schema is refused, for a refusal of a sequence's parameters.
fn refusal(sql: &str) -> (String, SequenceParameterError) {
    match parse(sql) {
        Err(Error::InvalidSequenceParameters { sequence_name, reason }) => (sequence_name, reason),
        other => panic!("expected refused sequence parameters, got {other:?}"),
    }
}

#[test]
fn an_option_left_out_takes_the_default_for_the_type_and_direction() {
    let db = parse(
        "CREATE SEQUENCE d1;
         CREATE SEQUENCE d2 INCREMENT -1;
         CREATE SEQUENCE d3 AS smallint;
         CREATE SEQUENCE d4 AS integer INCREMENT -3;
         CREATE SEQUENCE d5 MINVALUE 10;
         CREATE SEQUENCE d6 INCREMENT -2 MAXVALUE -10;
         CREATE SEQUENCE d7 NO MINVALUE NO MAXVALUE CACHE 5 CYCLE;
         CREATE SEQUENCE d8 START 5 MINVALUE 1 MAXVALUE 9 INCREMENT 2;
         CREATE SEQUENCE d9 START WITH -5 INCREMENT BY 1 MINVALUE -10;",
    )
    .expect("PostgreSQL accepts every declaration");

    assert_eq!(row(&db, "d1"), "bigint 1 1 1 9223372036854775807 1 f");
    assert_eq!(row(&db, "d2"), "bigint -1 -1 -9223372036854775808 -1 1 f");
    assert_eq!(row(&db, "d3"), "smallint 1 1 1 32767 1 f");
    assert_eq!(row(&db, "d4"), "integer -1 -3 -2147483648 -1 1 f");
    assert_eq!(row(&db, "d5"), "bigint 10 1 10 9223372036854775807 1 f");
    assert_eq!(row(&db, "d6"), "bigint -10 -2 -9223372036854775808 -10 1 f");
    assert_eq!(row(&db, "d7"), "bigint 1 1 1 9223372036854775807 5 t");
    assert_eq!(row(&db, "d8"), "bigint 5 2 1 9 1 f");
    assert_eq!(row(&db, "d9"), "bigint -5 1 -10 9223372036854775807 1 f");
}

#[test]
fn spelling_a_default_out_changes_nothing() {
    let db = parse(
        "CREATE SEQUENCE implicit;
         CREATE SEQUENCE spelled AS bigint START WITH 1 INCREMENT BY 1 NO MINVALUE \
         MAXVALUE 9223372036854775807 CACHE 1 NO CYCLE;",
    )
    .expect("schema parses");
    assert_eq!(row(&db, "implicit"), row(&db, "spelled"));
}

#[test]
fn every_integer_spelling_postgresql_reads_is_accepted() {
    let db = parse(
        "CREATE SEQUENCE underscored START 1_000;
         CREATE SEQUENCE hexadecimal START 0x10;
         CREATE SEQUENCE signed START +7;
         CREATE SEQUENCE catalog_named AS pg_catalog.int8;
         CREATE SEQUENCE widest MAXVALUE 9223372036854775807 MINVALUE -9223372036854775808;",
    )
    .expect("PostgreSQL accepts every declaration");

    assert_eq!(row(&db, "underscored"), "bigint 1000 1 1 9223372036854775807 1 f");
    assert_eq!(row(&db, "hexadecimal"), "bigint 16 1 1 9223372036854775807 1 f");
    assert_eq!(row(&db, "signed"), "bigint 7 1 1 9223372036854775807 1 f");
    assert_eq!(row(&db, "catalog_named"), "bigint 1 1 1 9223372036854775807 1 f");
    assert_eq!(
        row(&db, "widest"),
        "bigint -9223372036854775808 1 -9223372036854775808 9223372036854775807 1 f"
    );
}

#[test]
fn parameters_postgresql_refuses_are_refused() {
    use SequenceParameterError::{
        CacheNotPositive, MaxValueOutOfRange, MinValueNotBelowMaxValue, MinValueOutOfRange,
        NotABigint, StartAboveMaxValue, StartBelowMinValue, ZeroIncrement,
    };

    let refused = |sql: &str| refusal(sql).1;
    assert_eq!(
        refused("CREATE SEQUENCE s AS text;"),
        SequenceParameterError::UnsupportedType { data_type: "TEXT".to_string() }
    );
    assert_eq!(refused("CREATE SEQUENCE s INCREMENT 0;"), ZeroIncrement);
    assert_eq!(
        refused("CREATE SEQUENCE s MINVALUE 5 MAXVALUE 5;"),
        MinValueNotBelowMaxValue { min_value: 5, max_value: 5 }
    );
    assert_eq!(
        refused("CREATE SEQUENCE s START 0;"),
        StartBelowMinValue { start: 0, min_value: 1 }
    );
    assert_eq!(
        refused("CREATE SEQUENCE s START 100 MAXVALUE 50;"),
        StartAboveMaxValue { start: 100, max_value: 50 }
    );
    assert_eq!(refused("CREATE SEQUENCE s CACHE 0;"), CacheNotPositive { cache: 0 });
    assert_eq!(
        refused("CREATE SEQUENCE s AS smallint MAXVALUE 40000;"),
        MaxValueOutOfRange { max_value: 40000, data_type: SequenceType::SmallInt }
    );
    assert_eq!(
        refused("CREATE SEQUENCE s AS smallint MINVALUE -40000;"),
        MinValueOutOfRange { min_value: -40000, data_type: SequenceType::SmallInt }
    );
    assert_eq!(
        refused("CREATE SEQUENCE s INCREMENT 1.5;"),
        NotABigint { option: "INCREMENT", value: "1.5".to_string() }
    );
    assert_eq!(
        refused("CREATE SEQUENCE s INCREMENT 9223372036854775808;"),
        NotABigint { option: "INCREMENT", value: "9223372036854775808".to_string() }
    );
    assert_eq!(
        refused("CREATE SEQUENCE s START 1e3;"),
        NotABigint { option: "START", value: "1e3".to_string() }
    );
}

/// PostgreSQL reads the options before it creates the relation, so a name
/// another relation holds is not what refuses the statement.
#[test]
fn the_parameters_are_refused_before_the_name() {
    assert_eq!(
        refusal("CREATE TABLE s (id INT); CREATE SEQUENCE s INCREMENT 0;"),
        ("s".to_string(), SequenceParameterError::ZeroIncrement)
    );
}

#[test]
fn an_identity_sequence_counts_in_its_column_type_with_its_options() {
    let db = parse(
        "CREATE TABLE t (
             a INT GENERATED ALWAYS AS IDENTITY,
             b SMALLINT GENERATED BY DEFAULT AS IDENTITY (INCREMENT BY -1),
             c BIGINT GENERATED ALWAYS AS IDENTITY (MAXVALUE 3000000000)
         );",
    )
    .expect("schema parses");
    assert_eq!(row(&db, "t_a_seq"), "integer 1 1 1 2147483647 1 f");
    assert_eq!(row(&db, "t_b_seq"), "smallint -1 -1 -32768 -1 1 f");
    assert_eq!(row(&db, "t_c_seq"), "bigint 1 1 1 3000000000 1 f");

    assert_eq!(
        refusal("CREATE TABLE t (a TEXT GENERATED ALWAYS AS IDENTITY);"),
        (
            "t_a_seq".to_string(),
            SequenceParameterError::UnsupportedType { data_type: "TEXT".to_string() }
        )
    );
    assert_eq!(
        refusal("CREATE TABLE t (a INT GENERATED ALWAYS AS IDENTITY (MAXVALUE 3000000000));").1,
        SequenceParameterError::MaxValueOutOfRange {
            max_value: 3_000_000_000,
            data_type: SequenceType::Integer
        }
    );
}

/// A type change moves a bound left at the old type's limit to the new
/// type's and keeps every other value, which is then checked against the new
/// type.
#[test]
fn an_identity_type_change_moves_only_the_bounds_at_the_old_limits() {
    let db = parse(
        "CREATE TABLE narrowed (a BIGINT GENERATED ALWAYS AS IDENTITY);
         ALTER TABLE narrowed ALTER COLUMN a TYPE INT;
         CREATE TABLE descending (a INT GENERATED ALWAYS AS IDENTITY (INCREMENT -1));
         ALTER TABLE descending ALTER COLUMN a TYPE SMALLINT;
         CREATE TABLE explicit (a INT GENERATED ALWAYS AS IDENTITY (MINVALUE -10 MAXVALUE 10 START 1));
         ALTER TABLE explicit ALTER COLUMN a TYPE BIGINT;
         CREATE TABLE widened (a SMALLINT GENERATED ALWAYS AS IDENTITY);
         ALTER TABLE widened ALTER COLUMN a TYPE BIGINT;
         CREATE TABLE at_the_limit (a BIGINT GENERATED ALWAYS AS IDENTITY (MAXVALUE 9223372036854775807));
         ALTER TABLE at_the_limit ALTER COLUMN a TYPE INT;
         CREATE TABLE lowest (a BIGINT GENERATED ALWAYS AS IDENTITY (MINVALUE -9223372036854775808 START 1));
         ALTER TABLE lowest ALTER COLUMN a TYPE INT;
         CREATE TABLE reversed (a INT GENERATED ALWAYS AS IDENTITY (INCREMENT -1 MAXVALUE 2147483647 START 5));
         ALTER TABLE reversed ALTER COLUMN a TYPE BIGINT;",
    )
    .expect("PostgreSQL accepts every change");

    assert_eq!(row(&db, "narrowed_a_seq"), "integer 1 1 1 2147483647 1 f");
    assert_eq!(row(&db, "descending_a_seq"), "smallint -1 -1 -32768 -1 1 f");
    assert_eq!(row(&db, "explicit_a_seq"), "bigint 1 1 -10 10 1 f");
    assert_eq!(row(&db, "widened_a_seq"), "bigint 1 1 1 9223372036854775807 1 f");
    assert_eq!(row(&db, "at_the_limit_a_seq"), "integer 1 1 1 2147483647 1 f");
    assert_eq!(row(&db, "lowest_a_seq"), "integer 1 1 -2147483648 2147483647 1 f");
    assert_eq!(
        row(&db, "reversed_a_seq"),
        "bigint 5 -1 -9223372036854775808 9223372036854775807 1 f"
    );
}

#[test]
fn an_identity_type_change_postgresql_refuses_is_refused() {
    assert_eq!(
        refusal(
            "CREATE TABLE t (a BIGINT GENERATED ALWAYS AS IDENTITY (MAXVALUE 3000000000));
             ALTER TABLE t ALTER COLUMN a TYPE INT;"
        )
        .1,
        SequenceParameterError::MaxValueOutOfRange {
            max_value: 3_000_000_000,
            data_type: SequenceType::Integer
        }
    );
    assert_eq!(
        refusal(
            "CREATE TABLE t (a BIGINT GENERATED ALWAYS AS IDENTITY (START 5000000000 MINVALUE 1));
             ALTER TABLE t ALTER COLUMN a TYPE INT;"
        )
        .1,
        SequenceParameterError::StartAboveMaxValue {
            start: 5_000_000_000,
            max_value: 2_147_483_647
        }
    );
    assert_eq!(
        refusal(
            "CREATE TABLE t (a BIGINT GENERATED ALWAYS AS IDENTITY (MINVALUE -9223372036854775808));
             ALTER TABLE t ALTER COLUMN a TYPE INT;"
        )
        .1,
        SequenceParameterError::StartBelowMinValue {
            start: i64::MIN,
            min_value: -2_147_483_648
        }
    );
    assert_eq!(
        refusal(
            "CREATE TABLE t (a BIGINT GENERATED ALWAYS AS IDENTITY);
             ALTER TABLE t ALTER COLUMN a TYPE TEXT;"
        ),
        (
            "t_a_seq".to_string(),
            SequenceParameterError::UnsupportedType { data_type: "TEXT".to_string() }
        )
    );
}
