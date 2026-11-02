//! Tests that `public` behaves as the schema PostgreSQL makes it, one every
//! database starts with that can be renamed, dropped, re-owned and created
//! again.
//!
//! The expectations below were read from PostgreSQL 16, except that a plain
//! `CREATE SCHEMA public` while it exists is accepted and changes nothing,
//! since `pg_dump -n public` writes that statement.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Objects in `public` calling and reading one another, with a foreign key
/// and an extension reaching into it.
const PUBLIC: &str = "CREATE SEQUENCE q;
     CREATE TABLE t (id INT PRIMARY KEY, n INT DEFAULT nextval('q'));
     CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
     CREATE VIEW v AS SELECT id, f() AS one FROM t;
     CREATE EXTENSION pg_trgm;
     CREATE SCHEMA other;
     CREATE TABLE other.x (a INT REFERENCES public.t (id));";

fn table<'db>(
    db: &'db ParserDB,
    schema: &str,
    name: &str,
) -> Option<&'db <ParserDB as DatabaseLike>::Table> {
    db.table_by_target(
        TargetName::new(name, false).with_schema(schema, false),
        IdentifierCase::AsWritten,
    )
    .expect("unambiguous lookup")
}

#[test]
fn renaming_public_moves_what_it_holds() {
    let db = parse(&format!("{PUBLIC} ALTER SCHEMA public RENAME TO main;"))
        .expect("public can be renamed");

    let t = table(&db, "main", "t").expect("t moved");
    assert!(table(&db, "public", "t").is_none());
    let default = t
        .columns(&db)
        .expect("t is held")
        .find(|column| column.column_name() == "n")
        .and_then(ColumnLike::default_value)
        .expect("the default survives");
    assert!(default.contains("main.q"), "{default}");
    let sequence = db.sequences().next().expect("q is held");
    assert_eq!(sequence.sequence_schema(), Some("main"));
    let f = db.functions().find(|function| function.name() == "f").expect("f is held");
    assert_eq!(f.target_name().schema(), Some("main"));
    let v = db.views().next().expect("v is held");
    assert_eq!(v.view_schema(), Some("main"));
    let definition = v.definition().to_string();
    assert!(definition.contains("main.f()") && definition.contains("main.t"), "{definition}");
    let extension = db.extensions().next().expect("pg_trgm is held");
    assert_eq!(extension.schema(), Some("main"));
    let x = table(&db, "other", "x").expect("x stays");
    let key = x.foreign_keys(&db).expect("x is held").next().expect("the key survives");
    let target = key.referenced_table_name();
    assert_eq!((target.schema(), target.name()), (Some("main"), "t"));
}

#[test]
fn a_renamed_or_dropped_public_is_gone_until_created_again() {
    for gone in ["ALTER SCHEMA public RENAME TO main;", "DROP SCHEMA public;"] {
        let result = parse(&format!("{gone} CREATE TABLE y (a INT);"));
        assert!(
            matches!(
                &result,
                Err(Error::NoSchemaSelectedForRelation { object_kind: ObjectKind::Table, .. })
            ),
            "{gone} got {result:?}"
        );
        let result = parse(&format!("{gone} CREATE TABLE public.y (a INT);"));
        assert!(
            matches!(&result, Err(Error::SchemaNotFoundForRelation { schema_name, .. }) if schema_name == "public"),
            "{gone} got {result:?}"
        );
        parse(&format!("{gone} DROP SCHEMA IF EXISTS public;"))
            .expect("an absent public is skipped");
        let result = parse(&format!("{gone} DROP SCHEMA public;"));
        assert!(matches!(result, Err(Error::DropSchemaNotFound { .. })), "{gone} got {result:?}");

        let db = parse(&format!("{gone} CREATE SCHEMA public; CREATE TABLE y (a INT);"))
            .expect("public can be created again");
        assert!(table(&db, "public", "y").is_some(), "{gone}");
    }
}

#[test]
fn creating_public_while_it_exists_changes_nothing() {
    for statement in ["CREATE SCHEMA public;", "CREATE SCHEMA IF NOT EXISTS public;"] {
        let db = parse(&format!("CREATE TABLE t (a INT); {statement} CREATE TABLE u (a INT);"))
            .expect("public is already there");
        assert!(table(&db, "public", "t").is_some() && table(&db, "public", "u").is_some());
    }
}

#[test]
fn dropping_public_counts_what_it_holds() {
    let result = parse("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1'; DROP SCHEMA public;");
    assert!(
        matches!(&result, Err(Error::SchemaNotEmpty { schema_name }) if schema_name == "public"),
        "got {result:?}"
    );
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         CREATE VIEW v AS SELECT a, f() AS one FROM t;
         CREATE SEQUENCE q;
         DROP SCHEMA public CASCADE;",
    )
    .expect("the cascade is accepted");
    assert_eq!(db.tables().count(), 0);
    assert_eq!(db.views().count(), 0);
    assert_eq!(db.sequences().count(), 0);
    assert_eq!(db.functions().filter(|function| function.name() == "f").count(), 0);
}

#[test]
fn public_can_be_given_an_owner() {
    let db = parse("CREATE ROLE pub_owner; ALTER SCHEMA public OWNER TO pub_owner;")
        .expect("public can be re-owned");
    let (public, ()) =
        db.schemas().find(|(schema, ())| schema.name() == "public").expect("public is listed");
    assert_eq!(public.authorization(), Some("pub_owner"));
}

#[test]
fn a_schema_takes_the_name_public_only_once_public_is_gone() {
    let result = parse("CREATE SCHEMA app; ALTER SCHEMA app RENAME TO public;");
    assert!(
        matches!(&result, Err(Error::SchemaAlreadyExists { schema_name }) if schema_name == "public"),
        "got {result:?}"
    );
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (a INT);
         DROP SCHEMA public;
         ALTER SCHEMA app RENAME TO public;
         CREATE TABLE y (a INT);",
    )
    .expect("app becomes public");
    assert!(table(&db, "public", "t").is_some());
    assert!(table(&db, "public", "y").is_some());
}

/// A collation and an extension land through the path as a table does, so
/// with `public` gone they find no schema either.
#[test]
fn collations_and_extensions_find_no_schema_once_public_is_gone() {
    let result = parse(
        "DROP SCHEMA public;
         CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);",
    );
    assert!(matches!(result, Err(Error::NoSchemaSelectedForCollation { .. })), "got {result:?}");
    let result = parse("DROP SCHEMA public; CREATE EXTENSION pg_trgm;");
    assert!(matches!(result, Err(Error::NoSchemaSelectedForExtension { .. })), "got {result:?}");
}
