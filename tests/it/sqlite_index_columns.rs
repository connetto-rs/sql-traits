//! Tests that an index answers the record SQLite stores for each of its rows
//! as `pragma_index_xinfo` reports it. Every expectation is the
//! `cid|name|desc|coll|key` output of SQLite 3.51.1 for the same statements.
#![allow(clippy::expect_used)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn build(ddl: &str) -> ParserDB {
    ParserDB::parse::<SQLiteDialect>(ddl).expect("schema builds")
}

/// Whether SQLite builds an index for `index`.
fn builds_index<I: TableIndexLike<DB = ParserDB>>(database: &ParserDB, index: &I) -> bool {
    index.sqlite_index_columns(database).expect("the index is in this database").is_some()
}

/// The entries of the index SQLite builds for `index`, each rendered as the
/// `cid|name|desc|coll|key` row `pragma_index_xinfo` prints for it.
fn rows<I: TableIndexLike<DB = ParserDB>>(database: &ParserDB, index: &I) -> Vec<String> {
    let columns: Vec<_> = TableIndexLike::table(index, database)
        .columns(database)
        .expect("the table is in this database")
        .collect();
    let entries = index
        .sqlite_index_columns(database)
        .expect("the index is in this database")
        .expect("SQLite builds the index");
    entries
        .map(|entry| {
            let (cid, name) = match entry.indexed() {
                SqliteIndexed::Column(column) => {
                    let cid = columns
                        .iter()
                        .position(|declared| core::ptr::eq(*declared, column))
                        .expect("the column belongs to the indexed table");
                    (cid.to_string(), column.column_name())
                }
                SqliteIndexed::Expression(_) => ("-2".to_owned(), ""),
                SqliteIndexed::Rowid => ("-1".to_owned(), ""),
            };
            format!(
                "{cid}|{name}|{}|{}|{}",
                u8::from(entry.is_descending()),
                entry.collation(),
                u8::from(entry.is_key()),
            )
        })
        .collect()
}

/// The index `CREATE INDEX` named `name`.
fn index<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Index {
    database.indexes().find(|index| index.name() == Some(name)).expect("the index exists")
}

/// The key constraint at `position` among those of `table`, in the order the
/// table declares them, column constraints first.
fn constraint<'db>(
    database: &'db ParserDB,
    table: &str,
    position: usize,
) -> &'db <ParserDB as DatabaseLike>::UniqueIndex {
    database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
        .unique_indices(database)
        .expect("the table is in this database")
        .nth(position)
        .expect("the constraint exists")
}

/// The rows of the index `CREATE INDEX` named `name`.
fn index_rows(database: &ParserDB, name: &str) -> Vec<String> {
    rows(database, index(database, name))
}

/// The rows of the index SQLite builds for the key constraint at `position`
/// among those of `table`.
fn constraint_rows(database: &ParserDB, table: &str, position: usize) -> Vec<String> {
    rows(database, constraint(database, table, position))
}

fn expected(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|row| (*row).to_owned()).collect()
}

/// A rowid table appends the rowid to every index, and a key holding a
/// column compares under its declared collation unless one is written.
#[test]
fn rowid_table_indexes_append_the_rowid() {
    let database = build(
        "CREATE TABLE r(id INTEGER PRIMARY KEY, label TEXT COLLATE NOCASE, n INT, UNIQUE(label));
         CREATE INDEX plain ON r(label DESC);
         CREATE INDEX expr ON r(lower(label), lower(label) COLLATE RTRIM, (label), +label, label || '', (label COLLATE NOCASE) || '');
         CREATE INDEX partial ON r(label COLLATE BINARY) WHERE n > 0;",
    );
    assert_eq!(index_rows(&database, "plain"), expected(&["1|label|1|NOCASE|1", "-1||0|BINARY|0"]));
    assert_eq!(
        index_rows(&database, "expr"),
        expected(&[
            "-2||0|BINARY|1",
            "-2||0|RTRIM|1",
            "1|label|0|NOCASE|1",
            "-2||0|BINARY|1",
            "-2||0|BINARY|1",
            "-2||0|BINARY|1",
            "-1||0|BINARY|0",
        ])
    );
    assert_eq!(
        index_rows(&database, "partial"),
        expected(&["1|label|0|BINARY|1", "-1||0|BINARY|0"])
    );
    // The rowid alias is the rowid itself, so SQLite builds no index for it.
    assert!(!builds_index(&database, constraint(&database, "r", 0)));
    assert_eq!(
        constraint_rows(&database, "r", 1),
        expected(&["1|label|0|NOCASE|1", "-1||0|BINARY|0"])
    );
}

/// A key holds a column through any parentheses and `COLLATE`, and compares
/// under the outermost `COLLATE`. An index may key the rowid alias by name.
#[test]
fn keys_hold_columns_through_parentheses_and_collations() {
    let database = build(
        "CREATE TABLE r(id INTEGER PRIMARY KEY, label TEXT COLLATE NOCASE);
         CREATE INDEX rid ON r(id);
         CREATE INDEX nested ON r((label COLLATE RTRIM), ((lower(label)) COLLATE NOCASE), lower(label COLLATE RTRIM));
         CREATE INDEX stacked ON r(((label) COLLATE NOCASE) COLLATE rtrim DESC);",
    );
    assert_eq!(index_rows(&database, "rid"), expected(&["0|id|0|BINARY|1", "-1||0|BINARY|0"]));
    assert_eq!(
        index_rows(&database, "nested"),
        expected(&["1|label|0|RTRIM|1", "-2||0|NOCASE|1", "-2||0|BINARY|1", "-1||0|BINARY|0"])
    );
    assert_eq!(
        index_rows(&database, "stacked"),
        expected(&["1|label|1|rtrim|1", "-1||0|BINARY|0"])
    );
}

/// A `WITHOUT ROWID` table appends the primary key entries no key holds under
/// the same collation, in the primary key's direction for `CREATE INDEX` and
/// ascending for a key constraint. Its primary key carries every other column
/// under `BINARY`.
#[test]
fn without_rowid_indexes_append_the_primary_key() {
    let database = build(
        "CREATE TABLE w(a TEXT COLLATE NOCASE, b INT, c TEXT COLLATE RTRIM, PRIMARY KEY(a DESC,b), UNIQUE(c)) WITHOUT ROWID;
         CREATE INDEX wp ON w(c,a);
         CREATE INDEX wc ON w(a COLLATE BINARY,c DESC);
         CREATE INDEX we ON w(lower(a));
         CREATE UNIQUE INDEX wu ON w(c);
         CREATE INDEX repeated ON w(a,a COLLATE BINARY,b DESC);",
    );
    assert_eq!(
        constraint_rows(&database, "w", 0),
        expected(&["0|a|1|NOCASE|1", "1|b|0|BINARY|1", "2|c|0|BINARY|0"])
    );
    assert_eq!(
        constraint_rows(&database, "w", 1),
        expected(&["2|c|0|RTRIM|1", "0|a|0|NOCASE|0", "1|b|0|BINARY|0"])
    );
    assert_eq!(
        index_rows(&database, "wp"),
        expected(&["2|c|0|RTRIM|1", "0|a|0|NOCASE|1", "1|b|0|BINARY|0"])
    );
    assert_eq!(
        index_rows(&database, "wc"),
        expected(&["0|a|0|BINARY|1", "2|c|1|RTRIM|1", "0|a|1|NOCASE|0", "1|b|0|BINARY|0"])
    );
    assert_eq!(
        index_rows(&database, "we"),
        expected(&["-2||0|BINARY|1", "0|a|1|NOCASE|0", "1|b|0|BINARY|0"])
    );
    assert_eq!(
        index_rows(&database, "wu"),
        expected(&["2|c|0|RTRIM|1", "0|a|1|NOCASE|0", "1|b|0|BINARY|0"])
    );
    assert_eq!(
        index_rows(&database, "repeated"),
        expected(&["0|a|0|NOCASE|1", "0|a|0|BINARY|1", "1|b|1|BINARY|1"])
    );
}

/// A rowid table keeps the declared collations and directions of its key
/// constraints, a repeated column included, and appends the rowid.
#[test]
fn rowid_table_key_constraints_append_the_rowid() {
    let database = build(
        "CREATE TABLE d(a TEXT COLLATE NOCASE,b INT,PRIMARY KEY(a DESC,b),UNIQUE(a COLLATE RTRIM));
         CREATE TABLE e(a TEXT, b INT, PRIMARY KEY(a,b,a));",
    );
    assert_eq!(
        constraint_rows(&database, "d", 0),
        expected(&["0|a|1|NOCASE|1", "1|b|0|BINARY|1", "-1||0|BINARY|0"])
    );
    assert_eq!(constraint_rows(&database, "d", 1), expected(&["0|a|0|RTRIM|1", "-1||0|BINARY|0"]));
    assert_eq!(
        constraint_rows(&database, "e", 0),
        expected(&["0|a|0|BINARY|1", "1|b|0|BINARY|1", "0|a|0|BINARY|1", "-1||0|BINARY|0"])
    );
}

/// `PRIMARY KEY DESC` written on a column sorts descending, and on an
/// `INTEGER` column makes an index rather than the rowid alias.
#[test]
fn a_column_primary_key_keeps_its_direction() {
    let database = build(
        "CREATE TABLE t(a TEXT PRIMARY KEY DESC, b);
         CREATE TABLE i(a INTEGER PRIMARY KEY DESC, b);",
    );
    assert_eq!(constraint_rows(&database, "t", 0), expected(&["0|a|1|BINARY|1", "-1||0|BINARY|0"]));
    assert_eq!(constraint_rows(&database, "i", 0), expected(&["0|a|1|BINARY|1", "-1||0|BINARY|0"]));
}

/// Key constraints listing the same columns under the same collations share
/// one index, which keeps the directions of the first one declared.
#[test]
fn equal_key_constraints_share_one_index() {
    let database = build(
        "CREATE TABLE p(a TEXT PRIMARY KEY, UNIQUE(a));
         CREATE TABLE q(a TEXT, UNIQUE(a DESC), PRIMARY KEY(a));
         CREATE TABLE u(a TEXT COLLATE nocase, b, UNIQUE(a), UNIQUE(a COLLATE NOCASE), UNIQUE(a DESC, b));
         CREATE TABLE w(a TEXT, b INT, UNIQUE(a DESC), PRIMARY KEY(a)) WITHOUT ROWID;",
    );
    let shared = expected(&["0|a|0|BINARY|1", "-1||0|BINARY|0"]);
    assert_eq!(constraint_rows(&database, "p", 0), shared);
    assert_eq!(constraint_rows(&database, "p", 1), shared);
    let shared = expected(&["0|a|1|BINARY|1", "-1||0|BINARY|0"]);
    assert_eq!(constraint_rows(&database, "q", 0), shared);
    assert_eq!(constraint_rows(&database, "q", 1), shared);
    let shared = expected(&["0|a|0|nocase|1", "-1||0|BINARY|0"]);
    assert_eq!(constraint_rows(&database, "u", 0), shared);
    assert_eq!(constraint_rows(&database, "u", 1), shared);
    assert_eq!(
        constraint_rows(&database, "u", 2),
        expected(&["0|a|1|nocase|1", "1|b|0|BINARY|1", "-1||0|BINARY|0"])
    );
    let shared = expected(&["0|a|1|BINARY|1", "1|b|0|BINARY|0"]);
    assert_eq!(constraint_rows(&database, "w", 0), shared);
    assert_eq!(constraint_rows(&database, "w", 1), shared);
}

/// A `WITHOUT ROWID` table builds the index of an `INTEGER PRIMARY KEY` after
/// every other key constraint, so an equal one declared later decides its
/// direction, which every other index then appends.
#[test]
fn a_without_rowid_integer_primary_key_joins_a_later_equal_constraint() {
    let database = build(
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b, UNIQUE(a DESC)) WITHOUT ROWID;
         CREATE INDEX i ON t(b);",
    );
    let shared = expected(&["0|a|1|BINARY|1", "1|b|0|BINARY|0"]);
    assert_eq!(constraint_rows(&database, "t", 0), shared);
    assert_eq!(constraint_rows(&database, "t", 1), shared);
    assert_eq!(index_rows(&database, "i"), expected(&["1|b|0|BINARY|1", "0|a|1|BINARY|0"]));
}

/// The primary key of a `WITHOUT ROWID` table keeps one entry for a column
/// it lists twice under one collation, and carries no virtual column.
#[test]
fn a_without_rowid_primary_key_drops_repeats_and_virtual_columns() {
    let database = build(
        "CREATE TABLE t(a TEXT, b TEXT, PRIMARY KEY(a, b, a)) WITHOUT ROWID;
         CREATE INDEX i ON t(b COLLATE NOCASE);
         CREATE TABLE g(a TEXT, b INT, v INT AS (b+1) VIRTUAL, s INT AS (b+2) STORED, PRIMARY KEY(a)) WITHOUT ROWID;
         CREATE TABLE k(a TEXT, b INT, PRIMARY KEY(a,b), UNIQUE(b,a)) WITHOUT ROWID;",
    );
    assert_eq!(constraint_rows(&database, "t", 0), expected(&["0|a|0|BINARY|1", "1|b|0|BINARY|1"]));
    assert_eq!(
        index_rows(&database, "i"),
        expected(&["1|b|0|NOCASE|1", "0|a|0|BINARY|0", "1|b|0|BINARY|0"])
    );
    assert_eq!(
        constraint_rows(&database, "g", 0),
        expected(&["0|a|0|BINARY|1", "1|b|0|BINARY|0", "3|s|0|BINARY|0"])
    );
    assert_eq!(constraint_rows(&database, "k", 1), expected(&["1|b|0|BINARY|1", "0|a|0|BINARY|1"]));
}

/// An index of another dialect is no SQLite index.
#[test]
fn other_dialects_answer_none() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (a TEXT PRIMARY KEY); CREATE INDEX i ON t (a);",
    )
    .expect("schema builds");
    assert!(!builds_index(&database, index(&database, "i")));
    assert!(!builds_index(&database, constraint(&database, "t", 0)));
}

/// An index another database holds is refused.
#[test]
fn an_index_of_another_database_is_refused() {
    let holder = build("CREATE TABLE t (a TEXT UNIQUE); CREATE INDEX i ON t (a);");
    let other = build("CREATE TABLE t (b TEXT);");
    let index = holder.indexes().next().expect("the index exists");
    assert!(matches!(
        index.sqlite_index_columns(&other).map(|entries| entries.is_some()),
        Err(LookupError::ObjectNotInDatabase { .. })
    ));
    let table = holder
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let constraint = table.unique_indices(&holder).expect("t is held").next().expect("a key");
    assert!(matches!(
        constraint.sqlite_index_columns(&other).map(|entries| entries.is_some()),
        Err(LookupError::ObjectNotInDatabase { .. })
    ));
}
