//! Whether two column collations name the same comparison rule.
//!
//! A drift check compares the collation a catalog writes for a column with
//! the collation a live read of the same column reports. The two sides spell
//! the same rule differently, a written name without its schema against the
//! schema the engine stores, so the comparison settles on the rule each side
//! resolves to instead of comparing text. The live side here is shaped the
//! way PostgreSQL's `information_schema.columns` hands it back, schema and
//! name both quoted, or absent for the database default.

#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    prelude::*,
    traits::{ColumnCollation, NamedColumnCollation},
};
use sqlparser::dialect::PostgreSqlDialect;

/// The collation a live read of PostgreSQL reports for a column, with the
/// schema and name quoted the way `information_schema.columns` hands them
/// back.
fn live_collation(schema: &'static str, name: &'static str) -> ColumnCollation<'static> {
    ColumnCollation::Named(NamedColumnCollation::new(
        TargetName::new(name, true).with_schema(schema, true),
    ))
}

/// The live answer for a column whose stored collation is the database default.
fn live_default() -> ColumnCollation<'static> {
    ColumnCollation::DatabaseDefault
}

/// The unqualified table `t` a catalog creates in the default schema.
fn default_table() -> TargetName<'static> {
    TargetName::new("t", false)
}

/// Parses `sql`, reads the collation of `column` in the table `table` names,
/// and compares it with `live` under the PostgreSQL rules.
fn compare(
    sql: &str,
    table: TargetName<'static>,
    column: &str,
    live: &ColumnCollation<'_>,
) -> CollationMatch {
    let db = ParserDB::parse::<PostgreSqlDialect>(sql).expect("the DDL parses");
    let table = db
        .table_by_target(table, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let column = table
        .column(column, &db, IdentifierCase::AsWritten)
        .expect("column lookup")
        .expect("the column exists");
    let written = column.collation(&db).expect("the collation reads");
    written.is_same_rule(SqlparserDialect::PostgreSql, live)
}

#[test]
fn the_database_default_and_a_written_default_are_one_rule() {
    // No `COLLATE` at all answers the database default, as does the live read.
    assert_eq!(
        compare("CREATE TABLE t (a TEXT);", default_table(), "a", &live_default()),
        CollationMatch::Same
    );
    // A written `default` names the database default itself.
    assert_eq!(
        compare(
            "CREATE TABLE t (b TEXT COLLATE \"default\");",
            default_table(),
            "b",
            &live_default(),
        ),
        CollationMatch::Same
    );
}

#[test]
fn an_unqualified_builtin_resolves_to_pg_catalog() {
    assert_eq!(
        compare(
            "CREATE TABLE t (c TEXT COLLATE \"C\");",
            default_table(),
            "c",
            &live_collation("pg_catalog", "C"),
        ),
        CollationMatch::Same
    );
}

#[test]
fn an_unqualified_name_resolves_to_the_schema_the_engine_stores() {
    assert_eq!(
        compare(
            "
            CREATE COLLATION ci (
                provider = icu,
                locale = 'und-u-ks-level2',
                deterministic = false
            );
            CREATE TABLE t (e TEXT COLLATE ci);
            ",
            default_table(),
            "e",
            &live_collation("public", "ci"),
        ),
        CollationMatch::Same
    );
}

#[test]
fn a_different_named_rule_is_a_different_rule() {
    assert_eq!(
        compare(
            "CREATE TABLE t (f TEXT COLLATE \"C\");",
            default_table(),
            "f",
            &live_collation("pg_catalog", "POSIX"),
        ),
        CollationMatch::Different
    );
}

#[test]
fn a_named_rule_against_the_database_default_is_different() {
    // The stored collation is 950 against the default 100.
    assert_eq!(
        compare("CREATE TABLE t (g TEXT COLLATE \"C\");", default_table(), "g", &live_default()),
        CollationMatch::Different
    );
}

#[test]
fn an_unqualified_name_resolves_on_the_search_path_in_force() {
    let sql = "
        CREATE SCHEMA app;
        CREATE COLLATION app.ci (
            provider = icu,
            locale = 'und-u-ks-level2',
            deterministic = false
        );
        SET search_path = app;
        CREATE TABLE t (name TEXT COLLATE ci);
    ";
    let table = TargetName::new("t", false).with_schema("app", false);
    assert_eq!(compare(sql, table, "name", &live_collation("app", "ci")), CollationMatch::Same);
    let table = TargetName::new("t", false).with_schema("app", false);
    assert_eq!(
        compare(sql, table, "name", &live_collation("public", "ci")),
        CollationMatch::Different
    );
}

#[test]
fn postgres_folds_unquoted_names_before_resolving_them() {
    // An unquoted `C` folds to `c`, which the catalog created in public,
    // instead of naming the built-in `pg_catalog."C"`.
    let sql = "
        CREATE COLLATION c (
            provider = icu,
            locale = 'und-u-ks-level2',
            deterministic = false
        );
        CREATE TABLE t (name TEXT COLLATE C);
    ";
    assert_eq!(
        compare(sql, default_table(), "name", &live_collation("pg_catalog", "C")),
        CollationMatch::Different
    );
    assert_eq!(
        compare(sql, default_table(), "name", &live_collation("public", "c")),
        CollationMatch::Same
    );
}

#[test]
fn two_unqualified_spellings_of_one_name_are_the_same_rule() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "
        CREATE COLLATION ci (
            provider = icu,
            locale = 'und-u-ks-level2',
            deterministic = false
        );
        CREATE TABLE t (one TEXT COLLATE ci, two TEXT COLLATE ci);
        ",
    )
    .expect("the DDL parses");
    let table = db
        .table_by_target(default_table(), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table t exists");
    let one = table
        .column("one", &db, IdentifierCase::AsWritten)
        .expect("column lookup")
        .expect("column one exists");
    let two = table
        .column("two", &db, IdentifierCase::AsWritten)
        .expect("column lookup")
        .expect("column two exists");
    let one_rule = one.collation(&db).expect("the collation reads");
    let two_rule = two.collation(&db).expect("the collation reads");
    assert_eq!(
        one_rule.is_same_rule(SqlparserDialect::PostgreSql, &two_rule),
        CollationMatch::Same
    );
}

#[test]
fn a_side_that_resolves_to_nothing_leaves_the_rule_undetermined() {
    // A written name no database resolved keeps no identity, so the
    // comparison settles it to nothing instead of guessing.
    let written = ColumnCollation::Named(NamedColumnCollation::new(TargetName::new("ci", false)));
    assert_eq!(
        written.is_same_rule(SqlparserDialect::PostgreSql, &live_collation("public", "ci")),
        CollationMatch::Undetermined
    );
    assert_eq!(
        written.is_same_rule(SqlparserDialect::PostgreSql, &ColumnCollation::Unknown),
        CollationMatch::Undetermined
    );
    assert_eq!(
        ColumnCollation::Unknown
            .is_same_rule(SqlparserDialect::PostgreSql, &live_collation("public", "ci")),
        CollationMatch::Undetermined
    );
}

#[test]
fn a_resolved_identity_compares_part_by_part() {
    // `public.ci` against `public."ci"` is one rule, against `public."CI"`
    // it is not, and the schema part keeps its case under quotes.
    let written = ColumnCollation::Named(NamedColumnCollation::new(
        TargetName::new("ci", false).with_schema("public", false),
    ));
    assert_eq!(
        written.is_same_rule(SqlparserDialect::PostgreSql, &live_collation("public", "ci")),
        CollationMatch::Same
    );
    assert_eq!(
        written.is_same_rule(SqlparserDialect::PostgreSql, &live_collation("public", "CI")),
        CollationMatch::Different
    );
    let in_app = ColumnCollation::Named(NamedColumnCollation::new(
        TargetName::new("ci", false).with_schema("app", false),
    ));
    assert_eq!(
        in_app.is_same_rule(SqlparserDialect::PostgreSql, &live_collation("public", "ci")),
        CollationMatch::Different
    );
}

#[test]
fn only_postgres_settles_named_rules_here() {
    // Only PostgreSQL settles a named rule; every other dialect settles the
    // dialect-neutral outcomes alone.
    let written =
        ColumnCollation::Named(NamedColumnCollation::new(TargetName::new("utf8mb4_bin", false)));
    let live =
        ColumnCollation::Named(NamedColumnCollation::new(TargetName::new("UTF8MB4_BIN", false)));
    assert_eq!(written.is_same_rule(SqlparserDialect::MySql, &live), CollationMatch::Undetermined);
    assert_eq!(
        ColumnCollation::DatabaseDefault
            .is_same_rule(SqlparserDialect::MySql, &ColumnCollation::DatabaseDefault),
        CollationMatch::Same
    );
    assert_eq!(
        ColumnCollation::Unknown.is_same_rule(SqlparserDialect::SQLite, &written),
        CollationMatch::Undetermined
    );
}
