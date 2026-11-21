//! PostgreSQL virtual generated column type restrictions.
use sql_traits::{errors::Error, prelude::ParserDB};
use sqlparser::dialect::PostgreSqlDialect;

#[test]
fn virtual_declared_types() {
    for (definition, data_type, expression, reason) in [
        ("CREATE DOMAIN amount AS int;", "amount", "a", "domain"),
        ("CREATE DOMAIN amount AS int;", "amount[]", "ARRAY[a]", "domain"),
        ("CREATE TYPE mood AS ENUM ('x');", "mood", "'x'::mood", "user-defined"),
        ("CREATE TYPE mood AS ENUM ('x');", "mood[]", "ARRAY['x'::mood]", "user-defined"),
        ("CREATE TYPE pair AS (value int);", "pair", "ROW(a)::pair", "user-defined"),
    ] {
        for mode in ["", "VIRTUAL"] {
            let sql = format!(
                "{definition} CREATE TABLE t(a int, v {data_type} GENERATED ALWAYS AS ({expression}) {mode});"
            );
            let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
            assert!(
                matches!((&result, reason),
                    (Err(Error::VirtualGeneratedDomainType { column_name }), "domain")
                    | (Err(Error::VirtualGeneratedUserDefinedType { column_name }), "user-defined")
                    if column_name == "v"
                ),
                "{sql}: {result:?}"
            );
            assert_eq!(
                result.err().map(|error| error.to_string()),
                Some(format!("virtual generated column \"v\" cannot have a {reason} type"))
            );
        }
    }
}

#[test]
fn altering_virtual_type() {
    for (definition, data_type, reason) in [
        ("CREATE DOMAIN amount AS int;", "amount", "domain"),
        ("CREATE TYPE mood AS ENUM ('x');", "mood", "user-defined"),
        ("CREATE TYPE mood AS ENUM ('x');", "mood[]", "user-defined"),
        ("CREATE TYPE pair AS (value int);", "pair", "user-defined"),
    ] {
        let sql = format!(
            "{definition} CREATE TABLE t(a int, v int GENERATED ALWAYS AS (a)); ALTER TABLE t ALTER COLUMN v TYPE {data_type};"
        );
        let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
        assert!(
            matches!((&result, reason),
                (Err(Error::VirtualGeneratedDomainType { column_name }), "domain")
                | (Err(Error::VirtualGeneratedUserDefinedType { column_name }), "user-defined")
                if column_name == "v"
            ),
            "{sql}: {result:?}"
        );
        assert_eq!(
            result.err().map(|error| error.to_string()),
            Some(format!("virtual generated column \"v\" cannot have a {reason} type"))
        );
    }
}

#[test]
fn generated_default_and_using_changes() {
    for mode in ["VIRTUAL", "STORED"] {
        for (operation, expected) in [
            ("SET DEFAULT 4", "column \"v\" of relation \"t\" is a generated column"),
            (
                "TYPE bigint USING v::bigint",
                "cannot specify USING when altering type of generated column",
            ),
        ] {
            let sql = format!(
                "CREATE TABLE t(a int, v int GENERATED ALWAYS AS (a) {mode}); ALTER TABLE t ALTER COLUMN v {operation};"
            );
            let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
            assert!(
                matches!(
                    (&result, operation),
                    (Err(Error::GeneratedColumnDefault { .. }), "SET DEFAULT 4")
                        | (Err(Error::GeneratedColumnTypeUsing), "TYPE bigint USING v::bigint")
                ),
                "{sql}: {result:?}"
            );
            assert_eq!(result.err().map(|error| error.to_string()).as_deref(), Some(expected));
        }
    }
}

#[test]
fn builtin_virtual_types_are_accepted() {
    for sql in [
        "CREATE TABLE t(a int, v int[] GENERATED ALWAYS AS (ARRAY[a]));",
        "CREATE TABLE t(a text, v text GENERATED ALWAYS AS (a || 'x') COLLATE \"C\");",
        "CREATE TABLE t(a int, v int GENERATED ALWAYS AS (a)); ALTER TABLE t ALTER COLUMN v TYPE bigint;",
        "CREATE TABLE t(a int); ALTER TABLE t ADD COLUMN v int GENERATED ALWAYS AS (a);",
        "CREATE TABLE t(a int, v int GENERATED ALWAYS AS (a) CHECK (v > 0) NOT NULL);",
    ] {
        assert!(ParserDB::parse::<PostgreSqlDialect>(sql).is_ok(), "{sql}");
    }
}

#[test]
fn declared_type_refusal_order() {
    for (options, expected) in [
        (
            "GENERATED ALWAYS AS (random())",
            "virtual generated column \"v\" cannot have a domain type",
        ),
        (
            "GENERATED ALWAYS AS (a) DEFAULT 4",
            "both default and generation expression specified for column \"v\" of table \"t\"",
        ),
    ] {
        let sql =
            format!("CREATE DOMAIN amount AS int; CREATE TABLE t(a int, v amount {options});");
        let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
        assert_eq!(result.err().map(|error| error.to_string()).as_deref(), Some(expected));
    }
}
