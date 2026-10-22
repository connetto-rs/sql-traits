//! Leading documentation follows schema objects.
#![expect(clippy::expect_used, reason = "Tests require parsed schemas and valid lookups.")]

use sql_traits::prelude::*;
use sqlparser::{
    ast::CreateTable,
    dialect::{
        BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect,
        GenericDialect, HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect, PostgreSqlDialect,
        RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect, SparkSqlDialect, TeradataDialect,
    },
    parser::Parser,
};

fn table<'db>(db: &'db ParserDB, schema: Option<&str>, name: &str) -> &'db CreateTable {
    let target = match schema {
        Some(schema) => TargetName::new(name, false).with_schema(schema, false),
        None => TargetName::new(name, false),
    };
    db.table_by_target(target, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
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

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

#[test]
fn comment_markers_inside_string_literals_leave_documentation_intact() {
    let db = postgres(
        "-- users
CREATE TABLE users (
  -- the key
  id INT,
  closer TEXT DEFAULT '*/',
  opener TEXT DEFAULT '/* x',
  dashes TEXT DEFAULT '-- no'
);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(column_doc(&db, None, "users", "id"), Some("the key"));
    assert_eq!(column_doc(&db, None, "users", "dashes"), None);
}

#[test]
fn mysql_hash_comments_document() {
    let db = ParserDB::parse::<MySqlDialect>(
        "# events
CREATE TABLE events (
  # when it happened
  at INT
);",
    )
    .expect("schema builds");
    assert_eq!(table_doc(&db, None, "events"), Some("events"));
    assert_eq!(column_doc(&db, None, "events", "at"), Some("when it happened"));
}

#[test]
fn a_contiguous_comment_block_documents_whole() {
    let db = postgres(
        "-- Registered users.
-- One row per account.
CREATE TABLE users (
  /* The key,
     never reused. */
  id INT
);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("Registered users.\nOne row per account."));
    assert_eq!(column_doc(&db, None, "users", "id"), Some("The key,\nnever reused."));
}

#[test]
fn a_blank_line_detaches_a_comment() {
    let db = postgres(
        "-- file header
-- still the header

-- users
CREATE TABLE users (
  -- detached

  id INT
);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(column_doc(&db, None, "users", "id"), None);
}

#[test]
fn a_trailing_comment_documents_nothing_below_it() {
    let db = postgres(
        "CREATE TABLE a (x INT); -- about a
CREATE TABLE b (
  id INT, -- about id
  name TEXT
);",
    );
    assert_eq!(table_doc(&db, None, "b"), None);
    assert_eq!(column_doc(&db, None, "b", "id"), None);
    assert_eq!(column_doc(&db, None, "b", "name"), None);
}

#[test]
fn a_table_name_on_its_own_line_is_documented() {
    let db = postgres(
        "-- users
CREATE TABLE IF NOT EXISTS
  users (id INT);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
}

#[test]
fn documentation_follows_a_renamed_table_and_column() {
    let db = postgres(
        "-- people
CREATE TABLE users (
  -- the key
  id INT
);
ALTER TABLE users RENAME TO people;
ALTER TABLE people RENAME COLUMN id TO person_id;",
    );
    assert_eq!(table_doc(&db, None, "people"), Some("people"));
    assert_eq!(column_doc(&db, None, "people", "person_id"), Some("the key"));
}

#[test]
fn quoted_names_are_documented() {
    let db = postgres(
        "-- quoted table
CREATE TABLE \"Users\" (
  -- quoted column
  \"Id\" INT
);",
    );
    let users = db
        .table_by_target(TargetName::new("Users", true), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    assert_eq!(users.table_doc(&db).expect("in this database"), Some("quoted table"));
    let id = users
        .column("\"Id\"", &db, IdentifierCase::AsWritten)
        .expect("in this database")
        .expect("the column exists");
    assert_eq!(id.column_doc(&db).expect("in this database"), Some("quoted column"));
}

#[test]
fn a_column_dropped_and_added_again_is_undocumented() {
    let db = postgres(
        "CREATE TABLE users (
  id INT,
  -- the old note
  note TEXT
);
ALTER TABLE users DROP COLUMN note;
ALTER TABLE users ADD COLUMN note TEXT;",
    );
    assert_eq!(column_doc(&db, None, "users", "note"), None);
}

#[test]
fn an_inherited_column_does_not_take_the_parent_documentation() {
    let db = postgres(
        "CREATE TABLE parent (
  -- the parent key
  id INT
);
CREATE TABLE child (
  -- the child extra
  extra INT
) INHERITS (parent);",
    );
    assert_eq!(column_doc(&db, None, "parent", "id"), Some("the parent key"));
    assert_eq!(column_doc(&db, None, "child", "id"), None);
    assert_eq!(column_doc(&db, None, "child", "extra"), Some("the child extra"));
}

#[test]
fn same_named_tables_created_through_the_search_path_keep_their_own_documentation() {
    let db = postgres(
        "CREATE SCHEMA a;
CREATE SCHEMA b;
SET search_path TO a;
-- in a
CREATE TABLE t (id INT);
SET search_path TO b;
-- in b
CREATE TABLE t (id INT);",
    );
    assert_eq!(table_doc(&db, Some("a"), "t"), Some("in a"));
    assert_eq!(table_doc(&db, Some("b"), "t"), Some("in b"));
}

#[test]
fn each_file_documents_its_own_tables() {
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("leading_comment_documentation_each_file");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    std::fs::write(directory.join("1_users.sql"), "-- users\nCREATE TABLE users (id INT);\n")
        .expect("first file");
    std::fs::write(
        directory.join("2_posts.sql"),
        "CREATE TABLE posts (\n  -- the author\n  author INT\n);\n",
    )
    .expect("second file");

    let db = ParserDB::from_path::<PostgreSqlDialect>(&directory).expect("schema builds");
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(table_doc(&db, None, "posts"), None);
    assert_eq!(column_doc(&db, None, "posts", "author"), Some("the author"));
}

#[test]
fn comments_between_create_keywords_leave_the_leading_documentation_intact() {
    let db = postgres(
        "-- users\nCREATE /* keyword annotation */ TABLE users (\n  -- the key\n  id INT\n);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(column_doc(&db, None, "users", "id"), Some("the key"));
}

#[test]
fn comments_inside_the_declaration_prefix_do_not_document_the_table() {
    let db = postgres("CREATE\n-- keyword annotation\nTABLE users (id INT);");
    assert_eq!(table_doc(&db, None, "users"), None);
}

#[test]
fn blank_lines_inside_the_declaration_prefix_do_not_detach_documentation() {
    let db = postgres("-- users\nCREATE\n\nTABLE IF NOT EXISTS\n\nusers (id INT);");
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
}

#[test]
fn comments_sharing_a_line_do_not_document() {
    let db = postgres(
        "/* first */ /* second */\nCREATE TABLE users (\n  /* first */ /* second */\n  id INT\n);",
    );
    assert_eq!(table_doc(&db, None, "users"), None);
    assert_eq!(column_doc(&db, None, "users", "id"), None);
}

#[test]
fn a_standalone_suffix_after_inline_comments_documents() {
    let db = postgres(
        "/* first */ /* second */\n-- users\nCREATE TABLE users (\n  id INT, -- trailing\n  -- the name\n  name TEXT\n);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(column_doc(&db, None, "users", "name"), Some("the name"));
}

#[test]
fn empty_comments_do_not_document() {
    let db = postgres("-- \n/* \n */\nCREATE TABLE users (\n  -- \n  id INT\n);");
    assert_eq!(table_doc(&db, None, "users"), None);
    assert_eq!(column_doc(&db, None, "users", "id"), None);
}

#[test]
fn rollback_restores_documentation_after_drop_and_recreate() {
    for (begin, rollback) in
        [("BEGIN;", "ROLLBACK;"), ("BEGIN; SAVEPOINT original;", "ROLLBACK TO original; COMMIT;")]
    {
        let db = postgres(&format!(
            "-- users\nCREATE TABLE users (\n-- the key\nid INT\n);\n\
             {begin}\nDROP TABLE users;\n\
             -- discarded\nCREATE TABLE users (\n-- discarded key\nid INT\n);\n{rollback}",
        ));
        assert_eq!(table_doc(&db, None, "users"), Some("users"));
        assert_eq!(column_doc(&db, None, "users", "id"), Some("the key"));
    }
}

#[test]
fn a_like_copy_does_not_take_the_source_documentation() {
    let db = postgres(
        "-- the parent\nCREATE TABLE parent (\n-- the parent key\nid INT\n);\nCREATE TABLE child (LIKE parent);",
    );
    assert_eq!(table_doc(&db, None, "parent"), Some("the parent"));
    assert_eq!(column_doc(&db, None, "parent", "id"), Some("the parent key"));
    assert_eq!(table_doc(&db, None, "child"), None);
    assert_eq!(column_doc(&db, None, "child", "id"), None);
}

#[test]
fn empty_statements_and_comments_do_not_create_tables() {
    for sql in ["", " \n\t", "-- only a comment", "/* only a comment */", ";;; -- done\n;"] {
        assert_eq!(postgres(sql).tables().next(), None, "{sql}");
    }
}

#[test]
fn separators_and_trailing_comments_keep_their_statement_documentation() {
    let db = postgres(
        "; /* separator annotation */ ;\n-- users\nCREATE TABLE users (\n-- the name\nname TEXT DEFAULT 'semi;colon'\n);;;\n-- posts\nCREATE TABLE posts (id INT);;; -- trailing",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(column_doc(&db, None, "users", "name"), Some("the name"));
    assert_eq!(table_doc(&db, None, "posts"), Some("posts"));
}

#[test]
fn end_without_a_separator_ends_the_statement_list() {
    let db = postgres(
        "-- users\nCREATE TABLE users (id INT) END;\n-- ignored\nCREATE TABLE ignored (id INT);",
    );
    assert_eq!(db.tables().map(TableLike::table_name).collect::<Vec<_>>(), ["users"]);
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
}

#[test]
fn end_after_a_separator_allows_following_statements() {
    let db = postgres("CREATE TABLE users (id INT); END;\n-- posts\nCREATE TABLE posts (id INT);");
    assert_eq!(db.tables().map(TableLike::table_name).collect::<Vec<_>>(), ["posts", "users"]);
    assert_eq!(table_doc(&db, None, "posts"), Some("posts"));
}

#[test]
fn syntax_and_separator_errors_match_the_native_parser() {
    fn check<D: Dialect + Default + 'static>() {
        for sql in [
            "CREATE TABLE users (id INT) CREATE TABLE posts (id INT);",
            "CREATE TABLE users (id INT name TEXT);",
            "CREATE TABLE users (name TEXT DEFAULT 'unfinished);",
        ] {
            let expected = Parser::parse_sql(&D::default(), sql).expect_err("invalid SQL");
            let actual = ParserDB::parse::<D>(sql).expect_err("invalid SQL");
            assert!(
                matches!(
                    actual,
                    sql_traits::errors::Error::SqlParserError { error, file: None }
                        if error == expected
                ),
                "{sql}",
            );
        }
    }

    check::<GenericDialect>();
    check::<PostgreSqlDialect>();
    check::<MySqlDialect>();
    check::<SQLiteDialect>();
    check::<BigQueryDialect>();
    check::<ClickHouseDialect>();
    check::<DatabricksDialect>();
    check::<DuckDbDialect>();
    check::<HiveDialect>();
    check::<MsSqlDialect>();
    check::<OracleDialect>();
    check::<RedshiftSqlDialect>();
    check::<SnowflakeDialect>();
    check::<SparkSqlDialect>();
    check::<TeradataDialect>();
}

#[test]
fn function_body_semicolons_do_not_split_following_documentation() {
    let db = postgres(
        "CREATE FUNCTION answer() RETURNS INT LANGUAGE SQL AS $$ SELECT 1; SELECT 2; $$;\n-- users\nCREATE TABLE users (id INT);",
    );
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
}

#[test]
fn dialect_trailing_commas_keep_documentation() {
    let db = ParserDB::parse::<BigQueryDialect>(
        "-- users\nCREATE TABLE users (\n-- the key\nid INT64,\n);",
    )
    .expect("BigQuery trailing commas parse");
    assert_eq!(table_doc(&db, None, "users"), Some("users"));
    assert_eq!(column_doc(&db, None, "users", "id"), Some("the key"));
}

#[test]
fn file_syntax_errors_keep_their_path_and_precede_ingestion() {
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("leading_comment_documentation_error_order");
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    std::fs::write(
        directory.join("1_duplicate.sql"),
        "CREATE TABLE users (id INT); CREATE TABLE users (id INT);",
    )
    .expect("first file");
    let invalid_path = directory.join("2_invalid.sql");
    for sql in [
        "CREATE TABLE posts (id INT) CREATE TABLE hidden (id INT);",
        "CREATE TABLE posts (name TEXT DEFAULT 'unfinished);",
    ] {
        std::fs::write(&invalid_path, sql).expect("second file");
        let expected = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect_err("invalid SQL");
        let actual =
            ParserDB::from_path::<PostgreSqlDialect>(&directory).expect_err("invalid file");
        assert!(
            matches!(
                actual,
                sql_traits::errors::Error::SqlParserError { error, file: Some(file) }
                    if error == expected && file == invalid_path
            ),
            "{sql}",
        );
    }
}
