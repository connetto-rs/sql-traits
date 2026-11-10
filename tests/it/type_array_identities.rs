//! Generated PostgreSQL arrays keep their owning type identities.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

#[test]
fn displaced_array_keeps_its_name_after_the_collision_is_dropped() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE token AS ENUM ('one');
         CREATE TYPE _token AS ENUM ('two');
         DROP TYPE _token;
         DROP TYPE __token;",
    );
    assert!(matches!(
        result,
        Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"
    ));
}

#[test]
fn a_preoccupied_array_name_uses_a_numeric_suffix() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE _token AS ENUM ('two');
         CREATE TYPE token AS ENUM ('one');
         DROP TYPE _token;
         DROP TYPE _token_1;",
    );
    assert!(matches!(
        result,
        Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"
    ));
}

#[test]
fn a_shell_has_no_generated_array() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE token;
         DROP TYPE _token;",
    );
    assert!(matches!(result, Err(Error::TypeNotFound { type_name }) if type_name == "_token"));
}

#[test]
fn domain_and_rowtype_arrays_cannot_be_dropped_directly() {
    for declaration in [
        "CREATE DOMAIN token AS INT",
        "CREATE TABLE token (value INT)",
        "CREATE VIEW token AS SELECT 1 AS value",
        "CREATE MATERIALIZED VIEW token AS SELECT 1 AS value",
    ] {
        let result =
            ParserDB::parse::<PostgreSqlDialect>(&format!("{declaration}; DROP TYPE _token;"));
        assert!(matches!(
            result,
            Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"
        ));
    }
}

#[test]
fn renaming_an_owner_to_its_array_displaces_that_array_once() {
    for (declaration, alteration) in [
        ("CREATE TYPE token AS ENUM ('one')", "ALTER TYPE token RENAME TO _token"),
        ("CREATE TABLE token (value INT)", "ALTER TABLE token RENAME TO _token"),
        ("CREATE VIEW token AS SELECT 1 AS value", "ALTER VIEW token RENAME TO _token"),
        (
            "CREATE MATERIALIZED VIEW token AS SELECT 1 AS value",
            "ALTER MATERIALIZED VIEW token RENAME TO _token",
        ),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{declaration}; {alteration}; DROP TYPE __token;"
        ));
        assert!(
            matches!(
                result,
                Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "_token"
            ),
            "{declaration}"
        );
    }
}

#[test]
fn completing_a_shell_creates_its_array() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE token;
         CREATE TYPE token AS ENUM ('one');
         DROP TYPE _token;",
    );
    assert!(matches!(
        result,
        Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"
    ));
}

#[test]
fn rollback_restores_displaced_array_names() {
    for rollback in [
        "BEGIN; CREATE TYPE _token AS ENUM ('two'); ROLLBACK;",
        "BEGIN; SAVEPOINT s; CREATE TYPE _token AS ENUM ('two'); ROLLBACK TO s; COMMIT;",
        "BEGIN; DROP TYPE token; ROLLBACK;",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TYPE token AS ENUM ('one'); {rollback} DROP TYPE _token;"
        ));
        assert!(matches!(
            result,
            Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"
        ));
    }
}

#[test]
fn dropping_an_owner_releases_its_array_identity() {
    for lifecycle in [
        "CREATE TYPE token AS ENUM ('old'); DROP TYPE token;",
        "CREATE DOMAIN token AS INT; DROP DOMAIN token;",
        "CREATE TABLE token (value INT); DROP TABLE token;",
        "CREATE VIEW token AS SELECT 1 AS value; DROP VIEW token;",
        "CREATE MATERIALIZED VIEW token AS SELECT 1 AS value; DROP MATERIALIZED VIEW token;",
        "CREATE TABLE source (value INT); CREATE VIEW token AS SELECT value FROM source; DROP TABLE source CASCADE;",
        "CREATE SEQUENCE source; CREATE VIEW token AS SELECT last_value FROM source; DROP SEQUENCE source CASCADE;",
        "CREATE TABLE source (value INT); CREATE VIEW token AS SELECT value FROM source; ALTER TABLE source DROP COLUMN value CASCADE;",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{lifecycle}
             CREATE TYPE _token AS ENUM ('occupied');
             CREATE TYPE token AS ENUM ('new');
             DROP TYPE _token_1;"
        ));
        assert!(
            matches!(
                result,
                Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"
            ),
            "{lifecycle}"
        );
    }
}

#[test]
fn range_and_multirange_array_names_have_separate_owners() {
    for (name, owner) in [("_span", "span"), ("_span_multirange", "span_multirange")] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TYPE span AS RANGE (SUBTYPE = INT4); DROP TYPE {name};"
        ));
        assert!(matches!(
            result,
            Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == owner
        ));
    }
}

#[test]
fn a_multirange_array_uses_the_multirange_namespace() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA s;
         CREATE TYPE s._many AS ENUM ('occupied');
         CREATE TYPE span AS RANGE (SUBTYPE = INT4, MULTIRANGE_TYPE_NAME = s.many);
         DROP TYPE s._many_1;",
    );
    assert!(
        matches!(&result, Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "many"),
        "{result:?}"
    );
}

#[test]
fn unquoted_owner_names_fold_before_generating_arrays() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE Token AS ENUM ('one'); DROP TYPE _token;",
    );
    assert!(matches!(result, Err(Error::TypeDropRequiredBy { .. })));
}

#[test]
fn array_collision_suffixes_preserve_utf8_boundaries() {
    let owner = format!("{}é", "a".repeat(59));
    let occupied = format!("_{owner}");
    let generated = format!("_{}_1", "a".repeat(59));
    let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "CREATE TYPE \"{occupied}\" AS ENUM ('one');
         CREATE TYPE \"{owner}\" AS ENUM ('two');
         DROP TYPE \"{generated}\";"
    ));
    assert!(matches!(
        result,
        Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == owner
    ));
}

#[test]
fn moving_a_rowtype_preserves_its_displaced_array_name() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA s;
         CREATE SCHEMA d;
         CREATE TABLE s.token (value INT);
         CREATE TYPE s._token AS ENUM ('one');
         ALTER TABLE s.token RENAME TO d.token;
         DROP TYPE d.__token;",
    );
    assert!(
        matches!(&result, Err(Error::TypeDropRequiredBy { requiring_name, .. }) if requiring_name == "token"),
        "{result:?}"
    );
}

#[test]
fn moving_a_rowtype_refuses_type_and_array_collisions() {
    for (owner, destination) in [
        ("token", "CREATE TYPE d.token AS ENUM ('one')"),
        ("token", "CREATE TYPE d._token AS ENUM ('one')"),
        ("_token", "CREATE TYPE d.token AS ENUM ('one')"),
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA s;
             CREATE SCHEMA d;
             CREATE TABLE s.{owner} (value INT);
             {destination};
             ALTER TABLE s.{owner} RENAME TO d.{owner};"
        ));
        assert!(
            matches!(result, Err(Error::RelationNameAlreadyTaken { .. })),
            "{destination} {result:?}"
        );
    }
}
