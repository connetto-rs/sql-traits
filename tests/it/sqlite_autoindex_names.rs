//! The name of the index behind each primary key and unique constraint, as
//! the catalog lists it among the indexes of the table.
//!
//! SQLite names the index `sqlite_autoindex_<table>_<n>`, and every expected
//! answer here is what `pragma_index_list` reported on SQLite 3.51.1 for the
//! same DDL. Keys SQLite treats as one share an index, and the rowid alias
//! has none.

#![allow(clippy::expect_used)]

use sql_traits::{errors::ObjectKind, prelude::*};
use sqlparser::{
    dialect::{Dialect, GenericDialect, PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

/// Parses `sql` under `D`, folding identifiers as SQLite does.
fn database<D: Dialect + Default + 'static>(sql: &str) -> ParserDB {
    ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<D>(sql)
        .expect("the DDL is accepted")
}

/// The table stored as `name` in `schema`, or reached unqualified.
fn table<'db>(
    database: &'db ParserDB,
    schema: Option<&str>,
    name: &str,
) -> &'db <ParserDB as DatabaseLike>::Table {
    let target = TargetName::new(name, false);
    let target = match schema {
        Some(schema) => target.with_schema(schema, false),
        None => target,
    };
    database
        .table_by_target(target, IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .expect("the table exists")
}

/// The backing index name of every unique index of the table `name` in
/// `database`, in the order the table lists them.
fn names_in(database: &ParserDB, schema: Option<&str>, name: &str) -> Vec<Option<String>> {
    table(database, schema, name)
        .unique_indices(database)
        .expect("the table is held")
        .map(|index| {
            index
                .backing_index_name(database)
                .expect("the index is held")
                .map(std::borrow::Cow::into_owned)
        })
        .collect()
}

/// The backing index names SQLite gives the constraints of the one table
/// `sql` creates, named `name`.
fn sqlite_names(sql: &str, name: &str) -> Vec<Option<String>> {
    names_in(&database::<SQLiteDialect>(sql), None, name)
}

/// The autoindex names of `table` for each ordinal, [`None`] for no index.
fn expected(table: &str, ordinals: &[Option<u8>]) -> Vec<Option<String>> {
    ordinals
        .iter()
        .map(|ordinal| ordinal.map(|ordinal| format!("sqlite_autoindex_{table}_{ordinal}")))
        .collect()
}

#[test]
fn constraints_number_in_declaration_order_and_duplicates_share() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE a(x TEXT UNIQUE, y TEXT PRIMARY KEY, z TEXT UNIQUE, UNIQUE(x), UNIQUE(y), UNIQUE(x,y));",
            "a",
        ),
        expected("a", &[Some(1), Some(2), Some(3), Some(1), Some(2), Some(4)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE ord(x TEXT, y TEXT, UNIQUE(y), PRIMARY KEY(x), UNIQUE(x,y));",
            "ord"
        ),
        expected("ord", &[Some(1), Some(2), Some(3)]),
    );
}

#[test]
fn rowid_alias_has_no_index_and_a_unique_on_it_has_its_own() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE b(x INTEGER PRIMARY KEY, y TEXT UNIQUE, UNIQUE(x), UNIQUE(y));",
            "b",
        ),
        expected("b", &[None, Some(1), Some(2), Some(1)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE d(x INTEGER, y TEXT UNIQUE, PRIMARY KEY(x DESC), UNIQUE(x));",
            "d",
        ),
        expected("d", &[Some(1), None, Some(2)]),
    );
    assert_eq!(
        sqlite_names("CREATE TABLE r2(x \"INTEGER\" PRIMARY KEY, y TEXT UNIQUE);", "r2"),
        expected("r2", &[None, Some(1)]),
    );
}

#[test]
fn only_an_integer_key_ascending_on_its_column_is_the_alias() {
    assert_eq!(
        sqlite_names("CREATE TABLE c(x INTEGER PRIMARY KEY DESC, y TEXT UNIQUE);", "c"),
        expected("c", &[Some(1), Some(2)]),
    );
    assert_eq!(
        sqlite_names("CREATE TABLE r3(x INT PRIMARY KEY, y TEXT UNIQUE);", "r3"),
        expected("r3", &[Some(1), Some(2)]),
    );
}

#[test]
fn constraints_building_no_index_take_no_ordinal() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE chk(x TEXT NOT NULL UNIQUE CHECK (x <> ''), y INTEGER PRIMARY KEY, z TEXT REFERENCES chk(x), CHECK (z <> ''), UNIQUE(z));",
            "chk",
        ),
        expected("chk", &[Some(1), None, Some(2)]),
    );
}

#[test]
fn a_handle_cloned_out_of_the_database_answers_like_the_one_it_copies() {
    let database = database::<SQLiteDialect>(
        "CREATE TABLE b(x TEXT UNIQUE, y INTEGER PRIMARY KEY, z TEXT UNIQUE);",
    );
    let cloned: Vec<_> = table(&database, None, "b")
        .unique_indices(&database)
        .expect("the table is held")
        .cloned()
        .collect();
    let names: Vec<_> = cloned
        .iter()
        .map(|index| {
            index
                .backing_index_name(&database)
                .expect("an equal index is held")
                .map(std::borrow::Cow::into_owned)
        })
        .collect();
    assert_eq!(names, expected("b", &[Some(1), None, Some(2)]));
}

#[test]
fn written_constraint_names_leave_the_index_name_alone() {
    let database = database::<SQLiteDialect>(
        "CREATE TABLE named(x TEXT CONSTRAINT uq UNIQUE, y TEXT, CONSTRAINT pk PRIMARY KEY(y));",
    );
    let answers: Vec<_> = table(&database, None, "named")
        .unique_indices(&database)
        .expect("the table is held")
        .map(|index| {
            (index.name(), index.backing_index_name(&database).expect("the index is held"))
        })
        .collect();
    assert_eq!(
        answers,
        [
            (Some("uq"), Some("sqlite_autoindex_named_1".into())),
            (Some("pk"), Some("sqlite_autoindex_named_2".into())),
        ],
    );
}

#[test]
fn keys_share_an_index_across_directions_but_not_order_or_collation() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE diff(x TEXT, y TEXT, UNIQUE(x,y), UNIQUE(y,x), UNIQUE(x ASC,y DESC), UNIQUE(x COLLATE NOCASE,y));",
            "diff",
        ),
        expected("diff", &[Some(1), Some(2), Some(1), Some(3)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE dirs(x TEXT, y TEXT, UNIQUE(x DESC,y), UNIQUE(x,y DESC));",
            "dirs",
        ),
        expected("dirs", &[Some(1), Some(1)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE r5(x TEXT, y TEXT, UNIQUE(x,x), UNIQUE(x), UNIQUE(x,y,x), UNIQUE(x,y));",
            "r5"
        ),
        expected("r5", &[Some(1), Some(2), Some(3), Some(4)]),
    );
}

#[test]
fn collations_compare_effectively_and_without_case() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE coll(x TEXT COLLATE NOCASE, UNIQUE(x), UNIQUE(x COLLATE nocase), UNIQUE(x COLLATE BINARY));",
            "coll",
        ),
        expected("coll", &[Some(1), Some(1), Some(2)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE r4(x TEXT COLLATE NOCASE COLLATE BINARY, UNIQUE(x), UNIQUE(x COLLATE BINARY), UNIQUE(x COLLATE NOCASE));",
            "r4",
        ),
        expected("r4", &[Some(1), Some(1), Some(2)]),
    );
}

#[test]
fn primary_key_joins_an_earlier_unique_on_the_same_key() {
    assert_eq!(
        sqlite_names("CREATE TABLE rev(x TEXT UNIQUE PRIMARY KEY, y TEXT UNIQUE);", "rev"),
        expected("rev", &[Some(1), Some(1), Some(2)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE w2(x TEXT, y TEXT, UNIQUE(x), PRIMARY KEY(x), UNIQUE(y)) WITHOUT ROWID;",
            "w2",
        ),
        expected("w2", &[Some(1), Some(1), Some(2)]),
    );
}

#[test]
fn without_rowid_builds_a_text_key_in_place() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE w(x TEXT UNIQUE, y TEXT PRIMARY KEY, z TEXT UNIQUE, UNIQUE(y)) WITHOUT ROWID;",
            "w",
        ),
        expected("w", &[Some(1), Some(2), Some(3), Some(2)]),
    );
}

#[test]
fn without_rowid_builds_the_would_be_alias_last() {
    assert_eq!(
        sqlite_names("CREATE TABLE w3(x INTEGER PRIMARY KEY, y TEXT UNIQUE) WITHOUT ROWID;", "w3"),
        expected("w3", &[Some(2), Some(1)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE wi(x INTEGER PRIMARY KEY, y TEXT UNIQUE, z TEXT UNIQUE, UNIQUE(x)) WITHOUT ROWID;",
            "wi",
        ),
        expected("wi", &[Some(3), Some(1), Some(2), Some(3)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE wi2(x INTEGER, y TEXT UNIQUE, z TEXT UNIQUE, PRIMARY KEY(x DESC)) WITHOUT ROWID;",
            "wi2",
        ),
        expected("wi2", &[Some(1), Some(2), Some(3)]),
    );
    assert_eq!(
        sqlite_names("CREATE TABLE wi4(x INTEGER PRIMARY KEY) WITHOUT ROWID;", "wi4"),
        expected("wi4", &[Some(1)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE pre(x INTEGER UNIQUE PRIMARY KEY, y TEXT UNIQUE) WITHOUT ROWID;",
            "pre"
        ),
        expected("pre", &[Some(1), Some(1), Some(2)]),
    );
    assert_eq!(
        sqlite_names(
            "CREATE TABLE post(x INTEGER PRIMARY KEY UNIQUE, y TEXT UNIQUE) WITHOUT ROWID;",
            "post"
        ),
        expected("post", &[Some(1), Some(1), Some(2)]),
    );
}

#[test]
fn without_rowid_keeps_a_descending_column_key_in_place() {
    assert_eq!(
        sqlite_names(
            "CREATE TABLE wi3(x INTEGER PRIMARY KEY DESC, y TEXT UNIQUE) WITHOUT ROWID;",
            "wi3",
        ),
        expected("wi3", &[Some(1), Some(2)]),
    );
}

#[test]
fn names_keep_the_stored_table_spelling() {
    assert_eq!(
        sqlite_names("CREATE TABLE MiXeD(x TEXT UNIQUE);", "mixed"),
        [Some("sqlite_autoindex_MiXeD_1".to_owned())],
    );
    let database = database::<SQLiteDialect>("CREATE TABLE \"Q\"\"T\"(x TEXT UNIQUE);");
    let quoted = database.tables().next().expect("the table exists");
    let names: Vec<_> = quoted
        .unique_indices(&database)
        .expect("the table is held")
        .map(|index| index.backing_index_name(&database).expect("the index is held"))
        .collect();
    assert_eq!(names, [Some("sqlite_autoindex_Q\"T_1".into())]);
}

#[test]
fn a_rename_renames_the_autoindexes_but_not_an_explicit_index() {
    let creation = "CREATE TABLE \"MiX Name\"(x TEXT UNIQUE, y TEXT PRIMARY KEY);
        CREATE INDEX explicit_name ON \"MiX Name\"(x);";
    let rename = "ALTER TABLE \"MiX Name\" RENAME TO \"New CASE\";";
    let renamed = expected("New CASE", &[Some(1), Some(2)]);

    let at_once = database::<SQLiteDialect>(&format!("{creation} {rename}"));
    let statement = Parser::parse_sql(&SQLiteDialect {}, rename)
        .expect("the rename parses")
        .pop()
        .expect("one statement");
    let later = database::<SQLiteDialect>(creation)
        .into_ingestor()
        .apply_statement(statement)
        .expect("the rename applies")
        .finish();
    let stored = database::<SQLiteDialect>(
        "CREATE TABLE \"New CASE\"(x TEXT UNIQUE, y TEXT PRIMARY KEY);
         CREATE INDEX explicit_name ON \"New CASE\"(x);",
    );
    for database in [&at_once, &later, &stored] {
        let table = database
            .table_by_target(TargetName::new("New CASE", true), IdentifierCase::Folded)
            .expect("unambiguous lookup")
            .expect("the renamed table exists");
        let names: Vec<_> = table
            .unique_indices(database)
            .expect("the table is held")
            .map(|index| {
                index.backing_index_name(database).expect("the index is held").map(Into::into)
            })
            .collect();
        assert_eq!(names, renamed);
        let explicit: Vec<_> = table
            .indices(database)
            .expect("the table is held")
            .map(|index| index.backing_index_name(database).expect("the index is held"))
            .collect();
        assert_eq!(explicit, [Some("explicit_name".into())]);
    }
}

#[test]
fn each_database_numbers_its_own_tables_without_a_schema_prefix() {
    let database = database::<SQLiteDialect>(
        "ATTACH ':memory:' AS aux;
         CREATE TABLE main.Same(x TEXT UNIQUE);
         CREATE TABLE aux.Same(x TEXT UNIQUE);
         CREATE TEMP TABLE Same(x TEXT UNIQUE);",
    );
    for schema in ["main", "aux", "temp"] {
        assert_eq!(
            names_in(&database, Some(schema), "same"),
            [Some("sqlite_autoindex_Same_1".to_owned())],
            "{schema}",
        );
        let index = table(&database, Some(schema), "same")
            .unique_indices(&database)
            .expect("the table is held")
            .next()
            .expect("the key exists");
        assert_eq!(index.schema(), Some(schema));
    }
}

#[test]
fn postgres_answers_the_name_its_constraint_gives_the_index() {
    let database = database::<PostgreSqlDialect>(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, CONSTRAINT named UNIQUE (id, email));",
    );
    assert_eq!(
        names_in(&database, None, "users"),
        [
            Some("users_pkey".to_owned()),
            Some("users_email_key".to_owned()),
            Some("named".to_owned()),
        ],
    );
}

#[test]
fn other_dialects_answer_the_written_name() {
    let database = database::<GenericDialect>(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT CONSTRAINT uq UNIQUE);",
    );
    assert_eq!(names_in(&database, None, "users"), [None, Some("uq".to_owned())]);
}

#[test]
fn an_index_from_another_database_is_reported() {
    let holder = database::<SQLiteDialect>("CREATE TABLE t(x TEXT UNIQUE);");
    let other = database::<SQLiteDialect>("CREATE TABLE t(x TEXT);");
    let index = table(&holder, None, "t")
        .unique_indices(&holder)
        .expect("the table is held")
        .next()
        .expect("the key exists");
    assert_eq!(
        index.backing_index_name(&other),
        Err(ObjectKind::UniqueIndex.anonymous_not_in_database("t")),
    );
}
