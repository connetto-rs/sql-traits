//! Tests that `CREATE DOMAIN` is recorded with the type it is defined over,
//! that a column typed with a domain reaches that type through any chain of
//! domains and through arrays, and that domains are dropped, refused and
//! moved as PostgreSQL does.
//!
//! The expected outcomes are the ones PostgreSQL 17 and 18 give for the same
//! statements.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
    utils::scalar_family::ScalarFamily,
};
use sqlparser::{ast::DataType, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The type `column` of `table` is declared with.
fn column_type<'db>(database: &'db ParserDB, table: &str, column: &str) -> &'db DataType {
    database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists")
        .column(column, database, IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the column exists")
        .data_type(database)
}

/// The name of the type `column` of `table` reaches through its domains, or
/// `None` when it names no domain.
fn base_name(database: &ParserDB, table: &str, column: &str) -> Option<String> {
    column_type(database, table, column)
        .base_type(database)
        .map(|base| base.name(database).into_owned())
}

/// Every column of `table`, by name.
fn columns(database: &ParserDB, table: &str) -> Vec<String> {
    columns_of(database, TargetName::new(table, false))
}

/// Every column of the table `target` names, by name.
fn columns_of(database: &ParserDB, target: TargetName<'_>) -> Vec<String> {
    database
        .table_by_target(target, IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists")
        .columns(database)
        .expect("the columns resolve")
        .map(|column| column.column_name().to_owned())
        .collect()
}

/// A domain as `schema.name AS type`, with `-` for the default schema, read
/// the way generic consumer code reads it.
fn describe(domain: impl DomainLike<DB = ParserDB>, database: &ParserDB) -> String {
    format!(
        "{}.{} AS {}",
        domain.stored_schema().as_deref().unwrap_or("-"),
        domain.stored_name(),
        domain.data_type(database).name(database)
    )
}

/// Every domain as [`describe`] spells it, sorted.
fn domains(database: &ParserDB) -> Vec<String> {
    let mut listed: Vec<String> =
        database.domains().map(|domain| describe(domain, database)).collect();
    listed.sort();
    listed
}

const REQUEST: &str = "CREATE DOMAIN span AS interval;
     CREATE DOMAIN span2 AS span;
     CREATE DOMAIN spans AS interval[];
     CREATE TABLE t (a span PRIMARY KEY, b interval[] UNIQUE, h span2, s spans, e span[], n INT);";

#[test]
fn a_domain_reaches_its_base_type() {
    let database = parse(REQUEST).expect("the schema is accepted");
    let base = column_type(&database, "t", "a").base_type(&database).expect("a names a domain");
    assert_eq!(base.name(&database), "INTERVAL");
    assert_eq!(base.scalar_family(&database), Some(ScalarFamily::Interval));
}

#[test]
fn a_domain_over_a_domain_reaches_the_innermost_base_type() {
    let database = parse(REQUEST).expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "h").as_deref(), Some("INTERVAL"));
}

#[test]
fn a_domain_over_an_array_reaches_the_array() {
    let database = parse(REQUEST).expect("the schema is accepted");
    let base = column_type(&database, "t", "s").base_type(&database).expect("s names a domain");
    assert_eq!(base.name(&database), "INTERVAL[]");
    let element = base.array_element(&database).expect("the base type is an array");
    assert_eq!(element.scalar_family(&database), Some(ScalarFamily::Interval));
}

#[test]
fn an_array_of_a_domain_holds_the_domain_as_its_element() {
    let database = parse(REQUEST).expect("the schema is accepted");
    let element = column_type(&database, "t", "e").array_element(&database).expect("e is an array");
    assert_eq!(element.name(&database), "span");
    let base = element.base_type(&database).expect("the element names a domain");
    assert_eq!(base.name(&database), "INTERVAL");
}

#[test]
fn a_type_naming_no_domain_has_no_base_type() {
    let database = parse(REQUEST).expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "b"), None);
    assert_eq!(base_name(&database, "t", "n"), None);
    let undeclared = parse("CREATE TABLE u (x mood);").expect("an unknown type is kept");
    assert_eq!(base_name(&undeclared, "u", "x"), None);
}

#[test]
fn the_domain_itself_answers_no_scalar_family() {
    let database = parse(REQUEST).expect("the schema is accepted");
    assert_eq!(column_type(&database, "t", "a").scalar_family(&database), None);
}

#[test]
fn every_fact_of_the_base_type_is_reached() {
    let database = parse(
        "CREATE DOMAIN money2 AS NUMERIC(10, 2) CHECK (VALUE >= 0);
         CREATE TABLE p (price money2);",
    )
    .expect("the schema is accepted");
    let base =
        column_type(&database, "p", "price").base_type(&database).expect("price names a domain");
    assert_eq!(base.numeric_scale(&database), Some(2));
}

#[test]
fn domains_are_listed_with_the_type_written_for_them() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE DOMAIN span AS interval;
         CREATE DOMAIN s.span2 AS span;",
    )
    .expect("the schema is accepted");
    assert_eq!(domains(&database), ["-.span AS INTERVAL", "s.span2 AS span"]);
}

#[test]
fn a_quoted_domain_name_keeps_its_case() {
    let database = parse(
        r#"CREATE DOMAIN "Span" AS interval;
           CREATE DOMAIN span AS real;
           CREATE TABLE t (quoted "Span", folded SPAN);"#,
    )
    .expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "quoted").as_deref(), Some("INTERVAL"));
    assert_eq!(base_name(&database, "t", "folded").as_deref(), Some("REAL"));
}

#[test]
fn a_qualified_type_reaches_the_domain_of_that_schema() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE DOMAIN span AS real;
         CREATE DOMAIN s.span AS interval;
         CREATE TABLE t (here span, there s.span, explicit public.span);",
    )
    .expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "here").as_deref(), Some("REAL"));
    assert_eq!(base_name(&database, "t", "there").as_deref(), Some("INTERVAL"));
    assert_eq!(base_name(&database, "t", "explicit").as_deref(), Some("REAL"));
}

#[test]
fn a_column_keeps_the_domain_the_search_path_reached_when_it_was_declared() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE DOMAIN span AS real;
         CREATE DOMAIN s.span AS interval;
         SET search_path = s, public;
         CREATE TABLE public.t (x span);
         SET search_path = public;
         ALTER TABLE t ADD COLUMN y span;",
    )
    .expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "x").as_deref(), Some("INTERVAL"));
    assert_eq!(base_name(&database, "t", "y").as_deref(), Some("REAL"));
}

#[test]
fn a_domain_lands_in_the_first_existing_schema_on_the_path() {
    let database = parse(
        "CREATE SCHEMA s;
         SET search_path = missing, s, public;
         CREATE DOMAIN span AS interval;",
    )
    .expect("the schema is accepted");
    assert_eq!(domains(&database), ["s.span AS INTERVAL"]);
}

#[test]
fn a_domain_in_a_schema_never_created_is_refused() {
    assert!(parse("CREATE DOMAIN missing.span AS interval;").is_err());
}

#[test]
fn a_temporary_domain_shadows_one_on_the_path() {
    let database = parse(
        "CREATE DOMAIN span AS real;
         CREATE DOMAIN pg_temp.span AS interval;
         CREATE TABLE t (x span);",
    )
    .expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "x").as_deref(), Some("INTERVAL"));
}

#[test]
fn a_catalog_type_shadows_a_domain_of_its_name_on_the_path() {
    let database = parse(
        "CREATE DOMAIN name AS interval;
         CREATE TABLE t (catalog name, own public.name);",
    )
    .expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "catalog"), None);
    assert_eq!(base_name(&database, "t", "own").as_deref(), Some("INTERVAL"));
}

#[test]
fn a_domain_named_after_a_catalog_type_is_reached_where_the_path_puts_it_first() {
    let database = parse(
        "CREATE DOMAIN name AS interval;
         SET search_path = public, pg_catalog;
         CREATE TABLE t (x name);
         SET search_path = public;",
    )
    .expect("the schema is accepted");
    assert_eq!(base_name(&database, "t", "x").as_deref(), Some("INTERVAL"));
}

#[test]
fn a_domain_the_input_drops_is_reached_no_more() {
    let database = parse(
        "CREATE DOMAIN span AS interval;
         DROP DOMAIN span;
         CREATE DOMAIN span AS real;
         CREATE TABLE t (x span);",
    )
    .expect("the schema is accepted");
    assert_eq!(domains(&database), ["-.span AS REAL"]);
    assert_eq!(base_name(&database, "t", "x").as_deref(), Some("REAL"));
}

#[test]
fn dropping_a_domain_a_column_uses_is_refused() {
    let error = parse(
        "CREATE DOMAIN span AS interval;
         CREATE TABLE t (x span);
         DROP DOMAIN span;",
    )
    .expect_err("a column uses the domain");
    assert!(
        matches!(
            &error,
            Error::DomainHasDependents { domain_name, dependent_kind: ObjectKind::Column, dependent_name }
                if domain_name == "span" && dependent_name == "t.x"
        ),
        "{error:?}"
    );
}

#[test]
fn dropping_a_domain_an_array_column_uses_is_refused() {
    let error = parse(
        "CREATE DOMAIN span AS interval;
         CREATE TABLE t (x span[]);
         DROP DOMAIN span RESTRICT;",
    )
    .expect_err("a column uses an array of the domain");
    assert!(matches!(error, Error::DomainHasDependents { .. }), "{error:?}");
}

#[test]
fn dropping_a_domain_another_domain_is_over_is_refused() {
    let error = parse(
        "CREATE DOMAIN span AS interval;
         CREATE DOMAIN span2 AS span;
         DROP DOMAIN span;",
    )
    .expect_err("a domain is over the domain");
    assert!(
        matches!(
            &error,
            Error::DomainHasDependents { dependent_kind: ObjectKind::Domain, dependent_name, .. }
                if dependent_name == "span2"
        ),
        "{error:?}"
    );
}

#[test]
fn cascade_drops_the_columns_and_domains_using_the_domain() {
    let database = parse(
        "CREATE DOMAIN span AS interval;
         CREATE DOMAIN span2 AS span;
         CREATE DOMAIN other AS real;
         CREATE DOMAIN other2 AS other;
         CREATE TABLE t (a span, b span[], c span2, d other, e INT, m mood);
         CREATE INDEX t_c ON t (c);
         DROP DOMAIN span CASCADE;",
    )
    .expect("CASCADE drops what uses the domain");
    assert_eq!(domains(&database), ["-.other AS REAL", "-.other2 AS other"]);
    assert_eq!(columns(&database, "t"), ["d", "e", "m"]);
    assert_eq!(database.indexes().count(), 0);
}

#[test]
fn a_qualified_drop_takes_the_domain_of_that_schema() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE DOMAIN span AS real;
         CREATE DOMAIN s.span AS interval;
         CREATE TABLE t (x span, y INT);
         DROP DOMAIN s.span;",
    )
    .expect("no column uses s.span");
    assert_eq!(domains(&database), ["-.span AS REAL"]);
    assert_eq!(base_name(&database, "t", "x").as_deref(), Some("REAL"));
}

#[test]
fn cascade_drops_an_inherited_column_with_its_parent() {
    let database = parse(
        "CREATE DOMAIN span AS interval;
         CREATE TABLE parent (a span, k INT);
         CREATE TABLE child () INHERITS (parent);
         DROP DOMAIN span CASCADE;",
    )
    .expect("CASCADE drops what uses the domain");
    assert_eq!(columns(&database, "parent"), ["k"]);
    assert_eq!(columns(&database, "child"), ["k"]);
}

#[test]
fn dropping_a_missing_domain_is_refused_unless_if_exists() {
    let error = parse("DROP DOMAIN span;").expect_err("no such domain");
    assert!(
        matches!(&error, Error::DropDomainNotFound { domain_name } if domain_name == "span"),
        "{error:?}"
    );
    parse("DROP DOMAIN IF EXISTS span;").expect("IF EXISTS skips a missing domain");
}

#[test]
fn dropping_a_catalog_type_as_a_domain_is_refused_even_if_exists() {
    let error = parse("DROP DOMAIN IF EXISTS name;").expect_err("`name` is a catalog type");
    assert!(matches!(error, Error::DropDomainNotFound { .. }), "{error:?}");
}

#[test]
fn dropping_a_table_as_a_domain_is_refused() {
    let error =
        parse("CREATE TABLE tt (x INT); DROP DOMAIN tt;").expect_err("a table is not a domain");
    assert!(
        matches!(
            error,
            Error::RelationKindMismatch {
                expected_kind: ObjectKind::Domain,
                actual_kind: ObjectKind::Table,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn drop_type_drops_a_domain() {
    let database = parse(
        "CREATE DOMAIN span AS interval;
         CREATE DOMAIN other AS real;
         DROP TYPE span;",
    )
    .expect("DROP TYPE takes a domain");
    assert_eq!(domains(&database), ["-.other AS REAL"]);
    let error = parse(
        "CREATE DOMAIN span AS interval;
         CREATE TABLE t (x span);
         DROP TYPE span;",
    )
    .expect_err("a column uses the domain");
    assert!(matches!(error, Error::DomainHasDependents { .. }), "{error:?}");
}

#[test]
fn a_domain_shares_its_name_with_every_relation_that_has_a_row_type() {
    for sql in [
        "CREATE TABLE d (x INT); CREATE DOMAIN d AS INT;",
        "CREATE VIEW d AS SELECT 1 AS x; CREATE DOMAIN d AS INT;",
        "CREATE MATERIALIZED VIEW d AS SELECT 1 AS x; CREATE DOMAIN d AS INT;",
        "CREATE DOMAIN d AS INT; CREATE DOMAIN d AS REAL;",
        "CREATE DOMAIN d AS INT; CREATE TABLE d (x INT);",
        "CREATE DOMAIN d AS INT; CREATE VIEW d AS SELECT 1 AS x;",
        "CREATE DOMAIN d AS INT; CREATE SEQUENCE d;",
        "CREATE DOMAIN d AS INT; CREATE SEQUENCE IF NOT EXISTS d;",
        "CREATE DOMAIN t_id_seq AS INT; CREATE TABLE t (id SERIAL);",
        "CREATE DOMAIN t_id_seq AS INT; CREATE TABLE t (id INT GENERATED ALWAYS AS IDENTITY);",
    ] {
        let error = parse(sql).expect_err(sql);
        assert!(matches!(error, Error::RelationNameAlreadyTaken { .. }), "{sql}: {error:?}");
    }
}

#[test]
fn a_domain_takes_a_name_a_sequence_or_an_index_holds() {
    let database = parse(
        "CREATE SEQUENCE s;
         CREATE DOMAIN s AS INT;
         CREATE TABLE t (x INT);
         CREATE INDEX i ON t (x);
         CREATE DOMAIN i AS INT;
         CREATE DOMAIN j AS INT;
         CREATE INDEX j ON t (x);",
    )
    .expect("a sequence and an index have no row type");
    assert_eq!(domains(&database), ["-.i AS INT", "-.j AS INT", "-.s AS INT"]);
}

#[test]
fn a_domain_of_one_name_lives_in_each_schema() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE TABLE d (x INT);
         CREATE DOMAIN s.d AS INT;",
    )
    .expect("the names are in different schemas");
    assert_eq!(domains(&database), ["s.d AS INT"]);
}

#[test]
fn dropping_a_schema_holding_a_domain_is_refused() {
    let error = parse("CREATE SCHEMA s; CREATE DOMAIN s.span AS interval; DROP SCHEMA s;")
        .expect_err("the schema holds a domain");
    assert!(matches!(error, Error::SchemaNotEmpty { .. }), "{error:?}");
}

#[test]
fn dropping_a_schema_with_cascade_drops_its_domains_and_their_columns() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE SCHEMA o;
         CREATE DOMAIN s.span AS interval;
         CREATE TABLE t (x s.span, y INT);
         CREATE TABLE o.u (x s.span, y INT);
         DROP SCHEMA s CASCADE;",
    )
    .expect("CASCADE empties the schema");
    assert_eq!(database.domains().count(), 0);
    assert_eq!(columns(&database, "t"), ["y"]);
    assert_eq!(columns_of(&database, TargetName::new("u", false).with_schema("o", false)), ["y"]);
}

#[test]
fn a_domain_follows_its_schema_through_a_rename() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE DOMAIN pspan AS real;
         CREATE DOMAIN s.span AS interval;
         CREATE DOMAIN s.span2 AS s.span;
         CREATE DOMAIN s.wrap AS pspan;
         CREATE TABLE t (x s.span2, y pspan, n INT);
         ALTER SCHEMA s RENAME TO r;",
    )
    .expect("the schema is renamed");
    assert_eq!(
        domains(&database),
        ["-.pspan AS REAL", "r.span AS INTERVAL", "r.span2 AS r.span", "r.wrap AS pspan"]
    );
    assert_eq!(column_type(&database, "t", "x").name(&database), "r.span2");
    assert_eq!(base_name(&database, "t", "x").as_deref(), Some("INTERVAL"));
    assert_eq!(column_type(&database, "t", "y").name(&database), "pspan");
}

#[test]
fn a_rolled_back_domain_is_gone() {
    let database = parse(
        "BEGIN;
         CREATE DOMAIN span AS interval;
         ROLLBACK;",
    )
    .expect("the transaction rolls back");
    assert_eq!(database.domains().count(), 0);
}

#[test]
fn a_domain_declared_over_its_own_name_reaches_no_base_type() {
    let database =
        parse("CREATE DOMAIN d AS d; CREATE TABLE t (x d);").expect("an unknown base type is kept");
    assert_eq!(base_name(&database, "t", "x"), None);
}
