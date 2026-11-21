//! Tests that a table answers SQLite's `STRICT` and `WITHOUT ROWID` options as
//! `pragma_table_list` reports them in its `strict` and `wr` columns.
//!
//! Every expectation was measured on SQLite 3.51.1. The shadow tables are read
//! from the statements SQLite stores for them.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

/// The statements of one SQLite connection, with the shadow tables as SQLite
/// stores them.
const SCRIPT: &str = "CREATE TABLE plain(k TEXT PRIMARY KEY);
CREATE TABLE typed(k TEXT PRIMARY KEY) STRICT;
CREATE TABLE keyed(k TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE both(k TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
create table mixed(k text primary key) without rowid, sTrIcT;
CREATE TABLE repeated(k INT PRIMARY KEY), STRICT, WITHOUT ROWID, STRICT;
CREATE TEMP TABLE temporary(k TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
ATTACH ':memory:' AS aux;
CREATE TABLE aux.attached(k TEXT PRIMARY KEY) WITHOUT ROWID, STRICT;
ALTER TABLE both RENAME TO renamed;
ALTER TABLE renamed RENAME COLUMN k TO key;
ALTER TABLE renamed ADD COLUMN extra TEXT;
ALTER TABLE renamed DROP COLUMN extra;
CREATE VIRTUAL TABLE notes USING fts5(body);
CREATE TABLE 'notes_data'(id INTEGER PRIMARY KEY, block BLOB);
CREATE TABLE 'notes_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE 'notes_content'(id INTEGER PRIMARY KEY, c0);
CREATE TABLE 'notes_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE 'notes_config'(k PRIMARY KEY, v) WITHOUT ROWID;
CREATE VIRTUAL TABLE boxes USING rtree(id, min_x, max_x);
CREATE TABLE \"boxes_rowid\"(rowid INTEGER PRIMARY KEY,nodeno);
CREATE TABLE \"boxes_node\"(nodeno INTEGER PRIMARY KEY,data);
CREATE TABLE \"boxes_parent\"(nodeno INTEGER PRIMARY KEY,parentnode);";

/// The `schema`, `name`, `strict` and `wr` rows `pragma_table_list` returns
/// for [`SCRIPT`].
const MEASURED: [(&str, &str, bool, bool); 17] = [
    ("aux", "attached", true, true),
    ("main", "boxes", false, false),
    ("main", "boxes_node", false, false),
    ("main", "boxes_parent", false, false),
    ("main", "boxes_rowid", false, false),
    ("main", "keyed", false, true),
    ("main", "mixed", true, true),
    ("main", "notes", false, false),
    ("main", "notes_config", false, true),
    ("main", "notes_content", false, false),
    ("main", "notes_data", false, false),
    ("main", "notes_docsize", false, false),
    ("main", "notes_idx", false, true),
    ("main", "plain", false, false),
    ("main", "renamed", true, true),
    ("main", "typed", true, false),
    ("temp", "temporary", true, true),
];

#[test]
fn a_table_answers_strict_and_without_rowid_as_sqlite_lists_them() {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(SCRIPT)
        .expect("SQLite accepts the script");
    for (schema, name, strict, without_rowid) in MEASURED {
        let table = database
            .table_by_target(
                TargetName::new(name, false).with_schema(schema, false),
                IdentifierCase::Folded,
            )
            .expect("unambiguous lookup")
            .expect("the table exists");
        assert_eq!(
            (table.is_strict(), table.is_without_rowid()),
            (strict, without_rowid),
            "{schema}.{name}",
        );
    }
}

#[test]
fn a_postgresql_table_declares_no_sqlite_option() {
    let database = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (k TEXT PRIMARY KEY);")
        .expect("PostgreSQL accepts the table");
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    assert!(!table.is_strict());
    assert!(!table.is_without_rowid());
}
