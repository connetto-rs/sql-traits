//! Tests that `ParserDB` models SQLite virtual tables and the shadow tables
//! their modules keep, each expectation measured on SQLite 3.51.1 through
//! `sqlite_schema`, `pragma_table_list` and `pragma_table_xinfo`.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ObjectKind, VirtualTableArgumentRefusal},
    prelude::*,
};
use sqlparser::{
    ast::{SetExpr, Statement},
    dialect::SQLiteDialect,
    parser::Parser,
};

/// What SQLite 3.51.1 stores in `sqlite_schema` for an `fts5` table.
const FTS5: &str = "CREATE VIRTUAL TABLE f5 USING fts5(a, b UNINDEXED, tokenize='porter');
     CREATE TABLE 'f5_data'(id INTEGER PRIMARY KEY, block BLOB);
     CREATE TABLE 'f5_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
     CREATE TABLE 'f5_content'(id INTEGER PRIMARY KEY, c0, c1);
     CREATE TABLE 'f5_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
     CREATE TABLE 'f5_config'(k PRIMARY KEY, v) WITHOUT ROWID;";

/// The same `fts5` table with its shadow tables listed first.
const FTS5_SHADOWS_FIRST: &str = "CREATE TABLE 'f5_data'(id INTEGER PRIMARY KEY, block BLOB);
     CREATE TABLE 'f5_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
     CREATE TABLE 'f5_content'(id INTEGER PRIMARY KEY, c0, c1);
     CREATE TABLE 'f5_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
     CREATE TABLE 'f5_config'(k PRIMARY KEY, v) WITHOUT ROWID;
     CREATE VIRTUAL TABLE f5 USING fts5(a, b UNINDEXED, tokenize='porter');";

/// What SQLite 3.51.1 stores in `sqlite_schema` for an `fts4` table.
const FTS4: &str = "CREATE VIRTUAL TABLE f4 USING fts4(x, y, tokenize=porter);
     CREATE TABLE 'f4_content'(docid INTEGER PRIMARY KEY, c0x, c1y);
     CREATE TABLE 'f4_segments'(blockid INTEGER PRIMARY KEY, block BLOB);
     CREATE TABLE 'f4_segdir'(level INTEGER, idx INTEGER, start_block INTEGER, leaves_end_block INTEGER, end_block INTEGER, root BLOB, PRIMARY KEY(level, idx));
     CREATE TABLE 'f4_docsize'(docid INTEGER PRIMARY KEY, size BLOB);
     CREATE TABLE 'f4_stat'(id INTEGER PRIMARY KEY, value BLOB);";

/// What SQLite 3.51.1 stores in `sqlite_schema` for an `rtree` table.
const RTREE: &str = "CREATE VIRTUAL TABLE r USING rtree(id, minx, maxx, +aux);
     CREATE TABLE \"r_node\"(nodeno INTEGER PRIMARY KEY, data);
     CREATE TABLE \"r_parent\"(nodeno INTEGER PRIMARY KEY, parentnode);
     CREATE TABLE \"r_rowid\"(rowid INTEGER PRIMARY KEY, nodeno, a0);";

/// Parses `sql` the way SQLite compares names.
fn sqlite(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default().with_identifier_case(IdentifierCase::Folded).parse::<SQLiteDialect>(sql)
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("table {name} exists"))
}

/// The name, declared type and hidden flag of each column of `name`, as
/// `pragma_table_xinfo` lists them.
fn columns(db: &ParserDB, name: &str) -> Vec<(String, String, bool)> {
    table(db, name)
        .columns(db)
        .expect("the table is held")
        .map(|column| {
            (
                column.column_name().to_string(),
                column.data_type(db).name(db).into_owned(),
                column.is_hidden(db).expect("the column is held"),
            )
        })
        .collect()
}

/// A column as [`columns`] lists it, an empty `data_type` standing for an
/// untyped column, which the model names `UNSPECIFIED`.
fn column(name: &str, data_type: &str, hidden: bool) -> (String, String, bool) {
    let data_type = if data_type.is_empty() { "UNSPECIFIED" } else { data_type };
    (name.to_string(), data_type.to_string(), hidden)
}

/// The names of the tables whose shadow owner is `owner`, sorted.
fn shadows(db: &ParserDB, owner: &str) -> Vec<String> {
    let mut names: Vec<String> = db
        .tables()
        .filter(|table| {
            table
                .shadow_owner(db)
                .expect("the table is held")
                .is_some_and(|found| found.table_name().eq_ignore_ascii_case(owner))
        })
        .map(|table| table.table_name().to_string())
        .collect();
    names.sort();
    names
}

fn table_names(db: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> = db.tables().map(|table| table.table_name().to_string()).collect();
    names.sort();
    names
}

fn arguments(db: &ParserDB, name: &str) -> (String, Vec<String>) {
    let source = table(db, name)
        .virtual_source(db)
        .expect("the table is held")
        .unwrap_or_else(|| panic!("{name} is virtual"));
    (source.module_name().to_string(), source.arguments().to_vec())
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

#[test]
fn an_fts5_table_holds_its_columns_and_owns_its_shadow_tables() {
    let db = sqlite(FTS5).expect("SQLite stores this schema");
    assert_eq!(
        table_names(&db),
        names(&["f5", "f5_config", "f5_content", "f5_data", "f5_docsize", "f5_idx"])
    );
    let virtual_tables: Vec<&str> = db.virtual_tables().map(TableLike::table_name).collect();
    assert_eq!(virtual_tables, ["f5"]);
    let f5 = table(&db, "f5");
    assert!(f5.is_virtual(&db).expect("held"));
    assert!(f5.shadow_owner(&db).expect("held").is_none());
    assert_eq!(
        arguments(&db, "f5"),
        ("fts5".to_string(), names(&["a", "b UNINDEXED", "tokenize='porter'"]))
    );
    assert_eq!(
        columns(&db, "f5"),
        [
            column("a", "", false),
            column("b", "", false),
            column("f5", "", true),
            column("rank", "", true)
        ]
    );
    assert_eq!(
        shadows(&db, "f5"),
        names(&["f5_config", "f5_content", "f5_data", "f5_docsize", "f5_idx"])
    );
    assert!(!table(&db, "f5_data").is_virtual(&db).expect("held"));
}

#[test]
fn shadow_tables_listed_before_their_virtual_table_still_belong_to_it() {
    let db = sqlite(FTS5_SHADOWS_FIRST).expect("a dump may list shadow tables first");
    assert_eq!(
        shadows(&db, "f5"),
        names(&["f5_config", "f5_content", "f5_data", "f5_docsize", "f5_idx"])
    );
}

#[test]
fn an_fts4_table_holds_its_hidden_columns_and_owns_its_shadow_tables() {
    let db = sqlite(FTS4).expect("SQLite stores this schema");
    assert_eq!(
        columns(&db, "f4"),
        [
            column("x", "", false),
            column("y", "", false),
            column("f4", "", true),
            column("docid", "", true),
            column("__langid", "", true),
        ]
    );
    assert_eq!(
        shadows(&db, "f4"),
        names(&["f4_content", "f4_docsize", "f4_segdir", "f4_segments", "f4_stat"])
    );
}

#[test]
fn an_rtree_table_types_its_coordinates_and_owns_its_shadow_tables() {
    let db = sqlite(RTREE).expect("SQLite stores this schema");
    assert_eq!(
        columns(&db, "r"),
        [
            column("id", "INT", false),
            column("minx", "REAL", false),
            column("maxx", "REAL", false),
            column("aux", "", false),
        ]
    );
    assert_eq!(shadows(&db, "r"), names(&["r_node", "r_parent", "r_rowid"]));

    let db = sqlite("CREATE VIRTUAL TABLE r32 USING rtree_i32(id, x0 INT, x1, +note TEXT);")
        .expect("rtree_i32 takes integer coordinates");
    assert_eq!(
        columns(&db, "r32"),
        [
            column("id", "INT", false),
            column("x0", "INT", false),
            column("x1", "INT", false),
            column("note", "", false)
        ]
    );
}

#[test]
fn fts3_reads_the_first_token_of_each_column_argument() {
    let db = sqlite(
        "CREATE VIRTUAL TABLE f3 USING fts3(title TEXT, \"my col\" text, foo=bar, tokenize porter, tokenize=simple);",
    )
    .expect("fts3 takes these arguments");
    assert_eq!(
        columns(&db, "f3"),
        [
            column("title", "", false),
            column("my col", "", false),
            column("foo", "", false),
            column("tokenize", "", false),
            column("f3", "", true),
            column("docid", "", true),
            column("__langid", "", true),
        ]
    );
    let db = sqlite("CREATE VIRTUAL TABLE f3 USING fts3();").expect("fts3 takes no argument");
    assert_eq!(
        columns(&db, "f3"),
        [
            column("content", "", false),
            column("f3", "", true),
            column("docid", "", true),
            column("__langid", "", true)
        ]
    );
    let db = sqlite("CREATE VIRTUAL TABLE f4 USING fts4;").expect("fts4 takes no argument list");
    assert_eq!(columns(&db, "f4")[0], column("content", "", false));
}

#[test]
fn fts4_options_are_not_columns_and_languageid_names_a_hidden_column() {
    let db = sqlite(
        "CREATE VIRTUAL TABLE v USING fts4(x, languageid=\"Lid\", notindexed=x, matchinfo=fts3, order=desc, prefix='2,3', TOKENIZE=porter);",
    )
    .expect("fts4 takes these options");
    assert_eq!(
        columns(&db, "v"),
        [
            column("x", "", false),
            column("v", "", true),
            column("docid", "", true),
            column("Lid", "", true)
        ]
    );
}

#[test]
fn fts4_without_columns_takes_those_of_its_content_table() {
    let db = sqlite(
        "CREATE TABLE other(p, q, lid);
         CREATE VIRTUAL TABLE f USING fts4(content=other, languageid=lid);",
    )
    .expect("the content table exists");
    assert_eq!(
        columns(&db, "f"),
        [
            column("p", "", false),
            column("q", "", false),
            column("f", "", true),
            column("docid", "", true),
            column("lid", "", true)
        ]
    );
}

#[test]
fn fts5_dequotes_column_names_and_drops_unindexed() {
    let db = sqlite("CREATE VIRTUAL TABLE v USING fts5(\"my col\", [b] UNINDEXED, 'c', `d`, content='', columnsize=0);")
        .expect("fts5 takes quoted column names");
    assert_eq!(
        columns(&db, "v"),
        [
            column("my col", "", false),
            column("b", "", false),
            column("c", "", false),
            column("d", "", false),
            column("v", "", true),
            column("rank", "", true),
        ]
    );
}

#[test]
fn module_names_compare_without_case() {
    let db = sqlite(
        "CREATE VIRTUAL TABLE w USING FTS5(a); CREATE VIRTUAL TABLE r USING RTree(id, a, b);",
    )
    .expect("SQLite finds modules whatever their case");
    assert_eq!(columns(&db, "w").len(), 3);
    assert_eq!(columns(&db, "r").len(), 3);
}

#[test]
fn an_unknown_module_is_refused_with_its_name_and_the_table() {
    let refused = sqlite("CREATE VIRTUAL TABLE c USING csv(filename='data.csv');")
        .expect_err("the columns of csv are not derivable from its arguments");
    assert!(
        matches!(
            &refused,
            Error::UnknownVirtualTableModule { table_name, module_name }
                if table_name == "c" && module_name == "csv"
        ),
        "{refused:?}"
    );
}

#[test]
fn the_name_is_checked_before_the_module() {
    let db = sqlite("CREATE TABLE q(x); CREATE VIRTUAL TABLE IF NOT EXISTS q USING nosuch(a);")
        .expect("IF NOT EXISTS skips the statement before the module is looked up");
    assert!(!table(&db, "q").is_virtual(&db).expect("held"));

    let refused = sqlite("CREATE TABLE q(x); CREATE VIRTUAL TABLE q USING nosuch(a);")
        .expect_err("the name is taken");
    assert!(!matches!(refused, Error::UnknownVirtualTableModule { .. }), "{refused:?}");
}

#[test]
fn only_a_known_suffix_of_a_virtual_table_makes_a_shadow_table() {
    let db = sqlite(
        "CREATE TABLE t(x);
         CREATE TABLE t_data(x);
         CREATE VIRTUAL TABLE n USING fts5(a);
         CREATE TABLE n_other(x);
         CREATE TABLE n_node(x);
         CREATE TABLE N_DATA(x);
         CREATE VIRTUAL TABLE a_b USING rtree(id, x0, x1);
         CREATE TABLE a_b_node(x);",
    )
    .expect("SQLite accepts these tables outside defensive mode");
    assert_eq!(shadows(&db, "t"), Vec::<String>::new());
    assert_eq!(shadows(&db, "n"), names(&["N_DATA"]));
    assert_eq!(shadows(&db, "a_b"), names(&["a_b_node"]));
}

#[test]
fn dropping_a_virtual_table_drops_its_shadow_tables() {
    let db = sqlite(&format!("{FTS5} CREATE TABLE keep(x); DROP TABLE f5;"))
        .expect("SQLite drops the shadow tables with their virtual table");
    assert_eq!(table_names(&db), names(&["keep"]));
}

#[test]
fn a_shadow_table_drops_and_alters_alone() {
    let db = sqlite(&format!(
        "{FTS5} DROP TABLE f5_data; ALTER TABLE f5_idx ADD COLUMN extra; CREATE INDEX on_config ON f5_config(v);"
    ))
    .expect("SQLite refuses these only in defensive mode");
    assert!(table(&db, "f5").is_virtual(&db).expect("held"));
    assert_eq!(shadows(&db, "f5"), names(&["f5_config", "f5_content", "f5_docsize", "f5_idx"]));
}

#[test]
fn renaming_a_virtual_table_renames_its_shadow_tables_and_hidden_column() {
    let db = sqlite(&format!("{FTS5} ALTER TABLE f5 RENAME TO docs;"))
        .expect("SQLite renames the shadow tables along");
    assert_eq!(
        table_names(&db),
        names(&["docs", "docs_config", "docs_content", "docs_data", "docs_docsize", "docs_idx"])
    );
    assert_eq!(
        columns(&db, "docs"),
        [
            column("a", "", false),
            column("b", "", false),
            column("docs", "", true),
            column("rank", "", true)
        ]
    );
    assert_eq!(
        shadows(&db, "docs"),
        names(&["docs_config", "docs_content", "docs_data", "docs_docsize", "docs_idx"])
    );
    assert_eq!(arguments(&db, "docs").0, "fts5");

    let db = sqlite(&format!("{RTREE} ALTER TABLE r RENAME TO s;")).expect("rtree renames too");
    assert_eq!(shadows(&db, "s"), names(&["s_node", "s_parent", "s_rowid"]));
}

/// The column names of the table `name` in `schema`, `None` for the default.
fn column_names_in(db: &ParserDB, schema: Option<&str>, name: &str) -> Vec<String> {
    db.tables()
        .find(|table| table.table_name() == name && table.table_schema() == schema)
        .unwrap_or_else(|| panic!("table {schema:?}.{name} exists"))
        .columns(db)
        .expect("the table is held")
        .map(|column| column.column_name().to_string())
        .collect()
}

#[test]
fn renaming_a_qualified_virtual_table_renames_only_its_own_hidden_column() {
    let db = sqlite(
        "CREATE SCHEMA s;
         CREATE TABLE docs (f5 INT);
         CREATE VIRTUAL TABLE s.f5 USING fts5(a);
         ALTER TABLE s.f5 RENAME TO docs;",
    )
    .expect("SQLite renames a virtual table of an attached schema");
    assert_eq!(column_names_in(&db, Some("s"), "docs"), ["a", "docs", "rank"]);
    assert_eq!(column_names_in(&db, None, "docs"), ["f5"]);
}

#[test]
fn a_view_reading_the_hidden_column_named_after_the_table_blocks_its_rename() {
    let refused = sqlite(&format!(
        "{FTS5} CREATE VIEW v AS SELECT a FROM f5 WHERE f5 MATCH 'x'; ALTER TABLE f5 RENAME TO docs;"
    ))
    .expect_err("SQLite finds no column f5 in v after the rename");
    assert!(
        matches!(
            &refused,
            Error::VirtualTableRenameBlocked { table_name, column_name, dependent_kind: ObjectKind::View, dependent_name }
                if table_name == "f5" && column_name == "f5" && dependent_name == "v"
        ),
        "{refused:?}"
    );
    let refused = sqlite(&format!(
        "{FTS5} CREATE VIEW v AS SELECT a FROM f5 WHERE f5 MATCH 'x'; ALTER TABLE f5 RENAME TO v;"
    ))
    .expect_err("SQLite refuses the taken name before it reads the views");
    assert!(
        matches!(
            &refused,
            Error::RelationNameAlreadyTaken { conflicting_kind: ObjectKind::View, object_name, .. }
                if object_name == "v"
        ),
        "{refused:?}"
    );
    let db = sqlite(&format!(
        "{FTS5} CREATE VIEW v AS SELECT a, rank FROM f5; ALTER TABLE f5 RENAME TO docs;"
    ))
    .expect("a view leaving the hidden column alone follows the rename");
    assert_eq!(output_names(&db, "SELECT * FROM v"), ["a", "rank"]);
}

#[test]
fn a_virtual_table_refuses_column_changes_indexes_and_triggers() {
    for statement in [
        "ALTER TABLE f5 ADD COLUMN c;",
        "ALTER TABLE f5 RENAME COLUMN a TO c;",
        "ALTER TABLE f5 DROP COLUMN b;",
    ] {
        let refused = sqlite(&format!("{FTS5} {statement}")).expect_err(statement);
        assert!(
            matches!(
                refused,
                Error::AlterActionUnsupportedOnRelation {
                    object_kind: ObjectKind::VirtualTable,
                    ..
                }
            ),
            "{statement}: {refused:?}"
        );
    }
    for statement in
        ["CREATE INDEX i ON f5(a);", "CREATE TRIGGER tr AFTER INSERT ON f5 BEGIN SELECT 1; END;"]
    {
        let refused = sqlite(&format!("{FTS5} {statement}")).expect_err(statement);
        assert!(
            matches!(
                refused,
                Error::RelationKindMismatch {
                    expected_kind: ObjectKind::Table,
                    actual_kind: ObjectKind::VirtualTable,
                    ..
                }
            ),
            "{statement}: {refused:?}"
        );
    }
}

/// The names `query` outputs over the schema `db`.
fn output_names(db: &ParserDB, query: &str) -> Vec<String> {
    let Statement::Query(query) =
        Parser::parse_sql(&SQLiteDialect {}, query).expect("the query parses").remove(0)
    else {
        panic!("expected a query");
    };
    assert!(matches!(query.body.as_ref(), SetExpr::Select(_)));
    let scope =
        ColumnScope::from_query(&query, db, IdentifierCase::Folded).expect("the scope builds");
    let root = scope.scope_for_query(&query).expect("the root query is recorded");
    root.outputs()
        .map(|output| {
            match output {
                QueryOutput::Column { name: Some((name, _)), .. } => name.to_string(),
                other => panic!("expected a named column, found {other:?}"),
            }
        })
        .collect()
}

#[test]
fn a_wildcard_leaves_hidden_columns_out_while_naming_them_reaches_them() {
    let db =
        sqlite(&format!("{FTS5} CREATE VIEW v AS SELECT * FROM f5;")).expect("the view reads f5");
    assert_eq!(output_names(&db, "SELECT * FROM f5"), ["a", "b"]);
    assert_eq!(output_names(&db, "SELECT f5.* FROM f5"), ["a", "b"]);
    assert_eq!(
        output_names(&db, "SELECT a, f5, rank FROM f5 WHERE f5 MATCH 'x'"),
        ["a", "f5", "rank"]
    );
    assert_eq!(output_names(&db, "SELECT * FROM v"), ["a", "b"]);
}

/// A statement SQLite 3.51.1 refuses for the arguments a known module takes,
/// with a test the refusal `ParserDB` reports must pass.
type RefusalCase = (&'static str, fn(&VirtualTableArgumentRefusal) -> bool);

/// Asserts that `ParserDB` refuses each statement of `cases` with a refusal
/// its test accepts.
fn assert_refusals(cases: &[RefusalCase]) {
    for (statement, expected) in cases {
        match sqlite(statement) {
            Err(Error::VirtualTableArgumentsRefused { table_name, refusal, .. }) => {
                assert!(expected(&refusal), "{statement}: {refusal:?}");
                assert!(!table_name.is_empty(), "{statement}");
            }
            other => panic!("{statement}: expected an argument refusal, found {other:?}"),
        }
    }
}

#[test]
fn fts5_arguments_sqlite_refuses_are_refused() {
    use VirtualTableArgumentRefusal as R;
    assert_refusals(&[
        ("CREATE VIRTUAL TABLE v USING fts5();", |r| matches!(r, R::TooFewColumns)),
        ("CREATE VIRTUAL TABLE v USING fts5;", |r| matches!(r, R::TooFewColumns)),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, rank);",
            |r| matches!(r, R::ReservedColumnName { column_name } if column_name == "rank"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, ROWID);",
            |r| matches!(r, R::ReservedColumnName { column_name } if column_name == "ROWID"),
        ),
        ("CREATE VIRTUAL TABLE rank USING fts5(a);", |r| matches!(r, R::ReservedTableName)),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a b);",
            |r| matches!(r, R::UnrecognizedColumnOption { option, .. } if option == "b"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a UNINDEXED x);",
            |r| matches!(r, R::MalformedArgument { argument } if argument == "a UNINDEXED x"),
        ),
        ("CREATE VIRTUAL TABLE v USING fts5(\"a\" = 1);", |r| {
            matches!(r, R::MalformedArgument { .. })
        }),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, bogus=1);",
            |r| matches!(r, R::UnrecognizedOption { option } if option == "bogus"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, detail=bogus);",
            |r| matches!(r, R::MalformedOption { option, .. } if option == "detail"),
        ),
        ("CREATE VIRTUAL TABLE v USING fts5(a, prefix=0);", |r| {
            matches!(r, R::MalformedOption { .. })
        }),
        ("CREATE VIRTUAL TABLE v USING fts5(a, columnsize=2);", |r| {
            matches!(r, R::MalformedOption { .. })
        }),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, tokenize=porter, tokenize=ascii);",
            |r| matches!(r, R::RepeatedOption { option } if option == "tokenize"),
        ),
        ("CREATE VIRTUAL TABLE v USING fts5(a, content='', content='');", |r| {
            matches!(r, R::RepeatedOption { .. })
        }),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, contentless_delete=1);",
            |r| matches!(r, R::RequiresContentless { option } if option == "contentless_delete"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts5(a, content='', contentless_delete=1, columnsize=0);",
            |r| matches!(r, R::ConflictingOptions { .. }),
        ),
    ]);
}

#[test]
fn fts4_arguments_sqlite_refuses_are_refused() {
    use VirtualTableArgumentRefusal as R;
    assert_refusals(&[
        (
            "CREATE VIRTUAL TABLE v USING fts4(a, bogus=1);",
            |r| matches!(r, R::UnrecognizedOption { option } if option == "bogus"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts4(a, tokenize=porter, tokenize=simple);",
            |r| matches!(r, R::UnrecognizedOption { option } if option == "tokenize"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts4(a, order=sideways);",
            |r| matches!(r, R::MalformedOption { option, .. } if option == "order"),
        ),
        ("CREATE VIRTUAL TABLE v USING fts4(a, matchinfo=x);", |r| {
            matches!(r, R::MalformedOption { .. })
        }),
        ("CREATE VIRTUAL TABLE v USING fts4(a, prefix='x');", |r| {
            matches!(r, R::MalformedOption { .. })
        }),
        (
            "CREATE VIRTUAL TABLE v USING fts4(a, notindexed=b);",
            |r| matches!(r, R::NotIndexedColumnNotFound { column_name } if column_name == "b"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts4(a, compress=c);",
            |r| matches!(r, R::UnpairedOption { option, missing } if option == "compress" && missing == "uncompress"),
        ),
        (
            "CREATE VIRTUAL TABLE v USING fts4(content=missing);",
            |r| matches!(r, R::ContentTableNotFound { content_table } if content_table == "missing"),
        ),
    ]);
}

#[test]
fn rtree_arguments_sqlite_refuses_are_refused() {
    use VirtualTableArgumentRefusal as R;
    assert_refusals(&[
        ("CREATE VIRTUAL TABLE v USING rtree;", |r| matches!(r, R::TooFewColumns)),
        ("CREATE VIRTUAL TABLE v USING rtree(id, x0);", |r| matches!(r, R::TooFewColumns)),
        ("CREATE VIRTUAL TABLE v USING rtree(id, x0, x1, y0);", |r| {
            matches!(r, R::OddCoordinateCount)
        }),
        ("CREATE VIRTUAL TABLE v USING rtree(id, a, b, c, d, e, f, g, h, i, j, k, l);", |r| {
            matches!(r, R::TooManyColumns)
        }),
        ("CREATE VIRTUAL TABLE v USING rtree(id, x0, x1, +a, y0, y1);", |r| {
            matches!(r, R::AuxiliaryColumnNotLast)
        }),
    ]);
}

/// A column the module derives may not repeat another, the hidden ones
/// included, as `sqlite3_declare_vtab` refuses.
#[test]
fn derived_columns_may_not_repeat() {
    for statement in [
        "CREATE VIRTUAL TABLE v USING fts5(a, a);",
        "CREATE VIRTUAL TABLE w USING fts5(a, w);",
        "CREATE VIRTUAL TABLE v USING fts4(a, docid);",
        "CREATE VIRTUAL TABLE v USING rtree(id, x0, x1, x0);",
    ] {
        let refused = sqlite(statement).expect_err(statement);
        assert!(matches!(refused, Error::ColumnAlreadyExists { .. }), "{statement}: {refused:?}");
    }
}
