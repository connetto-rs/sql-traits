//! `COMMENT ON TABLE`, `COMMENT ON FOREIGN TABLE` and `COMMENT ON COLUMN`
//! document tables and columns the way PostgreSQL stores them.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 18.6.
#![expect(clippy::expect_used, reason = "Tests require parsed schemas and valid lookups.")]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::table_lookup::table;

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn refusal(sql: &str) -> Error {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect_err("PostgreSQL refuses the statement")
}

fn table_doc<'db>(db: &'db ParserDB, schema: Option<&str>, name: &str) -> Option<&'db str> {
    table(db, schema, name).table_doc(db).expect("the table is in this database")
}

fn column_doc<'db>(
    db: &'db ParserDB,
    schema: Option<&str>,
    table_name: &str,
    column: &str,
) -> Option<&'db str> {
    table(db, schema, table_name)
        .column(column, db, IdentifierCase::AsWritten)
        .expect("the table is in this database")
        .expect("the column exists")
        .column_doc(db)
        .expect("the column is in this database")
}

fn foreign_doc<'db>(db: &'db ParserDB, name: &str, column: Option<&str>) -> Option<&'db str> {
    let foreign = db
        .foreign_table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the foreign table exists");
    match column {
        None => foreign.table_doc(db).expect("the foreign table is in this database"),
        Some(column) => {
            foreign
                .column(column, db, IdentifierCase::AsWritten)
                .expect("the foreign table is in this database")
                .expect("the column exists")
                .column_doc(db)
                .expect("the column is in this database")
        }
    }
}

#[test]
fn the_last_comment_on_each_object_documents_it() {
    let db = postgres(
        "CREATE SCHEMA notes;
         CREATE TABLE notes.entries (id int PRIMARY KEY, body text, author text);
         COMMENT ON TABLE notes.entries IS 'One note per row.';
         COMMENT ON COLUMN notes.entries.body IS 'The text as typed.';
         COMMENT ON COLUMN notes.entries.author IS 'Who wrote it.';
         COMMENT ON COLUMN notes.entries.author IS 'The role that wrote it.';",
    );
    assert_eq!(table_doc(&db, Some("notes"), "entries"), Some("One note per row."));
    assert_eq!(column_doc(&db, Some("notes"), "entries", "body"), Some("The text as typed."));
    assert_eq!(
        column_doc(&db, Some("notes"), "entries", "author"),
        Some("The role that wrote it.")
    );
    assert_eq!(column_doc(&db, Some("notes"), "entries", "id"), None);
}

#[test]
fn a_comment_replaces_the_leading_comment_and_null_or_empty_text_removes_it() {
    let db = postgres(
        "-- users
         CREATE TABLE users (
           -- the key
           id INT,
           -- the name
           name TEXT,
           -- the mail
           mail TEXT
         );
         COMMENT ON TABLE users IS 'Registered users.';
         COMMENT ON COLUMN users.id IS 'The key, never reused.';
         COMMENT ON COLUMN users.name IS NULL;
         COMMENT ON COLUMN users.mail IS '';",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("Registered users."));
    assert_eq!(column_doc(&db, None, "users", "id"), Some("The key, never reused."));
    assert_eq!(column_doc(&db, None, "users", "name"), None);
    assert_eq!(column_doc(&db, None, "users", "mail"), None);

    let cleared =
        postgres("-- users\nCREATE TABLE users (id INT);\nCOMMENT ON TABLE users IS NULL;");
    assert_eq!(table_doc(&cleared, None, "users"), None);
}

#[test]
fn names_resolve_through_the_search_path_and_the_catalog() {
    let db = postgres(
        "CREATE SCHEMA notes;
         CREATE TABLE notes.entries (id INT, body TEXT, author TEXT);
         SET search_path TO notes;
         COMMENT ON TABLE entries IS 'on the path';
         COMMENT ON COLUMN entries.body IS 'table on the path';
         COMMENT ON COLUMN unknown_catalog.notes.entries.author IS 'catalog qualified';",
    );
    assert_eq!(table_doc(&db, Some("notes"), "entries"), Some("on the path"));
    assert_eq!(column_doc(&db, Some("notes"), "entries", "body"), Some("table on the path"));
    assert_eq!(column_doc(&db, Some("notes"), "entries", "author"), Some("catalog qualified"));
}

#[test]
fn a_foreign_table_takes_its_own_spelling_and_column_comments() {
    let db = postgres(
        "CREATE FOREIGN TABLE remote (id INT) SERVER warehouse;
         COMMENT ON FOREIGN TABLE remote IS 'rows kept elsewhere';
         COMMENT ON COLUMN remote.id IS 'the remote key';",
    );
    assert_eq!(foreign_doc(&db, "remote", None), Some("rows kept elsewhere"));
    assert_eq!(foreign_doc(&db, "remote", Some("id")), Some("the remote key"));
}

#[test]
fn documentation_follows_renames_moves_and_type_changes() {
    let db = postgres(
        "CREATE SCHEMA old;
         CREATE TABLE old.t (a TEXT COLLATE \"C\", b INT);
         COMMENT ON TABLE old.t IS 'the table';
         COMMENT ON COLUMN old.t.a IS 'the a column';
         COMMENT ON COLUMN old.t.b IS 'the b column';
         ALTER TABLE old.t RENAME COLUMN a TO renamed;
         ALTER TABLE old.t ALTER COLUMN renamed TYPE INT USING length(renamed);
         ALTER TABLE old.t ALTER COLUMN b TYPE BIGINT;
         ALTER TABLE old.t RENAME TO r;
         ALTER SCHEMA old RENAME TO moved;",
    );
    assert_eq!(table_doc(&db, Some("moved"), "r"), Some("the table"));
    assert_eq!(column_doc(&db, Some("moved"), "r", "renamed"), Some("the a column"));
    assert_eq!(column_doc(&db, Some("moved"), "r", "b"), Some("the b column"));
}

#[test]
fn documentation_leaves_with_its_object() {
    let db = postgres(
        "CREATE TABLE t (id INT, note TEXT);
         COMMENT ON TABLE t IS 'old table';
         COMMENT ON COLUMN t.note IS 'old note';
         ALTER TABLE t DROP COLUMN note;
         ALTER TABLE t ADD COLUMN note TEXT;
         CREATE TABLE gone (id INT);
         COMMENT ON TABLE gone IS 'old gone';
         DROP TABLE gone;
         CREATE TABLE gone (id INT);",
    );
    assert_eq!(table_doc(&db, None, "t"), Some("old table"));
    assert_eq!(column_doc(&db, None, "t", "note"), None);
    assert_eq!(table_doc(&db, None, "gone"), None);
}

#[test]
fn neither_a_child_nor_a_copy_takes_the_comments() {
    let db = postgres(
        "CREATE TABLE parent (id INT);
         COMMENT ON TABLE parent IS 'the parent';
         COMMENT ON COLUMN parent.id IS 'the parent key';
         CREATE TABLE child () INHERITS (parent);
         CREATE TABLE copy (LIKE parent);",
    );
    for name in ["child", "copy"] {
        assert_eq!(table_doc(&db, None, name), None, "{name}");
        assert_eq!(column_doc(&db, None, name, "id"), None, "{name}");
    }
}

#[test]
fn a_rolled_back_comment_leaves_no_trace() {
    let db = postgres(
        "CREATE TABLE t (id INT);
         COMMENT ON TABLE t IS 'kept';
         BEGIN;
         COMMENT ON TABLE t IS 'discarded';
         COMMENT ON COLUMN t.id IS 'discarded';
         ROLLBACK;",
    );
    assert_eq!(table_doc(&db, None, "t"), Some("kept"));
    assert_eq!(column_doc(&db, None, "t", "id"), None);
}

#[test]
fn a_view_or_composite_column_takes_a_comment_the_model_does_not_keep() {
    let db = postgres(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE TYPE ct AS (a INT);
         CREATE SCHEMA s;
         CREATE TYPE s.ct AS (b INT);
         COMMENT ON COLUMN v.a IS 'view column';
         COMMENT ON COLUMN m.a IS 'materialized view column';
         COMMENT ON COLUMN ct.a IS 'composite attribute';
         COMMENT ON COLUMN s.ct.b IS 'composite attribute in a schema';",
    );
    assert_eq!(column_doc(&db, None, "t", "a"), None);
}

#[test]
fn a_relation_of_another_kind_is_refused() {
    let base = "CREATE TABLE t (a INT);
                CREATE FOREIGN TABLE f (a INT) SERVER s;
                CREATE VIEW v AS SELECT a FROM t;
                CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
                CREATE SEQUENCE q;
                CREATE INDEX i ON t (a);";
    for (statement, relation, expected, actual) in [
        ("COMMENT ON TABLE f IS 'x';", "f", ObjectKind::Table, ObjectKind::ForeignTable),
        ("COMMENT ON TABLE v IS 'x';", "v", ObjectKind::Table, ObjectKind::View),
        ("COMMENT ON TABLE m IS 'x';", "m", ObjectKind::Table, ObjectKind::MaterializedView),
        ("COMMENT ON TABLE q IS 'x';", "q", ObjectKind::Table, ObjectKind::Sequence),
        ("COMMENT ON FOREIGN TABLE t IS 'x';", "t", ObjectKind::ForeignTable, ObjectKind::Table),
        ("COMMENT ON COLUMN q.last_value IS 'x';", "q", ObjectKind::Table, ObjectKind::Sequence),
        ("COMMENT ON COLUMN i.a IS 'x';", "i", ObjectKind::Table, ObjectKind::Index),
    ] {
        let refused = refusal(&format!("{base} {statement}"));
        assert!(
            matches!(
                &refused,
                Error::RelationKindMismatch { object_name, expected_kind, actual_kind }
                    if object_name == relation && *expected_kind == expected && *actual_kind == actual
            ),
            "{statement}: {refused:?}",
        );
    }
}

#[test]
fn a_missing_relation_or_column_is_refused() {
    let base = "CREATE TABLE t (a INT);
                CREATE VIEW v AS SELECT a FROM t;
                CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
                CREATE TYPE ct AS (a INT);";
    for (statement, kind) in [
        ("COMMENT ON TABLE missing IS 'x';", ObjectKind::Table),
        ("COMMENT ON FOREIGN TABLE missing IS 'x';", ObjectKind::ForeignTable),
        ("COMMENT ON COLUMN missing.a IS 'x';", ObjectKind::Table),
    ] {
        let refused = refusal(&format!("{base} {statement}"));
        assert!(
            matches!(
                &refused,
                Error::RelationNotFound { object_kind, object_name }
                    if *object_kind == kind && object_name == "missing"
            ),
            "{statement}: {refused:?}",
        );
    }
    for relation in ["t", "v", "m", "ct"] {
        let statement = format!("COMMENT ON COLUMN {relation}.missing IS 'x';");
        let refused = refusal(&format!("{base} {statement}"));
        assert!(
            matches!(
                &refused,
                Error::IdentifierLookupError(LookupError::ColumnNotFound { table_name, column_name })
                    if table_name == relation && column_name == "missing"
            ),
            "{statement}: {refused:?}",
        );
    }
    let refused = refusal(&format!("{base} COMMENT ON COLUMN a IS 'x';"));
    assert!(
        matches!(&refused, Error::UnqualifiedCommentColumn { column_name } if column_name == "a"),
        "{refused:?}",
    );
    let refused = refusal(&format!("{base} COMMENT ON COLUMN other.public.t.a IS 'x';"));
    assert!(
        matches!(&refused, Error::IdentifierLookupError(LookupError::InvalidObjectName { .. })),
        "{refused:?}",
    );
}

#[test]
fn if_exists_skips_a_missing_relation() {
    let db = postgres(
        "CREATE TABLE t (a INT);
         COMMENT IF EXISTS ON TABLE missing IS 'x';
         COMMENT IF EXISTS ON COLUMN missing.a IS 'x';",
    );
    assert_eq!(table_doc(&db, None, "t"), None);
}
