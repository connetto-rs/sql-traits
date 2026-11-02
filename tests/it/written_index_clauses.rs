//! Tests that an index reports the uniqueness, the access method, the
//! `INCLUDE` columns and the partial-index predicate its statement wrote.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Ident, IndexType},
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect},
    parser::Parser,
};

fn build(dialect: &dyn Dialect, ddl: &str) -> ParserDB {
    let statements = Parser::parse_sql(dialect, ddl).expect("SQL parses");
    ParserDB::from_statements(statements, "test".to_string()).expect("schema builds")
}

fn index<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Index {
    database.indexes().find(|index| index.name() == Some(name)).expect("the index exists")
}

fn unique_index<'db>(
    database: &'db ParserDB,
    name: &str,
) -> &'db <ParserDB as DatabaseLike>::UniqueIndex {
    database
        .tables()
        .flat_map(|table| table.unique_indices(database).expect("the table is in this database"))
        .find(|index| index.name() == Some(name))
        .expect("the unique index exists")
}

fn included<I: IndexLike<DB = ParserDB>>(database: &ParserDB, index: &I) -> Vec<String> {
    index
        .include_columns(database)
        .expect("the index is in this database")
        .map(|column| column.column_name().to_owned())
        .collect()
}

/// A `CREATE UNIQUE INDEX` is listed among the table's indexes rather than
/// its key constraints, so only the index itself can say it is unique.
#[test]
fn an_index_reports_whether_it_enforces_uniqueness() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT PRIMARY KEY, a INT UNIQUE, b INT, c INT,
             CONSTRAINT u UNIQUE (b));
         CREATE UNIQUE INDEX unique_c ON t (c);
         CREATE INDEX plain_c ON t (c);",
    );
    assert!(index(&database, "unique_c").is_unique());
    assert!(!index(&database, "plain_c").is_unique());
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let keys: Vec<bool> = table
        .unique_indices(&database)
        .expect("t is in this database")
        .map(IndexLike::is_unique)
        .collect();
    assert_eq!(keys, [true, true, true]);
}

/// An index written without `USING` takes the engine default, which this
/// catalog keeps apart from a method the statement named, `btree` included.
#[test]
fn an_index_reports_the_method_it_wrote_and_none_for_the_default() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT);
         CREATE INDEX unwritten ON t (a);
         CREATE INDEX btree ON t USING btree (a);
         CREATE INDEX gin ON t USING gin (a);
         CREATE INDEX custom ON t USING ivfflat (a);",
    );
    assert_eq!(index(&database, "unwritten").access_method(), None);
    assert_eq!(index(&database, "btree").access_method(), Some(&IndexType::BTree));
    assert_eq!(index(&database, "gin").access_method(), Some(&IndexType::GIN));
    assert_eq!(
        index(&database, "custom").access_method(),
        Some(&IndexType::Custom(Ident::new("ivfflat")))
    );
}

/// MySQL also accepts the method after the column list, among options such as
/// `COMMENT`, and a later clause overrides an earlier one.
#[test]
fn a_mysql_index_reports_the_last_method_it_wrote() {
    let database = build(
        &MySqlDialect {},
        "CREATE TABLE t (id INT, a INT);
         CREATE INDEX after ON t (a) USING HASH;
         CREATE INDEX twice USING HASH ON t (a) USING BTREE;
         CREATE INDEX commented ON t (a) USING HASH COMMENT 'hot';
         CREATE INDEX comment_only USING BTREE ON t (a) COMMENT 'hot';",
    );
    assert_eq!(index(&database, "after").access_method(), Some(&IndexType::Hash));
    assert_eq!(index(&database, "twice").access_method(), Some(&IndexType::BTree));
    assert_eq!(index(&database, "commented").access_method(), Some(&IndexType::Hash));
    assert_eq!(index(&database, "comment_only").access_method(), Some(&IndexType::BTree));
}

/// A key constraint is backed by an index, and the method it wrote is that
/// index's, the primary key included.
#[test]
fn a_key_constraint_reports_the_method_it_wrote() {
    let database = build(
        &MySqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT, c INT,
             CONSTRAINT pk PRIMARY KEY (id) USING BTREE,
             UNIQUE KEY before USING HASH (a),
             UNIQUE KEY after (b) USING BTREE,
             UNIQUE KEY unwritten (c));",
    );
    assert_eq!(unique_index(&database, "pk").access_method(), Some(&IndexType::BTree));
    assert_eq!(unique_index(&database, "before").access_method(), Some(&IndexType::Hash));
    assert_eq!(unique_index(&database, "after").access_method(), Some(&IndexType::BTree));
    assert_eq!(unique_index(&database, "unwritten").access_method(), None);
}

/// The `INCLUDE` list resolves to the table's columns in the order it was
/// written, folding an unquoted name as the creation did.
#[test]
fn an_index_reports_its_include_columns_in_written_order() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT, c INT);
         CREATE INDEX covering ON t (a) INCLUDE (C, b);
         CREATE INDEX plain ON t (a);",
    );
    assert_eq!(included(&database, index(&database, "covering")), ["c", "b"]);
    assert_eq!(included(&database, index(&database, "plain")), [""; 0]);
}

#[test]
fn a_key_constraint_reports_its_include_columns() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT,
             CONSTRAINT pk PRIMARY KEY (id) INCLUDE (b),
             CONSTRAINT u UNIQUE (a) INCLUDE (id, b),
             CONSTRAINT plain UNIQUE (b));",
    );
    assert_eq!(included(&database, unique_index(&database, "pk")), ["b"]);
    assert_eq!(included(&database, unique_index(&database, "u")), ["id", "b"]);
    assert_eq!(included(&database, unique_index(&database, "plain")), [""; 0]);
}

/// A constraint's `INCLUDE` list is plain identifiers, so it needs following
/// on a column rename apart from the expressions the walker reaches.
#[test]
fn a_key_constraint_include_list_follows_a_renamed_column() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT,
             CONSTRAINT pk PRIMARY KEY (id) INCLUDE (b),
             CONSTRAINT u UNIQUE (a) INCLUDE (b));
         ALTER TABLE t RENAME COLUMN b TO renamed;",
    );
    assert_eq!(included(&database, unique_index(&database, "pk")), ["renamed"]);
    assert_eq!(included(&database, unique_index(&database, "u")), ["renamed"]);
}

/// PostgreSQL drops a constraint whose index carries a dropped column, the
/// `INCLUDE` list counting like the key.
#[test]
fn a_key_constraint_goes_with_a_column_it_includes() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT,
             CONSTRAINT pk PRIMARY KEY (id) INCLUDE (b),
             CONSTRAINT u UNIQUE (a) INCLUDE (b));
         ALTER TABLE t DROP COLUMN b;",
    );
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(table.unique_indices(&database).expect("t is in this database").count(), 0);
}

#[test]
fn a_partial_index_reports_its_predicate() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT, CONSTRAINT u UNIQUE (a));
         CREATE INDEX partial ON t (a) WHERE b > 0 AND id IS NOT NULL;
         CREATE INDEX whole ON t (a);",
    );
    assert_eq!(
        index(&database, "partial").predicate().map(ToString::to_string).as_deref(),
        Some("b > 0 AND id IS NOT NULL")
    );
    assert_eq!(index(&database, "whole").predicate(), None);
    assert_eq!(unique_index(&database, "u").predicate(), None);
}

#[test]
fn a_partial_index_predicate_follows_a_renamed_column() {
    let database = build(
        &PostgreSqlDialect {},
        "CREATE TABLE t (id INT, a INT, b INT);
         CREATE INDEX partial ON t (a) WHERE b > 0;
         ALTER TABLE t RENAME COLUMN b TO renamed;",
    );
    assert_eq!(
        index(&database, "partial").predicate().map(ToString::to_string).as_deref(),
        Some("renamed > 0")
    );
}
