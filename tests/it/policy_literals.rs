//! A quoted literal in a policy expression holds the value PostgreSQL stored
//! when `CREATE POLICY` or `ALTER POLICY` read it, so a `timestamptz` or
//! `timetz` is the instant of the creating session's `TimeZone`.
//!
//! Every expected value is what PostgreSQL 18.6 stored for the same script,
//! read back from `pg_policies` under `TimeZone` `UTC`.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sql_values::{DateTimeError, PgType};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

const TABLES: &str =
    "CREATE TABLE t (id INT, tz TIMESTAMPTZ, d DATE, ts TIMESTAMP, tt TIMETZ, tm TIME, s TEXT);
     CREATE TABLE m (doc INT, granted TIMESTAMPTZ, expires TIMESTAMPTZ);";

fn parse(script: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{TABLES}{script}")).expect("schema builds")
}

fn policy<'db>(db: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreatePolicy {
    db.policies().find(|policy| policy.name() == name).expect("policy exists")
}

fn using(db: &ParserDB, name: &str) -> Result<String, LookupError> {
    let using = policy(db, name).using_expression(db)?;
    Ok(using.expect("a USING clause").to_string())
}

fn check(db: &ParserDB, name: &str) -> Result<String, LookupError> {
    let check = policy(db, name).check_expression(db)?;
    Ok(check.expect("a WITH CHECK clause").to_string())
}

fn unreadable(
    policy: &str,
    literal: &str,
    data_type: PgType,
    reason: DateTimeError,
) -> LookupError {
    LookupError::PolicyLiteralUnreadable {
        policy: policy.to_owned(),
        literal: literal.to_owned(),
        data_type,
        reason,
    }
}

#[test]
fn a_literal_read_as_timestamptz_is_the_instant_of_the_creating_zone() -> Result<(), LookupError> {
    let db = parse(
        "SET TIME ZONE 'Europe/Rome';
         CREATE POLICY p_bare ON t USING (tz < '2026-01-01 12:00');
         CREATE POLICY p_cast ON t USING (tz < '2026-07-01 12:00'::timestamptz);
         CREATE POLICY p_typed ON t USING (tz < timestamptz '2026-07-01 12:00');
         CREATE POLICY p_offset ON t USING (tz < '2026-07-01 12:00+05');
         CREATE POLICY p_nested ON t USING (tz < ('2026-01-01'));
         CREATE POLICY p_rev ON t USING ('2026-01-01' <= tz);
         CREATE POLICY p_dist ON t USING (tz IS DISTINCT FROM '2026-01-01');
         CREATE POLICY p_btw ON t USING (tz BETWEEN '2026-01-01' AND '2026-07-01');
         CREATE POLICY p_in ON t USING (tz IN ('2026-01-01', '2026-07-01'));
         CREATE POLICY p_prec ON t USING (tz < '2026-01-01 12:00:00.7'::timestamptz(0));",
    );
    let instant = |text: &str| format!("'{text}'::TIMESTAMP WITH TIME ZONE");
    assert_eq!(using(&db, "p_bare")?, format!("tz < {}", instant("2026-01-01 11:00:00+00")));
    assert_eq!(using(&db, "p_cast")?, "tz < '2026-07-01 10:00:00+00'::TIMESTAMPTZ");
    assert_eq!(using(&db, "p_typed")?, "tz < TIMESTAMPTZ '2026-07-01 10:00:00+00'");
    assert_eq!(using(&db, "p_offset")?, format!("tz < {}", instant("2026-07-01 07:00:00+00")));
    assert_eq!(using(&db, "p_nested")?, format!("tz < ({})", instant("2025-12-31 23:00:00+00")));
    assert_eq!(using(&db, "p_rev")?, format!("{} <= tz", instant("2025-12-31 23:00:00+00")));
    assert_eq!(
        using(&db, "p_dist")?,
        format!("tz IS DISTINCT FROM {}", instant("2025-12-31 23:00:00+00"))
    );
    assert_eq!(
        using(&db, "p_btw")?,
        format!(
            "tz BETWEEN {} AND {}",
            instant("2025-12-31 23:00:00+00"),
            instant("2026-06-30 22:00:00+00")
        )
    );
    assert_eq!(
        using(&db, "p_in")?,
        format!(
            "tz IN ({}, {})",
            instant("2025-12-31 23:00:00+00"),
            instant("2026-06-30 22:00:00+00")
        )
    );
    // PostgreSQL keeps the value whole and rounds it to the precision each
    // time the cast runs.
    assert_eq!(using(&db, "p_prec")?, "tz < '2026-01-01 11:00:00.7+00'::TIMESTAMPTZ(0)");
    Ok(())
}

#[test]
fn a_literal_without_a_zone_keeps_the_value_it_was_written_with() -> Result<(), LookupError> {
    let db = parse(
        "SET TIME ZONE 'Europe/Rome';
         CREATE POLICY p_dates ON t USING (d < '2026-01-01' AND ts < '2026-01-01 12:00');
         CREATE POLICY p_text ON t USING (s < '2026-01-01');
         CREATE POLICY p_inf ON t USING (tz < 'infinity' AND d > '-infinity'::date);",
    );
    assert_eq!(
        using(&db, "p_dates")?,
        "d < '2026-01-01'::DATE AND ts < '2026-01-01 12:00:00'::TIMESTAMP WITHOUT TIME ZONE"
    );
    assert_eq!(using(&db, "p_text")?, "s < '2026-01-01'");
    assert_eq!(
        using(&db, "p_inf")?,
        "tz < 'infinity'::TIMESTAMP WITH TIME ZONE AND d > '-infinity'::DATE"
    );
    Ok(())
}

#[test]
fn a_literal_takes_the_type_a_subquery_column_or_a_common_type_gives_it() -> Result<(), LookupError>
{
    let db = parse(
        "SET TIME ZONE 'Europe/Rome';
         CREATE POLICY p_sub ON t USING (EXISTS (SELECT 1 FROM m WHERE m.doc = t.id
             AND coalesce(m.granted, '2026-01-01') < m.expires));
         CREATE POLICY p_widen ON t USING (coalesce(d, tz, '2026-01-01') < now());
         CREATE POLICY p_nullif ON t USING (nullif(tz, '2026-01-01') IS NULL);
         CREATE POLICY p_greatest ON t USING (greatest(tz, '2026-01-01') < now());
         CREATE POLICY p_query_item ON t
             USING (d IN ((SELECT expires FROM m LIMIT 1), '2026-01-01 12:00'));",
    );
    let instant = "'2025-12-31 23:00:00+00'::TIMESTAMP WITH TIME ZONE";
    assert_eq!(
        using(&db, "p_sub")?,
        format!(
            "EXISTS (SELECT 1 FROM m WHERE m.doc = t.id AND coalesce(m.granted, {instant}) < m.expires)"
        )
    );
    assert_eq!(using(&db, "p_widen")?, format!("coalesce(d, tz, {instant}) < now()"));
    assert_eq!(using(&db, "p_nullif")?, format!("nullif(tz, {instant}) IS NULL"));
    assert_eq!(using(&db, "p_greatest")?, format!("greatest(tz, {instant}) < now()"));
    // The subquery's output joins the common type, which no type here reads.
    assert_eq!(
        using(&db, "p_query_item")?,
        "d IN ((SELECT expires FROM m LIMIT 1), '2026-01-01 12:00')"
    );
    Ok(())
}

#[test]
fn a_literal_needing_state_the_script_never_settles_answers_an_error_for_its_clause() {
    let db = parse(
        "CREATE POLICY p_zone ON t USING (tz < '2026-01-01 12:00') WITH CHECK (id > 0);
         CREATE POLICY p_today ON t USING (d < 'today'::date);
         CREATE POLICY p_order ON t USING (d < '01/02/2026');
         CREATE POLICY p_fine ON t USING (tz < '2026-01-01 12:00+00');
         SET TIME ZONE 'Europe/Rome';
         CREATE POLICY p_stamp ON t USING (ts < 'now');
         CREATE POLICY p_first ON t USING (d < 'today' AND tz < 'now');
         CREATE POLICY p_clock ON t USING (tt < '12:00');",
    );
    assert_eq!(
        using(&db, "p_zone"),
        Err(unreadable(
            "p_zone",
            "2026-01-01 12:00",
            PgType::TimestampTz,
            DateTimeError::NeedsTimeZone
        ))
    );
    assert_eq!(check(&db, "p_zone"), Ok("id > 0".to_owned()));
    assert_eq!(
        using(&db, "p_today"),
        Err(unreadable("p_today", "today", PgType::Date, DateTimeError::NeedsCurrentTime))
    );
    assert_eq!(
        using(&db, "p_order"),
        Err(unreadable("p_order", "01/02/2026", PgType::Date, DateTimeError::NeedsDateOrder))
    );
    assert_eq!(
        using(&db, "p_stamp"),
        Err(unreadable("p_stamp", "now", PgType::Timestamp, DateTimeError::NeedsCurrentTime))
    );
    let db_time = parse("CREATE POLICY p_time ON t USING (tm < 'now');");
    assert_eq!(
        using(&db_time, "p_time"),
        Err(unreadable("p_time", "now", PgType::Time, DateTimeError::NeedsCurrentTime))
    );
    // The first literal of a clause that cannot be read is the one answered.
    assert_eq!(
        using(&db, "p_first"),
        Err(unreadable("p_first", "today", PgType::Date, DateTimeError::NeedsCurrentTime))
    );
    assert_eq!(
        using(&db, "p_fine"),
        Ok("tz < '2026-01-01 12:00:00+00'::TIMESTAMP WITH TIME ZONE".to_owned())
    );
    // A zone whose offset changes needs the creation date for a time of day.
    assert_eq!(
        using(&db, "p_clock"),
        Err(unreadable("p_clock", "12:00", PgType::TimeTz, DateTimeError::NeedsCurrentTime))
    );
}

#[test]
fn date_style_orders_a_date_written_in_numbers_alone() -> Result<(), LookupError> {
    let db = parse(
        "SET TIME ZONE 'UTC';
         SET DateStyle = 'ISO, DMY';
         CREATE POLICY p_dmy ON t USING (d < '01/02/2026');
         SET datestyle TO german;
         CREATE POLICY p_german ON t USING (d < '01/02/2026');
         SET DateStyle = 'SQL, MDY';
         CREATE POLICY p_mdy ON t USING (d < '01/02/2026');
         SET datestyle = 'DMY, EURO';
         CREATE POLICY p_repeated ON t USING (d < '01/02/2026');
         SET DateStyle = 'ISO, YMD';
         CREATE POLICY p_ymd ON t USING (d < '01/02/03');
         SET DateStyle = 'DMY, MDY';
         CREATE POLICY p_conflicting_order ON t USING (d < '01/02/2026');
         SET DateStyle = 'ISO, SQL';
         CREATE POLICY p_conflicting_style ON t USING (d < '01/02/2026');",
    );
    assert_eq!(using(&db, "p_dmy")?, "d < '2026-02-01'::DATE");
    assert_eq!(using(&db, "p_repeated")?, "d < '2026-02-01'::DATE");
    assert_eq!(using(&db, "p_german")?, "d < '2026-02-01'::DATE");
    assert_eq!(using(&db, "p_mdy")?, "d < '2026-01-02'::DATE");
    assert_eq!(using(&db, "p_ymd")?, "d < '2001-02-03'::DATE");
    // PostgreSQL refuses both settings, so the order they leave is not guessed.
    for name in ["p_conflicting_order", "p_conflicting_style"] {
        assert_eq!(
            using(&db, name),
            Err(unreadable(name, "01/02/2026", PgType::Date, DateTimeError::NeedsDateOrder))
        );
    }
    Ok(())
}

#[test]
fn every_typed_position_reads_its_literal_as_postgresql_does() -> Result<(), LookupError> {
    let db = parse(
        "SET TIME ZONE 'Europe/Rome';
         CREATE FUNCTION \"coalesce\"(a TIMESTAMPTZ, b TEXT) RETURNS BOOLEAN LANGUAGE sql
             AS 'SELECT a IS NULL';
         CREATE POLICY p_time ON t USING (tm < '12:00');
         CREATE POLICY p_times ON t USING (coalesce(tm, tt, '12:00+02') IS NOT NULL);
         CREATE POLICY p_stamps ON t USING (coalesce(d, ts, '2026-01-01') IS NOT NULL);
         CREATE POLICY p_kept_date ON t USING (coalesce(tz, '2026-01-01'::date) < now());
         CREATE POLICY p_nested_col ON t USING ((tz) < '2026-01-01');
         CREATE POLICY p_coalesce_side ON t USING (coalesce(tz, ts) < '2026-01-01');
         CREATE POLICY p_nullif_side ON t USING (nullif(d, tz) < '2026-01-01');
         CREATE POLICY p_nullif_lit ON t USING (nullif('2026-01-01', tz) IS NULL);
         CREATE POLICY p_between_subject ON t
             USING ('2026-01-01' BETWEEN tz AND '2026-07-01'::timestamptz);
         CREATE POLICY p_in_column ON t USING (tz IN (ts, '2026-01-01'));
         CREATE POLICY p_no_column ON t USING ('2026-01-01 12:00'::timestamptz < now());
         CREATE POLICY p_opaque ON t USING (lower(s) = 'a');
         CREATE POLICY p_other_casts ON t USING (s = 'a'::text OR s = text 'b');
         CREATE POLICY p_quoted ON t USING (\"coalesce\"(tz, '2026-01-01'));
         CREATE POLICY p_derived ON t USING (EXISTS (SELECT 1
             FROM (SELECT expires AS x FROM m) q WHERE q.x < '2026-01-01'));
         CREATE POLICY p_computed ON t USING (EXISTS (SELECT 1
             FROM (SELECT now() AS x) q WHERE q.x < '2026-01-01'));",
    );
    let instant = |text: &str| format!("'{text}'::TIMESTAMP WITH TIME ZONE");
    let new_year = instant("2025-12-31 23:00:00+00");
    assert_eq!(using(&db, "p_time")?, "tm < '12:00:00'::TIME WITHOUT TIME ZONE");
    assert_eq!(
        using(&db, "p_times")?,
        "coalesce(tm, tt, '12:00:00+02'::TIME WITH TIME ZONE) IS NOT NULL"
    );
    assert_eq!(
        using(&db, "p_stamps")?,
        "coalesce(d, ts, '2026-01-01 00:00:00'::TIMESTAMP WITHOUT TIME ZONE) IS NOT NULL"
    );
    // A date converts to an instant in the zone of each statement.
    assert_eq!(using(&db, "p_kept_date")?, "coalesce(tz, '2026-01-01'::DATE) < now()");
    assert_eq!(using(&db, "p_nested_col")?, format!("(tz) < {new_year}"));
    assert_eq!(using(&db, "p_coalesce_side")?, format!("coalesce(tz, ts) < {new_year}"));
    // `nullif` answers its first argument's type.
    assert_eq!(using(&db, "p_nullif_side")?, "nullif(d, tz) < '2026-01-01'::DATE");
    assert_eq!(using(&db, "p_nullif_lit")?, format!("nullif({new_year}, tz) IS NULL"));
    assert_eq!(
        using(&db, "p_between_subject")?,
        format!("{new_year} BETWEEN tz AND '2026-06-30 22:00:00+00'::TIMESTAMPTZ")
    );
    assert_eq!(using(&db, "p_in_column")?, format!("tz IN (ts, {new_year})"));
    assert_eq!(using(&db, "p_no_column")?, "'2026-01-01 11:00:00+00'::TIMESTAMPTZ < now()");
    assert_eq!(using(&db, "p_opaque")?, "lower(s) = 'a'");
    assert_eq!(using(&db, "p_other_casts")?, "s = 'a'::TEXT OR s = TEXT 'b'");
    assert_eq!(using(&db, "p_quoted")?, "\"coalesce\"(tz, '2026-01-01')");
    assert_eq!(
        using(&db, "p_derived")?,
        format!("EXISTS (SELECT 1 FROM (SELECT expires AS x FROM m) q WHERE q.x < {new_year})")
    );
    // PostgreSQL types it by the type of `now()`, which is not inferred here,
    // so it is never guessed.
    assert_eq!(
        using(&db, "p_computed")?,
        "EXISTS (SELECT 1 FROM (SELECT now() AS x) q WHERE q.x < '2026-01-01')"
    );
    let row = parse(
        "SET TIME ZONE 'Europe/Rome';
         CREATE POLICY p_row ON t USING (coalesce(t.*) IS NOT NULL AND tz < '2026-01-01');",
    );
    assert_eq!(using(&row, "p_row")?, format!("coalesce(t.*) IS NOT NULL AND tz < {new_year}"));
    Ok(())
}

#[test]
fn the_session_time_zone_reads_a_literal_until_the_script_sets_its_own() -> Result<(), Error> {
    let db = ParseOptions::default()
        .with_session_time_zone("Asia/Tokyo")?
        .parse::<PostgreSqlDialect>(&format!(
            "{TABLES}
             CREATE POLICY p_session ON t USING (tz < '2026-01-01 12:00');
             SET TIME ZONE 'UTC';
             CREATE POLICY p_set ON t USING (tz < '2026-01-01 12:00');
             RESET timezone;
             CREATE POLICY p_reset ON t USING (tz < '2026-01-01 12:00');"
        ))?;
    let at = |hour: &str| format!("tz < '2026-01-01 {hour}:00:00+00'::TIMESTAMP WITH TIME ZONE");
    assert_eq!(using(&db, "p_session")?, at("03"));
    assert_eq!(using(&db, "p_set")?, at("12"));
    assert_eq!(using(&db, "p_reset")?, at("03"));

    assert!(matches!(
        ParseOptions::default().with_session_time_zone("Mars/Olympus"),
        Err(Error::InvalidTimeZone { .. })
    ));
    Ok(())
}

#[test]
fn alter_policy_reads_the_clauses_it_writes_in_the_zone_it_runs_in() -> Result<(), LookupError> {
    let db = parse(
        "CREATE POLICY p_alter ON t USING (tz < '2026-01-01 12:00')
             WITH CHECK (tz < '2026-01-01 12:00');
         SET TIME ZONE 'Asia/Tokyo';
         ALTER POLICY p_alter ON t USING (tz < '2026-01-01 12:00');",
    );
    assert_eq!(using(&db, "p_alter")?, "tz < '2026-01-01 03:00:00+00'::TIMESTAMP WITH TIME ZONE");
    assert_eq!(
        check(&db, "p_alter"),
        Err(unreadable(
            "p_alter",
            "2026-01-01 12:00",
            PgType::TimestampTz,
            DateTimeError::NeedsTimeZone
        ))
    );
    Ok(())
}

/// A clause's text, with the error it carries when it is written.
fn clause(expression: Option<PolicyExpression<'_>>) -> (String, Option<LookupError>) {
    match expression.expect("the clause exists") {
        PolicyExpression::Stored(expression) => (expression.to_string(), None),
        PolicyExpression::Written { expression, unreadable } => {
            (expression.to_string(), Some(unreadable))
        }
    }
}

#[test]
fn a_clause_with_an_unread_literal_answers_as_written_beside_its_error() -> Result<(), LookupError>
{
    let db = parse(
        "CREATE POLICY p_walk ON t USING (tz < '2026-01-01 12:00' AND id IN (SELECT doc FROM m))
             WITH CHECK (tz < '2026-01-01 12:00');
         SET TIME ZONE 'Asia/Tokyo';
         ALTER POLICY p_walk ON t WITH CHECK (tz < '2026-01-01 12:00' OR s = 'x');",
    );
    let walked = policy(&db, "p_walk");
    assert_eq!(
        clause(walked.using_clause(&db)?),
        (
            "tz < '2026-01-01 12:00' AND id IN (SELECT doc FROM m)".to_owned(),
            Some(unreadable(
                "p_walk",
                "2026-01-01 12:00",
                PgType::TimestampTz,
                DateTimeError::NeedsTimeZone
            ))
        )
    );
    assert_eq!(
        clause(walked.check_clause(&db)?),
        ("tz < '2026-01-01 03:00:00+00'::TIMESTAMP WITH TIME ZONE OR s = 'x'".to_owned(), None)
    );
    Ok(())
}

#[test]
fn a_dialect_without_policies_of_its_own_keeps_literals_as_written() -> Result<(), Error> {
    let db = ParserDB::parse::<GenericDialect>(&format!(
        "{TABLES} CREATE POLICY p ON t USING (tz < '2026-01-01 12:00');"
    ))?;
    assert_eq!(using(&db, "p")?, "tz < '2026-01-01 12:00'");
    Ok(())
}
