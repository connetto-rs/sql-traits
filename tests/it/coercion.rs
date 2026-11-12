//! Tests that call resolution converts argument types through the casts and
//! the type facts the database answers, and leaves the call open where they
//! are missing.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Function, SelectItem, SetExpr, Statement},
    dialect::{Dialect, PostgreSqlDialect},
    parser::Parser,
};

/// The call `expression` writes.
fn call(expression: &str) -> Function {
    let Statement::Query(query) =
        Parser::parse_sql(&PostgreSqlDialect {}, &format!("SELECT {expression}"))
            .expect("query parses")
            .remove(0)
    else {
        panic!("not a query")
    };
    let SetExpr::Select(select) = *query.body else { panic!("not a select") };
    match select.projection.into_iter().next() {
        Some(SelectItem::UnnamedExpr(Expr::Function(call))) => call,
        _ => panic!("not a call"),
    }
}

/// The declared type of the first parameter of the function `expression`
/// runs, or `None` when the call is refused or left open.
fn chosen(db: &ParserDB, expression: &str, types: &[Option<&str>]) -> Option<String> {
    let function = db
        .resolve_call(&call(expression), types, IdentifierCase::AsWritten)
        .expect("call resolves")?;
    function.argument_type_names(db).next().map(std::borrow::Cow::into_owned)
}

/// What `expression` returns given its argument types.
fn returns(db: &ParserDB, expression: &str, types: &[Option<&str>]) -> Option<String> {
    db.call_result_type_name(&call(expression), types, IdentifierCase::AsWritten)
        .expect("call resolves")
        .map(std::borrow::Cow::into_owned)
}

fn parse<D: Dialect + Default>(sql: &str) -> ParserDB {
    ParserDB::parse::<D>(sql).expect("schema parses")
}

/// A domain argument converts through its base type, so `code` over
/// `varchar` reaches the `text` overload and no other.
#[test]
fn a_domain_argument_converts_through_its_base_type() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE DOMAIN code AS VARCHAR(3);
         CREATE FUNCTION f(x TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION g(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION h(x VARCHAR) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION h(x INT) RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    );
    assert_eq!(chosen(&db, "f(c)", &[Some("code")]).as_deref(), Some("TEXT"));
    // The domain relabels to its own base type.
    assert_eq!(chosen(&db, "h(c)", &[Some("code")]).as_deref(), Some("VARCHAR"));
    // No cast takes `varchar` to `int4`.
    assert_eq!(chosen(&db, "g(c)", &[Some("code")]), None);
    assert_eq!(returns(&db, "g(c)", &[Some("code")]), None);
}

/// The array families bind a domain over an array as its base array type.
#[test]
fn a_domain_over_an_array_binds_the_array_families_as_its_base() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE DOMAIN ints AS INT[];
         CREATE FUNCTION head(xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT xs[1]';
         CREATE FUNCTION same(xs anyarray) RETURNS anyarray LANGUAGE sql AS 'SELECT xs';
         CREATE FUNCTION grow(xs anycompatiblearray, x anycompatible) RETURNS anycompatiblearray
             LANGUAGE sql AS 'SELECT xs || x';",
    );
    assert_eq!(returns(&db, "head(xs)", &[Some("ints")]).as_deref(), Some("int4"));
    assert_eq!(returns(&db, "same(xs)", &[Some("ints")]).as_deref(), Some("int4[]"));
    assert_eq!(
        returns(&db, "grow(xs, x)", &[Some("ints"), Some("BIGINT")]).as_deref(),
        Some("BIGINT[]")
    );
    // A domain over a scalar is no array.
    let db = parse::<PostgreSqlDialect>(
        "CREATE DOMAIN code AS TEXT;
         CREATE FUNCTION head(xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT xs[1]';",
    );
    assert_eq!(returns(&db, "head(xs)", &[Some("code")]), None);
}

/// A base type's declared category and preference rank it among overloads
/// an untyped literal reaches.
#[test]
fn a_custom_category_ranks_overloads_for_an_untyped_literal() {
    let schema = |options: &str| {
        format!(
            "CREATE FUNCTION tag_in(cstring) RETURNS tag LANGUAGE C AS 'lib', 'tag_in';
             CREATE FUNCTION tag_out(tag) RETURNS cstring LANGUAGE C AS 'lib', 'tag_out';
             CREATE TYPE tag (INPUT = tag_in, OUTPUT = tag_out{options});
             CREATE FUNCTION f(x tag) RETURNS INT LANGUAGE sql AS 'SELECT 1';
             CREATE FUNCTION f(x VARCHAR) RETURNS INT LANGUAGE sql AS 'SELECT 2';"
        )
    };
    // A user-defined category loses to the string category.
    let db = parse::<PostgreSqlDialect>(&schema(""));
    assert_eq!(chosen(&db, "f('a')", &[Some("unknown")]).as_deref(), Some("VARCHAR"));
    // A preferred string type wins over `varchar`.
    let db = parse::<PostgreSqlDialect>(&schema(", CATEGORY = 'S', PREFERRED = true"));
    assert_eq!(chosen(&db, "f('a')", &[Some("unknown")]).as_deref(), Some("tag"));
    // Two string types, neither preferred, leave the call ambiguous.
    let db = parse::<PostgreSqlDialect>(&schema(", CATEGORY = 'S'"));
    assert_eq!(chosen(&db, "f('a')", &[Some("unknown")]), None);
}

/// A table's row type converts to the row type of every table it descends
/// from and to `record`, and `record` converts to any composite type.
#[test]
fn row_types_convert_to_ancestors_and_record() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE TABLE parent (a INT);
         CREATE TABLE child () INHERITS (parent);
         CREATE TABLE grandchild () INHERITS (child);
         CREATE TABLE events (id INT) PARTITION BY RANGE (id);
         CREATE TABLE events_low PARTITION OF events FOR VALUES FROM (1) TO (9)
             PARTITION BY RANGE (id);
         CREATE TABLE events_lowest PARTITION OF events_low FOR VALUES FROM (1) TO (5);
         CREATE TABLE other (a INT);
         CREATE VIEW shown AS SELECT 1 AS a;
         CREATE TYPE pair AS (a INT);
         CREATE TYPE mood AS ENUM ('calm');
         CREATE FUNCTION p(r parent) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION e(r events) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION s(r shown) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION c(r pair) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION m(r mood) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION r(x record) RETURNS INT LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
         CREATE FUNCTION rs(x record[]) RETURNS INT LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';",
    );
    assert_eq!(chosen(&db, "p(x)", &[Some("grandchild")]).as_deref(), Some("parent"));
    assert_eq!(chosen(&db, "e(x)", &[Some("events_lowest")]).as_deref(), Some("events"));
    assert_eq!(chosen(&db, "p(x)", &[Some("other")]), None);
    assert_eq!(chosen(&db, "p(x)", &[Some("shown")]), None);
    assert_eq!(chosen(&db, "s(x)", &[Some("parent")]), None);
    assert_eq!(chosen(&db, "m(x)", &[Some("parent")]), None);
    assert_eq!(chosen(&db, "r(x)", &[Some("other")]).as_deref(), Some("record"));
    assert_eq!(chosen(&db, "rs(x)", &[Some("parent[]")]).as_deref(), Some("record[]"));
    assert_eq!(chosen(&db, "p(x)", &[Some("record")]).as_deref(), Some("parent"));
    // A typed table's row type converts to the type it was created `OF`, and
    // nothing tells which type that is.
    assert_eq!(chosen(&db, "c(x)", &[Some("parent")]), None);
    assert_eq!(returns(&db, "c(x)", &[Some("parent")]).as_deref(), Some("INT"));
}

/// A database answering no casts proves only identities and untyped
/// literals, so every call needing a conversion is left open.
#[test]
fn without_casts_a_needed_conversion_leaves_the_call_open() {
    let db = parse::<sqlparser::dialect::GenericDialect>(
        "CREATE FUNCTION f(x BIGINT) RETURNS BIGINT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION f(x TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION g(x anyelement) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION g(x anycompatible) RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    );
    assert_eq!(chosen(&db, "f(x)", &[Some("BIGINT")]).as_deref(), Some("BIGINT"));
    assert_eq!(chosen(&db, "f(x)", &[Some("INT")]), None);
    // Both overloads take an untyped literal, and no category ranks them.
    assert_eq!(chosen(&db, "f('1')", &[Some("unknown")]), None);
    // Both overloads bind, and no category ranks them.
    assert_eq!(chosen(&db, "g(x)", &[Some("INT")]), None);
    assert_eq!(returns(&db, "g(x)", &[Some("INT")]).as_deref(), Some("INT"));
    assert_eq!(db.common_type_name(&[Some("INT"), Some("BIGINT")]), None);
    assert_eq!(db.common_type_name(&[Some("INT"), Some("unknown")]).as_deref(), Some("INT"));
}

/// A type the database holds no facts about may be a domain over anything,
/// so every conversion and binding it takes part in is left open.
#[test]
fn a_type_without_facts_leaves_its_conversions_open() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE FUNCTION g(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION r(x record) RETURNS INT LANGUAGE plpgsql AS 'BEGIN RETURN 1; END';
         CREATE FUNCTION head(xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT xs[1]';
         CREATE FUNCTION grow(xs anycompatiblearray, x anycompatible) RETURNS anycompatiblearray
             LANGUAGE sql AS 'SELECT xs || x';
         CREATE FUNCTION pick(a anycompatible, b anycompatible) RETURNS anycompatible
             LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION scalar(x anynonarray) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION any_of(x anyelement) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION any_of(x \"any\") RETURNS INT LANGUAGE C AS 'lib', 'any_of';",
    );
    let missing = Some("missing");
    assert_eq!(chosen(&db, "g(x)", &[missing]), None);
    assert_eq!(chosen(&db, "r(x)", &[Some("TINYINT")]), None);
    assert_eq!(returns(&db, "head(x)", &[missing]), None);
    assert_eq!(returns(&db, "grow(x, y)", &[missing, Some("INT")]), None);
    assert_eq!(returns(&db, "pick(x, y)", &[missing, Some("INT")]), None);
    assert_eq!(chosen(&db, "scalar(x)", &[missing]), None);
    assert_eq!(chosen(&db, "any_of(x)", &[missing]), None);
    assert_eq!(db.common_type_name(&[missing, Some("INT")]), None);
}

/// A stored type name reads the same unqualified, qualified, and as the
/// generated name of its array.
#[test]
fn stored_type_names_are_read_through_the_search_path() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE TYPE mood AS ENUM ('calm');
         CREATE FUNCTION f(x mood[]) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT[]) RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    );
    for spelling in ["mood[]", "public.mood[]", "_mood", "public._mood"] {
        assert_eq!(chosen(&db, "f(x)", &[Some(spelling)]).as_deref(), Some("mood[]"), "{spelling}");
    }
}

/// When an untyped literal takes the type of the other arguments and that
/// leaves conversions the facts do not settle, the call stays open.
#[test]
fn the_last_resort_leaves_unsettled_conversions_open() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE TABLE t (a INT);
         CREATE TYPE c AS (a INT);
         CREATE TYPE d AS (a INT);
         CREATE FUNCTION f(x t, y c) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION f(x t, y d) RETURNS INT LANGUAGE sql AS 'SELECT 2';",
    );
    assert_eq!(chosen(&db, "f(x, '(1)')", &[Some("t"), Some("unknown")]), None);
}

/// Values meeting in one expression take the common type the casts and type
/// facts settle, a domain's base type among them.
#[test]
fn common_types_read_casts_and_type_facts() {
    let db = parse::<PostgreSqlDialect>(
        "CREATE DOMAIN code AS VARCHAR(3);
         CREATE TYPE mood AS ENUM ('calm');
         CREATE DOMAIN calm AS mood;
         CREATE DOMAIN flag AS \"char\";
         CREATE TABLE t (a INT);
         CREATE TYPE c AS (a INT);
         CREATE FUNCTION pick(a anycompatible, b anycompatible) RETURNS anycompatible
             LANGUAGE sql AS 'SELECT a';",
    );
    let unknown = Some("unknown");
    assert_eq!(db.common_type_name(&[Some("code"), unknown]).as_deref(), Some("varchar"));
    assert_eq!(
        db.common_type_name(&[Some("calm"), unknown]).as_deref(),
        Some("\"public\".\"mood\"")
    );
    assert_eq!(db.common_type_name(&[Some("flag"), unknown]).as_deref(), Some("pg_catalog.char"));
    // Untyped literals take no part, so a lone domain keeps its type.
    assert_eq!(returns(&db, "pick(x, y)", &[Some("code"), unknown]).as_deref(), Some("code"));
    assert_eq!(
        returns(&db, "pick(x, y)", &[Some("code"), Some("TEXT")]).as_deref(),
        Some("varchar")
    );
    // `timestamptz` is preferred, and no implicit cast reaches it from `time`.
    assert_eq!(db.common_type_name(&[Some("TIMESTAMPTZ"), Some("TIME")]), None);
    // Whether a table's row type converts to a composite type is open.
    assert_eq!(db.common_type_name(&[Some("t"), Some("c")]), None);
    assert_eq!(db.common_type_name(&[Some("c"), Some("t")]), None);
}

/// A configured cast decides selection as a built-in one does, both by
/// adding a conversion and by replacing a built-in cast's context.
#[test]
fn configured_casts_change_which_function_a_call_runs() {
    let schema = "CREATE SCHEMA app;
         CREATE TYPE app.celsius AS ENUM ('c');
         CREATE TYPE app.fahrenheit AS ENUM ('f');
         CREATE FUNCTION app.to_f(app.celsius) RETURNS app.fahrenheit
             LANGUAGE sql AS 'SELECT ''f''::app.fahrenheit';
         CREATE FUNCTION warm(t app.fahrenheit) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION wide(x BIGINT) RETURNS INT LANGUAGE sql AS 'SELECT 1';";
    let app = |name| TargetName::new(name, false).with_schema("app", false);
    let pg = |name| TargetName::new(name, false).with_schema("pg_catalog", false);
    let parse_with = |catalog: PostgresCatalog| {
        ParseOptions::default()
            .with_postgres_catalog(catalog)
            .parse::<PostgreSqlDialect>(schema)
            .expect("schema parses")
    };
    let celsius = [Some("app.celsius")];
    let int = [Some("INT")];

    let built_in = parse_with(PostgresCatalog::default());
    assert_eq!(chosen(&built_in, "warm(t)", &celsius), None);
    assert_eq!(chosen(&built_in, "wide(x)", &int).as_deref(), Some("BIGINT"));

    let converting =
        parse_with(PostgresCatalog::default().with_cast(PostgresCatalogCast::through_function(
            app("celsius"),
            app("fahrenheit"),
            CastContext::Implicit,
            app("to_f"),
            [app("celsius")],
        )));
    assert_eq!(chosen(&converting, "warm(t)", &celsius).as_deref(), Some("app.fahrenheit"));

    let explicit_only = parse_with(PostgresCatalog::default().with_cast(
        PostgresCatalogCast::binary_coercible(pg("int4"), pg("int8"), CastContext::Explicit),
    ));
    assert_eq!(chosen(&explicit_only, "wide(x)", &int), None);
}
