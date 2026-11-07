//! The `TimeZone` a deployment's request runs in, found as PostgreSQL finds a
//! session's from `ALTER ROLE ... SET` and the server default.
//!
//! Each expected zone is what `SHOW timezone` answered on PostgreSQL 18.6,
//! logged in as `l` to database `d`, with `Etc/UTC` in `postgresql.conf`.
#![allow(clippy::expect_used)]

use sql_traits::{errors::DeploymentError, prelude::*};
use sql_values::TimeZoneError;
use sqlparser::dialect::PostgreSqlDialect;

const ROLES: &str = "CREATE ROLE l LOGIN; CREATE ROLE a NOLOGIN; GRANT a TO l;";

/// The full chain, every step set.
const CHAIN: &str = "ALTER ROLE ALL SET timezone = 'Europe/Rome';
     ALTER ROLE ALL IN DATABASE d SET timezone = 'Europe/Paris';
     ALTER ROLE l SET timezone = 'Asia/Tokyo';
     ALTER ROLE l IN DATABASE d SET timezone = 'America/Chicago';
     ALTER ROLE a SET timezone = 'America/New_York';
     ALTER ROLE a IN DATABASE d SET timezone = 'America/Denver';";

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{ROLES} {sql}")).expect("schema builds")
}

/// A plain PostgreSQL deployment logging in as `l` to `d` and acting as `a`
/// with `SET ROLE`.
fn plain() -> Deployment {
    Deployment::new("l", ["a"], [])
        .expect("a valid description")
        .connected_to("d")
        .with_default_time_zone("Etc/UTC")
        .expect("a zone PostgreSQL reads")
}

/// The zone a request acting as `a` runs in, and the step that set it.
fn zone(
    database: &ParserDB,
    deployment: &Deployment,
) -> Result<(String, TimeZoneSource), DeploymentError> {
    let resolved = deployment.resolve(database).expect("every role is declared");
    resolved.time_zone("a").map(|zone| (zone.name().to_string(), zone.source()))
}

fn answer(name: &str, source: TimeZoneSource) -> (String, TimeZoneSource) {
    (name.to_string(), source)
}

#[test]
fn each_step_of_the_login_chain_answers_when_the_steps_above_are_unset() {
    // `SHOW timezone` after each `ALTER ROLE`, top of the chain first.
    let database = parse(CHAIN);
    assert_eq!(
        zone(&database, &plain()),
        Ok(answer("America/Chicago", TimeZoneSource::LoginRoleInDatabase))
    );

    // PostgreSQL 18.6: Asia/Tokyo once `l IN DATABASE d` is reset.
    let database = parse(&format!("{CHAIN} ALTER ROLE l IN DATABASE d RESET ALL;"));
    assert_eq!(zone(&database, &plain()), Ok(answer("Asia/Tokyo", TimeZoneSource::LoginRole)));

    // PostgreSQL 18.6: Europe/Paris once `l` is reset too.
    let database = parse(&format!(
        "{CHAIN} ALTER ROLE l IN DATABASE d RESET ALL; ALTER ROLE l RESET timezone;"
    ));
    assert_eq!(zone(&database, &plain()), Ok(answer("Europe/Paris", TimeZoneSource::Database)));

    // PostgreSQL 18.6: Europe/Rome once the database's own is reset.
    let database = parse(&format!(
        "{CHAIN} ALTER ROLE l IN DATABASE d RESET ALL; ALTER ROLE l RESET timezone;
         ALTER ROLE ALL IN DATABASE d RESET timezone;"
    ));
    assert_eq!(zone(&database, &plain()), Ok(answer("Europe/Rome", TimeZoneSource::AllRoles)));

    // PostgreSQL 18.6: Etc/UTC, `postgresql.conf`, once nothing is set.
    let database = parse(&format!(
        "{CHAIN} ALTER ROLE l IN DATABASE d RESET ALL; ALTER ROLE l RESET timezone;
         ALTER ROLE ALL IN DATABASE d RESET timezone; ALTER ROLE ALL RESET ALL;"
    ));
    assert_eq!(zone(&database, &plain()), Ok(answer("Etc/UTC", TimeZoneSource::ServerDefault)));
}

#[test]
fn set_role_does_not_apply_the_acting_role_settings() {
    // PostgreSQL 18.6: America/Chicago after both `SET ROLE a` and
    // `BEGIN; SET LOCAL ROLE a`, `a`'s own settings unread.
    let database = parse(CHAIN);
    assert_eq!(
        zone(&database, &plain()),
        Ok(answer("America/Chicago", TimeZoneSource::LoginRoleInDatabase))
    );

    let database = parse("ALTER ROLE a SET timezone = 'America/New_York';");
    assert_eq!(zone(&database, &plain()), Ok(answer("Etc/UTC", TimeZoneSource::ServerDefault)));
}

#[test]
fn per_request_role_settings_put_the_acting_role_first() {
    let per_request = plain().with_acting_role_settings();
    assert!(per_request.applies_acting_role_settings());

    let database = parse(CHAIN);
    assert_eq!(
        zone(&database, &per_request),
        Ok(answer("America/Denver", TimeZoneSource::ActingRoleInDatabase))
    );

    let database = parse(&format!("{CHAIN} ALTER ROLE a IN DATABASE d RESET timezone;"));
    assert_eq!(
        zone(&database, &per_request),
        Ok(answer("America/New_York", TimeZoneSource::ActingRole))
    );

    // Without settings of its own the acting role falls back on the login
    // role's chain.
    let database = parse(&format!(
        "{CHAIN} ALTER ROLE a IN DATABASE d RESET timezone; ALTER ROLE a RESET ALL;"
    ));
    assert_eq!(
        zone(&database, &per_request),
        Ok(answer("America/Chicago", TimeZoneSource::LoginRoleInDatabase))
    );
}

#[test]
fn settings_of_another_database_or_name_do_not_apply() {
    let database = parse(
        "ALTER ROLE l IN DATABASE other SET timezone = 'Asia/Tokyo';
         ALTER ROLE ALL IN DATABASE other SET timezone = 'Asia/Tokyo';
         ALTER ROLE l SET app.timezone = 'Asia/Tokyo';",
    );
    assert_eq!(zone(&database, &plain()), Ok(answer("Etc/UTC", TimeZoneSource::ServerDefault)));
}

#[test]
fn the_zone_is_named_as_written() {
    let database = parse("ALTER ROLE l SET TimeZone TO UTC;");
    assert_eq!(zone(&database, &plain()), Ok(answer("utc", TimeZoneSource::LoginRole)));

    let deployment = Deployment::new("l", ["a"], [])
        .expect("a valid description")
        .with_default_time_zone("europe/rome")
        .expect("a zone PostgreSQL reads");
    assert_eq!(deployment.default_time_zone(), Some("europe/rome"));
    assert_eq!(
        zone(&parse(""), &deployment),
        Ok(answer("europe/rome", TimeZoneSource::ServerDefault))
    );
}

#[test]
fn a_missing_default_is_refused_when_nothing_sets_the_zone() {
    let deployment =
        Deployment::new("l", ["a"], []).expect("a valid description").connected_to("d");
    assert_eq!(deployment.default_time_zone(), None);
    assert_eq!(zone(&parse(""), &deployment), Err(DeploymentError::NoDefaultTimeZone));
    assert_eq!(
        zone(&parse("ALTER ROLE ALL SET timezone = 'Europe/Rome';"), &deployment),
        Ok(answer("Europe/Rome", TimeZoneSource::AllRoles))
    );
}

#[test]
fn a_database_setting_needs_the_deployment_database() {
    let deployment = Deployment::new("l", ["a"], [])
        .expect("a valid description")
        .with_default_time_zone("Etc/UTC")
        .expect("a zone PostgreSQL reads");
    assert_eq!(deployment.database(), None);

    // Without one in the chain the database is never needed.
    assert_eq!(
        zone(&parse("ALTER ROLE l SET timezone = 'Asia/Tokyo';"), &deployment),
        Ok(answer("Asia/Tokyo", TimeZoneSource::LoginRole))
    );
    assert_eq!(
        zone(&parse("ALTER ROLE l IN DATABASE d SET timezone = 'Asia/Tokyo';"), &deployment),
        Err(DeploymentError::NoDatabase { role: Some("l".into()) })
    );
    assert_eq!(
        zone(&parse("ALTER ROLE ALL IN DATABASE d SET timezone = 'Asia/Tokyo';"), &deployment),
        Err(DeploymentError::NoDatabase { role: None })
    );
}

#[test]
fn a_default_postgresql_refuses_is_refused_at_construction() {
    let deployment = Deployment::new("l", ["a"], []).expect("a valid description");
    assert_eq!(
        deployment.clone().with_default_time_zone("CEST"),
        Err(DeploymentError::InvalidTimeZone {
            zone: "CEST".into(),
            reason: TimeZoneError::InvalidValue
        })
    );
    assert_eq!(
        deployment.with_default_time_zone("+168"),
        Err(DeploymentError::InvalidTimeZone {
            zone: "+168".into(),
            reason: TimeZoneError::OffsetOutOfRange
        })
    );
}

#[test]
fn only_an_acting_role_of_the_deployment_is_answered_for() {
    let database = parse("");
    let deployment = plain();
    let resolved = deployment.resolve(&database).expect("every role is declared");
    assert_eq!(
        resolved.time_zone("l").map(|zone| zone.name().to_string()),
        Err(DeploymentError::NotAnActingRole { role: "l".into() })
    );
}
