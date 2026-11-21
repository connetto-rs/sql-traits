//! Tests that `ColumnLike::declared_type_text` answers what SQLite 3.51.1
//! reports in `pragma_table_xinfo.type` for each column.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    dialect::{GenericDialect, PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

/// Each column declaration beside the type SQLite 3.51.1 reports for it.
const DECLARATIONS: [(&str, &str); 34] = [
    ("integer", "INTEGER"),
    ("InTeGeR", "INTEGER"),
    ("\"integer\"", "INTEGER"),
    ("int", "INT"),
    ("text", "TEXT"),
    ("TeXt", "TEXT"),
    ("real", "REAL"),
    ("blob", "BLOB"),
    ("any", "ANY"),
    ("float", "float"),
    ("double", "double"),
    ("boolean", "boolean"),
    ("VARCHAR( 10 )", "VARCHAR( 10 )"),
    ("VARCHAR(   10   )", "VARCHAR(   10   )"),
    ("NUMERIC(  5  ,   2   )", "NUMERIC(  5  ,   2   )"),
    ("varchar ( 010 )", "varchar ( 010 )"),
    ("varchar(10)   ", "varchar(10)"),
    ("numeric( 5 , 2 )", "numeric( 5 , 2 )"),
    ("DOUBLE PRECISION", "DOUBLE PRECISION"),
    ("int( +5 , -2 )", "int( +5 , -2 )"),
    ("\"my  Type\"", "my  Type"),
    ("[Mixed Type]", "Mixed Type"),
    ("`Other Type`", "Other Type"),
    ("\"a\"\"b\"", "a\"b"),
    ("\"my type\"( 4 )", "my type"),
    ("\"int\"( 3 )", "int"),
    ("", ""),
    ("\"\"", ""),
    ("INT AS (1)", "INT"),
    ("AS (2) STORED", ""),
    ("INT  GENERATED   ALWAYS AS (3)", "INT"),
    ("GENERATED ALWAYS AS (4)", ""),
    ("forever_lasting_always", "forever_lasting_"),
    ("xx_always", "xx_always"),
];

/// Parses `sql` the way SQLite compares names.
fn sqlite(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default().with_identifier_case(IdentifierCase::Folded).parse::<SQLiteDialect>(sql)
}

/// An ingestor comparing names the way SQLite does.
fn ingestor() -> ParserDBIngestor {
    ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .ingestor::<SQLiteDialect>("main".to_owned())
}

/// The name and declared type text of each column of `table`.
fn declared_types(db: &ParserDB, table: &str) -> Vec<(String, Option<String>)> {
    db.table_by_target(TargetName::new(table, false), IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("table {table} exists"))
        .columns(db)
        .expect("the table is held")
        .map(|column| {
            let declared = column.declared_type_text(db).expect("the column is held");
            (column.column_name().to_string(), declared.map(str::to_string))
        })
        .collect()
}

fn typed(columns: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
    columns.iter().map(|(name, text)| ((*name).to_string(), Some((*text).to_string()))).collect()
}

#[test]
fn each_declaration_answers_the_type_sqlite_reports() {
    for (declaration, expected) in DECLARATIONS {
        let sql = format!("CREATE TABLE t(a INT, x {declaration});");
        let db = sqlite(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        assert_eq!(declared_types(&db, "t"), typed(&[("a", "INT"), ("x", expected)]), "{sql}");
    }
}

#[test]
fn constraints_and_comments_around_a_type_stay_out_of_it() {
    let db = sqlite(
        "CREATE TABLE t(
            a VARCHAR( 10 ) /* trailing */ NOT NULL,
            b integer -- note
                PRIMARY KEY,
            c text DEFAULT 'x' COLLATE NOCASE,
            d DOUBLE /* gap */ PRECISION CHECK (d > 0),
            e REFERENCES t(a),
            f CONSTRAINT u UNIQUE,
            g NULL
        );",
    )
    .expect("SQLite stores this table");
    assert_eq!(
        declared_types(&db, "t"),
        typed(&[
            ("a", "VARCHAR( 10 )"),
            ("b", "INTEGER"),
            ("c", "TEXT"),
            ("d", "DOUBLE /* gap */ PRECISION"),
            ("e", ""),
            ("f", ""),
            ("g", ""),
        ])
    );
}

#[test]
fn a_declaration_is_found_past_earlier_statements_and_wide_characters() {
    let db = sqlite(
        "CREATE TABLE first(a int); -- café\n\
         CREATE TABLE \"é\" (\n  \"naïve ✓\" VARCHAR( 3 ),\tb  boolean /* ü */\n);",
    )
    .expect("SQLite stores both tables");
    assert_eq!(declared_types(&db, "é"), typed(&[("naïve ✓", "VARCHAR( 3 )"), ("b", "boolean")]));
}

#[test]
fn a_column_added_later_answers_its_own_declaration() {
    let db = ingestor()
        .apply_sql("CREATE TABLE s(a integer, b text, c);")
        .and_then(|ingestor| {
            ingestor.apply_sql("ALTER TABLE s ADD COLUMN z VarChar( 3 ) DEFAULT 1;")
        })
        .and_then(|ingestor| ingestor.apply_sql("ALTER TABLE s RENAME COLUMN a TO renamed;"))
        .expect("SQLite applies these statements")
        .finish();
    assert_eq!(
        declared_types(&db, "s"),
        typed(&[("renamed", "INTEGER"), ("b", "TEXT"), ("c", ""), ("z", "VarChar( 3 )")])
    );
}

#[test]
fn virtual_table_columns_answer_what_their_module_declares() {
    let db = sqlite(
        "CREATE VIRTUAL TABLE fulltext USING fts5(content, title UNINDEXED);
         CREATE VIRTUAL TABLE spatial USING rtree(id, min_x, max_x, +label TEXT, +untyped);
         CREATE VIRTUAL TABLE spatialint USING rtree_i32(id, min_x, max_x);",
    )
    .expect("SQLite stores these tables");
    assert_eq!(
        declared_types(&db, "fulltext"),
        typed(&[("content", ""), ("title", ""), ("fulltext", ""), ("rank", "")])
    );
    assert_eq!(
        declared_types(&db, "spatial"),
        typed(&[
            ("id", "INT"),
            ("min_x", "REAL"),
            ("max_x", "REAL"),
            ("label", ""),
            ("untyped", "")
        ])
    );
    assert_eq!(
        declared_types(&db, "spatialint"),
        typed(&[("id", "INT"), ("min_x", "INT"), ("max_x", "INT")])
    );
}

#[test]
fn a_supplied_module_declaration_answers_as_supplied() {
    let create = Parser::parse_sql(&SQLiteDialect {}, "CREATE VIRTUAL TABLE s USING dbstat;")
        .expect("the statement parses")
        .remove(0);
    let declared = DeclaredVirtualTable::new(
        [
            DeclaredColumn::new("name", "TEXT", false),
            DeclaredColumn::new("pageno", "INTEGER", false),
            DeclaredColumn::new("aggregate", "BOOLEAN", true),
        ],
        Vec::<String>::new(),
    );
    let db = ingestor()
        .apply_virtual_table(create, declared)
        .expect("the catalog states the columns of dbstat")
        .finish();
    assert_eq!(
        declared_types(&db, "s"),
        typed(&[("name", "TEXT"), ("pageno", "INTEGER"), ("aggregate", "BOOLEAN")])
    );
}

#[test]
fn a_declaration_without_its_text_has_no_declared_type_text() {
    let statement = Parser::parse_sql(&SQLiteDialect {}, "CREATE TABLE t(a VARCHAR( 10 ));")
        .expect("the statement parses")
        .remove(0);
    let db = ingestor().apply_statement(statement).expect("SQLite stores t").finish();
    assert_eq!(declared_types(&db, "t"), [("a".to_string(), None)]);
}

#[test]
fn a_table_created_from_a_query_has_no_declared_type_text() {
    let db = sqlite("CREATE TABLE s(a integer); CREATE TABLE t AS SELECT a, a + 1 AS b FROM s;")
        .expect("SQLite stores both tables");
    assert_eq!(declared_types(&db, "t"), [("a".to_string(), None), ("b".to_string(), None)]);
}

#[test]
fn other_dialects_have_no_declared_type_text() {
    let postgres = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t(a VARCHAR( 10 ));")
        .expect("PostgreSQL stores t");
    assert_eq!(declared_types(&postgres, "t"), [("a".to_string(), None)]);
    let generic = ParserDB::parse::<GenericDialect>(
        "CREATE VIRTUAL TABLE spatial USING rtree(id, min_x, max_x);",
    )
    .expect("the generic dialect stores spatial");
    assert_eq!(
        declared_types(&generic, "spatial"),
        [("id".to_string(), None), ("min_x".to_string(), None), ("max_x".to_string(), None)]
    );
}

#[test]
fn a_column_answers_alike_through_a_reference_or_an_arc() {
    let db = sqlite("CREATE TABLE t(a VARCHAR( 10 ));").expect("SQLite stores t");
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table.column("a", &db, IdentifierCase::Folded).expect("lookup").expect("a exists");
    assert_eq!(ColumnLike::declared_type_text(&column, &db), Ok(Some("VARCHAR( 10 )")));
    let shared = std::sync::Arc::new(column.clone());
    assert_eq!(shared.declared_type_text(&db), Ok(Some("VARCHAR( 10 )")));
}

#[test]
fn a_column_of_another_database_is_refused() {
    let held = sqlite("CREATE TABLE t(a int);").expect("SQLite stores t");
    let other = sqlite("CREATE TABLE u(b int);").expect("SQLite stores u");
    let table = held
        .table_by_target(TargetName::new("t", false), IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table.column("a", &held, IdentifierCase::Folded).expect("lookup").expect("a");
    assert!(column.declared_type_text(&other).is_err());
}
