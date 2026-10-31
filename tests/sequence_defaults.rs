//! Tests that a column default drawing from a sequence holds the sequence the
//! way PostgreSQL's dependency on it does: whatever drops the sequence is
//! refused while such a default remains, and `CASCADE` takes the default
//! along.
//!
//! Every expectation was measured against PostgreSQL 18.4.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/sequence_names.rs"));

/// A column as `type nullability default`, the way the catalog would list
/// it.
fn column(database: &ParserDB, table: &str, column: &str) -> String {
    let column = database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .column(column, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists");
    let nullable = column.is_nullable(database).expect("table is in this database");
    format!(
        "{} {} {}",
        column.data_type(database),
        if nullable { "null" } else { "not null" },
        column.default_value().unwrap_or_else(|| "no default".to_string())
    )
}

/// The default a drop was refused for, as `sequence <- table.column`.
fn refused_by_default(result: Result<ParserDB, Error>) -> String {
    match result {
        Err(Error::SequenceUsedByDefault { sequence_name, table_name, column_name }) => {
            format!("{sequence_name} <- {table_name}.{column_name}")
        }
        other => panic!("expected a default holding the sequence, got {other:?}"),
    }
}

#[test]
fn a_default_naming_a_sequence_holds_it() {
    for default in [
        "nextval('s')",
        "nextval('s'::regclass) + 1",
        "currval('S')",
        "setval('s', 5)",
        "pg_catalog.nextval('s')",
        "'s'::regclass",
        "regclass 's'",
    ] {
        let sql = format!(
            "CREATE SEQUENCE s; CREATE TABLE t (id BIGINT DEFAULT {default}); DROP SEQUENCE s;"
        );
        assert_eq!(refused_by_default(parse(&sql)), "s <- t.id", "{default}");
    }
}

#[test]
fn cascade_takes_the_defaults_along() {
    let db = parse(
        "CREATE SEQUENCE s;
         CREATE TABLE t (id INT NOT NULL DEFAULT nextval('s'), x INT DEFAULT currval('s'::regclass), y INT DEFAULT 3);
         DROP SEQUENCE s CASCADE;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&db), [] as [&str; 0]);
    assert_eq!(column(&db, "t", "id"), "INT not null no default");
    assert_eq!(column(&db, "t", "x"), "INT null no default");
    assert_eq!(column(&db, "t", "y"), "INT null 3");
}

#[test]
fn the_name_is_bound_where_it_reached() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.s;
         CREATE SEQUENCE \"S\";
         SET search_path = app, public;
         CREATE TABLE public.t (id INT DEFAULT nextval('s'), q INT DEFAULT nextval('\"S\"'), p INT DEFAULT nextval('public.\"S\"'));
         SET search_path = public;",
    )
    .expect("schema parses");
    assert_eq!(column(&db, "t", "id"), "INT null nextval('app.s')");
    assert_eq!(column(&db, "t", "q"), "INT null nextval('\"S\"')");
    assert_eq!(column(&db, "t", "p"), "INT null nextval('\"S\"')");
    assert_eq!(
        refused_by_default(parse(
            "CREATE SCHEMA app;
         CREATE SEQUENCE app.s;
         SET search_path = app, public;
         CREATE TABLE public.t (id INT DEFAULT nextval('s'));
         SET search_path = public;
         DROP SEQUENCE app.s;"
        )),
        "s <- t.id"
    );

    let db = parse(
        "CREATE SEQUENCE s;
         CREATE SCHEMA app;
         CREATE SEQUENCE app.s;
         CREATE TABLE t (id INT DEFAULT nextval('app.s'), k INT DEFAULT other.nextval('s'));
         DROP SEQUENCE s;",
    )
    .expect("neither default draws from public.s");
    assert_eq!(column(&db, "t", "k"), "INT null other.nextval('s')");
}

#[test]
fn a_column_added_to_a_parent_in_a_schema_draws_from_the_sequence_there() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (x INT);
         CREATE TABLE app.c () INHERITS (app.t);
         ALTER TABLE app.t ADD COLUMN id serial;",
    )
    .expect("parses");
    let child = db
        .table_by_target(
            TargetName::new("c", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("app.c exists");
    let id = child.column("id", &db, IdentifierCase::AsWritten).expect("lookup").expect("id");
    assert_eq!(id.default_value().as_deref(), Some("nextval('app.t_id_seq'::REGCLASS)"));
}

#[test]
fn a_default_naming_no_relation_is_refused() {
    assert!(matches!(
        parse("CREATE TABLE t (id INT DEFAULT nextval('nope'));"),
        Err(Error::RelationNotFoundForDefault { relation_name, table_name, column_name })
            if relation_name == "nope" && table_name == "t" && column_name == "id"
    ));
    assert!(matches!(
        parse(
            "CREATE TABLE t (id INT); ALTER TABLE t ALTER COLUMN id SET DEFAULT nextval('nope');"
        ),
        Err(Error::RelationNotFoundForDefault { .. })
    ));
    parse("CREATE TABLE u (x INT); CREATE TABLE t (id INT DEFAULT nextval('u'));")
        .expect("a table is a relation the name may reach");
    for literal in ["", "\"s", "s t", "a.b.c.d", "elsewhere.public.s"] {
        assert!(
            matches!(
                parse(&format!("CREATE SEQUENCE s; CREATE TABLE t (id INT DEFAULT nextval('{literal}'));")),
                Err(Error::InvalidRelationNameForDefault { relation_name, .. }) if relation_name == literal
            ),
            "{literal}"
        );
    }
    parse("CREATE SEQUENCE s; CREATE TABLE t (id INT DEFAULT nextval(' s '), k INT DEFAULT nextval('s'::text));")
        .expect("blanks are trimmed and a text argument binds nothing");
}

#[test]
fn a_serial_column_written_not_null_keeps_one_not_null() {
    for (sql, table) in [
        ("CREATE TABLE t (id serial NOT NULL); DROP SEQUENCE t_id_seq CASCADE;", "t"),
        ("CREATE TABLE u (id serial NOT NULL); CREATE TABLE t (LIKE u);", "t"),
    ] {
        let db = parse(sql).expect("parses");
        let written = db
            .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .to_string();
        assert_eq!(written, "CREATE TABLE t (id INTEGER NOT NULL)", "{sql}");
    }
}

#[test]
fn a_serial_column_draws_from_its_sequence() {
    assert_eq!(
        refused_by_default(parse("CREATE TABLE t (id serial); DROP SEQUENCE t_id_seq;")),
        "t_id_seq <- t.id"
    );

    let db = parse("CREATE TABLE t (id serial); DROP SEQUENCE t_id_seq CASCADE;").expect("parses");
    assert_eq!(column(&db, "t", "id"), "INTEGER not null no default");
    assert!(
        !db.tables()
            .next()
            .expect("t")
            .columns(&db)
            .expect("columns")
            .next()
            .expect("id")
            .is_serial()
    );
}

#[test]
fn a_serial_column_losing_its_default_leaves_the_sequence_free() {
    for change in ["DROP DEFAULT", "SET DEFAULT 5"] {
        let db = parse(&format!(
            "CREATE TABLE t (id serial);
             ALTER TABLE t ALTER COLUMN id {change};
             DROP SEQUENCE t_id_seq;"
        ))
        .expect("the sequence is free");
        assert_eq!(sequence_names(&db), [] as [&str; 0], "{change}");
    }
    let db = parse("CREATE TABLE t (id serial); ALTER TABLE t ALTER COLUMN id SET DEFAULT 5;")
        .expect("parses");
    assert_eq!(column(&db, "t", "id"), "INTEGER not null 5");
    assert_eq!(sequence_names(&db), ["public.t_id_seq"], "the sequence stays owned");
}

#[test]
fn a_serial_column_changing_type_keeps_drawing() {
    let db = parse("CREATE TABLE t (id serial); ALTER TABLE t ALTER COLUMN id TYPE BIGINT;")
        .expect("parses");
    assert_eq!(column(&db, "t", "id"), "BIGINT not null nextval('t_id_seq'::REGCLASS)");
    assert_eq!(
        refused_by_default(parse(
            "CREATE TABLE t (id serial); ALTER TABLE t ALTER COLUMN id TYPE BIGINT; DROP SEQUENCE t_id_seq;"
        )),
        "t_id_seq <- t.id"
    );
}

#[test]
fn an_inheriting_table_draws_from_its_parent_sequence() {
    for setup in [
        "CREATE TABLE t (id serial); CREATE TABLE c () INHERITS (t);",
        "CREATE TABLE t (x INT); CREATE TABLE c () INHERITS (t); ALTER TABLE t ADD COLUMN id serial;",
    ] {
        let db = parse(&format!("{setup} DROP SEQUENCE t_id_seq CASCADE;")).expect("parses");
        assert_eq!(column(&db, "t", "id"), "INTEGER not null no default", "{setup}");
        assert_eq!(column(&db, "c", "id"), "INTEGER not null no default", "{setup}");
    }
    let db = parse(
        "CREATE TABLE p (id serial, k INT) PARTITION BY RANGE (k);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
         DROP SEQUENCE p_id_seq CASCADE;",
    )
    .expect("parses");
    assert_eq!(column(&db, "p1", "id"), "INTEGER not null no default");
}

#[test]
fn a_like_copy_of_a_serial_column_is_an_integer() {
    let db = parse("CREATE TABLE t (id serial); CREATE TABLE u (LIKE t); DROP TABLE t;")
        .expect("the copy draws from nothing");
    assert_eq!(column(&db, "u", "id"), "INTEGER not null no default");

    let db = parse("CREATE TABLE t (id serial); CREATE TABLE u (LIKE t INCLUDING DEFAULTS);")
        .expect("parses");
    assert_eq!(column(&db, "u", "id"), "INTEGER not null nextval('t_id_seq'::REGCLASS)");
    assert_eq!(
        refused_by_default(parse(
            "CREATE TABLE t (id serial); CREATE TABLE u (LIKE t INCLUDING DEFAULTS); DROP TABLE t;"
        )),
        "t_id_seq <- u.id"
    );
}

#[test]
fn dropping_the_owner_is_refused_while_another_default_draws() {
    for (owner, drop) in [
        ("CREATE TABLE t (id serial);", "DROP TABLE t"),
        ("CREATE TABLE t (id serial, k INT);", "ALTER TABLE t DROP COLUMN id"),
        ("CREATE TABLE t (id INT GENERATED ALWAYS AS IDENTITY);", "DROP TABLE t"),
        ("CREATE VIEW v AS SELECT 1 AS a; CREATE SEQUENCE t_id_seq OWNED BY v.a;", "DROP VIEW v"),
    ] {
        let setup = format!("{owner} CREATE TABLE u (z INT DEFAULT nextval('t_id_seq'));");
        assert_eq!(
            refused_by_default(parse(&format!("{setup} {drop};"))),
            "t_id_seq <- u.z",
            "{drop}"
        );
        let db = parse(&format!("{setup} {drop} CASCADE;")).expect("cascade takes the default");
        assert_eq!(column(&db, "u", "z"), "INT null no default", "{drop}");
        assert_eq!(sequence_names(&db), [] as [&str; 0], "{drop}");
    }

    // The table's own default goes with it.
    parse("CREATE TABLE t (id serial, x INT DEFAULT nextval('t_id_seq')); DROP TABLE t;")
        .expect("both defaults leave with the table");
    assert_eq!(
        refused_by_default(parse(
            "CREATE TABLE t (id serial, x INT DEFAULT nextval('t_id_seq')); ALTER TABLE t DROP COLUMN id;"
        )),
        "t_id_seq <- t.x"
    );
}

#[test]
fn dropping_a_schema_takes_defaults_drawing_from_its_sequences() {
    let db = parse(
        "CREATE SCHEMA x;
         CREATE SEQUENCE x.s;
         CREATE TABLE u (z INT DEFAULT nextval('x.s'));
         DROP SCHEMA x CASCADE;",
    )
    .expect("parses");
    assert_eq!(column(&db, "u", "z"), "INT null no default");
}

#[test]
fn a_default_may_name_a_sequence_its_own_statement_creates() {
    let db =
        parse("CREATE TABLE t (id serial, x INT DEFAULT nextval('t_id_seq'));").expect("parses");
    assert_eq!(column(&db, "t", "x"), "INT null nextval('t_id_seq')");
}

#[test]
fn only_postgresql_holds_a_sequence_by_a_default() {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE SEQUENCE s; CREATE TABLE t (id INT DEFAULT nextval('s'), n INT DEFAULT nextval('nope')); DROP SEQUENCE s;",
    )
    .expect("parses");
    assert_eq!(sequence_names(&db), [] as [&str; 0]);
}
