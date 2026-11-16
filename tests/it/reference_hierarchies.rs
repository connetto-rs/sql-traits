//! Tests that a column takes the extension hierarchy of every column it
//! references, through foreign keys of any width into primary or unique keys,
//! and that its references conflict when two of them name unrelated entities.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the schema parses")
}

fn column<'db>(
    database: &'db ParserDB,
    table_name: &str,
    column_name: &str,
) -> &'db <ParserDB as DatabaseLike>::Column {
    table(database, None, table_name)
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the column exists")
}

fn names<'db>(tables: &[&'db <ParserDB as DatabaseLike>::Table]) -> Vec<&'db str> {
    let mut names: Vec<&str> = tables.iter().map(|&table| table.table_name()).collect();
    names.sort_unstable();
    names
}

/// The extension hierarchy of `table_name.column_name`, as sorted table names.
fn hierarchy<'db>(database: &'db ParserDB, table_name: &str, column_name: &str) -> Vec<&'db str> {
    names(
        &column(database, table_name, column_name)
            .extension_hierarchy(database)
            .expect("every key resolves"),
    )
}

/// The conflicting references of `table_name.column_name`, each as the table
/// its foreign key references and the hierarchy that key contributes.
fn conflicts<'db>(
    database: &'db ParserDB,
    table_name: &str,
    column_name: &str,
) -> Vec<(&'db str, Vec<&'db str>)> {
    let mut conflicts: Vec<(&str, Vec<&str>)> = column(database, table_name, column_name)
        .conflicting_references(database)
        .expect("every key resolves")
        .into_iter()
        .map(|(foreign_key, contribution)| {
            (
                foreign_key.referenced_table(database).expect("the target resolves").table_name(),
                names(&contribution),
            )
        })
        .collect();
    conflicts.sort_unstable();
    conflicts
}

/// The hierarchy of a column that identifies no entity.
const NO_TABLE: [&str; 0] = [];

/// The conflicts of a column whose references name one entity.
const NO_CONFLICT: [(&str, Vec<&str>); 0] = [];

const TENANT_SCOPED: &str = "
CREATE TABLE organizations (id INTEGER PRIMARY KEY);
CREATE TABLE teams (id INTEGER PRIMARY KEY);
CREATE TABLE resources (
    id INTEGER PRIMARY KEY,
    organization_id INTEGER NOT NULL REFERENCES organizations (id),
    UNIQUE (organization_id, id)
);
CREATE TABLE grants (
    id INTEGER PRIMARY KEY,
    resource_id INTEGER NOT NULL,
    organization_id INTEGER NOT NULL,
    UNIQUE (organization_id, id),
    FOREIGN KEY (organization_id, resource_id) REFERENCES resources (organization_id, id)
);
CREATE TABLE grant_notes (
    id INTEGER PRIMARY KEY,
    grant_id INTEGER NOT NULL,
    organization_id INTEGER NOT NULL REFERENCES organizations (id),
    FOREIGN KEY (organization_id, grant_id) REFERENCES grants (organization_id, id)
);
CREATE TABLE team_resources (
    id INTEGER PRIMARY KEY,
    resource_id INTEGER NOT NULL,
    organization_id INTEGER NOT NULL REFERENCES teams (id),
    FOREIGN KEY (organization_id, resource_id) REFERENCES resources (organization_id, id)
);
";

/// A composite key places each of its columns in the hierarchy of the column
/// it references at the same position.
#[test]
fn a_composite_key_carries_the_hierarchy_of_each_referenced_column() {
    let database = parse(TENANT_SCOPED);
    assert_eq!(hierarchy(&database, "grants", "organization_id"), ["organizations"]);
    assert_eq!(hierarchy(&database, "grants", "resource_id"), ["resources"]);
    assert_eq!(conflicts(&database, "grants", "organization_id"), NO_CONFLICT);
    assert_eq!(conflicts(&database, "grants", "resource_id"), NO_CONFLICT);
}

/// The hierarchy follows a chain of composite keys, so a column that also
/// references the organization directly agrees with the chain.
#[test]
fn a_chain_of_composite_keys_reaches_the_root_entity() {
    let database = parse(TENANT_SCOPED);
    assert_eq!(hierarchy(&database, "grant_notes", "organization_id"), ["organizations"]);
    assert_eq!(conflicts(&database, "grant_notes", "organization_id"), NO_CONFLICT);
}

/// A column that references a team directly and an organization through a
/// composite key must be both, two unrelated entities.
#[test]
fn a_direct_key_against_a_composite_key_into_another_entity_conflicts() {
    let database = parse(TENANT_SCOPED);
    assert_eq!(
        conflicts(&database, "team_resources", "organization_id"),
        [("resources", vec!["organizations"]), ("teams", vec!["teams"])],
    );
}

/// A column that alone is a unique key identifies a row of its table, so a
/// key into it carries that table and whatever the column references.
#[test]
fn a_key_into_a_unique_column_carries_its_table_and_its_references() {
    let database = parse(
        "
CREATE TABLE a (id INT PRIMARY KEY);
CREATE TABLE b (id INT PRIMARY KEY, a_id INT UNIQUE REFERENCES a (id));
CREATE TABLE d (id INT PRIMARY KEY, b_a_id INT REFERENCES b (a_id));
",
    );
    assert_eq!(hierarchy(&database, "b", "a_id"), ["a", "b"]);
    assert_eq!(hierarchy(&database, "d", "b_a_id"), ["a", "b"]);
    assert_eq!(conflicts(&database, "d", "b_a_id"), NO_CONFLICT);
}

/// Each key of a column counts on its own, so referencing the table under
/// check does not hide a second key into an unrelated entity.
#[test]
fn two_keys_into_unrelated_entities_conflict() {
    let database = parse(
        "
CREATE TABLE a (id INT PRIMARY KEY);
CREATE TABLE b (id INT PRIMARY KEY, code INT UNIQUE);
CREATE TABLE d (id INT PRIMARY KEY, x INT REFERENCES a (id), FOREIGN KEY (x) REFERENCES b (id));
CREATE TABLE e (id INT PRIMARY KEY, x INT REFERENCES a (id), FOREIGN KEY (x) REFERENCES b (code));
",
    );
    assert_eq!(hierarchy(&database, "d", "x"), ["a", "b"]);
    assert_eq!(conflicts(&database, "d", "x"), [("a", vec!["a"]), ("b", vec!["b"])]);
    assert_eq!(conflicts(&database, "e", "x"), [("a", vec!["a"]), ("b", vec!["b"])]);
}

/// Keys into two extensions of one root agree on the root, and an extension's
/// key does not conflict with the key it extends.
#[test]
fn keys_into_extensions_of_one_root_agree() {
    let database = parse(
        "
CREATE TABLE root (id SERIAL PRIMARY KEY);
CREATE TABLE left_root (id INT PRIMARY KEY REFERENCES root (id));
CREATE TABLE right_root (id INT PRIMARY KEY REFERENCES root (id));
CREATE TABLE left_child (id INT PRIMARY KEY REFERENCES left_root (id));
CREATE TABLE right_child (id INT PRIMARY KEY REFERENCES right_root (id));
CREATE TABLE other (id INT PRIMARY KEY);
CREATE TABLE mytable (
    id INT PRIMARY KEY,
    other_id INT,
    FOREIGN KEY (other_id) REFERENCES left_child (id),
    FOREIGN KEY (other_id) REFERENCES right_child (id)
);
CREATE TABLE stray (
    id INT PRIMARY KEY,
    left_id INT REFERENCES left_child (id),
    FOREIGN KEY (left_id) REFERENCES other (id)
);
",
    );
    assert_eq!(hierarchy(&database, "left_child", "id"), ["left_child", "left_root", "root"]);
    assert_eq!(conflicts(&database, "left_child", "id"), NO_CONFLICT);
    assert_eq!(
        hierarchy(&database, "mytable", "other_id"),
        ["left_child", "left_root", "right_child", "right_root", "root"],
    );
    assert_eq!(conflicts(&database, "mytable", "other_id"), NO_CONFLICT);
    assert_eq!(
        conflicts(&database, "stray", "left_id"),
        [("left_child", vec!["left_child", "left_root", "root"]), ("other", vec!["other"])],
    );
}

/// A column of a composite key, or one a partial or expression unique index
/// covers, does not identify a row on its own, so it is in no hierarchy.
#[test]
fn a_column_that_is_not_a_key_on_its_own_carries_no_table() {
    let database = parse(
        "
CREATE TABLE pair (a INT, b INT, PRIMARY KEY (a, b));
CREATE TABLE codes (id INT PRIMARY KEY, partial_code INT, lowered TEXT, plain INT);
CREATE UNIQUE INDEX codes_partial ON codes (partial_code) WHERE partial_code > 0;
CREATE UNIQUE INDEX codes_lowered ON codes (lower(lowered));
",
    );
    assert_eq!(hierarchy(&database, "pair", "a"), NO_TABLE);
    assert_eq!(hierarchy(&database, "codes", "partial_code"), NO_TABLE);
    assert_eq!(hierarchy(&database, "codes", "lowered"), NO_TABLE);
    assert_eq!(hierarchy(&database, "codes", "plain"), NO_TABLE);
    assert_eq!(hierarchy(&database, "codes", "id"), ["codes"]);
}

/// A cycle of keys ends, and every table on it is in the hierarchy.
#[test]
fn a_cycle_of_keys_terminates() {
    let database = parse(
        "
CREATE TABLE loop_self (id INT PRIMARY KEY REFERENCES loop_self (id));
CREATE TABLE ping (id INT PRIMARY KEY, code INT UNIQUE);
CREATE TABLE pong (id INT PRIMARY KEY, code INT UNIQUE REFERENCES ping (code));
ALTER TABLE ping ADD FOREIGN KEY (code) REFERENCES pong (code);
",
    );
    assert_eq!(hierarchy(&database, "loop_self", "id"), ["loop_self"]);
    assert_eq!(conflicts(&database, "loop_self", "id"), NO_CONFLICT);
    assert_eq!(hierarchy(&database, "ping", "code"), ["ping", "pong"]);
    assert_eq!(hierarchy(&database, "pong", "code"), ["ping", "pong"]);
    assert_eq!(conflicts(&database, "pong", "code"), NO_CONFLICT);
}
