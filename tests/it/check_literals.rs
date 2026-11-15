//! A quoted literal in a check holds the value PostgreSQL stored when the
//! statement creating the check read it, so a `timestamptz` or `timetz` is
//! the instant of that session's `TimeZone`.
//!
//! Every expected value is what PostgreSQL 18 stored for the same script,
//! read back through `pg_get_constraintdef` under `TimeZone` `UTC`.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sql_values::{DateTimeError, PgType};
use sqlparser::{
    ast::Expr,
    dialect::{GenericDialect, PostgreSqlDialect},
};

fn parse(script: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(script).expect("schema builds")
}

fn named<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::CheckConstraint {
    db.tables()
        .flat_map(|table| table.check_constraints(db).expect("held"))
        .find(|check| check.constraint_name(db).expect("held").as_deref() == Some(name))
        .expect("check exists")
}

fn stored(db: &ParserDB, name: &str) -> Result<String, LookupError> {
    Ok(named(db, name).expression(db)?.to_string())
}

fn written(db: &ParserDB, name: &str) -> Result<String, LookupError> {
    Ok(named(db, name).written_expression(db)?.expect("a parsed check").to_string())
}

fn unreadable(
    table: &str,
    check: &str,
    literal: &str,
    data_type: PgType,
    reason: DateTimeError,
) -> LookupError {
    LookupError::CheckLiteralUnreadable {
        table: table.to_owned(),
        check: Some(check.to_owned()),
        literal: literal.to_owned(),
        data_type,
        reason,
    }
}

const ROME: &str = "SET TIME ZONE 'Europe/Rome';
CREATE TABLE c (
    tz TIMESTAMPTZ CHECK (tz > '2026-01-01 12:00'),
    d DATE, ts TIMESTAMP, s TEXT,
    CONSTRAINT c_mixed CHECK (ts < '2026-01-01 12:00' AND d > '2026-01-01' AND s <> '2026-01-01'),
    CONSTRAINT c_typed CHECK (tz < timestamptz '2026-07-01 12:00' AND tz > '2026-07-01 12:00'::timestamptz)
);";

#[test]
fn a_check_literal_holds_the_value_the_creating_session_stored() -> Result<(), LookupError> {
    let db = parse(ROME);
    assert_eq!(
        stored(&db, "c_tz_check")?,
        "tz > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
    );
    assert_eq!(
        stored(&db, "c_mixed")?,
        "ts < '2026-01-01 12:00:00'::TIMESTAMP WITHOUT TIME ZONE AND d > '2026-01-01'::DATE AND s \
         <> '2026-01-01'"
    );
    assert_eq!(
        stored(&db, "c_typed")?,
        "tz < TIMESTAMPTZ '2026-07-01 10:00:00+00' AND tz > '2026-07-01 10:00:00+00'::TIMESTAMPTZ"
    );
    assert_eq!(written(&db, "c_tz_check")?, "tz > '2026-01-01 12:00'");
    assert_eq!(
        written(&db, "c_typed")?,
        "tz < TIMESTAMPTZ '2026-07-01 12:00' AND tz > '2026-07-01 12:00'::TIMESTAMPTZ"
    );
    Ok(())
}

#[test]
fn a_check_added_later_reads_its_literals_in_the_session_adding_it() -> Result<(), LookupError> {
    let db = parse(&format!(
        "{ROME}
         SET TIME ZONE 'Asia/Tokyo';
         ALTER TABLE c ADD CONSTRAINT c_added CHECK (tz <> '2026-01-01 12:00');
         ALTER TABLE c ADD COLUMN extra INT;"
    ));
    assert_eq!(stored(&db, "c_added")?, "tz <> '2026-01-01 03:00:00+00'::TIMESTAMP WITH TIME ZONE");
    // The rebuild adding the column leaves the checks it carries as stored.
    assert_eq!(
        stored(&db, "c_tz_check")?,
        "tz > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
    );
    Ok(())
}

fn stored_on(db: &ParserDB, table: &str, name: &str) -> Result<String, LookupError> {
    let table = db.tables().find(|held| held.table_name() == table).expect("table exists");
    let check = table
        .check_constraints(db)?
        .find(|check| check.constraint_name(db).expect("held").as_deref() == Some(name))
        .expect("check exists");
    Ok(check.expression(db)?.to_string())
}

#[test]
fn a_check_passed_down_keeps_the_values_the_parent_stored() -> Result<(), LookupError> {
    let db = parse(
        "SET TIME ZONE 'Europe/Rome';
         CREATE TABLE parent (
             ts TIMESTAMPTZ CONSTRAINT p_col CHECK (ts < '2027-01-01 12:00'),
             CONSTRAINT p_ts CHECK (ts > '2026-01-01 12:00')
         );
         CREATE TABLE ranged (
             ts TIMESTAMPTZ CONSTRAINT r_col CHECK (ts < '2027-01-01 12:00'),
             CONSTRAINT r_ts CHECK (ts > '2026-01-01 12:00')
         ) PARTITION BY RANGE (ts);
         SET TIME ZONE 'Asia/Tokyo';
         CREATE TABLE child () INHERITS (parent);
         CREATE TABLE mid PARTITION OF ranged FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')
             PARTITION BY RANGE (ts);
         SET TIME ZONE 'America/New_York';
         CREATE TABLE grandchild () INHERITS (child);
         CREATE TABLE leaf PARTITION OF mid FOR VALUES FROM ('2026-01-01') TO ('2026-06-01');",
    );
    for table in ["parent", "child", "grandchild"] {
        assert_eq!(
            stored_on(&db, table, "p_ts")?,
            "ts > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
        );
        assert_eq!(
            stored_on(&db, table, "p_col")?,
            "ts < '2027-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
        );
    }
    for table in ["ranged", "mid", "leaf"] {
        assert_eq!(
            stored_on(&db, table, "r_ts")?,
            "ts > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
        );
        assert_eq!(
            stored_on(&db, table, "r_col")?,
            "ts < '2027-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
        );
    }
    Ok(())
}

#[test]
fn a_check_unreadable_in_the_parent_stays_unreadable_in_a_child() {
    let db = parse(
        "CREATE TABLE parent (ts TIMESTAMPTZ, CONSTRAINT p_ts CHECK (ts > '2026-01-01 12:00'));
         SET TIME ZONE 'Asia/Tokyo';
         CREATE TABLE child () INHERITS (parent);",
    );
    for table in ["parent", "child"] {
        assert_eq!(
            stored_on(&db, table, "p_ts"),
            Err(unreadable(
                table,
                "p_ts",
                "2026-01-01 12:00",
                PgType::TimestampTz,
                DateTimeError::NeedsTimeZone
            ))
        );
    }
}

#[test]
fn a_rebuilt_check_keeps_its_literals_through_renames_and_retyping() -> Result<(), LookupError> {
    let db = parse(&format!(
        "{ROME}
         SET TIME ZONE 'Asia/Tokyo';
         ALTER TABLE c RENAME COLUMN ts TO stamp;
         CREATE TABLE r (d DATE CONSTRAINT r_d CHECK (d > '2026-01-01'), n INT);
         ALTER TABLE r ALTER COLUMN d TYPE TIMESTAMPTZ;"
    ));
    assert_eq!(
        stored(&db, "c_mixed")?,
        "stamp < '2026-01-01 12:00:00'::TIMESTAMP WITHOUT TIME ZONE AND d > '2026-01-01'::DATE \
         AND s <> '2026-01-01'"
    );
    assert_eq!(
        written(&db, "c_mixed")?,
        "stamp < '2026-01-01 12:00' AND d > '2026-01-01' AND s <> '2026-01-01'"
    );
    // PostgreSQL parses the stored text again, whose literal is a date.
    assert_eq!(stored(&db, "r_d")?, "d > '2026-01-01'::DATE");
    Ok(())
}

#[test]
fn a_check_literal_needing_state_the_script_never_settles_answers_an_error() {
    let db = parse(
        "CREATE TABLE z (
             tz TIMESTAMPTZ CONSTRAINT z_zone CHECK (tz > '2026-01-01 12:00'),
             d DATE CONSTRAINT z_today CHECK (d < 'today'),
             n INT CONSTRAINT z_fine CHECK (n > 0)
         );
         SET TIME ZONE 'Europe/Rome';
         ALTER TABLE z ADD COLUMN extra INT;",
    );
    assert_eq!(
        stored(&db, "z_zone"),
        Err(unreadable(
            "z",
            "z_zone",
            "2026-01-01 12:00",
            PgType::TimestampTz,
            DateTimeError::NeedsTimeZone
        ))
    );
    assert_eq!(
        stored(&db, "z_today"),
        Err(unreadable("z", "z_today", "today", PgType::Date, DateTimeError::NeedsCurrentTime))
    );
    assert_eq!(stored(&db, "z_fine"), Ok("n > 0".to_owned()));
    let ClauseExpression::Written { expression, unreadable } =
        named(&db, "z_zone").check_clause(&db).expect("held")
    else {
        panic!("a check holding an unread literal answers as written")
    };
    assert_eq!(expression.to_string(), "tz > '2026-01-01 12:00'");
    assert!(matches!(unreadable, LookupError::CheckLiteralUnreadable { .. }));
}

#[test]
fn a_call_of_the_written_check_reaches_the_functions_of_the_stored_one() -> Result<(), LookupError>
{
    let db = parse(
        "SET TIME ZONE 'UTC';
         CREATE FUNCTION f(x TIMESTAMP) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';
         CREATE TABLE k (ts TIMESTAMP, CONSTRAINT k_f CHECK (ts < '2026-01-01' AND f(ts)));",
    );
    let check = named(&db, "k_f");
    for expression in [check.expression(&db)?, check.written_expression(&db)?.expect("parsed")] {
        let Expr::BinaryOp { right, .. } = expression else { panic!("a conjunction") };
        let Expr::Function(call) = right.as_ref() else { panic!("a call") };
        let reached: Vec<_> =
            check.functions_reached_by(call, &db)?.map(FunctionLike::name).collect();
        assert_eq!(reached, ["f"]);
    }
    Ok(())
}

#[test]
fn a_dialect_without_these_types_keeps_check_literals_as_written() -> Result<(), Error> {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE g (tz TIMESTAMPTZ, CHECK (tz > '2026-01-01 12:00'));",
    )?;
    let table = db.tables().next().expect("table exists");
    let check = table.check_constraints(&db)?.next().expect("check exists");
    assert_eq!(check.expression(&db)?.to_string(), "tz > '2026-01-01 12:00'");
    Ok(())
}

#[test]
fn temporal_literals_take_expression_result_types() -> Result<(), LookupError> {
    let database = parse(
        r#"SET TIME ZONE 'Europe/Rome';
        CREATE DOMAIN stamp AS TIMESTAMPTZ;
        CREATE TABLE e (ts TIMESTAMPTZ, d DATE, st stamp, tr TSTZRANGE,
        CONSTRAINT c_case CHECK (CASE WHEN d > '2026-01-01' THEN ts ELSE '2026-01-01 12:00' END IS NOT NULL),
        CONSTRAINT c_array CHECK (ts = ANY ('{"2026-01-01 12:00",NULL,"2026-06-01 12:00"}')),
        CONSTRAINT c_domain CHECK (st > '2026-01-01 12:00'),
        CONSTRAINT c_function CHECK (date_trunc('day', ts) > '2026-01-01 12:00'),
        CONSTRAINT c_range CHECK (tr <> '[2026-01-01 12:00, 2026-06-01 12:00)'),
        CONSTRAINT c_operator CHECK (d + 1 > '2026-01-01'),
        CONSTRAINT c_row CHECK ((ts, d) > ('2026-01-01 12:00', '2026-01-01')));"#,
    );
    for (name, expected) in [
        (
            "c_case",
            "CASE WHEN d > '2026-01-01'::DATE THEN ts ELSE '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE END IS NOT NULL",
        ),
        (
            "c_array",
            r#"ts = ANY('{"2026-01-01 11:00:00+00",NULL,"2026-06-01 10:00:00+00"}'::TIMESTAMP WITH TIME ZONE[])"#,
        ),
        ("c_domain", "st > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"),
        (
            "c_function",
            "date_trunc('day', ts) > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE",
        ),
        ("c_range", r#"tr <> '["2026-01-01 11:00:00+00","2026-06-01 10:00:00+00")'::tstzrange"#),
        ("c_operator", "d + 1 > '2026-01-01'::DATE"),
        (
            "c_row",
            "(ts, d) > ('2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE, '2026-01-01'::DATE)",
        ),
    ] {
        assert_eq!(stored(&database, name)?, expected, "{name}");
    }
    Ok(())
}

#[test]
fn a_case_result_requires_the_creating_time_zone() {
    let database = parse(
        "CREATE TABLE e (ts TIMESTAMPTZ, CONSTRAINT c_case CHECK (
         CASE WHEN true THEN ts ELSE '2026-01-01 12:00' END IS NOT NULL));",
    );
    assert!(matches!(stored(&database, "c_case"), Err(LookupError::CheckLiteralUnreadable { .. })));
}

#[test]
fn temporal_containers_preserve_bounds_dimensions_and_nulls() -> Result<(), LookupError> {
    let database = parse(
        r#"SET TIME ZONE 'Europe/Rome';
        CREATE DOMAIN stamp AS TIMESTAMPTZ;
        CREATE DOMAIN nested_stamp AS stamp;
        CREATE TABLE e (ts TIMESTAMPTZ, st nested_stamp, dr DATERANGE, nr TSRANGE,
        CONSTRAINT c_nested CHECK(st > '2026-01-01 12:00'),
        CONSTRAINT c_dates CHECK(dr <> '(2026-01-01,2026-01-03]'),
        CONSTRAINT c_empty CHECK(dr <> '(2026-01-01,2026-01-02)'),
        CONSTRAINT c_unbounded CHECK(nr <> '(,2026-01-01 12:00]'),
        CONSTRAINT c_infinite CHECK(dr <> '(infinity,infinity]'),
        CONSTRAINT c_matrix CHECK(ts = ANY ('[0:1][2:3]={{"2026-01-01 12:00",NULL},{infinity,-infinity}}')));"#,
    );
    for (name, expected) in [
        ("c_nested", "st > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"),
        ("c_dates", "dr <> '[2026-01-02,2026-01-04)'::daterange"),
        ("c_empty", "dr <> 'empty'::daterange"),
        ("c_infinite", "dr <> 'empty'::daterange"),
        ("c_unbounded", r#"nr <> '(,"2026-01-01 12:00:00"]'::tsrange"#),
        (
            "c_matrix",
            r#"ts = ANY('[0:1][2:3]={{"2026-01-01 11:00:00+00",NULL},{infinity,-infinity}}'::TIMESTAMP WITH TIME ZONE[])"#,
        ),
    ] {
        assert_eq!(stored(&database, name)?, expected, "{name}");
    }
    Ok(())
}

#[test]
fn temporal_container_output_quotes_only_required_members() -> Result<(), LookupError> {
    let database = parse(
        r#"SET TIME ZONE 'Europe/Rome';
        CREATE TABLE e (ts TIMESTAMPTZ, dr DATERANGE,
        CONSTRAINT c_infinite_point CHECK(dr <> '[infinity,infinity]'),
        CONSTRAINT c_negative_infinity CHECK(dr <> '[-infinity,-infinity]'),
        CONSTRAINT c_quote CHECK(ts = ANY ('{"2026-01-01 12:00",2026-06-01\ 12:00,nUlL}')),
        CONSTRAINT c_dimensions CHECK(ts = ANY ('[1:1]={"2026-01-01 12:00"}')),
        CONSTRAINT c_null CHECK(ts = ANY ('{NULL}')),
        CONSTRAINT c_zero CHECK(ts = ANY ('{}')));"#,
    );
    for (name, expected) in [
        ("c_infinite_point", "dr <> '[infinity,infinity]'::daterange"),
        ("c_negative_infinity", "dr <> '[-infinity,-infinity]'::daterange"),
        (
            "c_quote",
            r#"ts = ANY('{"2026-01-01 11:00:00+00","2026-06-01 10:00:00+00",NULL}'::TIMESTAMP WITH TIME ZONE[])"#,
        ),
        ("c_dimensions", r#"ts = ANY('{"2026-01-01 11:00:00+00"}'::TIMESTAMP WITH TIME ZONE[])"#),
        ("c_null", "ts = ANY('{NULL}'::TIMESTAMP WITH TIME ZONE[])"),
        ("c_zero", "ts = ANY('{}'::TIMESTAMP WITH TIME ZONE[])"),
    ] {
        assert_eq!(stored(&database, name)?, expected, "{name}");
    }
    Ok(())
}

#[test]
fn case_subjects_and_nullable_results_type_literals() -> Result<(), LookupError> {
    let database = parse(
        "SET TIME ZONE 'Europe/Rome';
        CREATE TABLE e (ts TIMESTAMPTZ, d DATE,
        CONSTRAINT c_simple CHECK(CASE d WHEN '2026-01-01' THEN ts ELSE '2026-06-01 12:00' END IS NOT NULL),
        CONSTRAINT c_result CHECK((CASE WHEN d IS NULL THEN NULL ELSE ts END) > '2026-01-01 12:00'));",
    );
    assert_eq!(
        stored(&database, "c_simple")?,
        "CASE d WHEN '2026-01-01'::DATE THEN ts ELSE '2026-06-01 10:00:00+00'::TIMESTAMP WITH TIME ZONE END IS NOT NULL"
    );
    assert_eq!(
        stored(&database, "c_result")?,
        "(CASE WHEN d IS NULL THEN NULL ELSE ts END) > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE"
    );
    Ok(())
}
