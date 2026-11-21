//! Index keys require a default operator class for their access method.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const SETUP: &str = "CREATE SCHEMA k10; SET search_path = k10, pg_catalog; CREATE TYPE mood AS ENUM ('ok'); CREATE TYPE pair AS (value integer); CREATE DOMAIN bad_domain AS json; CREATE DOMAIN good_domain AS text; CREATE TYPE custom_range AS RANGE (subtype=integer); CREATE DOMAIN nested_bad AS bad_domain;";

const TYPES: &[(&str, &str, &str)] = &[
    ("smallint", "smallint", "brin hash"),
    ("integer", "integer", "brin hash"),
    ("bigint", "bigint", "brin hash"),
    ("boolean", "boolean", "hash"),
    ("real", "real", "brin hash"),
    ("double precision", "double precision", "brin hash"),
    ("numeric", "numeric", "brin hash"),
    ("money", "money", ""),
    ("text", "text", "spgist brin hash"),
    ("varchar(12)", "character varying", "spgist brin hash"),
    ("character(12)", "character", "brin hash"),
    ("\"char\"", "\"char\"", "brin hash"),
    ("name", "name", "brin hash"),
    ("bytea", "bytea", "brin hash"),
    ("bit(3)", "bit", "brin"),
    ("bit varying(3)", "bit varying", "brin"),
    ("date", "date", "brin hash"),
    ("time", "time without time zone", "brin hash"),
    ("time with time zone", "time with time zone", "brin hash"),
    ("timestamp", "timestamp without time zone", "brin hash"),
    ("timestamp with time zone", "timestamp with time zone", "brin hash"),
    ("interval", "interval", "brin hash"),
    ("uuid", "uuid", "brin hash"),
    ("inet", "inet", "spgist brin hash"),
    ("cidr", "cidr", "spgist brin hash"),
    ("macaddr", "macaddr", "brin hash"),
    ("macaddr8", "macaddr8", "brin hash"),
    ("json", "json", ""),
    ("jsonb", "jsonb", "gin hash"),
    ("jsonpath", "jsonpath", ""),
    ("xml", "xml", ""),
    ("point", "point", "gist spgist"),
    ("line", "line", ""),
    ("lseg", "lseg", ""),
    ("box", "box", "gist spgist brin"),
    ("path", "path", ""),
    ("polygon", "polygon", "gist spgist"),
    ("circle", "circle", "gist"),
    ("tsvector", "tsvector", "gist gin"),
    ("tsquery", "tsquery", "gist"),
    ("integer[]", "integer[]", "gin hash"),
    ("json[]", "json[]", "hash"),
    ("int4range", "int4range", "gist spgist brin hash"),
    ("int4multirange", "int4multirange", "gist hash"),
    ("mood", "mood", "hash"),
    ("pair", "pair", "hash"),
    ("bad_domain", "bad_domain", ""),
    ("good_domain", "good_domain", "spgist brin hash"),
    ("oid", "oid", "brin hash"),
    ("regclass", "regclass", "brin hash"),
    ("regtype", "regtype", "brin hash"),
    ("regproc", "regproc", "brin hash"),
    ("regprocedure", "regprocedure", "brin hash"),
    ("regoper", "regoper", "brin hash"),
    ("regoperator", "regoperator", "brin hash"),
    ("regnamespace", "regnamespace", "brin hash"),
    ("regrole", "regrole", "brin hash"),
    ("regconfig", "regconfig", "brin hash"),
    ("regdictionary", "regdictionary", "brin hash"),
    ("tid", "tid", "brin hash"),
    ("xid", "xid", "hash"),
    ("xid8", "xid8", "hash"),
    ("cid", "cid", "hash"),
    ("oidvector", "oidvector", "gin hash"),
    ("int2vector", "int2vector", "gin hash"),
    ("pg_lsn", "pg_lsn", "brin hash"),
    ("aclitem", "aclitem", "hash"),
    ("pg_snapshot", "pg_snapshot", ""),
    ("txid_snapshot", "txid_snapshot", ""),
    ("refcursor", "refcursor", ""),
    ("regcollation", "regcollation", "brin hash"),
    ("gtsvector", "gtsvector", ""),
    ("pg_brin_bloom_summary", "pg_brin_bloom_summary", ""),
    ("pg_brin_minmax_multi_summary", "pg_brin_minmax_multi_summary", ""),
    ("pg_dependencies", "pg_dependencies", "brin hash"),
    ("pg_mcv_list", "pg_mcv_list", "brin hash"),
    ("pg_ndistinct", "pg_ndistinct", "brin hash"),
    ("pg_node_tree", "pg_node_tree", "spgist brin hash"),
    ("daterange", "daterange", "gist spgist brin hash"),
    ("datemultirange", "datemultirange", "gist hash"),
    ("numrange", "numrange", "gist spgist brin hash"),
    ("nummultirange", "nummultirange", "gist hash"),
    ("int8range", "int8range", "gist spgist brin hash"),
    ("int8multirange", "int8multirange", "gist hash"),
    ("tsrange", "tsrange", "gist spgist brin hash"),
    ("tsmultirange", "tsmultirange", "gist hash"),
    ("tstzrange", "tstzrange", "gist spgist brin hash"),
    ("tstzmultirange", "tstzmultirange", "gist hash"),
    ("custom_range", "custom_range", "gist spgist brin hash"),
    ("custom_multirange", "custom_multirange", "gist hash"),
    ("nested_bad", "nested_bad", ""),
    ("bad_domain[]", "bad_domain[]", "hash"),
    ("mood[]", "mood[]", "gin hash"),
    ("pair[]", "pair[]", "gin hash"),
];

#[test]
fn measured_type_matrix() {
    let mut failures = Vec::new();
    for &(spelling, display, accepted_methods) in TYPES {
        for method in ["gist", "gin", "spgist", "brin", "hash"] {
            if matches!(spelling, "json[]" | "bad_domain[]") && method == "gin" {
                continue;
            }
            let statement = format!(
                "{SETUP} CREATE TABLE probe (value {spelling}); CREATE INDEX probe_index ON probe USING {method}(value);"
            );
            let result = ParserDB::parse::<PostgreSqlDialect>(&statement);
            let expected = format!(
                "data type {display} has no default operator class for access method \"{method}\""
            );
            if accepted_methods.split_whitespace().any(|accepted| accepted == method) {
                if let Err(error) = result {
                    failures.push(format!("{spelling} {method} expected acceptance, got {error}"));
                }
            } else if !matches!(&result, Err(Error::MissingDefaultOperatorClass { type_name, access_method }) if type_name == display && *access_method == method)
                || result.as_ref().err().map(ToString::to_string).as_deref()
                    != Some(expected.as_str())
            {
                failures.push(format!(
                    "{spelling} {method} expected {expected}, got {:?}",
                    result.as_ref().map(|_| ())
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const FORMS: &[(&str, &str, &str)] = &[
    (
        "exclude_gist_bad",
        "CREATE TABLE probe (value integer, EXCLUDE USING gist (value WITH =))",
        "data type integer has no default operator class for access method \"gist\"",
    ),
    (
        "exclude_gist_good",
        "CREATE TABLE probe (value int4range, EXCLUDE USING gist (value WITH &&))",
        "accepted",
    ),
    (
        "exclude_spgist_bad",
        "CREATE TABLE probe (value integer, EXCLUDE USING spgist (value WITH =))",
        "data type integer has no default operator class for access method \"spgist\"",
    ),
    (
        "exclude_spgist_good",
        "CREATE TABLE probe (value int4range, EXCLUDE USING spgist (value WITH &&))",
        "accepted",
    ),
    (
        "exclude_hash_bad",
        "CREATE TABLE probe (value json, EXCLUDE USING hash (value WITH =))",
        "data type json has no default operator class for access method \"hash\"",
    ),
    (
        "exclude_hash_good",
        "CREATE TABLE probe (value integer, EXCLUDE USING hash (value WITH =))",
        "accepted",
    ),
    (
        "domain_qualified",
        "CREATE TABLE probe (value k10.bad_domain); SET LOCAL search_path=pg_catalog; CREATE INDEX probe_index ON k10.probe USING gist(value)",
        "data type k10.bad_domain has no default operator class for access method \"gist\"",
    ),
    (
        "alter_gist_bad",
        "CREATE TABLE probe (value point); CREATE INDEX probe_index ON probe USING gist(value); ALTER TABLE probe ALTER COLUMN value TYPE integer USING 1",
        "data type integer has no default operator class for access method \"gist\"",
    ),
    (
        "alter_gist_good",
        "CREATE TABLE probe (value point); CREATE INDEX probe_index ON probe USING gist(value); ALTER TABLE probe ALTER COLUMN value TYPE box USING box(value)",
        "accepted",
    ),
    (
        "alter_gin_bad",
        "CREATE TABLE probe (value jsonb); CREATE INDEX probe_index ON probe USING gin(value); ALTER TABLE probe ALTER COLUMN value TYPE json USING value::json",
        "data type json has no default operator class for access method \"gin\"",
    ),
    (
        "alter_gin_good",
        "CREATE TABLE probe (value jsonb); CREATE INDEX probe_index ON probe USING gin(value); ALTER TABLE probe ALTER COLUMN value TYPE integer[] USING ARRAY[1]",
        "accepted",
    ),
    (
        "alter_brin_bad",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING brin(value); ALTER TABLE probe ALTER COLUMN value TYPE json USING '{}'::json",
        "data type json has no default operator class for access method \"brin\"",
    ),
    (
        "alter_brin_good",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING brin(value); ALTER TABLE probe ALTER COLUMN value TYPE bigint",
        "accepted",
    ),
    (
        "alter_hash_bad",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING hash(value); ALTER TABLE probe ALTER COLUMN value TYPE json USING '{}'::json",
        "data type json has no default operator class for access method \"hash\"",
    ),
    (
        "alter_hash_good",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING hash(value); ALTER TABLE probe ALTER COLUMN value TYPE bigint",
        "accepted",
    ),
    (
        "parent_bad",
        "CREATE TABLE probe (part integer, value integer) PARTITION BY RANGE(part); CREATE INDEX probe_index ON probe USING gist(value)",
        "data type integer has no default operator class for access method \"gist\"",
    ),
    (
        "parent_good",
        "CREATE TABLE probe (part integer, value point) PARTITION BY RANGE(part); CREATE INDEX probe_index ON probe USING gist(value)",
        "accepted",
    ),
    (
        "partition_bad",
        "CREATE TABLE probe (part integer, value integer) PARTITION BY RANGE(part); CREATE TABLE child PARTITION OF probe FOR VALUES FROM (0) TO (10); CREATE INDEX probe_index ON child USING gist(value)",
        "data type integer has no default operator class for access method \"gist\"",
    ),
    (
        "partition_good",
        "CREATE TABLE probe (part integer, value point) PARTITION BY RANGE(part); CREATE TABLE child PARTITION OF probe FOR VALUES FROM (0) TO (10); CREATE INDEX probe_index ON child USING gist(value)",
        "accepted",
    ),
    (
        "materialized_unresolved",
        "CREATE MATERIALIZED VIEW materialized AS SELECT 1 AS value; CREATE INDEX probe_index ON materialized USING gist(value)",
        "accepted",
    ),
    (
        "materialized_cast_bad",
        "CREATE MATERIALIZED VIEW materialized AS SELECT 1 AS value; CREATE INDEX probe_index ON materialized USING gist((value::integer))",
        "data type integer has no default operator class for access method \"gist\"",
    ),
    (
        "materialized_good",
        "CREATE MATERIALIZED VIEW materialized AS SELECT point(1,2) AS value; CREATE INDEX probe_index ON materialized USING gist(value)",
        "accepted",
    ),
];

#[test]
fn measured_statement_forms() {
    let mut failures = Vec::new();
    for &(label, statement, expected) in FORMS {
        let result = ParserDB::parse::<PostgreSqlDialect>(&format!("{SETUP} {statement}"));
        let actual =
            result.as_ref().err().map_or_else(|| "accepted".to_owned(), ToString::to_string);
        if actual != expected {
            failures.push(format!("{label} expected {expected}, got {actual}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn explicit_classes_and_unresolved_types() -> Result<(), Error> {
    for statement in [
        "CREATE TABLE probe (value inet); CREATE INDEX probe_index ON probe USING gist(value inet_ops)",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING gist(value extension_ops)",
        "CREATE TABLE probe (value integer, EXCLUDE USING gist (value extension_ops WITH =))",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING gist((abs(value)))",
        "CREATE TABLE probe (value extension_type); CREATE INDEX probe_index ON probe USING gist(value)",
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING extension_method(value)",
        "CREATE TABLE probe (value json[]); CREATE INDEX probe_index ON probe USING gin(value)",
        "CREATE DOMAIN bad AS json; CREATE TABLE probe (value bad[]); CREATE INDEX probe_index ON probe USING gin(value)",
        "CREATE TABLE probe (value point, included json); CREATE INDEX probe_index ON probe USING gist(value) INCLUDE (included)",
    ] {
        ParserDB::parse::<PostgreSqlDialect>(statement)?;
    }
    ParserDB::parse::<sqlparser::dialect::GenericDialect>(
        "CREATE TABLE probe (value integer); CREATE INDEX probe_index ON probe USING gist(value)",
    )?;
    Ok(())
}

#[test]
fn method_and_type_identifier_case() {
    for method in ["gist", "GIST", "\"gist\""] {
        for type_name in ["integer", "INT4", "pg_catalog.int4", "\"int4\""] {
            let statement = format!(
                "CREATE TABLE probe (value {type_name}); CREATE INDEX probe_index ON probe USING {method}(value)"
            );
            assert!(
                matches!(
                    ParserDB::parse::<PostgreSqlDialect>(&statement),
                    Err(Error::MissingDefaultOperatorClass { type_name, access_method: "gist" }) if type_name == "integer"
                ),
                "{statement}"
            );
        }
        let statement =
            format!("CREATE TABLE probe (value integer, EXCLUDE USING {method}(value WITH =))");
        assert!(
            matches!(
                ParserDB::parse::<PostgreSqlDialect>(&statement),
                Err(Error::MissingDefaultOperatorClass { type_name, access_method: "gist" }) if type_name == "integer"
            ),
            "{statement}"
        );
    }
}

#[test]
fn serial_keys_use_integer_classes() {
    for (declaration, reported, method) in [
        ("serial", "integer", "gist"),
        ("smallserial", "smallint", "gin"),
        ("bigserial", "bigint", "spgist"),
    ] {
        let statement = format!(
            "CREATE TABLE probe (value {declaration}); CREATE INDEX probe_index ON probe USING {method}(value)"
        );
        assert!(
            matches!(
                ParserDB::parse::<PostgreSqlDialect>(&statement),
                Err(Error::MissingDefaultOperatorClass { type_name, access_method }) if type_name == reported && access_method == method
            ),
            "{statement}"
        );
    }
}
