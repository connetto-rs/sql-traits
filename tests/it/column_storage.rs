//! Tests that `ALTER COLUMN ... SET STORAGE` refuses what PostgreSQL 18
//! refuses and otherwise leaves the model as it was, since the model records
//! no storage mode.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The SQLSTATE and message PostgreSQL reports for `sql`'s refusal.
fn refusal(sql: &str) -> (String, String) {
    let error = parse(sql).expect_err(sql);
    let postgres = error.postgres_error().expect(sql);
    (postgres.sqlstate().to_string(), postgres.message().to_string())
}

const BASE_TYPES: &str = "
    CREATE TYPE toasted;
    CREATE FUNCTION toasted_in(cstring) RETURNS toasted LANGUAGE internal IMMUTABLE STRICT AS 'textin';
    CREATE FUNCTION toasted_out(toasted) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'textout';
    CREATE TYPE toasted (INPUT = toasted_in, OUTPUT = toasted_out, INTERNALLENGTH = VARIABLE, STORAGE = extended);
    CREATE TYPE flat;
    CREATE FUNCTION flat_in(cstring) RETURNS flat LANGUAGE internal IMMUTABLE STRICT AS 'textin';
    CREATE FUNCTION flat_out(flat) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'textout';
    CREATE TYPE flat (INPUT = flat_in, OUTPUT = flat_out, INTERNALLENGTH = VARIABLE);";

#[test]
fn setting_storage_leaves_every_column_as_declared() {
    let declarations = "CREATE TABLE t (
            a TEXT NOT NULL DEFAULT 'x' COLLATE \"C\",
            b INT GENERATED ALWAYS AS IDENTITY,
            c TEXT GENERATED ALWAYS AS (a || 'y') STORED
        );
        CREATE TABLE child () INHERITS (t);
        CREATE TABLE p (a TEXT DEFAULT 'z') PARTITION BY LIST (a);
        CREATE TABLE p1 PARTITION OF p FOR VALUES IN ('z');";
    let before = parse(declarations).expect("the tables exist");
    for (alter, tables) in [
        ("ALTER TABLE t ALTER COLUMN a SET STORAGE EXTERNAL", ["t", "child"]),
        ("ALTER TABLE ONLY t ALTER COLUMN c SET STORAGE MAIN", ["t", "child"]),
        ("ALTER TABLE t ALTER COLUMN b SET STORAGE PLAIN", ["t", "child"]),
        ("ALTER TABLE p ALTER COLUMN a SET STORAGE MAIN", ["p", "p1"]),
        ("ALTER TABLE ONLY p ALTER COLUMN a SET STORAGE PLAIN", ["p", "p1"]),
        ("ALTER TABLE p1 ALTER COLUMN a SET STORAGE EXTENDED", ["p", "p1"]),
    ] {
        let after = parse(&format!("{declarations} {alter};")).expect(alter);
        for name in tables {
            assert_eq!(
                table(&after, None, name).to_string(),
                table(&before, None, name).to_string(),
                "{alter}: {name}"
            );
        }
    }
}

#[test]
fn a_type_stored_plain_takes_no_other_storage() {
    for (definition, data_type, storage, described) in [
        ("", "INT", "EXTERNAL", "integer"),
        ("", "BOOLEAN", "MAIN", "boolean"),
        ("", "UUID", "MAIN", "uuid"),
        ("", "\"char\"", "EXTENDED", "\"char\""),
        ("", "TIMESTAMPTZ", "MAIN", "timestamp with time zone"),
        ("", "OID", "EXTERNAL", "oid"),
        ("CREATE DOMAIN amount AS INT;", "amount", "EXTERNAL", "amount"),
        ("CREATE SCHEMA s; CREATE DOMAIN s.amount AS INT;", "s.amount", "MAIN", "s.amount"),
        (
            "CREATE DOMAIN amount AS INT; CREATE DOMAIN positive AS amount;",
            "positive",
            "MAIN",
            "positive",
        ),
        ("CREATE TYPE mood AS ENUM ('calm');", "mood", "EXTENDED", "mood"),
        (BASE_TYPES, "flat", "MAIN", "flat"),
        (
            &format!("{BASE_TYPES} CREATE DOMAIN flat_domain AS flat;"),
            "flat_domain",
            "MAIN",
            "flat_domain",
        ),
    ] {
        let declared = format!("{definition} CREATE TABLE t (v {data_type});");
        let refused = format!("{declared} ALTER TABLE t ALTER COLUMN v SET STORAGE {storage};");
        assert_eq!(
            refusal(&refused),
            (
                "0A000".to_owned(),
                format!("column data type {described} can only have storage PLAIN")
            ),
            "{refused}"
        );
        let plain = format!("{declared} ALTER TABLE t ALTER COLUMN v SET STORAGE PLAIN;");
        parse(&plain).expect(&plain);
    }
}

#[test]
fn a_toastable_type_takes_every_storage() {
    for (definition, data_type) in [
        ("", "TEXT"),
        ("", "NUMERIC"),
        ("", "CHAR(3)"),
        ("", "INT[]"),
        ("CREATE DOMAIN amount AS INT;", "amount[]"),
        ("CREATE DOMAIN words AS TEXT;", "words"),
        ("CREATE TYPE mood AS ENUM ('calm');", "mood[]"),
        ("CREATE TYPE pair AS (x INT);", "pair"),
        ("CREATE TYPE span AS RANGE (SUBTYPE = float8);", "span"),
        ("CREATE TYPE span AS RANGE (SUBTYPE = float8);", "span_multirange"),
        ("CREATE TABLE row_source (x INT);", "row_source"),
        (BASE_TYPES, "toasted"),
        (BASE_TYPES, "flat[]"),
    ] {
        for storage in ["PLAIN", "EXTERNAL", "EXTENDED", "MAIN"] {
            let sql = format!(
                "{definition} CREATE TABLE t (v {data_type}); ALTER TABLE t ALTER COLUMN v SET STORAGE {storage};"
            );
            parse(&sql).expect(&sql);
        }
    }
}

#[test]
fn storage_is_judged_against_the_type_the_statement_leaves() {
    for operations in [
        "ALTER COLUMN v TYPE TEXT, ALTER COLUMN v SET STORAGE EXTERNAL",
        "ALTER COLUMN v SET STORAGE EXTERNAL, ALTER COLUMN v TYPE TEXT",
    ] {
        let sql = format!("CREATE TABLE t (v INT); ALTER TABLE t {operations};");
        parse(&sql).expect(&sql);
    }
    assert_eq!(
        refusal(
            "CREATE TABLE t (v TEXT); ALTER TABLE t ALTER COLUMN v SET STORAGE MAIN, ALTER COLUMN v TYPE INT USING v::INT;"
        ),
        ("0A000".to_owned(), "column data type integer can only have storage PLAIN".to_owned())
    );
}

#[test]
fn a_missing_column_is_refused() {
    for (setup, relation) in [
        ("CREATE TABLE r (v INT)", "r"),
        ("CREATE TABLE base (v INT); CREATE MATERIALIZED VIEW r AS SELECT v FROM base", "r"),
    ] {
        let sql =
            format!("{setup}; ALTER TABLE {relation} ALTER COLUMN missing SET STORAGE PLAIN;");
        assert_eq!(
            refusal(&sql),
            (
                "42703".to_owned(),
                format!("column \"missing\" of relation \"{relation}\" does not exist")
            ),
            "{sql}"
        );
    }
}

#[test]
fn storage_follows_the_relation_kind() {
    let setup = "CREATE TABLE base (v TEXT);
        CREATE VIEW plain_view AS SELECT v FROM base;
        CREATE MATERIALIZED VIEW stored_view AS SELECT v FROM base;
        CREATE FOREIGN DATA WRAPPER wrapper;
        CREATE SERVER server FOREIGN DATA WRAPPER wrapper;
        CREATE FOREIGN TABLE remote (v TEXT) SERVER server;";
    for accepted in [
        "ALTER TABLE stored_view ALTER COLUMN v SET STORAGE PLAIN",
        "ALTER TABLE remote ALTER COLUMN v SET STORAGE MAIN",
        "ALTER FOREIGN TABLE remote ALTER COLUMN v SET STORAGE EXTERNAL",
    ] {
        let sql = format!("{setup} {accepted};");
        parse(&sql).expect(&sql);
    }
    let sql = format!("{setup} ALTER TABLE plain_view ALTER COLUMN v SET STORAGE PLAIN;");
    assert_eq!(
        refusal(&sql),
        (
            "42809".to_owned(),
            "ALTER action ALTER COLUMN ... SET STORAGE cannot be performed on relation \"plain_view\""
                .to_owned()
        )
    );
}
