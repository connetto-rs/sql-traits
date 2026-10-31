//! Tests that a function's `RETURN` body is bound to the relations it reads,
//! as PostgreSQL does at `CREATE FUNCTION` time.
//!
//! Every rule asserted here was measured against PostgreSQL 18.6 in Docker
//! rather than read from documentation.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{ast::CreateFunction, dialect::PostgreSqlDialect, parser::Parser};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn function<'db>(db: &'db ParserDB, name: &str) -> &'db CreateFunction {
    db.function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the function is recorded")
}

fn body(db: &ParserDB, name: &str) -> String {
    function(db, name).body_expression().expect("a RETURN body").to_string()
}
/// A body resolves under the session's `search_path` in effect at
/// `CREATE FUNCTION`; the function's own `SET search_path` clause plays no
/// part.
#[test]
fn a_return_body_resolves_under_the_creation_search_path() {
    let db = parse(
        "SET search_path = public;
         CREATE SCHEMA app;
         CREATE TABLE t (a INT);
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql SET search_path = app
             RETURN (SELECT a FROM t);",
    )
    .expect("the body binds under the session path");
    assert_eq!(
        body(&db, "r"),
        "(SELECT a FROM t)",
        "the public table answers the name, not the app one"
    );
}

/// A relation the body names that the database does not hold refuses the
/// statement, before it records or replaces anything.
#[test]
fn a_return_body_naming_an_absent_relation_is_refused() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM nope);",
    )
    .expect_err("nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, function_name }
            if relation_name == "nope" && function_name == "r"),
        "got {refused:?}"
    );

    // A schema-qualified miss is refused under the name the body wrote.
    let refused = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM app.nope);",
    )
    .expect_err("app.nope is not held");
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { relation_name, .. }
            if relation_name == "app.nope"),
        "got {refused:?}"
    );

    // A name a visible `WITH` item answers is left alone, as for a view.
    parse(
        "CREATE FUNCTION r() RETURNS INT LANGUAGE sql
            RETURN ((WITH nope AS (SELECT 1 AS a) SELECT a FROM nope));",
    )
    .expect("the WITH item answers the name");

    // A failed replacement refuses before it touches the old function.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);",
    )
    .expect("the function is recorded");
    let statement = Parser::parse_sql(
        &PostgreSqlDialect {},
        "CREATE OR REPLACE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM nope);",
    )
    .expect("SQL parses")
    .pop()
    .expect("one statement");
    let Err(refused) = db.into_ingestor().apply_statement(statement) else {
        panic!("a body naming an absent relation is refused");
    };
    assert!(
        matches!(&refused, Error::RelationNotFoundForFunctionBody { function_name, .. }
            if function_name == "r"),
        "got {refused:?}"
    );
}

/// The stored body follows renames and schema moves of the relations it
/// reads, tables, views and materialized views alike. The pinned parser does
/// not read `ALTER TABLE ... SET SCHEMA`, so the cross-schema case uses the
/// `RENAME TABLE` spelling the model accepts for a schema move.
#[test]
fn a_return_body_follows_renames_of_what_it_reads() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM t2)");

    // A qualified column reference follows too.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT t.a FROM t);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT t2.a FROM t2)");

    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE u (a INT);
         CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM u);
         RENAME TABLE u TO app.u;",
    )
    .expect("the move is accepted");
    assert_eq!(body(&db, "g"), "(SELECT a FROM app.u)");
    let moved = db
        .table_by_target(
            TargetName::new("u", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table is recorded");
    assert_eq!(moved.table_schema().map(ToString::to_string).as_deref(), Some("app"));

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM v);
         ALTER TABLE v RENAME TO w;",
    )
    .expect("the view rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM w)");

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM m);
         ALTER TABLE m RENAME TO n;",
    )
    .expect("the materialized view rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT a FROM n)");

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         ALTER TABLE t RENAME COLUMN a TO b;",
    )
    .expect("the column rename is accepted");
    assert_eq!(body(&db, "r"), "(SELECT b AS a FROM t)");
}

/// A relation the body reads cannot be dropped without `CASCADE`, and
/// `CASCADE` takes the function with it.
#[test]
fn a_relation_a_function_reads_cannot_be_dropped_from_under_it() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         DROP TABLE t;",
    )
    .expect_err("r reads t");
    assert!(
        matches!(&refused, Error::RelationHasDependents { object_kind, object_name, dependent_kind, dependent_name }
            if *object_kind == ObjectKind::Table
                && object_name == "t"
                && *dependent_kind == ObjectKind::Function
                && dependent_name == "r"),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         DROP TABLE t CASCADE;",
    )
    .expect("cascade is accepted");
    assert!(
        db.function_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "the function went with the table"
    );

    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM v);
         DROP VIEW v;",
    )
    .expect_err("r reads v");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::Function),
        "got {refused:?}"
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM v);
         DROP VIEW v CASCADE;",
    )
    .expect("cascade is accepted");
    assert!(
        db.function_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "the function went with the view"
    );

    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM m);
         DROP MATERIALIZED VIEW m;",
    )
    .expect_err("r reads m");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::Function),
        "got {refused:?}"
    );
}

/// A body may read a temporary relation, and the function stays where it was
/// created, not temporary.
#[test]
fn a_return_body_may_read_a_temporary_relation() {
    let db = parse(
        "CREATE TEMP TABLE scratch (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM scratch);",
    )
    .expect("a body may read a temporary table");
    assert_eq!(body(&db, "r"), "(SELECT a FROM pg_temp.scratch)");
    assert!(
        function(&db, "r").target_name().schema().is_none(),
        "the function stays in the default schema"
    );

    // The temporary read still blocks a drop of the table.
    let refused = parse(
        "CREATE TEMP TABLE scratch (a INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM scratch);
         DROP TABLE scratch;",
    )
    .expect_err("r reads scratch");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::Function),
        "got {refused:?}"
    );
}

/// A body written as a string is never bound and holds no dependency.
#[test]
fn a_string_body_is_never_bound() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION q() RETURNS INT LANGUAGE sql AS 'SELECT a FROM t';
         CREATE FUNCTION w() RETURNS INT LANGUAGE sql AS $$SELECT a FROM nope$$;",
    )
    .expect("string bodies are never bound");
    assert_eq!(function(&db, "q").body(), Some("SELECT a FROM t"));
    assert!(function(&db, "q").body_expression().is_none());
    assert_eq!(function(&db, "w").body(), Some("SELECT a FROM nope"));

    // Nothing a string body names blocks a drop.
    parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a INT);
         CREATE FUNCTION q() RETURNS INT LANGUAGE sql AS 'SELECT a FROM t';
         DROP TABLE app.t;",
    )
    .expect("a string body holds no dependency");
}

/// `CREATE OR REPLACE` binds the replacement afresh, so the old body's
/// relations stop being read.
#[test]
fn a_replacement_binds_the_new_body_afresh() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (b INT);
         CREATE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t);
         CREATE OR REPLACE FUNCTION r() RETURNS INT LANGUAGE sql RETURN (SELECT b FROM u);
         DROP TABLE t;",
    )
    .expect("the old body no longer reads t");
    assert_eq!(body(&db, "r"), "(SELECT b FROM u)");
    assert!(
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "t went with the old body's last reader"
    );
}
