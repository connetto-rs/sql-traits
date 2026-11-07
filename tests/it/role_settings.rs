//! `ALTER ROLE ... SET` and `RESET` keep one entry per role, database and
//! setting, as `pg_db_role_setting` keys them, and `SET ... FROM CURRENT`
//! stores the value the script's own session holds.
//!
//! Every expected entry is what PostgreSQL 18.6 stored for the same script,
//! read back from `pg_db_role_setting`.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sql_values::TimeZoneError;
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn refusal(sql: &str) -> Error {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect_err("PostgreSQL refuses the script")
}

/// Every entry as (role, database, name, value), `None` for all roles or all
/// databases, in the order they were first set.
fn entries(db: &ParserDB) -> Vec<(Option<&str>, Option<&str>, &str, &str)> {
    db.role_settings()
        .map(|setting| (setting.role(), setting.database(), setting.name(), setting.value()))
        .collect()
}

#[test]
fn each_role_database_and_setting_keeps_one_entry() {
    let db = parse(
        "CREATE ROLE r;
         ALTER ROLE r SET timezone = 'Asia/Tokyo';
         ALTER ROLE r IN DATABASE app SET timezone TO 'America/Chicago';
         ALTER ROLE ALL SET timezone = 'Europe/Rome';
         ALTER ROLE ALL IN DATABASE app SET timezone = 'Europe/Paris';
         ALTER ROLE r SET TimeZone = 'Asia/Kolkata';",
    );
    assert_eq!(
        entries(&db),
        [
            (Some("r"), None, "timezone", "Asia/Kolkata"),
            (Some("r"), Some("app"), "timezone", "America/Chicago"),
            (None, None, "timezone", "Europe/Rome"),
            (None, Some("app"), "timezone", "Europe/Paris"),
        ]
    );
    let setting = db.role_settings().next().expect("r's own entry");
    assert!(setting.is_named("TIMEZONE"));
    assert!(!setting.is_named("time zone"));
}

#[test]
fn a_value_is_stored_as_postgresql_prints_it() {
    // PostgreSQL 18.6 stored app.a=foo, app.b=Foo, app.c=true, app.d=7,
    // app.e=-7, app.f=1.50, app.g=-1.50, app.h=a\tb, app.i=x, app.j= y  and
    // app.k=fÜr, downcasing only the ASCII letters of a bare name.
    let db = parse(
        "CREATE ROLE r;
         ALTER ROLE r SET app.a = Foo;
         ALTER ROLE r SET app.b = \"Foo\";
         ALTER ROLE r SET app.c = TRUE;
         ALTER ROLE r SET app.d = 007;
         ALTER ROLE r SET app.e = -007;
         ALTER ROLE r SET app.f = +1.50;
         ALTER ROLE r SET app.g = -1.50;
         ALTER ROLE r SET app.h = E'a\\\\tb';
         ALTER ROLE r SET app.i = $$x$$;
         ALTER ROLE r SET app.j = ' y ';
         ALTER ROLE r SET app.k = FÜr;",
    );
    let values: Vec<(&str, &str)> =
        db.role_settings().map(|setting| (setting.name(), setting.value())).collect();
    assert_eq!(
        values,
        [
            ("app.a", "foo"),
            ("app.b", "Foo"),
            ("app.c", "true"),
            ("app.d", "7"),
            ("app.e", "-7"),
            ("app.f", "1.50"),
            ("app.g", "-1.50"),
            ("app.h", "a\\tb"),
            ("app.i", "x"),
            ("app.j", " y "),
            ("app.k", "fÜr"),
        ]
    );
}

#[test]
fn reset_default_and_reset_all_remove_entries_of_one_key() {
    let db = parse(
        "CREATE ROLE r;
         ALTER ROLE r SET timezone = 'Asia/Tokyo';
         ALTER ROLE r SET work_mem = '64MB';
         ALTER ROLE r SET app.x = 'y';
         ALTER ROLE r IN DATABASE app SET timezone = 'Europe/Rome';
         ALTER ROLE ALL SET work_mem = '8MB';
         ALTER ROLE r RESET timezone;
         ALTER ROLE r SET app.x = DEFAULT;
         ALTER ROLE r SET app.never = DEFAULT;",
    );
    assert_eq!(
        entries(&db),
        [
            (Some("r"), None, "work_mem", "64MB"),
            (Some("r"), Some("app"), "timezone", "Europe/Rome"),
            (None, None, "work_mem", "8MB"),
        ]
    );

    // `RESET ALL` empties one role and database, leaving the others.
    let db = parse(
        "CREATE ROLE r;
         ALTER ROLE r SET timezone = 'Asia/Tokyo';
         ALTER ROLE r SET work_mem = '64MB';
         ALTER ROLE r IN DATABASE app SET timezone = 'Europe/Rome';
         ALTER ROLE ALL SET timezone = 'UTC';
         ALTER ROLE r RESET ALL;",
    );
    assert_eq!(
        entries(&db),
        [(Some("r"), Some("app"), "timezone", "Europe/Rome"), (None, None, "timezone", "UTC")]
    );

    let db = parse(
        "ALTER ROLE ALL SET timezone = 'UTC';
         ALTER ROLE ALL IN DATABASE app SET timezone = 'Europe/Rome';
         ALTER ROLE ALL IN DATABASE app RESET ALL;",
    );
    assert_eq!(entries(&db), [(None, None, "timezone", "UTC")]);
}

#[test]
fn a_quoted_all_is_a_role_and_names_fold_unless_quoted() {
    let db = parse(
        "CREATE ROLE \"ALL\";
         CREATE ROLE \"Admin\";
         ALTER ROLE \"ALL\" SET timezone = 'UTC';
         ALTER ROLE \"Admin\" IN DATABASE App SET timezone = 'UTC';
         ALTER ROLE \"Admin\" IN DATABASE \"App\" SET timezone = 'UTC';",
    );
    assert_eq!(
        entries(&db),
        [
            (Some("ALL"), None, "timezone", "UTC"),
            (Some("Admin"), Some("app"), "timezone", "UTC"),
            (Some("Admin"), Some("App"), "timezone", "UTC"),
        ]
    );
}

/// Supabase migrations set parameters on platform roles a schema never
/// creates, so an entry is kept under the name it was set on.
#[test]
fn a_setting_on_an_undeclared_role_is_kept_under_its_name() {
    let db = parse("ALTER ROLE authenticated SET statement_timeout = '8s';");
    assert_eq!(entries(&db), [(Some("authenticated"), None, "statement_timeout", "8s")]);
}

#[test]
fn entries_follow_a_renamed_role_and_go_with_a_dropped_one() {
    // PostgreSQL 18.6 keys the entry by the role's oid, so it reads under the
    // new name, and `DROP ROLE` deletes it.
    let db = parse(
        "CREATE ROLE s1;
         CREATE ROLE kept;
         ALTER ROLE s1 SET timezone = 'Asia/Tokyo';
         ALTER ROLE kept SET timezone = 'UTC';
         ALTER ROLE s1 RENAME TO s2;",
    );
    assert_eq!(
        entries(&db),
        [(Some("s2"), None, "timezone", "Asia/Tokyo"), (Some("kept"), None, "timezone", "UTC")]
    );

    let db = parse(
        "CREATE ROLE s1;
         CREATE ROLE kept;
         ALTER ROLE s1 SET timezone = 'Asia/Tokyo';
         ALTER ROLE kept SET timezone = 'UTC';
         DROP ROLE s1;",
    );
    assert_eq!(entries(&db), [(Some("kept"), None, "timezone", "UTC")]);
}

#[test]
fn a_rolled_back_setting_is_not_kept() {
    let db = parse(
        "CREATE ROLE r;
         BEGIN;
         ALTER ROLE r SET timezone = 'Asia/Tokyo';
         ROLLBACK;",
    );
    assert_eq!(entries(&db), []);
}

#[test]
fn the_session_role_keyword_names_the_role_the_session_runs_as() {
    // PostgreSQL 18.6 stored `ALTER ROLE SESSION_USER SET app.s = 'w'` under
    // the login role.
    let db = parse(
        "CREATE ROLE owner_r;
         CREATE ROLE login_r;
         SET SESSION AUTHORIZATION login_r;
         SET ROLE owner_r;
         ALTER ROLE CURRENT_USER SET app.c = 'v';
         ALTER ROLE SESSION_USER SET app.s = 'w';",
    );
    assert_eq!(
        entries(&db),
        [(Some("owner_r"), None, "app.c", "v"), (Some("login_r"), None, "app.s", "w")]
    );

    assert!(matches!(
        refusal("ALTER ROLE CURRENT_USER SET app.c = 'v';"),
        Error::UnnamedSessionRoleSetting { statement }
            if statement == "ALTER ROLE CURRENT_USER SET app.c TO 'v'"
    ));
}

#[test]
fn a_value_postgresql_does_not_read_as_a_literal_is_refused() {
    // PostgreSQL 18.6 raises a syntax error for both.
    for (sql, rendered) in [
        ("ALTER ROLE r SET timezone = now();", "ALTER ROLE r SET timezone TO now()"),
        (
            "ALTER ROLE r SET timezone = interval '1 hour';",
            "ALTER ROLE r SET timezone TO INTERVAL '1 hour'",
        ),
    ] {
        assert!(
            matches!(
                refusal(&format!("CREATE ROLE r; {sql}")),
                Error::NonLiteralSetting { statement } if statement == rendered
            ),
            "{sql} is refused"
        );
    }
}

#[test]
fn a_time_zone_postgresql_refuses_is_refused() {
    // PostgreSQL 18.6: ERROR: invalid value for parameter "TimeZone": "bogus".
    assert!(matches!(
        refusal("CREATE ROLE r; ALTER ROLE r SET timezone = 'bogus';"),
        Error::InvalidTimeZone { value, reason: TimeZoneError::InvalidValue } if value == "bogus"
    ));
    assert!(matches!(
        refusal("SET TIME ZONE 'bogus';"),
        Error::InvalidTimeZone { value, reason: TimeZoneError::InvalidValue } if value == "bogus"
    ));
    // A setting of another name is not read as a zone.
    parse("CREATE ROLE r; ALTER ROLE r SET app.timezone = 'bogus';");
}

#[test]
fn a_constructed_entry_answers_as_the_catalog_row_holds_it() {
    // PostgreSQL 18.6 stored TimeZone=Asia/Tokyo under r in app for
    // `ALTER ROLE r IN DATABASE app SET timezone = 'Asia/Tokyo'`.
    let setting = RoleSetting::new(Some("r".into()), Some("app".into()), "TimeZone", "Asia/Tokyo")
        .expect("PostgreSQL accepts the zone");
    assert_eq!(
        (setting.role(), setting.database(), setting.name(), setting.value()),
        (Some("r"), Some("app"), "TimeZone", "Asia/Tokyo")
    );
    assert!(setting.is_named("timezone"));

    assert!(matches!(
        RoleSetting::new(None, None, "TimeZone", "bogus"),
        Err(Error::InvalidTimeZone { value, reason: TimeZoneError::InvalidValue }) if value == "bogus"
    ));
    // A setting of another name is not read as a zone.
    RoleSetting::new(None, None, "app.timezone", "bogus").expect("PostgreSQL stores any text");
}

#[test]
fn from_current_stores_the_value_the_session_set() {
    // PostgreSQL 18.6 stored TimeZone=Asia/Tokyo.
    let db = parse(
        "CREATE ROLE r;
         SET timezone = 'Asia/Tokyo';
         ALTER ROLE r SET timezone FROM CURRENT;",
    );
    assert_eq!(entries(&db), [(Some("r"), None, "timezone", "Asia/Tokyo")]);

    // PostgreSQL 18.6 stored TimeZone=Asia/Kolkata, the `SET LOCAL` value.
    let db = parse(
        "CREATE ROLE r;
         SET TIME ZONE 'Asia/Tokyo';
         BEGIN;
         SET LOCAL timezone = 'Asia/Kolkata';
         ALTER ROLE r SET timezone FROM CURRENT;
         COMMIT;",
    );
    assert_eq!(entries(&db), [(Some("r"), None, "timezone", "Asia/Kolkata")]);

    // PostgreSQL 18.6 stored TimeZone=Asia/Tokyo, the rollback undoing the
    // `SET` inside the transaction.
    let db = parse(
        "CREATE ROLE r;
         SET timezone = 'Asia/Tokyo';
         BEGIN;
         SET timezone = 'Europe/Rome';
         ROLLBACK;
         ALTER ROLE r SET timezone FROM CURRENT;",
    );
    assert_eq!(entries(&db), [(Some("r"), None, "timezone", "Asia/Tokyo")]);

    // A `SET LOCAL` ends with its transaction.
    let db = parse(
        "CREATE ROLE r;
         SET timezone = 'Asia/Tokyo';
         BEGIN;
         SET LOCAL TIME ZONE 'Europe/Rome';
         COMMIT;
         ALTER ROLE r SET timezone FROM CURRENT;",
    );
    assert_eq!(entries(&db), [(Some("r"), None, "timezone", "Asia/Tokyo")]);

    // PostgreSQL 18.6 stored TimeZone=Europe/Rome both inside and after the
    // transaction, a plain `SET` replacing what `SET LOCAL` held.
    let db = parse(
        "CREATE ROLE r;
         BEGIN;
         SET LOCAL timezone = 'Asia/Tokyo';
         SET timezone = 'Europe/Rome';
         ALTER ROLE r IN DATABASE app SET timezone FROM CURRENT;
         COMMIT;
         ALTER ROLE r SET timezone FROM CURRENT;",
    );
    assert_eq!(
        entries(&db),
        [
            (Some("r"), Some("app"), "timezone", "Europe/Rome"),
            (Some("r"), None, "timezone", "Europe/Rome"),
        ]
    );

    // A `SET ROLE` leaves settings alone.
    let db = parse(
        "CREATE ROLE r;
         SET app.x = 'y';
         SET ROLE r;
         ALTER ROLE r SET app.x FROM CURRENT;",
    );
    assert_eq!(entries(&db), [(Some("r"), None, "app.x", "y")]);
}

#[test]
fn from_current_is_refused_where_the_session_value_is_unknown() {
    // Each script leaves the session at a value the server's configuration
    // and the migration's login role decide, which the schema cannot know.
    for sql in [
        "ALTER ROLE r SET timezone FROM CURRENT;",
        "SET timezone = 'Asia/Tokyo'; RESET timezone; ALTER ROLE r SET timezone FROM CURRENT;",
        "SET timezone = 'Asia/Tokyo'; RESET ALL; ALTER ROLE r SET timezone FROM CURRENT;",
        "SET timezone = 'Asia/Tokyo'; DISCARD ALL; ALTER ROLE r SET timezone FROM CURRENT;",
        "SET timezone = 'Asia/Tokyo'; SET TIME ZONE LOCAL; ALTER ROLE r SET timezone FROM CURRENT;",
        "SET timezone = 'Asia/Tokyo'; SET timezone = DEFAULT; ALTER ROLE r SET timezone FROM CURRENT;",
        "SET timezone = 'Asia/Tokyo'; SET TIME ZONE DEFAULT; ALTER ROLE r SET timezone FROM CURRENT;",
        "SET TIME ZONE INTERVAL '-08:00' HOUR TO MINUTE; ALTER ROLE r SET timezone FROM CURRENT;",
        "BEGIN; SET timezone = 'Asia/Tokyo'; ROLLBACK; ALTER ROLE r SET timezone FROM CURRENT;",
        // A `SET LOCAL` outside a transaction changes nothing.
        "SET LOCAL timezone = 'Asia/Tokyo'; ALTER ROLE r SET timezone FROM CURRENT;",
    ] {
        assert!(
            matches!(
                refusal(&format!("CREATE ROLE r; {sql}")),
                Error::UnknownCurrentSetting { setting, statement }
                    if setting == "timezone"
                        && statement == "ALTER ROLE r SET timezone FROM CURRENT"
            ),
            "{sql} is refused"
        );
    }
}
