//! Tests that an index reports the schema it lives in, which is the schema of
//! the table it is built on, and that a qualifier written on its name may
//! only name that schema.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The schema each index of `sql` reports, with its quoting, sorted.
fn index_schemas(db: &ParserDB) -> Vec<(Option<String>, bool)> {
    let mut schemas: Vec<_> = db
        .indexes()
        .map(|index| (index.schema().map(ToString::to_string), index.schema_is_quoted()))
        .collect();
    schemas.sort();
    schemas
}

#[test]
fn an_index_reports_the_schema_of_its_table() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.lines (id INT, body TEXT);
         CREATE INDEX lines_by_id ON app.lines (id);",
    )
    .expect("schema builds");

    assert_eq!(index_schemas(&db), [(Some("app".to_string()), false)]);
}

#[test]
fn an_index_on_a_quoted_schema_reports_its_quoting() {
    let db = parse(
        "CREATE SCHEMA \"App\";
         CREATE TABLE \"App\".lines (id INT);
         CREATE INDEX lines_by_id ON \"App\".lines (id);",
    )
    .expect("schema builds");

    assert_eq!(index_schemas(&db), [(Some("App".to_string()), true)]);
}

/// A table reached through the search path is stored with the schema it was
/// found in, and its index reports that schema.
#[test]
fn an_index_follows_its_table_through_the_search_path() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.lines (id INT);
         SET search_path TO app;
         CREATE INDEX lines_by_id ON lines (id);",
    )
    .expect("schema builds");

    assert_eq!(index_schemas(&db), [(Some("app".to_string()), false)]);
}

/// An index on a table stored without a schema reports none, as the table
/// does.
#[test]
fn an_index_on_an_unqualified_table_reports_no_schema() {
    let db = parse(
        "CREATE TABLE lines (id INT);
         CREATE INDEX lines_by_id ON lines (id);",
    )
    .expect("schema builds");

    assert_eq!(index_schemas(&db), [(None, false)]);
}

/// The index PostgreSQL builds behind a `PRIMARY KEY` or `UNIQUE` constraint
/// lives in the table's schema too.
#[test]
fn a_constraint_index_reports_the_schema_of_its_table() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.users (id INT PRIMARY KEY, email TEXT, CONSTRAINT uq_email UNIQUE (email));",
    )
    .expect("schema builds");
    let table = db
        .table_by_target(
            TargetName::new("users", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table exists");

    let schemas: Vec<_> = table
        .unique_indices(&db)
        .expect("unique indices")
        .map(|unique| (IndexLike::schema(unique), IndexLike::schema_is_quoted(unique)))
        .collect();
    assert_eq!(schemas, [(Some("app"), false), (Some("app"), false)]);
}

/// A dialect allowing a qualifier on the index name keeps the index with its
/// table, so a qualifier naming the table's schema, under any spelling that
/// compares equal, is accepted.
#[test]
fn a_qualifier_naming_the_tables_schema_is_accepted() {
    for sql in [
        "CREATE SCHEMA app; CREATE TABLE app.t (id INT); CREATE INDEX app.i ON app.t (id);",
        "CREATE SCHEMA app; CREATE TABLE app.t (id INT); CREATE INDEX APP.i ON app.t (id);",
        "CREATE TABLE t (id INT); CREATE INDEX public.i ON t (id);",
    ] {
        let db = ParserDB::parse::<GenericDialect>(sql).unwrap_or_else(|error| {
            panic!("{sql} must build: {error}");
        });
        let table_schema = db.tables().next().expect("one table").table_schema();
        let index = db.indexes().next().expect("one index");
        assert_eq!(index.name(), Some("i"), "{sql}");
        assert_eq!(index.schema(), table_schema, "{sql}");
    }
}

/// No engine puts an index anywhere but beside its table, so a qualifier
/// naming another schema is refused.
#[test]
fn a_qualifier_naming_another_schema_is_refused() {
    for (sql, schema, table) in [
        (
            "CREATE SCHEMA app; CREATE SCHEMA other; CREATE TABLE app.t (id INT);
             CREATE INDEX other.i ON app.t (id);",
            "other",
            "t",
        ),
        ("CREATE SCHEMA app; CREATE TABLE t (id INT); CREATE INDEX app.i ON t (id);", "app", "t"),
        (
            "CREATE SCHEMA app; CREATE TABLE app.t (id INT);
             CREATE INDEX \"APP\".i ON app.t (id);",
            "APP",
            "t",
        ),
    ] {
        let result = ParserDB::parse::<GenericDialect>(sql);
        assert!(
            matches!(
                &result,
                Err(Error::IndexInAnotherSchema { index_name, schema_name, table_name })
                    if index_name == "i" && schema_name == schema && table_name == table
            ),
            "{sql}: {result:?}"
        );
    }
}

/// `IF NOT EXISTS` skips a name already taken, and never a qualifier no engine
/// accepts.
#[test]
fn if_not_exists_does_not_excuse_another_schema() {
    let result = ParserDB::parse::<GenericDialect>(
        "CREATE SCHEMA app; CREATE SCHEMA other; CREATE TABLE app.t (id INT);
         CREATE INDEX i ON app.t (id);
         CREATE INDEX IF NOT EXISTS other.i ON app.t (id);",
    );

    assert!(matches!(result, Err(Error::IndexInAnotherSchema { .. })), "{result:?}");
}
