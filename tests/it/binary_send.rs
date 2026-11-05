//! Tests that a column type answers what PostgreSQL's `pgoutput` does with its
//! values under the `binary` option. Every expected answer was measured on
//! PostgreSQL 18.6 with `pg_logical_slot_peek_binary_changes` and `binary`
//! set to `true`.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

/// The declared type of column `name` of table `t`.
fn data_type<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::DataType {
    database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t is declared")
        .column(name, database, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("column is declared")
        .data_type(database)
}

/// What a binary send of column `name` of table `t` does, the type it fails
/// on or cannot tell about named.
fn sent(database: &ParserDB, name: &str) -> String {
    match data_type(database, name).binary_send(database) {
        BinarySend::Binary => "binary".to_owned(),
        BinarySend::Text => "text".to_owned(),
        BinarySend::Fails(failing) => format!("fails on {}", failing.name(database)),
        BinarySend::Unknown(unknown) => format!("unknown {}", unknown.name(database)),
    }
}

/// Asserts the outcome of each column of `t`, declared with the type each
/// case names, after `preamble`.
fn assert_sent(preamble: &str, cases: &[(&str, &str)]) {
    let columns: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(index, (declared, _))| format!("c{index} {declared}"))
        .collect();
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{preamble} CREATE TABLE t ({});",
        columns.join(", ")
    ))
    .expect("schema builds");
    for (index, (declared, expected)) in cases.iter().enumerate() {
        assert_eq!(sent(&database, &format!("c{index}")), *expected, "{declared}");
    }
}

const ACL_DOMAINS: &str = "CREATE DOMAIN d_acl AS aclitem;
     CREATE DOMAIN d_aclarr AS aclitem[];
     CREATE DOMAIN d_d_aclarr AS d_aclarr;";

/// A type without a send function travels as text at the top of a column,
/// and fails a stream once an array reaches it, under a domain or not.
#[test]
fn a_catalog_type_without_send_fails_only_when_reached() {
    assert_sent(
        ACL_DOMAINS,
        &[
            ("aclitem", "text"),
            ("gtsvector", "text"),
            ("d_acl", "text"),
            ("aclitem[]", "fails on aclitem"),
            ("aclitem[][]", "fails on aclitem"),
            ("d_aclarr", "fails on aclitem"),
            ("d_d_aclarr", "fails on aclitem"),
            ("d_acl[]", "fails on d_acl"),
        ],
    );
}

/// Two base types over `int4`'s functions, `nosend` declared without `SEND`
/// and `withsend` with it, and the ranges, composites and domains measured
/// over them.
const USER_TYPES: &str = "
    CREATE DOMAIN d_acl AS aclitem;
    CREATE TYPE nosend;
    CREATE FUNCTION nosend_in(cstring) RETURNS nosend LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
    CREATE FUNCTION nosend_out(nosend) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
    CREATE TYPE nosend (INPUT = nosend_in, OUTPUT = nosend_out, INTERNALLENGTH = 4, PASSEDBYVALUE);
    CREATE TYPE withsend;
    CREATE FUNCTION withsend_in(cstring) RETURNS withsend LANGUAGE internal IMMUTABLE STRICT AS 'int4in';
    CREATE FUNCTION withsend_out(withsend) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'int4out';
    CREATE FUNCTION withsend_send(withsend) RETURNS bytea LANGUAGE internal IMMUTABLE STRICT AS 'int4send';
    CREATE FUNCTION withsend_recv(internal) RETURNS withsend LANGUAGE internal IMMUTABLE STRICT AS 'int4recv';
    CREATE TYPE withsend (INPUT = withsend_in, OUTPUT = withsend_out, SEND = withsend_send,
        RECEIVE = withsend_recv, INTERNALLENGTH = 4, PASSEDBYVALUE);
    CREATE FUNCTION nosend_lt(nosend, nosend) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4lt';
    CREATE FUNCTION nosend_le(nosend, nosend) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4le';
    CREATE FUNCTION nosend_eq(nosend, nosend) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4eq';
    CREATE FUNCTION nosend_ge(nosend, nosend) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4ge';
    CREATE FUNCTION nosend_gt(nosend, nosend) RETURNS bool LANGUAGE internal IMMUTABLE STRICT AS 'int4gt';
    CREATE FUNCTION nosend_cmp(nosend, nosend) RETURNS int LANGUAGE internal IMMUTABLE STRICT AS 'btint4cmp';
    CREATE OPERATOR < (LEFTARG = nosend, RIGHTARG = nosend, FUNCTION = nosend_lt);
    CREATE OPERATOR <= (LEFTARG = nosend, RIGHTARG = nosend, FUNCTION = nosend_le);
    CREATE OPERATOR = (LEFTARG = nosend, RIGHTARG = nosend, FUNCTION = nosend_eq);
    CREATE OPERATOR >= (LEFTARG = nosend, RIGHTARG = nosend, FUNCTION = nosend_ge);
    CREATE OPERATOR > (LEFTARG = nosend, RIGHTARG = nosend, FUNCTION = nosend_gt);
    CREATE OPERATOR CLASS nosend_ops DEFAULT FOR TYPE nosend USING btree AS
        OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >,
        FUNCTION 1 nosend_cmp(nosend, nosend);
    CREATE TYPE nosendrange AS RANGE (SUBTYPE = nosend);
    CREATE TYPE comp_nosend AS (a INT, b nosend);
    CREATE TYPE comp_acl AS (a INT, b aclitem);
    CREATE TYPE comp_ok AS (a INT, b TEXT[], c int4range);
    CREATE TYPE comp_d AS (x d_acl);
    CREATE TYPE comp_outer AS (y INT, x comp_nosend);
    CREATE TYPE comp_two AS (a comp_ok, b nosend[]);
    CREATE DOMAIN d_range AS nosendrange;
    CREATE DOMAIN d_comp AS comp_nosend;
    CREATE TYPE mood AS ENUM ('a');
";

/// A base type answers the `SEND` it declares, and every range, multirange,
/// composite and domain reaching a base type without one fails, naming the
/// first type without a send function its send functions reach.
#[test]
fn user_defined_types_fail_where_postgresql_fails() {
    assert_sent(
        USER_TYPES,
        &[
            ("nosend", "text"),
            ("nosend[]", "fails on nosend"),
            ("withsend", "binary"),
            ("withsend[]", "binary"),
            ("nosendrange", "fails on nosend"),
            ("nosendmultirange", "fails on nosend"),
            ("d_range", "fails on nosend"),
            ("comp_nosend", "fails on nosend"),
            ("comp_acl", "fails on aclitem"),
            ("comp_ok", "binary"),
            ("d_comp", "fails on nosend"),
            ("comp_d", "fails on d_acl"),
            ("comp_d[]", "fails on d_acl"),
            ("comp_outer", "fails on nosend"),
            ("comp_two", "fails on nosend"),
            ("mood", "binary"),
        ],
    );
}

/// A type that cannot be told about leaves the answer unknown unless another
/// type on the way is known to fail. The row type of a table is one this
/// model does not follow.
#[test]
fn a_known_failure_outweighs_an_unknown_type() {
    assert_sent(
        "CREATE TYPE comp_unknown AS (a box2d, b INT);
         CREATE TYPE comp_mixed AS (a box2d, b aclitem);
         CREATE TABLE inner_t (a INT);",
        &[
            ("comp_unknown", "unknown box2d"),
            ("comp_mixed", "fails on aclitem"),
            ("inner_t", "unknown inner_t"),
        ],
    );
}

/// A column declared before a drop or a rename answers for the type it holds
/// afterwards, not for a later type taking the old name.
#[test]
fn the_answer_follows_drops_and_renames() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE comp_acl AS (a INT, b aclitem);
         CREATE TYPE comp_gone AS (a INT, b comp_acl);
         CREATE TYPE comp_kept AS (a aclitem[]);
         CREATE TABLE t (c0 comp_gone, c1 comp_kept);
         DROP TYPE comp_acl CASCADE;
         ALTER TYPE comp_kept RENAME TO comp_renamed;
         CREATE TYPE comp_kept AS (a INT);",
    )
    .expect("schema builds");
    assert_eq!(sent(&database, "c0"), "binary");
    assert_eq!(sent(&database, "c1"), "fails on aclitem");
    assert_eq!(data_type(&database, "c1").name(&database), "comp_renamed");
}

/// Every built-in scalar, array, range and multirange PostgreSQL ships sends
/// in binary.
#[test]
fn catalog_types_with_send_travel_in_binary() {
    assert_sent(
        "",
        &[
            ("INT", "binary"),
            ("TEXT[]", "binary"),
            ("NUMERIC(10,2)[][]", "binary"),
            ("int4range", "binary"),
            ("tstzmultirange", "binary"),
            ("daterange[]", "binary"),
            ("LINE", "binary"),
            ("pg_catalog.int8range", "binary"),
        ],
    );
}

/// A type the schema never declares, such as an extension's, cannot be told,
/// and neither can an array of it.
#[test]
fn an_undeclared_type_is_unknown() {
    assert_sent(
        "CREATE EXTENSION postgis;",
        &[
            ("box2d", "unknown box2d"),
            ("box2d[]", "unknown box2d"),
            ("public.aclitem", "unknown public.aclitem"),
        ],
    );
}

/// The subtype of every built-in range and multirange is the one `pg_range`
/// records.
#[test]
fn catalog_ranges_answer_their_subtype() {
    let cases = [
        ("int4range", Some("INT4")),
        ("int8multirange", Some("INT8")),
        ("numrange", Some("NUMERIC")),
        ("tsrange", Some("TIMESTAMP")),
        ("tstzmultirange", Some("TIMESTAMPTZ")),
        ("pg_catalog.daterange", Some("DATE")),
        ("int4range[]", None),
        ("INT", None),
    ];
    let columns: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(index, (declared, _))| format!("c{index} {declared}"))
        .collect();
    let database =
        ParserDB::parse::<PostgreSqlDialect>(&format!("CREATE TABLE t ({});", columns.join(", ")))
            .expect("schema builds");
    for (index, (declared, expected)) in cases.into_iter().enumerate() {
        let subtype = data_type(&database, &format!("c{index}")).range_subtype(&database);
        assert_eq!(
            subtype.map(|subtype| subtype.name(&database)).as_deref(),
            expected,
            "{declared}"
        );
    }
}

/// Only PostgreSQL has binary send functions, so another engine's schema
/// cannot tell.
#[test]
fn another_engine_cannot_tell() {
    let database = ParserDB::parse::<MySqlDialect>("CREATE TABLE t (id INT, flags JSON);")
        .expect("schema builds");
    for name in ["id", "flags"] {
        assert_eq!(data_type(&database, name).send_function(&database), None, "{name}");
        assert_eq!(
            sent(&database, name),
            format!("unknown {}", data_type(&database, name).name(&database))
        );
    }
}
