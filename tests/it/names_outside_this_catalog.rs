//! Tests that a name reaching outside the catalog is refused rather than read
//! as a local object.
//!
//! A three-part name names another database, and a column reference qualified
//! by another table names a column this table does not have. Both used to be
//! answered by dropping the part that carries the meaning, so a schema
//! recorded a local table for a name a real server refuses, and a check
//! constraint was recorded against a column nobody named.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn build(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn assert_refused_as_too_many_parts(sql: &str) {
    match build(sql) {
        Err(Error::IdentifierLookupError(LookupError::InvalidObjectName {
            object_name,
            reason,
        })) => {
            assert!(
                reason.contains("one-part or two-part"),
                "`{sql}` was refused for another reason: {reason}"
            );
            assert!(!object_name.is_empty(), "`{sql}` refused without naming the name it refused");
        }
        Err(other) => panic!("`{sql}` was refused for another reason: {other}"),
        Ok(_) => panic!("`{sql}` was accepted"),
    }
}

/// A creation refuses a name that reaches into another database, which is
/// what a reference to the same name has always done.
#[test]
fn a_creation_naming_another_database_is_refused() {
    assert_refused_as_too_many_parts("CREATE TABLE other_db.public.docs (id INT)");
    assert_refused_as_too_many_parts(
        "CREATE FUNCTION other_db.public.f() RETURNS INT LANGUAGE sql AS 'SELECT 1'",
    );
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT); CREATE VIEW other_db.public.v AS SELECT id FROM docs",
    );
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT);
         CREATE MATERIALIZED VIEW other_db.public.m AS SELECT id FROM docs",
    );
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT); CREATE INDEX other_db.public.i ON docs (id)",
    );
}

/// The references that already refused such a name keep refusing it.
#[test]
fn a_reference_to_another_database_stays_refused() {
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT); ALTER TABLE docs RENAME TO other_db.public.papers",
    );
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT);
         CREATE TABLE child (d INT REFERENCES other_db.public.docs(id))",
    );
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT); CREATE POLICY p ON other_db.public.docs USING (true)",
    );
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT);
         CREATE ROLE r;
         GRANT SELECT ON other_db.public.docs TO r",
    );
}

/// A creation may carry this catalog outermost, which PostgreSQL 18 accepts
/// for a table, a function and a view, and the leading part is dropped rather
/// than recorded as part of the name.
#[test]
fn a_creation_naming_this_catalog_is_accepted_and_shortened() {
    let db = build(
        "CREATE TABLE unknown_catalog.public.docs (id INT);
         CREATE FUNCTION unknown_catalog.public.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE VIEW unknown_catalog.public.v AS SELECT id FROM docs;
         CREATE MATERIALIZED VIEW unknown_catalog.public.m AS SELECT id FROM docs;",
    )
    .expect("this catalog's own name builds");

    let docs = db
        .table_by_target(
            TargetName::new("docs", false).with_schema("public", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table is recorded in public");
    assert_eq!(docs.table_name(), "docs");
    assert_eq!(docs.table_schema(), Some("public"));
    assert!(
        db.function_by_target(
            TargetName::new("f", false).with_schema("public", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
    assert!(
        db.view_by_target(
            TargetName::new("v", false).with_schema("public", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
    assert!(
        db.materialized_view_by_target(
            TargetName::new("m", false).with_schema("public", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
}

/// An index name carries no qualifier at all in PostgreSQL 18, and certainly
/// not this catalog, so the catalog spelling is refused there even though a
/// table accepts it.
#[test]
fn an_index_name_takes_no_catalog() {
    assert_refused_as_too_many_parts(
        "CREATE TABLE docs (id INT); CREATE INDEX unknown_catalog.public.i ON docs (id)",
    );
}

/// A one-part or two-part name is unaffected.
#[test]
fn local_names_are_unaffected() {
    let db = build(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE plain (id INT);
         CREATE FUNCTION app.f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE INDEX app.i ON app.docs (id);",
    )
    .expect("local names build");

    assert!(
        db.table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
    assert!(
        db.table_by_target(TargetName::new("plain", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
    assert!(
        db.function_by_target(
            TargetName::new("f", false).with_schema("app", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );

    // The index reads its column through the qualified table it is on.
    let docs = db
        .table_by_target(
            TargetName::new("docs", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table exists");
    let index = db.indexes().next().expect("the index is recorded");
    let columns: Vec<_> = index
        .columns(&db)
        .expect("columns resolve")
        .map(|column| column.column_name().to_string())
        .collect();
    assert_eq!(columns, vec!["id"]);
    assert_eq!(IndexLike::table(index, &db).table_name(), docs.table_name());
}

/// A check constraint may qualify a column by its own table, and nothing
/// else: another table's column is not this table's column under a shorter
/// name.
#[test]
fn a_check_constraint_qualified_by_another_table_is_refused() {
    let own = build("CREATE TABLE t (id INT, CHECK (t.id > 0));").expect("self-qualified builds");
    let table = own
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let check = table
        .check_constraints(&own)
        .expect("constraints resolve")
        .next()
        .expect("one check constraint");
    let columns: Vec<_> = check
        .columns(&own)
        .expect("columns resolve")
        .map(|column| column.column_name().to_string())
        .collect();
    assert_eq!(columns, vec!["id"]);

    let quoted = build("CREATE TABLE \"T\" (id INT, CHECK (\"T\".id > 0));");
    assert!(quoted.is_ok(), "a quoted self-qualified reference builds: {quoted:?}");

    // The depths a server accepts, replayed against PostgreSQL 18: the
    // table, its schema, and this catalog outermost.
    for sql in [
        "CREATE SCHEMA s; CREATE TABLE s.t (id INT, CHECK (s.t.id > 0));",
        "CREATE TABLE pt (id INT, CHECK (public.pt.id > 0));",
        "CREATE TABLE ct (id INT, CHECK (unknown_catalog.public.ct.id > 0));",
        "CREATE SCHEMA s2; CREATE TABLE s2.ct2 (id INT, CHECK (unknown_catalog.s2.ct2.id > 0));",
    ] {
        assert!(build(sql).is_ok(), "`{sql}` was refused: {:?}", build(sql).err());
    }

    // A catalog this one is not keeps its own objects.
    assert!(
        build("CREATE TABLE ot (id INT, CHECK (other_db.public.ot.id > 0));").is_err(),
        "another catalog's column was read as local"
    );

    for sql in [
        "CREATE SCHEMA app; CREATE TABLE app.t (id INT); CREATE TABLE t2 (id INT, CHECK (app.t.id > 0));",
        "CREATE TABLE t3 (id INT, CHECK (a.b.c.id > 0));",
        "CREATE TABLE t4 (id INT, CHECK (t3.id > 0));",
    ] {
        assert!(build(sql).is_err(), "`{sql}` was accepted");
    }
}
