//! Tests that an enumerated column answers its members, the kind of its values
//! and its storage width, so a changed, added or reordered member shows in the
//! answer of every engine that stores one.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::{ClickHouseDialect, DuckDbDialect, MySqlDialect, PostgreSqlDialect};

/// An enumeration answer with its members collected.
type Answer = (EnumerationKind, Option<EnumWidth>, Vec<(String, Option<i16>)>);

/// The enumeration column `name` of table `t` answers, its members collected.
fn enumeration(database: &ParserDB, name: &str) -> Option<Answer> {
    let data_type = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t is declared")
        .column(name, database, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("column is declared")
        .data_type(database);
    collect(data_type.enumeration(database))
}

fn collect<'db>(
    answer: Option<Enumeration<impl Iterator<Item = EnumerationMember<'db>>>>,
) -> Option<Answer> {
    let answer = answer?;
    let (kind, width) = (answer.kind(), answer.width());
    let members =
        answer.into_members().map(|member| (member.label().to_owned(), member.value())).collect();
    Some((kind, width, members))
}

fn members(labels: &[&str]) -> Vec<(String, Option<i16>)> {
    labels.iter().map(|label| ((*label).to_owned(), None)).collect()
}

fn valued(members: &[(&str, i16)]) -> Vec<(String, Option<i16>)> {
    members.iter().map(|(label, value)| ((*label).to_owned(), Some(*value))).collect()
}

/// MySQL identifies an `ENUM` member and a `SET` member by its position, so
/// the members answer in declaration order without values, and a reordered,
/// renamed or added member changes the answer.
#[test]
fn a_mysql_enum_and_set_answer_their_members_in_declaration_order() {
    let db = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (
            status ENUM('pending', 'ready'),
            renamed ENUM('pending', 'complete'),
            added ENUM('pending', 'ready', 'complete'),
            reordered ENUM('ready', 'pending'),
            flags SET('ready', 'busy'),
            reflags SET('busy', 'ready'),
            n INT
        );",
    )
    .expect("schema builds");

    let status = enumeration(&db, "status");
    assert_eq!(status, Some((EnumerationKind::Enum, None, members(&["pending", "ready"]))));
    for other in ["renamed", "added", "reordered"] {
        assert_ne!(enumeration(&db, other), status, "{other}");
    }
    assert_eq!(
        enumeration(&db, "flags"),
        Some((EnumerationKind::Set, None, members(&["ready", "busy"])))
    );
    assert_ne!(enumeration(&db, "reflags"), enumeration(&db, "flags"));
    assert_eq!(enumeration(&db, "n"), None);
}

/// DuckDB's inline `ENUM` identifies its members by position as MySQL's does.
#[test]
fn a_duckdb_inline_enum_answers_its_members_without_values() {
    let db = ParserDB::parse::<DuckDbDialect>("CREATE TABLE t (mood ENUM('sad', 'ok'));")
        .expect("schema builds");
    assert_eq!(
        enumeration(&db, "mood"),
        Some((EnumerationKind::Enum, None, members(&["sad", "ok"])))
    );
}

/// ClickHouse stores a member as its value, written or assigned, so every
/// spelling of one stored type answers alike, in the order of the values.
#[test]
fn a_clickhouse_enum_answers_its_stored_values_in_value_order() {
    let db = ParserDB::parse::<ClickHouseDialect>(
        "CREATE TABLE t (
            explicit Enum8('ready' = 1, 'busy' = 2),
            implicit Enum8('ready', 'busy'),
            first_only Enum8('ready' = 1, 'busy'),
            reordered Enum8('busy' = 2, 'ready' = 1),
            swapped Enum8('ready' = 2, 'busy' = 1),
            wide Enum16('ready' = 1, 'busy' = 2),
            shifted Enum8('ready' = -3, 'busy', 'idle'),
            first_implicit Enum8('ready', 'busy' = 5, 'idle' = -1),
            plus Enum8('ready' = +1, 'busy' = +2),
            nullable Nullable(Enum8('ready' = 1, 'busy' = 2)),
            low LowCardinality(Enum8('ready' = 1, 'busy' = 2))
        ) ENGINE = Memory;",
    )
    .expect("schema builds");

    let explicit =
        Some((EnumerationKind::Enum, Some(EnumWidth::Enum8), valued(&[("ready", 1), ("busy", 2)])));
    for same in ["explicit", "implicit", "first_only", "reordered", "plus", "nullable", "low"] {
        assert_eq!(enumeration(&db, same), explicit, "{same}");
    }
    assert_eq!(
        enumeration(&db, "swapped"),
        Some((EnumerationKind::Enum, Some(EnumWidth::Enum8), valued(&[("busy", 1), ("ready", 2)])))
    );
    assert_eq!(
        enumeration(&db, "wide"),
        Some((
            EnumerationKind::Enum,
            Some(EnumWidth::Enum16),
            valued(&[("ready", 1), ("busy", 2)])
        ))
    );
    assert_eq!(
        enumeration(&db, "shifted"),
        Some((
            EnumerationKind::Enum,
            Some(EnumWidth::Enum8),
            valued(&[("ready", -3), ("busy", -2), ("idle", -1)])
        ))
    );
    assert_eq!(
        enumeration(&db, "first_implicit"),
        Some((
            EnumerationKind::Enum,
            Some(EnumWidth::Enum8),
            valued(&[("idle", -1), ("ready", 1), ("busy", 5)])
        ))
    );
}

/// A bare ClickHouse `Enum` takes the narrowest width that holds its values.
#[test]
fn a_bare_clickhouse_enum_answers_the_width_its_values_need() {
    let db = ParserDB::parse::<ClickHouseDialect>(
        "CREATE TABLE t (
            narrow Enum('low' = -128, 'high' = 127),
            wide Enum('low' = -129, 'high' = 127),
            widest Enum('low' = -32768, 'high' = 32767)
        ) ENGINE = Memory;",
    )
    .expect("schema builds");
    let width = |name| enumeration(&db, name).and_then(|(_, width, _)| width);
    assert_eq!(width("narrow"), Some(EnumWidth::Enum8));
    assert_eq!(width("wide"), Some(EnumWidth::Enum16));
    assert_eq!(width("widest"), Some(EnumWidth::Enum16));
}

/// A declaration ClickHouse refuses for its values names no enumerated type.
#[test]
fn a_clickhouse_enum_with_values_clickhouse_refuses_answers_none() {
    let db = ParserDB::parse::<ClickHouseDialect>(
        "CREATE TABLE t (
            mixed Enum8('a' = 1, 'b', 'c' = 3),
            too_wide Enum8('a' = 128),
            beyond Enum('a' = 32768),
            implicit_overflow Enum8('a' = 127, 'b'),
            fractional Enum8('a' = 1.5),
            placeholder Enum8('a' = ?)
        ) ENGINE = Memory;",
    )
    .expect("schema builds");
    for refused in ["mixed", "too_wide", "beyond", "implicit_overflow", "fractional", "placeholder"]
    {
        assert_eq!(enumeration(&db, refused), None, "{refused}");
    }
}

/// A column of a PostgreSQL enum type answers the type's labels, as `ALTER
/// TYPE ... ADD VALUE` leaves them, while a domain over it and an array of it
/// answer none of their own.
#[test]
fn a_postgres_enum_type_answers_its_labels_through_the_catalog() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE status AS ENUM ('pending', 'ready');
         ALTER TYPE status ADD VALUE 'queued' BEFORE 'ready';
         CREATE DOMAIN checked_status AS status;
         CREATE TYPE pair AS (left_side INT, right_side TEXT);
         CREATE TABLE t (s status, d checked_status, a status[], p pair, n INT);",
    )
    .expect("schema builds");

    let labels = Some((EnumerationKind::Enum, None, members(&["pending", "queued", "ready"])));
    assert_eq!(enumeration(&db, "s"), labels);
    assert_eq!(enumeration(&db, "d"), None);
    assert_eq!(enumeration(&db, "a"), None);
    assert_eq!(enumeration(&db, "p"), None);
    assert_eq!(enumeration(&db, "n"), None);

    let column = |name| {
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("t is declared")
            .column(name, &db, IdentifierCase::AsWritten)
            .expect("t is in this database")
            .expect("column is declared")
            .data_type(&db)
    };
    let base = column("d").base_type(&db).expect("d is a domain");
    assert_eq!(collect(base.enumeration(&db)), labels);
    let element = column("a").array_element(&db).expect("a is an array");
    assert_eq!(collect(element.enumeration(&db)), labels);
}
