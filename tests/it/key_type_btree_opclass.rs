//! Btree keys require a default operator class for their resolved type.
//! Expectations were measured on PostgreSQL 18.7.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const REFUSED_TYPES: &[&str] = &[
    "json",
    "xml",
    "point",
    "line",
    "lseg",
    "box",
    "path",
    "polygon",
    "circle",
    "aclitem",
    "cid",
    "gtsvector",
    "jsonpath",
    "pg_brin_bloom_summary",
    "pg_brin_minmax_multi_summary",
    "pg_snapshot",
    "refcursor",
    "txid_snapshot",
    "xid",
];

/// Parses PostgreSQL schema statements.
fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Requires the missing default operator class error.
fn refused(sql: &str, expected_type: &str) {
    let outcome = parse(sql);
    assert!(
        matches!(&outcome, Err(Error::MissingDefaultBtreeOperatorClass { type_name }) if type_name == expected_type),
        "expected missing default class for {expected_type}, got {outcome:?} for {sql}"
    );
    assert_eq!(
        outcome.err().map(|error| error.to_string()),
        Some(format!(
            "data type {expected_type} has no default operator class for access method \"btree\""
        ))
    );
}

/// Every measured unsupported scalar is refused by each key declaration.
#[test]
fn unsupported_scalar_keys() {
    for spelling in REFUSED_TYPES {
        for declaration in ["PRIMARY KEY", "UNIQUE"] {
            refused(&format!("CREATE TABLE r (id {spelling} {declaration});"), spelling);
        }
        for declaration in [
            "CREATE INDEX ix ON r (id)",
            "CREATE INDEX ix ON r USING btree (id)",
            "CREATE UNIQUE INDEX ix ON r (id)",
        ] {
            refused(&format!("CREATE TABLE r (id {spelling});\n{declaration};"), spelling);
        }
    }
}

/// Keys are checked when constraints or indexed column types change.
#[test]
fn key_creation_and_type_changes() {
    for sql in [
        "CREATE TABLE r (id json, UNIQUE (id));",
        "CREATE TABLE r (id json, PRIMARY KEY (id));",
        "CREATE TABLE r (id json);\nALTER TABLE r ADD UNIQUE (id);",
        "CREATE TABLE r (id json);\nALTER TABLE r ADD PRIMARY KEY (id);",
        "CREATE TABLE r (other int);\nALTER TABLE r ADD COLUMN id json UNIQUE;",
        "CREATE TABLE r (id text UNIQUE);\nALTER TABLE r ALTER COLUMN id TYPE json USING id::json;",
        "CREATE TABLE r (id text);\nCREATE INDEX ix ON r (id);\nALTER TABLE r ALTER COLUMN id TYPE json USING id::json;",
        "CREATE TABLE r (id int, other json, UNIQUE (id, other)) PARTITION BY RANGE (id);",
        "CREATE TABLE r (id json);\nCREATE INDEX ix ON r ((r.id));",
        "CREATE TABLE r (id pg_catalog.json UNIQUE);",
        "CREATE TABLE r (id \"json\" UNIQUE);",
        "CREATE TABLE r (id json, EXCLUDE (id WITH =));",
        "CREATE TABLE r (id text);\nCREATE MATERIALIZED VIEW materialized AS SELECT id FROM r;\nCREATE INDEX ix ON materialized ((id::json));",
        "CREATE TABLE parent (id json);\nCREATE TABLE r () INHERITS (parent);\nCREATE INDEX ix ON r (id);",
        "CREATE TABLE r (id json);\nCREATE INDEX ix ON r ((id));",
        "CREATE TABLE r (id text);\nCREATE INDEX ix ON r ((id::json));",
    ] {
        refused(sql, "json");
    }
}

/// Domains retain their own diagnostic name and use the base type's class.
#[test]
fn domains_without_classes() {
    for spelling in &REFUSED_TYPES[..9] {
        refused(
            &format!("CREATE DOMAIN bad AS {spelling};\nCREATE TABLE r (id bad UNIQUE);"),
            "bad",
        );
    }
    refused(
        "CREATE DOMAIN bad AS json;\nCREATE DOMAIN outer_domain AS bad;\nCREATE TABLE r (id outer_domain PRIMARY KEY);",
        "outer_domain",
    );
    refused(
        "CREATE DOMAIN \"MixedDomain\" AS json;\nCREATE TABLE r (id \"MixedDomain\" PRIMARY KEY);",
        "\"MixedDomain\"",
    );
    refused(
        "CREATE SCHEMA k9;\nCREATE DOMAIN k9.hidden AS json;\nSET search_path = pg_catalog;\nCREATE TABLE k9.r (id k9.hidden UNIQUE);",
        "k9.hidden",
    );
}

/// Arrays and composites retain their generic operator classes.
#[test]
fn supported_key_types() -> Result<(), Error> {
    for spelling in [
        "name",
        "int2vector",
        "oidvector",
        "json[]",
        "xml[]",
        "point[]",
        "line[]",
        "lseg[]",
        "box[]",
        "path[]",
        "polygon[]",
        "circle[]",
        "int[]",
        "bit",
        "bool",
        "bpchar",
        "bytea",
        "\"char\"",
        "cidr",
        "date",
        "datemultirange",
        "daterange",
        "float4",
        "float8",
        "inet",
        "int2",
        "int4",
        "int4multirange",
        "int4range",
        "int8",
        "int8multirange",
        "int8range",
        "interval",
        "jsonb",
        "macaddr",
        "macaddr8",
        "money",
        "numeric",
        "nummultirange",
        "numrange",
        "oid",
        "pg_dependencies",
        "pg_lsn",
        "pg_mcv_list",
        "pg_ndistinct",
        "pg_node_tree",
        "regclass",
        "regcollation",
        "regconfig",
        "regdictionary",
        "regnamespace",
        "regoper",
        "regoperator",
        "regproc",
        "regprocedure",
        "regrole",
        "regtype",
        "text",
        "tid",
        "time",
        "timestamp",
        "timestamptz",
        "timetz",
        "tsmultirange",
        "tsquery",
        "tsrange",
        "tstzmultirange",
        "tstzrange",
        "tsvector",
        "uuid",
        "varbit",
        "varchar",
        "xid8",
    ] {
        let database = parse(&format!("CREATE TABLE r (id {spelling} PRIMARY KEY);"))?;
        let table =
            database.table_by_target(TargetName::new("r", false), IdentifierCase::AsWritten)?;
        assert_eq!(
            table
                .map(|table| table.primary_key_columns(&database).map(Iterator::count))
                .transpose()?,
            Some(1),
            "{spelling}"
        );
    }
    parse("CREATE TYPE composite AS (id json);\nCREATE TABLE r (id composite PRIMARY KEY);")?;
    Ok(())
}

/// Included columns and casts use only the index key's type.
#[test]
fn eligible_index_operations() -> Result<(), Error> {
    for sql in [
        "CREATE TABLE r (id int, other json);\nCREATE INDEX ix ON r (id) INCLUDE (other);",
        "CREATE TABLE r (id json);\nCREATE INDEX ix ON r ((id::text));",
        "CREATE TABLE r (id text);\nCREATE INDEX ix ON r (id text_pattern_ops);",
        "CREATE TABLE r (id text UNIQUE);\nALTER TABLE r RENAME COLUMN id TO other;",
        "CREATE TABLE r (id text UNIQUE);\nALTER TABLE r DROP CONSTRAINT r_id_key;\nALTER TABLE r ALTER COLUMN id TYPE json USING id::json;",
        "CREATE DOMAIN good AS json[];\nCREATE TABLE r (id good PRIMARY KEY);",
        "CREATE DOMAIN good AS text;\nCREATE TABLE r (id good PRIMARY KEY);",
        "CREATE TYPE good AS ENUM ('a');\nCREATE TABLE r (id good PRIMARY KEY);",
        "CREATE TABLE r (id point);\nCREATE INDEX ix ON r USING gist (id);",
        "CREATE TABLE r (id int PRIMARY KEY) PARTITION BY RANGE (id);\nCREATE TABLE child (id int NOT NULL);\nALTER TABLE r ATTACH PARTITION child FOR VALUES FROM (0) TO (10);",
        "CREATE TABLE r (id text);\nCREATE UNIQUE INDEX ix ON r (id);\nALTER TABLE r ADD CONSTRAINT uk UNIQUE USING INDEX ix;",
        "CREATE TABLE r (id text);\nCREATE INDEX ix ON r (id);\nALTER INDEX ix RENAME TO iy;\nDROP INDEX iy;\nALTER TABLE r ALTER COLUMN id TYPE json USING id::json;",
        "CREATE TABLE r (id int PRIMARY KEY) PARTITION BY RANGE (id);\nCREATE TABLE child PARTITION OF r FOR VALUES FROM (0) TO (10);\nALTER TABLE r DETACH PARTITION child;",
    ] {
        parse(sql)?;
    }
    Ok(())
}
