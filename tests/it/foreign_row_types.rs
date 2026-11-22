//! Foreign table row types participate in type dependencies.
#![expect(clippy::expect_used, reason = "Tests require successful model construction")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const SETUP: &str = "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER sv FOREIGN DATA WRAPPER w; CREATE FOREIGN TABLE p (id int) SERVER sv;";

fn parse(statement: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SETUP} {statement}"))
}

fn assert_refusal(statement: &str, sqlstate: &str, message: &str) {
    let error = parse(statement).expect_err("dependent object prevents statement");
    let postgres = error.postgres_error().expect("PostgreSQL diagnostic");
    assert_eq!(postgres.sqlstate().to_string(), sqlstate);
    assert_eq!(postgres.message(), message);
}

#[test]
fn columns_and_arrays_prevent_restricted_drop() {
    for data_type in ["p", "p[]"] {
        for restriction in ["", " RESTRICT"] {
            assert_refusal(
                &format!("CREATE TABLE x (col {data_type}); DROP FOREIGN TABLE p{restriction};"),
                "2BP01",
                "cannot drop foreign table p because other objects depend on it",
            );
        }
    }
}

#[test]
fn cascading_drop_removes_dependent_columns() -> Result<(), Error> {
    let database =
        parse("CREATE TABLE x (col p, arr p[], keep int); DROP FOREIGN TABLE p CASCADE;")?;
    let table = database.tables().next().expect("dependent table remains");
    assert_eq!(
        table.columns(&database)?.map(|column| column.column_name().to_owned()).collect::<Vec<_>>(),
        ["keep"]
    );
    assert_eq!(database.foreign_tables().count(), 0);
    Ok(())
}

#[test]
fn rename_updates_column_types() -> Result<(), Error> {
    let database = parse("CREATE TABLE x (col p, arr p[]); ALTER FOREIGN TABLE p RENAME TO q;")?;
    let table = database.tables().next().expect("dependent table");
    let types = table
        .columns(&database)?
        .map(|column| column.data_type(&database).to_string())
        .collect::<Vec<_>>();
    assert_eq!(types, ["q", "q[]"]);
    assert_eq!(database.foreign_tables().next().expect("renamed table").table_name(), "q");
    Ok(())
}

#[test]
fn schema_move_updates_column_types() -> Result<(), Error> {
    let database = parse(
        "CREATE SCHEMA s; CREATE TABLE x (col p, arr p[]); ALTER FOREIGN TABLE p SET SCHEMA s;",
    )?;
    let table = database.tables().next().expect("dependent table");
    let types = table
        .columns(&database)?
        .map(|column| column.data_type(&database).to_string())
        .collect::<Vec<_>>();
    assert_eq!(types, ["s.p", "s.p[]"]);
    Ok(())
}

#[test]
fn composite_type_cannot_take_row_type_name() {
    assert_refusal("CREATE TYPE p AS (value int);", "42710", "type \"p\" already exists");
}

#[test]
fn domain_prevents_restricted_drop() {
    assert_refusal(
        "CREATE DOMAIN d AS p; CREATE TABLE x (col d); DROP FOREIGN TABLE p;",
        "2BP01",
        "cannot drop foreign table p because other objects depend on it",
    );
}

#[test]
fn cascading_drop_removes_domain_and_its_columns() -> Result<(), Error> {
    let database = parse(
        "CREATE DOMAIN d AS p; CREATE TABLE x (col d, keep int); DROP FOREIGN TABLE p CASCADE;",
    )?;
    let table = database.tables().next().expect("dependent table remains");
    assert_eq!(
        table.columns(&database)?.map(|column| column.column_name().to_owned()).collect::<Vec<_>>(),
        ["keep"]
    );
    assert_eq!(database.domains().count(), 0);
    Ok(())
}

#[test]
fn ordinary_row_type_dependency_uses_the_same_drop_path() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE p (id int); CREATE TABLE x (col p); DROP TABLE p;",
    )
    .expect_err("row type dependency prevents drop");
    let postgres = error.postgres_error().expect("PostgreSQL diagnostic");
    assert_eq!(postgres.sqlstate().to_string(), "2BP01");
    assert_eq!(postgres.message(), "cannot drop table p because other objects depend on it");
}

#[test]
fn renamed_and_moved_row_types_keep_dependencies() {
    assert_refusal(
        "CREATE TABLE x(col p[]); ALTER FOREIGN TABLE p RENAME TO q; DROP FOREIGN TABLE q;",
        "2BP01",
        "cannot drop foreign table q because other objects depend on it",
    );
    assert_refusal(
        "CREATE SCHEMA s; CREATE TABLE x(col p[]); ALTER FOREIGN TABLE p SET SCHEMA s; DROP FOREIGN TABLE s.p;",
        "2BP01",
        "cannot drop foreign table s.p because other objects depend on it",
    );
}

#[test]
fn array_domain_prevents_restricted_drop() {
    assert_refusal(
        "CREATE DOMAIN d AS p[]; CREATE TABLE x(col d); DROP FOREIGN TABLE p RESTRICT;",
        "2BP01",
        "cannot drop foreign table p because other objects depend on it",
    );
}

#[test]
fn dropping_both_row_type_and_dependent_is_allowed() -> Result<(), Error> {
    for targets in ["p, x", "x, p"] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE p(id int); CREATE TABLE x(col p); DROP TABLE {targets};"
        ))?;
        assert_eq!(database.tables().count(), 0);
    }
    Ok(())
}

#[test]
fn row_type_cannot_be_dropped_independently() {
    assert_refusal(
        "DROP TYPE p;",
        "2BP01",
        "cannot drop type p because foreign table p requires it",
    );
    assert_refusal("CREATE DOMAIN p AS int;", "42710", "type \"p\" already exists");
}

#[test]
fn dropping_both_foreign_row_type_and_dependent_is_allowed() -> Result<(), Error> {
    for targets in ["p, x", "x, p"] {
        let database = parse(&format!(
            "CREATE FOREIGN TABLE x(col p) SERVER sv; DROP FOREIGN TABLE {targets};"
        ))?;
        assert_eq!(database.foreign_tables().count(), 0);
    }
    Ok(())
}
