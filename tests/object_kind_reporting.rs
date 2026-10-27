//! Tests that every kind of database object reports itself when the database
//! being queried does not hold it.
//!
//! Each object here is taken from a live database and then queried against a
//! different one, which is the same mismatch a renamed-away table produces and
//! needs no hand-built AST node.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

const HOST: &str = "CREATE TABLE anon (
        id INTEGER PRIMARY KEY,
        name TEXT,
        UNIQUE (name),
        CHECK (id > 0)
    );
    CREATE TABLE named (
        id INTEGER PRIMARY KEY,
        name TEXT,
        CONSTRAINT uq_named UNIQUE (name),
        CONSTRAINT ck_named CHECK (id > 0)
    );
    CREATE INDEX idx_anon_name ON anon (name);
    CREATE FUNCTION visible() RETURNS BOOLEAN AS 'SELECT true;';
    CREATE POLICY p ON anon USING (visible());";

/// A database holding none of the objects declared by [`HOST`].
const OTHER: &str = "CREATE TABLE unrelated (id INTEGER PRIMARY KEY);";

fn databases() -> (ParserDB, ParserDB) {
    (
        ParserDB::parse::<PostgreSqlDialect>(HOST).expect("host schema builds"),
        ParserDB::parse::<PostgreSqlDialect>(OTHER).expect("other schema builds"),
    )
}

/// Returns the unique index of `table` covering `expression`.
///
/// Selecting by what the constraint covers rather than by iteration position
/// matters here: a table's primary key is also an anonymous unique index, so a
/// positional pick could silently grab it and the anonymous assertions below
/// would still pass.
fn unique_index_over<'db>(
    database: &'db ParserDB,
    table_name: &str,
    expression: &str,
) -> &'db <ParserDB as DatabaseLike>::UniqueIndex {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    table
        .unique_indices(database)
        .expect("table is in this database")
        .find(|index| {
            index.expression(database).expect("index is in this database").to_string() == expression
        })
        .expect("table declares a unique constraint over that expression")
}

/// Returns the check constraint of `table` enforcing `expression`.
///
/// Selected by what it enforces for the same reason as [`unique_index_over`]:
/// `HOST` is shared by every test here, so a positional pick would silently
/// change which constraint is exercised if the fixture ever grew another.
fn check_constraint_over<'db>(
    database: &'db ParserDB,
    table_name: &str,
    expression: &str,
) -> &'db <ParserDB as DatabaseLike>::CheckConstraint {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    table
        .check_constraints(database)
        .expect("table is in this database")
        .find(|check| check.expression(database).to_string() == expression)
        .expect("table declares a check constraint enforcing that expression")
}

#[test]
fn a_column_reports_its_absent_table() {
    let (host, other) = databases();
    let table = host
        .table_by_target(TargetName::new("anon", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("anon exists");
    let column = table
        .column("name", &host, IdentifierCase::AsWritten)
        .expect("anon is in the host database")
        .expect("anon declares name");

    assert_eq!(column.column_doc(&other).err(), Some(ObjectKind::Table.not_in_database("anon")));
}

#[test]
fn an_index_reports_itself_by_name() {
    let (host, other) = databases();
    let index = host.indexes().next().expect("the host database holds an index");

    assert_eq!(
        index.expression(&other).err(),
        Some(ObjectKind::Index.not_in_database("idx_anon_name"))
    );
}

#[test]
fn a_named_unique_constraint_reports_itself_by_name() {
    let (host, other) = databases();
    let unique_index = unique_index_over(&host, "named", "(name)");

    assert_eq!(
        unique_index.expression(&other).err(),
        Some(ObjectKind::UniqueIndex.not_in_database("uq_named"))
    );
}

/// An anonymous constraint has no identity of its own, so it is reported by the
/// table it is declared on.
#[test]
fn an_anonymous_unique_constraint_reports_its_table() {
    let (host, other) = databases();
    let unique_index = unique_index_over(&host, "anon", "(name)");

    assert_eq!(
        unique_index.expression(&other).err(),
        Some(ObjectKind::UniqueIndex.anonymous_not_in_database("anon"))
    );
}

#[test]
fn a_named_check_constraint_reports_itself_by_name() {
    let (host, other) = databases();
    let check = check_constraint_over(&host, "named", "id > 0");

    assert_eq!(
        CheckConstraintLike::table(check, &other).err(),
        Some(ObjectKind::CheckConstraint.not_in_database("ck_named"))
    );
    assert_eq!(
        check.columns(&other).err(),
        Some(ObjectKind::CheckConstraint.not_in_database("ck_named"))
    );
    assert_eq!(
        check.functions(&other).err(),
        Some(ObjectKind::CheckConstraint.not_in_database("ck_named"))
    );
}

#[test]
fn an_anonymous_check_constraint_reports_its_table() {
    let (host, other) = databases();
    let check = check_constraint_over(&host, "anon", "id > 0");

    assert_eq!(
        CheckConstraintLike::table(check, &other).err(),
        Some(ObjectKind::CheckConstraint.anonymous_not_in_database("anon"))
    );
}

#[test]
fn a_policy_reports_itself_by_name() {
    let (host, other) = databases();
    let policy = host.policies().next().expect("the host database holds a policy");

    assert_eq!(policy.using_functions(&other).err(), Some(ObjectKind::Policy.not_in_database("p")));
    assert_eq!(policy.check_functions(&other).err(), Some(ObjectKind::Policy.not_in_database("p")));
}

/// `CREATE INDEX` without a name is valid, so the anonymous branch is reachable
/// for an index too, not only for a constraint.
#[test]
fn an_anonymous_index_reports_its_table() {
    let host = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); CREATE INDEX ON t (name);",
    )
    .expect("host schema builds");
    let other = ParserDB::parse::<PostgreSqlDialect>(OTHER).expect("other schema builds");
    let index = host.indexes().next().expect("the host database holds an index");

    assert!(index.name().is_none(), "the index really is anonymous");
    assert_eq!(
        index.expression(&other).err(),
        Some(ObjectKind::Index.anonymous_not_in_database("t"))
    );
}

/// PostgreSQL names a unique constraint and MySQL names the index behind it.
/// sqlparser keeps those in separate fields, so both spellings have to resolve
/// to a name rather than falling through to the anonymous rendering.
#[test]
fn a_mysql_unique_key_reports_itself_by_name() {
    let host = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (id INT, name TEXT, UNIQUE KEY uq_name (name));",
    )
    .expect("host schema builds");
    let other = ParserDB::parse::<MySqlDialect>(OTHER).expect("other schema builds");
    let unique_index = unique_index_over(&host, "t", "(name)");

    assert_eq!(
        unique_index.expression(&other).err(),
        Some(ObjectKind::UniqueIndex.not_in_database("uq_name"))
    );
}

/// Every other assertion in this file builds its expectation with the same
/// constructor the production code uses, so kind and identity are checked but
/// the spelling is not: a typo in one `Display` arm would change both sides
/// together and pass. These two tests are the only place the rendered text is
/// pinned, so they cover every kind and both shapes.
#[test]
fn every_kind_renders_its_own_spelling() {
    for (kind, spelling) in [
        (ObjectKind::Table, "Table"),
        (ObjectKind::View, "View"),
        (ObjectKind::MaterializedView, "Materialized view"),
        (ObjectKind::Column, "Column"),
        (ObjectKind::Index, "Index"),
        (ObjectKind::UniqueIndex, "Unique index"),
        (ObjectKind::CheckConstraint, "Check constraint"),
        (ObjectKind::Policy, "Policy"),
        (ObjectKind::Function, "Function"),
        (ObjectKind::Trigger, "Trigger"),
        (ObjectKind::Rule, "Rule"),
        (ObjectKind::Role, "Role"),
        (ObjectKind::Schema, "Schema"),
    ] {
        assert_eq!(
            kind.not_in_database("x").to_string(),
            format!("{spelling} `x` is not present in the database being queried.")
        );
    }
}

#[test]
fn an_anonymous_object_renders_by_its_table() {
    assert_eq!(
        ObjectKind::CheckConstraint.anonymous_not_in_database("anon").to_string(),
        "Check constraint declared on table `anon` is not present in the database being queried."
    );
}

/// The kinds are distinguishable without comparing rendered text, which is why
/// `object_kind` is typed.
#[test]
fn the_kind_is_matchable() {
    let error = ObjectKind::UniqueIndex.anonymous_not_in_database("anon");
    let LookupError::ObjectNotInDatabase { object_kind, .. } = error else {
        panic!("expected an ObjectNotInDatabase error");
    };

    assert_eq!(object_kind, ObjectKind::UniqueIndex);
    assert_ne!(object_kind, ObjectKind::Index);
}
