//! Typed tables, `CREATE TABLE name OF type`, whose columns are the
//! attributes of a composite type.

#![expect(
    clippy::expect_used,
    reason = "Missing objects and unexpected acceptance fail these tests."
)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const TYPE: &str = "CREATE TYPE t AS (a int, b text);";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn table<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("table exists")
}

fn has_table(database: &ParserDB, name: &str) -> bool {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup runs")
        .is_some()
}

fn column_names(database: &ParserDB, name: &str) -> Vec<String> {
    table(database, name)
        .columns(database)
        .expect("columns resolve")
        .map(|column| column.column_name().to_string())
        .collect()
}

fn column_types(database: &ParserDB, name: &str) -> Vec<String> {
    table(database, name)
        .columns(database)
        .expect("columns resolve")
        .map(|column| column.data_type(database).name(database).to_string())
        .collect()
}

fn column<'db>(
    database: &'db ParserDB,
    table_name: &str,
    column_name: &str,
) -> &'db <ParserDB as DatabaseLike>::Column {
    table(database, table_name)
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("lookup runs")
        .expect("column exists")
}

/// The schema and name of the type a table is typed by.
fn of_type(database: &ParserDB, name: &str) -> Option<(Option<String>, String)> {
    table(database, name)
        .of_type(database)
        .expect("the type resolves")
        .map(|type_| (type_.schema().map(ToString::to_string), type_.name().to_string()))
}

/// The schema and name of a type in the default schema.
fn public_type(name: &str) -> (Option<String>, String) {
    (None, name.to_string())
}

/// The SQLSTATE and message PostgreSQL answers for the statement that fails.
fn refusal(sql: &str) -> (String, String) {
    let error = parse(sql).expect_err(sql);
    let diagnostic = error.postgres_error().expect("PostgreSQL diagnostic");
    (diagnostic.sqlstate().to_string(), diagnostic.message().to_string())
}

fn assert_refusals(cases: &[(&str, &str, &str)]) {
    for (sql, sqlstate, message) in cases {
        assert_eq!(refusal(sql), ((*sqlstate).to_string(), (*message).to_string()), "{sql}");
    }
}

/// The table takes one column per attribute, in attribute order, with the
/// attribute's type and collation, and remembers the type.
#[test]
fn columns_come_from_the_type() -> Result<(), Error> {
    let database = parse(
        "CREATE TYPE t AS (a int, b varchar(5) COLLATE \"C\", n numeric(4,2));
         CREATE TABLE c OF t;
         CREATE TABLE plain (a int, b varchar(5) COLLATE \"C\", n numeric(4,2));",
    )?;
    assert_eq!(column_names(&database, "c"), ["a", "b", "n"]);
    assert_eq!(column_types(&database, "c"), column_types(&database, "plain"));
    assert_eq!(column(&database, "c", "a").collation(&database)?, ColumnCollation::DatabaseDefault);
    assert!(matches!(
        column(&database, "c", "b").collation(&database)?,
        ColumnCollation::Named(collation) if collation.name().name() == "C"
    ));
    assert!(column(&database, "c", "a").is_nullable(&database)?);
    assert_eq!(of_type(&database, "c"), Some(public_type("t")));
    assert_eq!(of_type(&database, "plain"), None);
    Ok(())
}

/// A domain attribute gives the column the domain.
#[test]
fn domain_attributes_stay_domains() -> Result<(), Error> {
    let database = parse(
        "CREATE DOMAIN d AS int CHECK (VALUE > 0); CREATE TYPE t AS (a d);
         CREATE TABLE c OF t; CREATE TABLE plain (a d);",
    )?;
    assert_eq!(column_types(&database, "c"), column_types(&database, "plain"));
    Ok(())
}

/// Written entries add options and constraints to the type's columns.
#[test]
fn written_entries_add_options() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE TABLE c OF t (a WITH OPTIONS NOT NULL, b DEFAULT 'x', PRIMARY KEY (a), CHECK (a > 0));"
    ))?;
    assert_eq!(column_names(&database, "c"), ["a", "b"]);
    assert_eq!(column_types(&database, "c"), ["INT", "TEXT"]);
    let a = column(&database, "c", "a");
    assert!(!a.is_nullable(&database)?);
    assert!(a.is_primary_key(&database)?);
    assert!(a.has_check_constraints(&database)?);
    let b = column(&database, "c", "b");
    assert_eq!(b.default_expression().map(ToString::to_string).as_deref(), Some("'x'"));
    assert!(b.is_nullable(&database)?);
    Ok(())
}

/// Entries naming an attribute without options, and with `NULL`, `UNIQUE`,
/// `CHECK` and `REFERENCES`, are accepted.
#[test]
fn every_column_constraint_is_accepted() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE TABLE r (id int PRIMARY KEY);
         CREATE TABLE c OF t (a NULL REFERENCES r, b UNIQUE CHECK (b <> ''));
         CREATE TABLE d OF t (a);"
    ))?;
    assert!(column(&database, "c", "a").is_part_of_foreign_key(&database)?);
    assert_eq!(column_names(&database, "d"), ["a", "b"]);
    Ok(())
}

/// A `COLLATE` on a written entry leaves the column on the type's collation.
#[test]
fn written_collation_is_ignored() -> Result<(), Error> {
    let database = parse(&format!("{TYPE} CREATE TABLE c OF t (b COLLATE \"C\");"))?;
    assert_eq!(column(&database, "c", "b").collation(&database)?, ColumnCollation::DatabaseDefault);
    Ok(())
}

/// An empty composite gives a table with no columns.
#[test]
fn empty_type_gives_no_columns() -> Result<(), Error> {
    let database = parse("CREATE TYPE t AS (); CREATE TABLE c OF t;")?;
    assert_eq!(column_names(&database, "c"), Vec::<String>::new());
    assert_eq!(of_type(&database, "c"), Some(public_type("t")));
    Ok(())
}

/// Accepted spellings around the `OF` clause.
#[test]
fn accepted_forms() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE TEMP TABLE tmp OF t; CREATE UNLOGGED TABLE u OF t;
         CREATE TABLE w OF t WITH (fillfactor = 70); CREATE TABLE c OF t; CREATE TABLE d OF public.t;
         CREATE TABLE IF NOT EXISTS c OF nope;"
    ))?;
    for name in ["u", "w", "c", "d"] {
        assert_eq!(column_names(&database, name), ["a", "b"], "{name}");
        assert_eq!(of_type(&database, name), Some(public_type("t")), "{name}");
    }
    let temporary = database
        .table_by_target(
            TargetName::new("tmp", false).with_schema("pg_temp", false),
            IdentifierCase::AsWritten,
        )?
        .expect("temporary typed table");
    let type_ = temporary.of_type(&database)?.expect("temporary table is typed");
    assert_eq!((type_.schema(), type_.name()), (None, "t"));
    let database = parse(
        "CREATE SCHEMA s; CREATE TYPE s.t AS (a int); CREATE TYPE \"T\" AS (q int);
         CREATE TABLE c OF s.t; CREATE TABLE s.c OF s.t; CREATE TABLE q OF \"T\";",
    )?;
    assert_eq!(of_type(&database, "c"), Some((Some("s".to_string()), "t".to_string())));
    assert_eq!(column_names(&database, "q"), ["q"]);
    Ok(())
}

/// A typed partitioned table takes partitions, which are not typed.
#[test]
fn typed_partitioned_root() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE TABLE c OF t PARTITION BY RANGE (a);
         CREATE TABLE c1 PARTITION OF c (b DEFAULT 'z') FOR VALUES FROM (1) TO (2);
         CREATE TABLE q (a int, b text);
         ALTER TABLE c ATTACH PARTITION q FOR VALUES FROM (2) TO (3);"
    ))?;
    assert_eq!(column_names(&database, "c1"), ["a", "b"]);
    assert_eq!(of_type(&database, "c1"), None);
    assert_eq!(of_type(&database, "c"), Some(public_type("t")));
    assert_refusals(&[(
        &format!(
            "{TYPE} CREATE TABLE c OF t PARTITION BY RANGE (a);
             CREATE TABLE c1 PARTITION OF c (z DEFAULT 1) FOR VALUES FROM (1) TO (2);"
        ),
        "42703",
        "column \"z\" does not exist",
    )]);
    Ok(())
}

/// The type is resolved and checked before anything else about the table.
#[test]
fn type_refusals() {
    assert_refusals(&[
        ("CREATE TABLE c OF nope;", "42704", "type \"nope\" does not exist"),
        ("CREATE TABLE c OF int;", "42704", "type \"int\" does not exist"),
        ("CREATE TABLE c OF nope.t;", "3F000", "schema \"nope\" does not exist"),
        (
            "CREATE SCHEMA s; CREATE TYPE s.t AS (a int); CREATE TABLE c OF t;",
            "42704",
            "type \"t\" does not exist",
        ),
        (
            "CREATE TYPE \"T\" AS (a int); CREATE TABLE c OF T;",
            "42704",
            "type \"t\" does not exist",
        ),
        ("CREATE SEQUENCE sq; CREATE TABLE c OF sq;", "42704", "type \"sq\" does not exist"),
        ("CREATE TYPE sh; CREATE TABLE c OF sh;", "42704", "type \"sh\" is only a shell"),
        ("CREATE TABLE c OF int4;", "42809", "type integer is not a composite type"),
        ("CREATE TABLE c OF text;", "42809", "type text is not a composite type"),
        ("CREATE TABLE c OF pg_catalog.text;", "42809", "type text is not a composite type"),
        ("CREATE TABLE c OF record;", "42809", "type record is not a composite type"),
        (
            "CREATE TYPE e AS ENUM ('x'); CREATE TABLE c OF e;",
            "42809",
            "type e is not a composite type",
        ),
        ("CREATE DOMAIN d AS int; CREATE TABLE c OF d;", "42809", "type d is not a composite type"),
        (
            "CREATE TYPE r AS RANGE (subtype = int4); CREATE TABLE c OF r;",
            "42809",
            "type r is not a composite type",
        ),
        (
            "CREATE TYPE r AS RANGE (subtype = int4); CREATE TABLE c OF r_multirange;",
            "42809",
            "type r_multirange is not a composite type",
        ),
        (
            "CREATE TYPE t AS (a int); CREATE TABLE c OF _t;",
            "42809",
            "type t[] is not a composite type",
        ),
        (
            "CREATE SCHEMA s; CREATE TYPE s.e AS ENUM ('x'); CREATE TABLE c OF s.e;",
            "42809",
            "type s.e is not a composite type",
        ),
        (
            "CREATE TABLE c (x int); CREATE TABLE c OF nope;",
            "42704",
            "type \"nope\" does not exist",
        ),
        (
            "CREATE TYPE e AS ENUM ('x'); CREATE TABLE c (x int); CREATE TABLE c OF e;",
            "42809",
            "type e is not a composite type",
        ),
    ]);
    for setup in [
        "CREATE TABLE p (a int);",
        "CREATE TABLE p (a int) PARTITION BY RANGE (a);",
        "CREATE VIEW p AS SELECT 1 AS a;",
        "CREATE MATERIALIZED VIEW p AS SELECT 1 AS a;",
        "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER sv FOREIGN DATA WRAPPER w;
         CREATE FOREIGN TABLE p (id int) SERVER sv;",
        "CREATE TYPE t AS (a int); CREATE TABLE p OF t;",
    ] {
        assert_refusals(&[(
            &format!("{setup} CREATE TABLE c OF p;"),
            "42809",
            "type p is the row type of another table",
        )]);
    }
}

/// Written entries are checked after the type, identity and generation
/// first, then repeats, then names the type lacks.
#[test]
fn entry_refusals() {
    let cases = [
        (
            "(a GENERATED ALWAYS AS IDENTITY)",
            "0A000",
            "identity columns are not supported on typed tables",
        ),
        (
            "(z GENERATED ALWAYS AS IDENTITY)",
            "0A000",
            "identity columns are not supported on typed tables",
        ),
        (
            "(b GENERATED ALWAYS AS ('x') STORED)",
            "0A000",
            "generated columns are not supported on typed tables",
        ),
        ("(a NOT NULL, a DEFAULT 1)", "42701", "column \"a\" specified more than once"),
        ("(z NOT NULL, a NOT NULL, a DEFAULT 1)", "42701", "column \"a\" specified more than once"),
        ("(z NOT NULL)", "42703", "column \"z\" does not exist"),
        ("(CHECK (z > 0))", "42703", "column \"z\" does not exist"),
        ("(PRIMARY KEY (z))", "42703", "column \"z\" named in key does not exist"),
        (
            "(a PRIMARY KEY, b PRIMARY KEY)",
            "42P16",
            "multiple primary keys for table \"c\" are not allowed",
        ),
    ];
    for (entries, sqlstate, message) in cases {
        assert_refusals(&[(&format!("{TYPE} CREATE TABLE c OF t {entries};"), sqlstate, message)]);
    }
}

/// A typed table takes a relation name and a type name like any table.
#[test]
fn name_refusals() {
    assert_refusals(&[
        (&format!("{TYPE} CREATE TABLE t OF t;"), "42P07", "relation \"t\" already exists"),
        (
            &format!("{TYPE} CREATE TYPE c AS ENUM ('x'); CREATE TABLE c OF t;"),
            "42710",
            "type \"c\" already exists",
        ),
        (
            &format!("{TYPE} CREATE TABLE c OF t; CREATE TYPE c AS (x int);"),
            "42710",
            "type \"c\" already exists",
        ),
    ]);
}

/// The column list of a typed table follows its type only.
#[test]
fn alter_table_refusals() {
    let typed = format!("{TYPE} CREATE TABLE c OF t;");
    let cases = [
        ("ALTER TABLE c ADD COLUMN z int;", "42809", "cannot add column to typed table"),
        (
            "ALTER TABLE c ADD COLUMN IF NOT EXISTS a int;",
            "42809",
            "cannot add column to typed table",
        ),
        (
            "ALTER TABLE c ALTER COLUMN a SET NOT NULL, ADD COLUMN z int;",
            "42809",
            "cannot add column to typed table",
        ),
        ("ALTER TABLE c DROP COLUMN a;", "42809", "cannot drop column from typed table"),
        ("ALTER TABLE c DROP COLUMN IF EXISTS z;", "42809", "cannot drop column from typed table"),
        (
            "ALTER TABLE c ALTER COLUMN a TYPE bigint;",
            "42809",
            "cannot alter column type of typed table",
        ),
        (
            "ALTER TABLE c ALTER COLUMN a SET DATA TYPE bigint USING a::bigint;",
            "42809",
            "cannot alter column type of typed table",
        ),
        (
            "ALTER TABLE c ALTER COLUMN b SET DEFAULT 'x', ALTER COLUMN a TYPE bigint;",
            "42809",
            "cannot alter column type of typed table",
        ),
        (
            "ALTER TABLE c ALTER COLUMN zz TYPE bigint;",
            "42703",
            "column \"zz\" of relation \"c\" does not exist",
        ),
        ("ALTER TABLE c RENAME COLUMN a TO z;", "42809", "cannot rename column of typed table"),
        ("ALTER TABLE c RENAME COLUMN zz TO z;", "42809", "cannot rename column of typed table"),
        (
            "ALTER TABLE ONLY c RENAME COLUMN a TO z;",
            "42809",
            "cannot rename column of typed table",
        ),
        (
            "ALTER TABLE IF EXISTS c RENAME COLUMN a TO z;",
            "42809",
            "cannot rename column of typed table",
        ),
        (
            "CREATE TABLE p (a int); ALTER TABLE c INHERIT p;",
            "42809",
            "cannot change inheritance of typed table",
        ),
        ("ALTER TABLE c INHERIT nope;", "42809", "cannot change inheritance of typed table"),
        (
            "CREATE TABLE p (a int, b text) PARTITION BY RANGE (a); ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (2);",
            "42809",
            "cannot attach a typed table as partition",
        ),
        (
            "CREATE TABLE p (z int) PARTITION BY RANGE (z); ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (2);",
            "42809",
            "cannot attach a typed table as partition",
        ),
        (
            "CREATE TABLE kid () INHERITS (c); CREATE TABLE p (a int, b text) PARTITION BY RANGE (a); ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (2);",
            "42809",
            "cannot attach a typed table as partition",
        ),
    ];
    for (statement, sqlstate, message) in cases {
        assert_refusals(&[(&format!("{typed} {statement}"), sqlstate, message)]);
    }
    let root = format!("{TYPE} CREATE TABLE c OF t PARTITION BY RANGE (a);");
    assert_refusals(&[
        (
            &format!("{root} ALTER TABLE c DROP COLUMN b;"),
            "42809",
            "cannot drop column from typed table",
        ),
        (
            &format!("{root} ALTER TABLE c ATTACH PARTITION c FOR VALUES FROM (1) TO (2);"),
            "42809",
            "cannot attach a typed table as partition",
        ),
        (
            &format!(
                "{root} CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (1) TO (2); ALTER TABLE c1 ADD COLUMN z int;"
            ),
            "42809",
            "cannot add column to a partition",
        ),
        (
            &format!(
                "{root} CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (1) TO (2); ALTER TABLE c1 ALTER COLUMN b TYPE varchar;"
            ),
            "42P16",
            "cannot alter inherited column \"b\"",
        ),
    ]);
}

/// What `ALTER TABLE` may still do to a typed table, which stays typed.
#[test]
fn alter_table_accepted() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE TABLE c OF t;
         ALTER TABLE c ALTER COLUMN a SET NOT NULL, ALTER COLUMN b SET DEFAULT 'q', ADD CONSTRAINT k UNIQUE (a), ADD CHECK (a > 0);
         ALTER TABLE c ALTER COLUMN a ADD GENERATED ALWAYS AS IDENTITY;
         ALTER TABLE c RENAME TO d;"
    ))?;
    assert!(!column(&database, "d", "a").is_nullable(&database)?);
    assert!(column(&database, "d", "a").identity_generation(&database).is_some());
    assert_eq!(of_type(&database, "d"), Some(public_type("t")));
    let database = parse(&format!(
        "{TYPE} CREATE TABLE c OF t (a NOT NULL); ALTER TABLE c ALTER COLUMN a DROP NOT NULL;"
    ))?;
    assert!(column(&database, "c", "a").is_nullable(&database)?);
    Ok(())
}

/// Moving a typed table to another schema keeps it typed by the type that
/// stays behind.
#[test]
fn set_schema_keeps_the_type() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE SCHEMA o; CREATE TABLE c OF t; ALTER TABLE c SET SCHEMA o;"
    ))?;
    let moved = database
        .table_by_target(
            TargetName::new("c", false).with_schema("o", false),
            IdentifierCase::AsWritten,
        )?
        .expect("moved table");
    let type_ = moved.of_type(&database)?.expect("still typed");
    assert_eq!((type_.schema(), type_.name()), (None, "t"));
    let moved_columns: Vec<&str> = moved.columns(&database)?.map(ColumnLike::column_name).collect();
    assert_eq!(moved_columns, ["a", "b"]);
    Ok(())
}

/// A typed table is an ordinary parent, foreign key target, view source and
/// `LIKE` source, and none of those is typed.
#[test]
fn typed_table_as_a_source() -> Result<(), Error> {
    let database = parse(&format!(
        "{TYPE} CREATE TABLE c OF t (PRIMARY KEY (a));
         CREATE TABLE d (z int) INHERITS (c);
         CREATE TABLE p (a int NOT NULL, b text); ALTER TABLE p INHERIT c;
         CREATE TABLE l (LIKE c);
         CREATE TABLE r (x int REFERENCES c);
         CREATE VIEW v AS SELECT a, b FROM c;
         COMMENT ON COLUMN c.a IS 'x';
         CREATE ROLE reader; GRANT SELECT (a) ON c TO reader;"
    ))?;
    assert_eq!(column_names(&database, "d"), ["a", "b", "z"]);
    assert_eq!(column_names(&database, "l"), ["a", "b"]);
    for name in ["d", "p", "l", "r"] {
        assert_eq!(of_type(&database, name), None, "{name}");
    }
    assert!(
        database.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?.is_some()
    );
    assert_refusals(&[(
        &format!("{TYPE} CREATE TABLE c OF t; CREATE VIEW v AS SELECT z FROM c;"),
        "42703",
        "column \"z\" does not exist",
    )]);
    Ok(())
}

/// The typed table follows its type through renames of the type and of its
/// schema, and outlives being dropped by nothing but its type.
#[test]
fn type_lifecycle() -> Result<(), Error> {
    let database = parse(&format!("{TYPE} CREATE TABLE c OF t; ALTER TYPE t RENAME TO u;"))?;
    assert_eq!(of_type(&database, "c"), Some(public_type("u")));
    let database = parse(
        "CREATE SCHEMA s; CREATE TYPE s.t AS (a int); CREATE TABLE c OF s.t;
         ALTER SCHEMA s RENAME TO s2;",
    )?;
    assert_eq!(of_type(&database, "c"), Some((Some("s2".to_string()), "t".to_string())));
    let database = parse(&format!("{TYPE} CREATE TABLE c OF t; DROP TABLE c; DROP TYPE t;"))?;
    assert!(!has_table(&database, "c"));
    Ok(())
}

/// Dropping the type is refused while a typed table uses it, and a cascade
/// takes every typed table along with what depends on it.
#[test]
fn drop_type() -> Result<(), Error> {
    assert_refusals(&[
        (
            &format!("{TYPE} CREATE TABLE c OF t; DROP TYPE t;"),
            "2BP01",
            "cannot drop type t because other objects depend on it",
        ),
        (
            "CREATE SCHEMA s; CREATE TYPE s.t AS (a int); CREATE TABLE c OF s.t; DROP SCHEMA s;",
            "2BP01",
            "cannot drop schema s because other objects depend on it",
        ),
        (
            "CREATE DOMAIN d AS int; CREATE TYPE t AS (a d); CREATE TABLE c OF t; DROP DOMAIN d;",
            "2BP01",
            "cannot drop type d because other objects depend on it",
        ),
    ]);
    let database = parse(&format!(
        "{TYPE} CREATE TABLE c OF t PARTITION BY RANGE (a);
         CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (1) TO (2);
         CREATE VIEW v AS SELECT * FROM c;
         CREATE TABLE k OF t; CREATE TABLE kk () INHERITS (k);
         CREATE TABLE other (x int);
         DROP TYPE t CASCADE;"
    ))?;
    for name in ["c", "c1", "k", "kk"] {
        assert!(!has_table(&database, name), "{name}");
    }
    assert!(
        database.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?.is_none()
    );
    assert!(has_table(&database, "other"));
    let database = parse(
        "CREATE SCHEMA s; CREATE TYPE s.t AS (a int); CREATE TABLE c OF s.t; DROP SCHEMA s CASCADE;",
    )?;
    assert!(!has_table(&database, "c"));
    let database = parse(
        "CREATE DOMAIN d AS int; CREATE TYPE t AS (a d, b int); CREATE TABLE c OF t;
         DROP DOMAIN d CASCADE;",
    )?;
    assert_eq!(column_names(&database, "c"), ["b"]);
    assert_eq!(of_type(&database, "c"), Some(public_type("t")));
    Ok(())
}

/// Only PostgreSQL has typed tables.
#[test]
fn non_postgres_dialects_refuse() {
    use sqlparser::dialect::{GenericDialect, MySqlDialect, SQLiteDialect};
    let sql = "CREATE TABLE p (a int); CREATE TABLE c OF p;";
    for (error, expected) in [
        (
            ParserDB::parse::<GenericDialect>(sql).expect_err("generic syntax"),
            "syntax error at or near \"OF\"",
        ),
        (
            ParserDB::parse::<MySqlDialect>(sql).expect_err("MySQL syntax"),
            "syntax error at or near \"OF\"",
        ),
        (
            ParserDB::parse::<SQLiteDialect>(sql).expect_err("SQLite syntax"),
            "near \"OF\": syntax error",
        ),
    ] {
        assert!(matches!(error, Error::SqlParserError { .. }), "{error:?}");
        assert!(error.to_string().contains(expected), "{error}");
    }
}
