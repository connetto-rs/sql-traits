//! The conversion facts a database answers about its types.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared types.")]

use sql_traits::prelude::*;
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
}

fn facts<'db>(db: &'db ParserDB, name: TargetName<'_>) -> TypeFacts<'db> {
    db.type_facts(name).unwrap()
}

#[test]
fn built_in_types_carry_their_catalog_category_and_preference() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let int4 = facts(&db, pg("int4"));
    assert_eq!(int4.category(), TypeCategory::NUMERIC);
    assert!(!int4.is_preferred());
    assert!(int4.domain().is_none());
    assert!(int4.array_element().is_none());
    assert_eq!(int4.array_type(), Some(&pg("_int4")));
    assert!(facts(&db, pg("float8")).is_preferred());
    assert!(facts(&db, pg("text")).is_preferred());
    assert_eq!(facts(&db, pg("unknown")).category(), TypeCategory::UNKNOWN);

    let array = facts(&db, pg("_int4"));
    assert_eq!(array.category(), TypeCategory::ARRAY);
    assert_eq!(array.array_element(), Some(&pg("int4")));
    // An array type has no array of its own.
    assert!(array.array_type().is_none());
    assert!(facts(&db, pg("unknown")).array_type().is_none());

    // An unqualified built-in name reads as its `pg_catalog` type.
    assert_eq!(facts(&db, TargetName::new("int4", false)).category(), TypeCategory::NUMERIC);
    assert!(db.type_facts(pg("no_such_type")).is_none());
}

#[test]
fn databases_without_postgresql_types_hold_no_facts() {
    let sqlite = ParserDB::parse::<SQLiteDialect>("").unwrap();
    assert!(sqlite.type_facts(pg("int4")).is_none());
}

#[test]
fn declared_types_carry_the_category_of_their_kind() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.mood AS ENUM ('happy');
         CREATE TYPE app.pair AS (a INT, b INT);
         CREATE TYPE app.span AS RANGE (SUBTYPE = INT4);
         CREATE TABLE app.readings (a INT);
         CREATE TYPE app.pending;",
    )
    .unwrap();
    assert_eq!(facts(&db, app("mood")).category(), TypeCategory::ENUM);
    assert_eq!(facts(&db, app("pair")).category(), TypeCategory::COMPOSITE);
    assert_eq!(facts(&db, app("span")).category(), TypeCategory::RANGE);
    assert_eq!(facts(&db, app("span_multirange")).category(), TypeCategory::RANGE);
    assert_eq!(facts(&db, app("readings")).category(), TypeCategory::COMPOSITE);
    assert_eq!(facts(&db, app("pending")).category(), TypeCategory::PSEUDO);

    let array = facts(&db, app("_mood"));
    assert_eq!(array.category(), TypeCategory::ARRAY);
    assert!(!array.is_preferred());
    assert_eq!(array.array_element(), Some(&app("mood")));
    assert_eq!(facts(&db, app("mood")).array_type(), Some(&app("_mood")));
    assert_eq!(facts(&db, app("_readings")).array_element(), Some(&app("readings")));
    assert_eq!(facts(&db, app("readings")).array_type(), Some(&app("_readings")));
    assert!(db.type_facts(app("_pending")).is_none());
}

#[test]
fn a_base_type_keeps_its_declared_category_and_preference() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.money2;
         CREATE FUNCTION app.money2_in(cstring) RETURNS app.money2 LANGUAGE C AS 'lib', 'money2_in';
         CREATE FUNCTION app.money2_out(app.money2) RETURNS cstring LANGUAGE C AS 'lib', 'money2_out';
         CREATE TYPE app.money2 (INPUT = app.money2_in, OUTPUT = app.money2_out, CATEGORY = 'N', PREFERRED = true);
         CREATE TYPE app.plain (INPUT = app.money2_in, OUTPUT = app.money2_out);",
    )
    .unwrap();
    let declared = facts(&db, app("money2"));
    assert_eq!(declared.category(), TypeCategory::NUMERIC);
    assert_eq!(declared.category().code(), b'N');
    assert!(declared.is_preferred());
    let plain = facts(&db, app("plain"));
    assert_eq!(plain.category(), TypeCategory::USER_DEFINED);
    assert!(!plain.is_preferred());
}

/// Each type answers its `pg_type.typtype` as PostgreSQL 18 stores it, a
/// base type of the enum category staying a base type.
#[test]
fn types_answer_the_kind_of_their_definition() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.mood AS ENUM ('happy');
         CREATE TYPE app.pair AS (a INT, b INT);
         CREATE TYPE app.span AS RANGE (SUBTYPE = INT4);
         CREATE TABLE app.readings (a INT);
         CREATE TYPE app.pending;
         CREATE TYPE app.fake (INPUT = fake_in, OUTPUT = fake_out, CATEGORY = 'E');
         CREATE DOMAIN app.calm AS app.mood;",
    )
    .unwrap();
    let kinds = [
        (pg("int4"), TypeKind::Base),
        (pg("_int4"), TypeKind::Base),
        (pg("int4range"), TypeKind::Range),
        (pg("int4multirange"), TypeKind::Multirange),
        (pg("anyenum"), TypeKind::Pseudo),
        (pg("unknown"), TypeKind::Pseudo),
        (app("mood"), TypeKind::Enum),
        (app("_mood"), TypeKind::Base),
        (app("pair"), TypeKind::Composite),
        (app("span"), TypeKind::Range),
        (app("span_multirange"), TypeKind::Multirange),
        (app("readings"), TypeKind::Composite),
        (app("pending"), TypeKind::Pseudo),
        (app("fake"), TypeKind::Base),
        (app("calm"), TypeKind::Domain),
    ];
    for (name, kind) in kinds {
        let label = name.to_string();
        assert_eq!(facts(&db, name).kind(), Some(kind), "{label}");
    }
    assert_eq!(facts(&db, app("fake")).category(), TypeCategory::ENUM);
}

#[test]
fn a_domain_takes_its_base_category_and_reports_the_base_typmod() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE DOMAIN app.code AS VARCHAR(3);
         CREATE DOMAIN app.short_code AS app.code;
         CREATE DOMAIN app.amount AS NUMERIC(10, 2);
         CREATE DOMAIN app.note AS TEXT;
         CREATE DOMAIN app.stamp AS TIMESTAMP(3);
         CREATE DOMAIN app.flags AS BIT(8);
         CREATE DOMAIN app.padded AS CHAR(5);",
    )
    .unwrap();
    let code = facts(&db, app("code"));
    assert_eq!(code.category(), TypeCategory::STRING);
    assert!(!code.is_preferred());
    let domain = code.domain().unwrap();
    assert_eq!(domain.base(), &pg("varchar"));
    assert_eq!(domain.base_typmod(), Some(7));

    // A domain over a domain coerces through the innermost base type.
    let nested = facts(&db, app("short_code"));
    assert_eq!(nested.domain().unwrap().base(), &pg("varchar"));
    assert_eq!(nested.domain().unwrap().base_typmod(), Some(7));

    assert_eq!(facts(&db, app("amount")).domain().unwrap().base_typmod(), Some((10 << 16 | 2) + 4));
    assert_eq!(facts(&db, app("note")).domain().unwrap().base_typmod(), None);
    assert!(!facts(&db, app("note")).is_preferred());
    assert_eq!(facts(&db, app("stamp")).domain().unwrap().base_typmod(), Some(3));
    assert_eq!(facts(&db, app("flags")).domain().unwrap().base_typmod(), Some(8));
    assert_eq!(facts(&db, app("padded")).domain().unwrap().base_typmod(), Some(9));
}

#[test]
fn domains_report_the_typmod_of_every_modified_built_in_base() {
    let cases: &[(&str, Option<i32>)] = &[
        ("VARCHAR", None),
        ("CHAR", Some(5)),
        ("NUMERIC(6)", Some((6 << 16) + 4)),
        ("NUMERIC", None),
        ("BIT", Some(1)),
        ("BIT VARYING(4)", Some(4)),
        ("VARBIT", None),
        ("TIME(2) WITH TIME ZONE", Some(2)),
        ("TIMESTAMP", None),
        ("INTERVAL", None),
        ("INTERVAL(2)", Some((0x7fff << 16) | 2)),
        ("INTERVAL YEAR", Some((1 << 2 << 16) | 0xffff)),
        ("INTERVAL MONTH", Some((1 << 1 << 16) | 0xffff)),
        ("INTERVAL DAY", Some((1 << 3 << 16) | 0xffff)),
        ("INTERVAL HOUR", Some((1 << 10 << 16) | 0xffff)),
        ("INTERVAL MINUTE", Some((1 << 11 << 16) | 0xffff)),
        ("INTERVAL SECOND", Some((1 << 12 << 16) | 0xffff)),
        ("INTERVAL YEAR TO MONTH", Some((((1 << 2) | (1 << 1)) << 16) | 0xffff)),
        ("INTERVAL DAY TO HOUR", Some((((1 << 3) | (1 << 10)) << 16) | 0xffff)),
        ("INTERVAL DAY TO MINUTE", Some((((1 << 3) | (1 << 10) | (1 << 11)) << 16) | 0xffff)),
        (
            "INTERVAL DAY TO SECOND(3)",
            Some((((1 << 3) | (1 << 10) | (1 << 11) | (1 << 12)) << 16) | 3),
        ),
        ("INTERVAL HOUR TO MINUTE", Some((((1 << 10) | (1 << 11)) << 16) | 0xffff)),
        ("INTERVAL HOUR TO SECOND", Some((((1 << 10) | (1 << 11) | (1 << 12)) << 16) | 0xffff)),
        ("INTERVAL MINUTE TO SECOND", Some((((1 << 11) | (1 << 12)) << 16) | 0xffff)),
        ("VARCHAR(3)[]", Some(7)),
    ];
    for (base, typmod) in cases {
        let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA app; CREATE DOMAIN app.wrapped AS {base};"
        ))
        .unwrap();
        let wrapped = facts(&db, app("wrapped"));
        assert_eq!(wrapped.domain().unwrap().base_typmod(), *typmod, "{base}");
    }
}

#[test]
fn a_domain_over_an_array_coerces_through_the_array_type() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.mood AS ENUM ('happy');
         CREATE DOMAIN app.codes AS VARCHAR(3)[];
         CREATE DOMAIN app.moods AS app.mood[];
         CREATE SEQUENCE app.counter;",
    )
    .unwrap();
    let codes = facts(&db, app("codes"));
    assert_eq!(codes.category(), TypeCategory::ARRAY);
    assert_eq!(codes.domain().unwrap().base(), &pg("_varchar"));
    let moods = facts(&db, app("moods"));
    assert_eq!(moods.category(), TypeCategory::ARRAY);
    assert_eq!(moods.domain().unwrap().base(), &app("_mood"));
    // A sequence has no row type.
    assert!(db.type_facts(app("counter")).is_none());
}

fn quoted_pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, true).with_schema("pg_catalog", false)
}

/// Each type reports `pg_type.typcollation` as PostgreSQL 18 stores it.
#[test]
fn collatable_types_report_their_collation() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TYPE app.mood AS ENUM ('happy');
         CREATE TABLE app.readings (a INT);
         CREATE DOMAIN app.code AS TEXT COLLATE \"C\";
         CREATE DOMAIN app.short_code AS app.code;
         CREATE DOMAIN app.label AS VARCHAR(5);
         CREATE DOMAIN app.quantity AS INT;
         CREATE TYPE app.ci;
         CREATE FUNCTION app.ci_in(cstring) RETURNS app.ci LANGUAGE C AS 'lib', 'ci_in';
         CREATE FUNCTION app.ci_out(app.ci) RETURNS cstring LANGUAGE C AS 'lib', 'ci_out';
         CREATE TYPE app.ci (INPUT = app.ci_in, OUTPUT = app.ci_out, COLLATABLE = true);
         CREATE COLLATION german (provider = icu, locale = 'de');
         CREATE DOMAIN app.german_text AS TEXT COLLATE german;
         CREATE DOMAIN app.lost AS TEXT COLLATE missing;",
    )
    .unwrap();
    let collation = |name| db.type_facts(name).unwrap().collation().cloned();
    let default = Some(quoted_pg("default"));
    for name in ["text", "varchar", "bpchar", "_text", "_varchar"] {
        assert_eq!(collation(pg(name)), default, "{name}");
    }
    assert_eq!(collation(pg("name")), Some(quoted_pg("C")));
    assert_eq!(collation(pg("_name")), Some(quoted_pg("C")));
    for name in ["int4", "_int4", "bool", "jsonb"] {
        assert_eq!(collation(pg(name)), None, "{name}");
    }
    assert_eq!(collation(app("mood")), None);
    assert_eq!(collation(app("readings")), None);
    assert_eq!(collation(app("code")), Some(quoted_pg("C")));
    assert_eq!(collation(app("_code")), Some(quoted_pg("C")));
    assert_eq!(collation(app("short_code")), Some(quoted_pg("C")));
    assert_eq!(collation(app("label")), default);
    assert_eq!(collation(app("quantity")), None);
    assert_eq!(collation(app("ci")), default);
    assert_eq!(
        collation(app("german_text")),
        Some(TargetName::new("german", false).with_schema("public", false))
    );
    // A collation the database cannot name leaves the domain's facts open.
    assert!(db.type_facts(app("lost")).is_none());
}
