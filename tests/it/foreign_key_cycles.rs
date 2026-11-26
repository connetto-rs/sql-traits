//! Tests that `depends_on` and `dependent_tables` answer on a schema whose
//! foreign keys form a cycle of more than one table, which PostgreSQL accepts
//! once an `ALTER TABLE` adds the key closing the cycle.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

const SCHEMA: &str = "
CREATE TABLE owners (id INT PRIMARY KEY);
CREATE TABLE users (id INT PRIMARY KEY, namespace_id INT, owner_id INT REFERENCES owners (id));
CREATE TABLE namespaces (id INT PRIMARY KEY, user_id INT REFERENCES users (id));
ALTER TABLE users ADD FOREIGN KEY (namespace_id) REFERENCES namespaces (id);
CREATE TABLE posts (id INT PRIMARY KEY, namespace_id INT REFERENCES namespaces (id));
CREATE TABLE first_ring (id INT PRIMARY KEY, next_id INT);
CREATE TABLE second_ring (id INT PRIMARY KEY, next_id INT REFERENCES first_ring (id));
CREATE TABLE third_ring (id INT PRIMARY KEY, next_id INT REFERENCES second_ring (id));
ALTER TABLE first_ring ADD FOREIGN KEY (next_id) REFERENCES third_ring (id);
CREATE TABLE unrelated (id INT PRIMARY KEY);
";

fn depends_on(database: &ParserDB, dependent: &str, dependency: &str) -> Result<bool, Error> {
    Ok(table(database, None, dependent).depends_on(database, table(database, None, dependency))?)
}

#[test]
fn two_table_cycle_members_depend_on_each_other() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    assert!(depends_on(&database, "users", "namespaces")?);
    assert!(depends_on(&database, "namespaces", "users")?);
    Ok(())
}

#[test]
fn cycle_member_does_not_depend_on_a_table_outside_its_reach() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    assert!(!depends_on(&database, "users", "unrelated")?);
    assert!(!depends_on(&database, "namespaces", "posts")?);
    assert!(!depends_on(&database, "first_ring", "unrelated")?);
    Ok(())
}

#[test]
fn cycle_member_depends_on_a_table_another_member_references() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    assert!(depends_on(&database, "namespaces", "owners")?);
    assert!(depends_on(&database, "posts", "owners")?);
    Ok(())
}

#[test]
fn three_table_cycle_members_depend_on_every_member() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    for dependent in ["first_ring", "second_ring", "third_ring"] {
        for dependency in ["first_ring", "second_ring", "third_ring"] {
            assert!(depends_on(&database, dependent, dependency)?, "{dependent} on {dependency}");
        }
    }
    Ok(())
}

#[test]
fn dependent_tables_lists_every_table_reaching_through_a_cycle() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let mut dependent_tables: Vec<&str> = table(&database, None, "owners")
        .dependent_tables(&database)?
        .map(TableLike::table_name)
        .collect();
    dependent_tables.sort_unstable();
    assert_eq!(dependent_tables, ["namespaces", "posts", "users"]);
    Ok(())
}
