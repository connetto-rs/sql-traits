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
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

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

/// `public` stays owned by `pg_database_owner` whatever role applies the DDL,
/// until an owner change or a recreation.
#[test]
fn public_starts_owned_by_pg_database_owner() {
    let owner = |db: &ParserDB, schema: &str| {
        db.schema(schema).expect("schema is listed").authorization().map(str::to_owned)
    };
    let db = parse("CREATE TABLE t (a INT);").expect("t lands in public");
    assert_eq!(owner(&db, "public"), Some("pg_database_owner".to_owned()));
    let db = ParseOptions::default()
        .with_session_role("migrator")
        .parse::<PostgreSqlDialect>("CREATE ROLE migrator; CREATE TABLE t (a INT);")
        .expect("t lands in public");
    assert_eq!(owner(&db, "public"), Some("pg_database_owner".to_owned()));
    let db = parse("ALTER SCHEMA public RENAME TO main;").expect("public can be renamed");
    assert_eq!(owner(&db, "main"), Some("pg_database_owner".to_owned()));

    let db = parse("CREATE ROLE pub_owner; ALTER SCHEMA public OWNER TO pub_owner;")
        .expect("public can be re-owned");
    assert_eq!(owner(&db, "public"), Some("pub_owner".to_owned()));
    let db = parse("DROP SCHEMA public; CREATE SCHEMA public;").expect("public can be created");
    assert_eq!(owner(&db, "public"), None, "a recreated public belongs to its creator");

    let db =
        ParserDB::parse::<GenericDialect>("CREATE TABLE t (a INT);").expect("t lands in public");
    assert_eq!(owner(&db, "public"), None, "only PostgreSQL has the predefined role");
}

/// Grants, revokes and owner changes may name `public`'s owner without a
/// `CREATE ROLE`.
#[test]
fn pg_database_owner_needs_no_create_role() {
    let sql = "CREATE SCHEMA app; CREATE TABLE app.t (a INT);
         GRANT USAGE ON SCHEMA app TO pg_database_owner;
         REVOKE CREATE ON SCHEMA public FROM \"pg_database_owner\";
         ALTER TABLE app.t OWNER TO PG_DATABASE_OWNER;
         ALTER SCHEMA app OWNER TO pg_database_owner;";
    let db = parse(sql).expect("every PostgreSQL database holds pg_database_owner");
    assert_eq!(db.schema("app").expect("app is listed").authorization(), Some("pg_database_owner"));
    assert!(db.role("pg_database_owner").is_none(), "the role is not declared");
    assert!(db.validate_access_targets().is_ok());

    let refused =
        ParserDB::parse::<GenericDialect>("GRANT USAGE ON SCHEMA public TO pg_database_owner;");
    assert!(
        matches!(&refused, Err(Error::RoleNotFoundForGrant { role_name })
            if role_name == "pg_database_owner"),
        "got {refused:?}"
    );
}

/// The catalog lists `public` from the start, under every dialect, and stops
/// listing it once it is dropped or renamed.
#[test]
fn public_is_listed_while_it_exists() {
    let listed = |db: &ParserDB| -> Vec<String> {
        DatabaseLike::schemas(db).map(|schema| schema.name().to_owned()).collect()
    };

    let db = parse("CREATE TABLE t (id INT);").expect("t lands in public");
    assert_eq!(listed(&db), ["public"]);
    let db =
        ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);").expect("t lands in public");
    assert_eq!(listed(&db), ["public"]);

    let db = parse("CREATE SCHEMA app; DROP SCHEMA public;").expect("public can be dropped");
    assert_eq!(listed(&db), ["app"]);
    let db = parse("ALTER SCHEMA public RENAME TO main;").expect("public can be renamed");
    assert_eq!(listed(&db), ["main"]);
    let db = parse("DROP SCHEMA public; CREATE SCHEMA public;").expect("public can be created");
    assert_eq!(listed(&db), ["public"]);
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
