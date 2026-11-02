//! Tests that renaming a table carries every reference to it along.
//!
//! Dropping a table already swept its indexes, policies, triggers and
//! permissions, and refused while another table's foreign key pointed at it.
//! Renaming one changed only the table's own node, so everything pointing at it
//! kept naming a table that no longer existed. The failures surfaced far from
//! the rename, as an index reporting a table absent from the database or a
//! foreign key target that no longer resolved.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

#[test]
fn an_index_follows_the_rename() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE INDEX i ON t (a);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("t exists when the index and the rename land");

    let index = database.indexes().next().expect("the index survives the rename");
    let table = IndexLike::table(index, &database);
    assert_eq!(table.table_name(), "t2");
    assert_eq!(
        table.indices(&database).expect("the index host is in this database").count(),
        1,
        "the index is reachable from the renamed table, so it was re-attached rather than orphaned"
    );
}

#[test]
fn a_policy_follows_the_rename() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         CREATE POLICY p ON t USING (true);
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("t exists when the policy and the rename land");

    let policy = database.policies().next().expect("the policy survives the rename");
    let table = policy.table(&database).expect("the rename carried the policy along");
    assert_eq!(table.table_name(), "t2");
}

#[test]
fn a_permission_follows_the_rename() {
    let database = parse(
        "CREATE ROLE app;
         CREATE TABLE t (id INT PRIMARY KEY);
         GRANT SELECT ON t TO app;
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("t exists when the grant and the rename land");

    assert_eq!(
        database.unresolved_access_references().expect("grant targets are well formed").count(),
        0,
        "the grant names the renamed table, so nothing is left dangling"
    );
}

#[test]
fn a_child_foreign_key_follows_the_rename_of_its_parent() {
    let database = parse(
        "CREATE TABLE parent (id INT PRIMARY KEY);
         CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent (id));
         ALTER TABLE parent RENAME TO parent2;",
    )
    .expect("parent exists when the child and the rename land");

    // Parsing at all proves the target resolves, since a dangling reference is
    // refused as it is read. What is worth pinning here is the new name.

    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("child was created");
    let foreign_key =
        child.foreign_keys(&database).expect("child is in this database").next().expect("one key");
    assert_eq!(
        foreign_key
            .referenced_table(&database)
            .expect("the target resolves after the rename")
            .table_name(),
        "parent2"
    );
}

/// A foreign key written as a table constraint rather than inline on the column
/// lives in a different part of the node, and both spellings have to follow.
#[test]
fn a_table_constraint_foreign_key_follows_the_rename() {
    let database = parse(
        "CREATE TABLE parent (id INT PRIMARY KEY);
         CREATE TABLE child (
             id INT PRIMARY KEY,
             pid INT,
             CONSTRAINT fk FOREIGN KEY (pid) REFERENCES parent (id)
         );
         ALTER TABLE parent RENAME TO parent2;",
    )
    .expect("parent exists when the child and the rename land");

    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("child was created");
    let foreign_key =
        child.foreign_keys(&database).expect("child is in this database").next().expect("one key");
    assert_eq!(foreign_key.referenced_table_name().name(), "parent2");
}

/// The renamed table's own foreign key back to itself is rewritten in the same
/// pass that changes its name, since rebuilding the node resolves its targets.
#[test]
fn a_self_referential_foreign_key_follows_the_rename() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, parent INT REFERENCES t (id));
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("the self reference resolves against the renamed table");

    let table = database
        .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t2 exists after the rename");
    let foreign_key =
        table.foreign_keys(&database).expect("t2 is in this database").next().expect("one key");
    assert_eq!(foreign_key.referenced_table_name().name(), "t2");
    assert_eq!(table.foreign_keys(&database).expect("t2 is in this database").count(), 1);
}

/// `RENAME TO` names the table without a schema and cannot move it, so the
/// schema the table already carried has to survive.
#[test]
fn a_qualified_table_keeps_its_schema() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.t (id INT PRIMARY KEY);
         ALTER TABLE s.t RENAME TO t2;",
    )
    .expect("s.t exists");

    let table = database.tables().next().expect("one table");
    assert_eq!((table.table_schema(), table.table_name()), (Some("s"), "t2"));
}

#[test]
fn a_qualified_table_carries_its_references_within_the_schema() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.parent (id INT PRIMARY KEY);
         CREATE TABLE s.child (id INT PRIMARY KEY, pid INT REFERENCES s.parent (id));
         CREATE POLICY p ON s.parent USING (true);
         ALTER TABLE s.parent RENAME TO parent2;",
    )
    .expect("s.parent exists");

    let child = database
        .table_by_target(
            TargetName::new("child", false).with_schema("s", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("s.child was created");
    let foreign_key =
        child.foreign_keys(&database).expect("child is in this database").next().expect("one key");
    assert_eq!(foreign_key.referenced_table_name().name(), "parent2");
    let policy = database.policies().next().expect("the policy survives");
    let table = policy.table(&database).expect("the policy target resolves");
    assert_eq!((table.table_schema(), table.table_name()), (Some("s"), "parent2"));
}

/// `RENAME TABLE` may name a schema on both sides, which does move the table,
/// and then no part of the old spelling of a reference resolves any more.
#[test]
fn a_rename_across_schemas_requalifies_its_references() {
    let database = ParserDB::parse::<MySqlDialect>(
        "CREATE SCHEMA a;
         CREATE SCHEMA b;
         CREATE TABLE a.parent (id INT PRIMARY KEY);
         CREATE TABLE a.child (id INT PRIMARY KEY, pid INT REFERENCES a.parent (id));
         RENAME TABLE a.parent TO b.parent;",
    )
    .expect("a.parent exists");

    let parent = database
        .table_by_target(
            TargetName::new("parent", false).with_schema("b", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table moved to b");
    assert_eq!(parent.table_schema(), Some("b"));
    let child = database
        .table_by_target(
            TargetName::new("child", false).with_schema("a", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("a.child stayed put");
    let foreign_key =
        child.foreign_keys(&database).expect("child is in this database").next().expect("one key");
    let target = foreign_key.referenced_table_name();
    assert_eq!(
        (target.schema(), target.name()),
        (Some("b"), "parent"),
        "the child's foreign key names the table in its new schema"
    );
}

/// Dropping a table keeps behaving as it did, including the refusal while
/// another table's foreign key still points at it.
#[test]
fn dropping_a_table_is_unchanged() {
    let referenced = parse(
        "CREATE TABLE parent (id INT PRIMARY KEY);
         CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent (id));
         DROP TABLE parent;",
    )
    .expect_err("a referenced table cannot be dropped");
    assert!(matches!(referenced, Error::TableReferenced { .. }), "got {referenced:?}");

    let swept = parse(
        "CREATE ROLE app;
         CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE INDEX i ON t (a);
         CREATE POLICY p ON t USING (true);
         GRANT SELECT ON t TO app;
         DROP TABLE t;",
    )
    .expect("nothing references t");
    assert_eq!(swept.tables().count(), 0);
    assert_eq!(swept.indexes().count(), 0);
    assert_eq!(swept.policies().count(), 0);
    assert_eq!(
        swept.unresolved_access_references().expect("grant targets are well formed").count(),
        0
    );
}

/// Renaming onto a name another table already holds is a collision the model
/// cannot represent, so it has to be refused rather than silently merged.
#[test]
fn renaming_onto_an_existing_table_is_refused() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         CREATE TABLE u (id INT PRIMARY KEY);
         ALTER TABLE t RENAME TO u;",
    )
    .expect_err("u already exists");
    assert!(
        matches!(error, Error::IdentifierLookupError(LookupError::TableLookupConflict { .. })),
        "got {error:?}"
    );
}

/// Settings that live beside the node rather than in it have to survive the
/// rebuild the rename performs.
#[test]
fn row_level_security_survives_the_rename() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;
         ALTER TABLE t FORCE ROW LEVEL SECURITY;
         ALTER TABLE t RENAME TO t2;",
    )
    .expect("t exists");

    let table = database
        .table_by_target(TargetName::new("t2", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t2 exists after the rename");
    assert!(table.has_row_level_security(&database).expect("t2 is in this database"));
    assert!(table.has_forced_row_level_security(&database).expect("t2 is in this database"));
}
