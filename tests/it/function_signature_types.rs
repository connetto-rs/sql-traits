//! Tests that the types a function's signature names are bound when the
//! function is created, follow every rename of the type, and hold the type in
//! place the way PostgreSQL's dependency on them does.
//!
//! The expected outcomes are the ones PostgreSQL 18 gives for the same
//! statements.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::{ast::ArgMode, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The types, for every schema the tests create, and functions in schema `o`
/// naming each of them.
const SCHEMA: &str = "CREATE SCHEMA app; CREATE SCHEMA o;
     CREATE TYPE app.e AS ENUM ('a');
     CREATE DOMAIN app.d AS int4;
     CREATE TYPE app.r AS RANGE (subtype = int4);
     CREATE TABLE app.t (x int4);
     CREATE VIEW app.v AS SELECT 1 AS a;
     CREATE MATERIALIZED VIEW app.m AS SELECT 1 AS a;
     CREATE FUNCTION o.args(x app.e, y app.d[], OUT z app.r_multirange) LANGUAGE sql
         AS 'SELECT NULL::app.r_multirange';
     CREATE FUNCTION o.rows(x app.t, y app.v, z app.m) RETURNS SETOF app.e LANGUAGE sql
         AS 'SELECT NULL::app.e';
     CREATE FUNCTION o.columns() RETURNS TABLE (c app.d) LANGUAGE sql AS 'SELECT 1';";

/// Every function of schema `o` as `name(modes and types) -> result type`,
/// sorted.
fn signatures(database: &ParserDB) -> Vec<String> {
    let mut signatures: Vec<String> = database
        .functions()
        .filter(|function| function.target_name().schema() == Some("o"))
        .map(|function| {
            let arguments: Vec<String> = function
                .argument_modes()
                .zip(function.argument_type_names(database))
                .map(|(mode, data_type)| {
                    match mode {
                        ArgMode::In => data_type.into_owned(),
                        mode => format!("{mode} {data_type}"),
                    }
                })
                .collect();
            format!(
                "{}({}) -> {}",
                function.name(),
                arguments.join(", "),
                function.result_type_name(database).unwrap_or_default()
            )
        })
        .collect();
    signatures.sort();
    signatures
}

/// [`signatures`] with the enum, domain, multirange, table, view and
/// materialized view named as in `names`, in the schema it ends with.
fn expected(names: [&str; 7]) -> Vec<String> {
    let [enumeration, domain, multirange, table, view, materialized, schema] = names;
    vec![
        format!(
            "args({schema}.{enumeration}, {schema}.{domain}[], OUT {schema}.{multirange}) -> \
             {schema}.{multirange}"
        ),
        format!("columns() -> {schema}.{domain}"),
        format!(
            "rows({schema}.{table}, {schema}.{view}, {schema}.{materialized}) -> \
             {schema}.{enumeration}"
        ),
    ]
}

const NAMES: [&str; 7] = ["e", "d", "r_multirange", "t", "v", "m", "app"];

#[test]
fn a_signature_names_the_types_as_written_when_qualified() {
    let database = parse(SCHEMA).expect("the schema applies");
    assert_eq!(signatures(&database), expected(NAMES));
}

/// A bare name is bound to the schema of the type the path reaches, so a
/// later path does not change what it names.
#[test]
fn a_bare_name_binds_to_the_type_the_path_reaches() {
    let database = parse(
        "CREATE SCHEMA app; CREATE SCHEMA o; SET search_path = app;
         CREATE TYPE e AS ENUM ('a'); CREATE DOMAIN d AS int4; CREATE TABLE t (x int4);
         CREATE FUNCTION o.rows(x e, y d[], z t) RETURNS SETOF e LANGUAGE sql
             AS 'SELECT NULL::app.e';
         SET search_path = public;",
    )
    .expect("the schema applies");
    assert_eq!(signatures(&database), ["rows(app.e, app.d[], app.t) -> app.e"]);

    parse(
        "CREATE SCHEMA app; SET search_path = app, public;
         CREATE TYPE e AS ENUM ('a');
         CREATE FUNCTION f(x e) RETURNS int4 LANGUAGE sql AS 'SELECT 1';
         DROP FUNCTION f(e);",
    )
    .expect("a bare name in the drop reaches the same type");
}

#[test]
fn renaming_a_type_renames_it_in_every_signature() {
    let database = parse(&format!(
        "{SCHEMA}
         ALTER TYPE app.e RENAME TO e2;
         ALTER TYPE app.d RENAME TO d2;
         ALTER TYPE app.r_multirange RENAME TO rm2;
         ALTER TABLE app.t RENAME TO t2;
         ALTER VIEW app.v RENAME TO v2;
         ALTER MATERIALIZED VIEW app.m RENAME TO m2;"
    ))
    .expect("the renames apply");
    assert_eq!(signatures(&database), expected(["e2", "d2", "rm2", "t2", "v2", "m2", "app"]));
}

#[test]
fn renaming_a_schema_renames_it_in_every_signature() {
    let database =
        parse(&format!("{SCHEMA} ALTER SCHEMA app RENAME TO moved;")).expect("the rename applies");
    let mut names = NAMES;
    names[6] = "moved";
    assert_eq!(signatures(&database), expected(names));
}

#[test]
fn a_type_a_signature_names_is_dropped_only_with_cascade() {
    for (drop, refusal) in [
        ("DROP TYPE app.e;", "e"),
        ("DROP DOMAIN app.d;", "d"),
        ("DROP TYPE app.r;", "r"),
        ("DROP TABLE app.t;", "t"),
        ("DROP VIEW app.v;", "v"),
        ("DROP MATERIALIZED VIEW app.m;", "m"),
    ] {
        let result = parse(&format!("{SCHEMA} {drop}"));
        let refused = match &result {
            Err(
                Error::TypeHasDependents { type_name, dependent_kind, .. }
                | Error::DomainHasDependents { domain_name: type_name, dependent_kind, .. }
                | Error::RelationHasDependents { object_name: type_name, dependent_kind, .. },
            ) => *dependent_kind == ObjectKind::Function && type_name.ends_with(refusal),
            _ => false,
        };
        assert!(refused, "{drop}: {result:?}");
    }
}

#[test]
fn a_cascading_drop_takes_the_functions_naming_the_type() {
    for (drop, kept) in [
        ("DROP TYPE app.e CASCADE;", vec!["columns"]),
        ("DROP DOMAIN app.d CASCADE;", vec!["rows"]),
        ("DROP TYPE app.r CASCADE;", vec!["columns", "rows"]),
        ("DROP TABLE app.t CASCADE;", vec!["args", "columns"]),
        ("DROP VIEW app.v CASCADE;", vec!["args", "columns"]),
        ("DROP MATERIALIZED VIEW app.m CASCADE;", vec!["args", "columns"]),
        ("DROP SCHEMA app CASCADE;", vec![]),
    ] {
        let database = parse(&format!("{SCHEMA} {drop}")).expect("the drop applies");
        let mut names: Vec<&str> = database
            .functions()
            .filter(|function| function.target_name().schema() == Some("o"))
            .map(FunctionLike::name)
            .collect();
        names.sort_unstable();
        assert_eq!(names, kept, "{drop}");
    }
}

/// A domain over a dropped type goes with it, and so do the functions naming
/// the domain.
#[test]
fn a_cascade_reaches_the_functions_naming_what_it_takes() {
    let database = parse(
        "CREATE TYPE e AS ENUM ('a'); CREATE DOMAIN de AS e;
         CREATE FUNCTION f(x de) RETURNS int4 LANGUAGE sql AS 'SELECT 1';
         DROP TYPE e CASCADE;",
    )
    .expect("the drop applies");
    assert!(!database.functions().any(|function| function.name() == "f"));
}

/// A cascade takes a function naming the type along with what calls it, as
/// PostgreSQL 18 takes `function f(e)` and `constraint t_check`.
#[test]
fn a_cascade_takes_what_calls_a_function_it_takes() {
    parse(
        "CREATE TYPE e AS ENUM ('a');
         CREATE FUNCTION f(x e) RETURNS int4 LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE t (x int4 CHECK (f('a') = 1));
         DROP TYPE e CASCADE;
         CREATE FUNCTION f() RETURNS int4 LANGUAGE sql AS 'SELECT 1';
         ALTER TABLE t ADD CONSTRAINT t_check CHECK (x > 0);",
    )
    .expect("the cascade takes f and the check calling it");
}

/// A range's own constructors name the range and go with it, holding nothing
/// in place.
#[test]
fn a_range_drop_is_not_held_by_its_constructors() {
    parse("CREATE TYPE span AS RANGE (subtype = int4); DROP TYPE span;")
        .expect("the constructors go with the range");
}
