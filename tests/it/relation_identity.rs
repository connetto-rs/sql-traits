//! A table, view or column keeps one identity for as long as it exists,
//! through its renames and rebuilds, and a dropped object's identity is never
//! handed out again.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

extern crate alloc;

use alloc::sync::Arc;

use sql_traits::{
    structs::{
        ColumnMetadata, IdentifierCase, ParserDB, ParserDBBuilder, ParserDBIngestor,
        TableAttribute, TableMetadata, TargetName,
    },
    traits::{ColumnLike, DatabaseLike, TableLike, ViewLike},
};
use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};

fn apply(mut input: ParserDBIngestor, sql: &str) -> ParserDBIngestor {
    for statement in Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("SQL parses") {
        input = input.apply_statement(statement).expect("statement applies");
    }
    input
}

fn ingest(sql: &str) -> ParserDBIngestor {
    apply(ParserDBIngestor::new::<PostgreSqlDialect>("identity".to_owned()), sql)
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("table exists")
}

fn column<'db>(
    db: &'db ParserDB,
    table_name: &str,
    column_name: &str,
) -> &'db <ParserDB as DatabaseLike>::Column {
    table(db, table_name)
        .column(column_name, db, IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("column exists")
}

fn column_names(db: &ParserDB, table_name: &str) -> Vec<String> {
    table(db, table_name)
        .columns(db)
        .expect("columns resolve")
        .map(|column| column.column_name().to_owned())
        .collect()
}

#[test]
fn renamed_table_and_column_resolve_from_an_earlier_snapshot() {
    let input = ingest("CREATE TABLE t (a INT, c INT);");
    let before = input.snapshot();
    let table_id = before.table_identity(table(&before, "t")).expect("table has an identity");
    let column_id =
        before.column_identity(column(&before, "t", "a")).expect("column has an identity");

    let after =
        apply(input, "ALTER TABLE t RENAME TO u; ALTER TABLE u RENAME COLUMN a TO b;").finish();

    let renamed = after.table_by_identity(table_id).expect("the table still exists");
    assert_eq!(renamed.table_name(), "u");
    let renamed_column = after.column_by_identity(column_id).expect("the column still exists");
    assert_eq!(renamed_column.column_name(), "b");
    assert_eq!(ColumnLike::table(renamed_column, &after).table_name(), "u");
    assert_eq!(after.table_identity(renamed), Some(table_id));
    assert_eq!(after.column_identity(renamed_column), Some(column_id));
}

#[test]
fn a_table_created_again_under_a_dropped_name_is_a_new_table() {
    let input = ingest("CREATE TABLE t (a INT);");
    let first = input.snapshot();
    let first_table = first.table_identity(table(&first, "t")).expect("table has an identity");
    let first_column = first.column_identity(column(&first, "t", "a")).expect("column identity");

    let second = apply(input, "DROP TABLE t; CREATE TABLE t (a INT);").finish();

    let second_table = second.table_identity(table(&second, "t")).expect("table has an identity");
    let second_column =
        second.column_identity(column(&second, "t", "a")).expect("column has an identity");
    assert_ne!(first_table, second_table);
    assert_ne!(first_column, second_column);
    assert!(second.table_by_identity(first_table).is_none());
    assert!(second.column_by_identity(first_column).is_none());
}

#[test]
fn a_column_keeps_its_identity_while_its_neighbours_come_and_go() {
    let input = ingest("CREATE TABLE t (a INT, b INT, c INT);");
    let before = input.snapshot();
    let a = before.column_identity(column(&before, "t", "a")).expect("identity");
    let b = before.column_identity(column(&before, "t", "b")).expect("identity");
    let c = before.column_identity(column(&before, "t", "c")).expect("identity");

    let after = apply(
        input,
        "ALTER TABLE t DROP COLUMN a;
         ALTER TABLE t ALTER COLUMN c TYPE BIGINT;
         ALTER TABLE t ADD CONSTRAINT c_positive CHECK (c > 0);
         ALTER TABLE t ADD COLUMN a INT;",
    )
    .finish();

    assert_eq!(column_names(&after, "t"), ["b", "c", "a"]);
    assert_eq!(after.column_by_identity(b).map(ColumnLike::column_name), Some("b"));
    assert_eq!(after.column_by_identity(c).map(ColumnLike::column_name), Some("c"));
    assert!(after.column_by_identity(a).is_none());
    let readded = after.column_identity(column(&after, "t", "a")).expect("identity");
    assert!(![a, b, c].contains(&readded));
}

#[test]
fn identities_are_distinct_across_tables_views_and_columns() {
    let db = ingest(
        "CREATE TABLE t (a INT, b INT);
         CREATE TABLE s (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM s;",
    )
    .finish();

    let mut relations = vec![
        db.table_identity(table(&db, "t")).expect("identity"),
        db.table_identity(table(&db, "s")).expect("identity"),
        db.view_identity(db.views().next().expect("view")).expect("identity"),
        db.materialized_view_identity(db.materialized_views().next().expect("view"))
            .expect("identity"),
    ];
    relations.sort_unstable();
    relations.dedup();
    assert_eq!(relations.len(), 4);
    assert!(db.materialized_view_by_identity(relations[0]).is_none());

    let mut columns = vec![
        db.column_identity(column(&db, "t", "a")).expect("identity"),
        db.column_identity(column(&db, "t", "b")).expect("identity"),
        db.column_identity(column(&db, "s", "a")).expect("identity"),
    ];
    columns.sort_unstable();
    columns.dedup();
    assert_eq!(columns.len(), 3);
}

#[test]
fn a_rolled_back_creation_never_has_its_identity_handed_out_again() {
    for (inside, after) in [
        ("BEGIN; CREATE TABLE t (a INT);", "ROLLBACK; CREATE TABLE t (a INT);"),
        (
            "BEGIN; SAVEPOINT s; CREATE TABLE t (a INT);",
            "ROLLBACK TO SAVEPOINT s; COMMIT; CREATE TABLE t (a INT);",
        ),
    ] {
        let input = ingest(inside);
        let created = input.snapshot();
        let table_id = created.table_identity(table(&created, "t")).expect("identity");
        let column_id = created.column_identity(column(&created, "t", "a")).expect("identity");

        let db = apply(input, after).finish();
        assert_ne!(db.table_identity(table(&db, "t")), Some(table_id), "{inside} {after}");
        assert_ne!(db.column_identity(column(&db, "t", "a")), Some(column_id), "{inside} {after}");
        assert!(db.table_by_identity(table_id).is_none());
    }

    // A session ending inside a transaction rolls it back too.
    let input = ingest("BEGIN; CREATE TABLE t (a INT);");
    let created = input.snapshot();
    let table_id = created.table_identity(table(&created, "t")).expect("identity");
    let reopened = apply(input.finish().into_ingestor(), "CREATE TABLE t (a INT);").finish();
    assert_ne!(reopened.table_identity(table(&reopened, "t")), Some(table_id));
}

#[test]
fn a_rolled_back_rename_resolves_an_identity_to_its_old_name_again() {
    for (renames, rollback) in
        [("BEGIN;", "ROLLBACK;"), ("BEGIN; SAVEPOINT s;", "ROLLBACK TO SAVEPOINT s; COMMIT;")]
    {
        let input = ingest("CREATE TABLE t (a INT);");
        let before = input.snapshot();
        let table_id = before.table_identity(table(&before, "t")).expect("identity");
        let column_id = before.column_identity(column(&before, "t", "a")).expect("identity");

        let input = apply(
            input,
            &format!("{renames} ALTER TABLE t RENAME TO u; ALTER TABLE u RENAME COLUMN a TO b;"),
        );
        let renamed = input.snapshot();
        assert_eq!(renamed.table_by_identity(table_id).map(TableLike::table_name), Some("u"));

        let db = apply(input, rollback).finish();
        assert_eq!(db.table_by_identity(table_id).map(TableLike::table_name), Some("t"));
        assert_eq!(db.column_by_identity(column_id).map(ColumnLike::column_name), Some("a"));
    }
}

#[test]
fn a_database_assembled_through_the_builder_gives_each_object_an_identity() {
    let Some(Statement::CreateTable(node)) =
        Parser::parse_sql(&PostgreSqlDialect {}, "CREATE TABLE t (a INT, b INT);")
            .expect("SQL parses")
            .pop()
    else {
        panic!("expected a table")
    };
    let node = Arc::new(node);
    let columns: Vec<_> = node
        .columns
        .iter()
        .map(|definition| Arc::new(TableAttribute::new(node.clone(), definition.clone())))
        .collect();
    let empty = ingest("").finish();
    let mut builder = ParserDBBuilder::new("built".to_owned(), *empty.dialect());
    for column in &columns {
        builder = builder.add_column(column.clone(), ColumnMetadata::default());
    }
    let db: ParserDB =
        builder.add_table(node, TableMetadata::default()).expect("the table is added").into();

    let table_id = db.table_identity(db.tables().next().expect("one table")).expect("identity");
    assert_eq!(db.table_by_identity(table_id).map(TableLike::table_name), Some("t"));
    let identities: Vec<_> = columns
        .iter()
        .map(|column| db.column_identity(column).expect("every column has an identity"))
        .collect();
    assert_ne!(identities[0], identities[1]);
    assert_eq!(db.column_by_identity(identities[1]).map(ColumnLike::column_name), Some("b"));
}

#[test]
fn a_view_keeps_its_identity_through_rename_and_replacement() {
    let input = ingest(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;",
    );
    let before = input.snapshot();
    let view = before.view_identity(before.views().next().expect("view")).expect("identity");
    let materialized = before
        .materialized_view_identity(before.materialized_views().next().expect("view"))
        .expect("identity");

    let after = apply(
        input,
        "ALTER TABLE v RENAME TO w;
         CREATE OR REPLACE VIEW w AS SELECT a, b FROM t;
         ALTER TABLE m RENAME TO n;",
    )
    .finish();

    let renamed = after.view_by_identity(view).expect("the view still exists");
    assert_eq!(renamed.view_name(), "w");
    assert_eq!(after.view_identity(renamed), Some(view));
    let renamed = after.materialized_view_by_identity(materialized).expect("still exists");
    assert_eq!(renamed.view_name(), "n");
    assert!(after.table_by_identity(view).is_none());
    assert!(after.view_by_identity(materialized).is_none());
}

#[test]
fn an_inheriting_table_keeps_its_column_identity_through_a_parent_rename() {
    let input =
        ingest("CREATE TABLE parent (a INT); CREATE TABLE child (b INT) INHERITS (parent);");
    let before = input.snapshot();
    let parent_a = before.column_identity(column(&before, "parent", "a")).expect("identity");
    let child_a = before.column_identity(column(&before, "child", "a")).expect("identity");
    let child_b = before.column_identity(column(&before, "child", "b")).expect("identity");
    assert_ne!(parent_a, child_a);

    let after = apply(input, "ALTER TABLE parent RENAME COLUMN a TO z;").finish();

    assert_eq!(after.column_by_identity(parent_a).map(ColumnLike::column_name), Some("z"));
    let child = after.column_by_identity(child_a).expect("the inherited column still exists");
    assert_eq!(child.column_name(), "z");
    assert_eq!(ColumnLike::table(child, &after).table_name(), "child");
    assert_eq!(after.column_by_identity(child_b).map(ColumnLike::column_name), Some("b"));
}

#[test]
fn a_database_reopened_for_ingestion_never_reuses_an_identity() {
    let input = ingest("CREATE TABLE t (a INT);");
    let first = input.snapshot();
    let dropped_table = first.table_identity(table(&first, "t")).expect("identity");
    let dropped_column = first.column_identity(column(&first, "t", "a")).expect("identity");

    let db = apply(input, "DROP TABLE t;").finish();
    let reopened = apply(db.into_ingestor(), "CREATE TABLE t (a INT);").finish();

    let table_id = reopened.table_identity(table(&reopened, "t")).expect("identity");
    let column_id = reopened.column_identity(column(&reopened, "t", "a")).expect("identity");
    assert_ne!(table_id, dropped_table);
    assert_ne!(column_id, dropped_column);
}

#[test]
fn an_object_from_another_database_has_no_identity_here() {
    let db = ingest("CREATE TABLE t (a INT);").finish();
    let other = ingest("CREATE TABLE elsewhere (a INT);").finish();
    assert!(db.table_identity(table(&other, "elsewhere")).is_none());
    assert!(db.column_identity(column(&other, "elsewhere", "a")).is_none());
}
