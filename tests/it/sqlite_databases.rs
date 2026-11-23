//! Tests that `ParserDB` reads SQLite's databases as its schemas: `main`, the
//! one a name naming no database means, `temp`, and each one `ATTACH
//! DATABASE` adds. Each expectation was measured on SQLite 3.51.1.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

/// Parses `sql` the way SQLite compares names, folding every identifier.
fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<SQLiteDialect>(sql)
}

/// Parses `sql` under PostgreSQL's rule, where quoting decides.
fn parse_as_written(sql: &str) -> Result<ParserDB, Error> {
    ParseOptions::default()
        .with_identifier_case(IdentifierCase::AsWritten)
        .parse::<SQLiteDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("SQLite accepts the script")
}

/// The table stored under `name` in `schema`, without any search.
fn stored<'db>(
    database: &'db ParserDB,
    schema: Option<&str>,
    name: &str,
) -> Option<&'db <ParserDB as DatabaseLike>::Table> {
    let target = TargetName::new(name, false);
    let target = match schema {
        Some(schema) => target.with_schema(schema, false),
        None => target,
    };
    database.table_by_target(target, IdentifierCase::Folded).expect("unambiguous lookup")
}

/// The table an unqualified `name` reaches, searching the databases in order.
fn resolved<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .resolve_target_table(TargetName::new(name, false), IdentifierCase::Folded)
        .expect("unambiguous lookup")
        .expect("the name resolves")
}

/// The schemas the database holds, sorted by name as every listing is.
fn schema_names(database: &ParserDB) -> Vec<&str> {
    DatabaseLike::schemas(database).map(SchemaLike::name).collect()
}

#[test]
fn a_new_database_holds_main_alone() {
    let database = database("");
    assert_eq!(schema_names(&database), ["main"]);
    let path: Vec<(&str, bool)> = database.search_path().collect();
    assert_eq!(path, [("temp", false), ("main", false)]);
}

#[test]
fn an_unqualified_table_is_the_main_table() {
    let database = database("CREATE TABLE t(x);");
    let table = stored(&database, Some("main"), "t").expect("main.t names it");
    assert_eq!(table.table_schema(), None);
    assert!(!table.is_temporary());
    assert!(stored(&database, None, "t").is_some());
}

#[test]
fn a_main_table_is_the_unqualified_table() {
    let database = database("CREATE TABLE main.m(x);");
    let table = stored(&database, None, "m").expect("an unqualified name reaches it");
    assert_eq!(table.table_schema(), Some("main"));
    assert_eq!(resolved(&database, "m").table_schema(), Some("main"));
}

#[test]
fn both_spellings_of_main_hold_one_name() {
    for sql in
        ["CREATE TABLE t(x); CREATE TABLE main.t(y);", "CREATE TABLE main.t(x); CREATE TABLE t(y);"]
    {
        assert!(parse(sql).is_err(), "{sql}");
    }
}

#[test]
fn temporary_tables_land_in_temp() {
    for sql in
        ["CREATE TEMP TABLE t(x);", "CREATE TABLE temp.t(x);", "CREATE TEMPORARY TABLE TEMP.t(x);"]
    {
        let database = database(sql);
        let table = database.tables().next().expect("one table");
        assert_eq!(table.table_schema(), Some("temp"), "{sql}");
        assert!(table.is_temporary(), "{sql}");
        assert_eq!(schema_names(&database), ["main"], "{sql}");
    }
    assert!(parse("CREATE TEMP TABLE t(x); CREATE TABLE temp.t(y);").is_err());
}

#[test]
fn a_temporary_table_takes_no_other_database() {
    assert!(parse("CREATE TEMP TABLE main.t(x);").is_err());
}

#[test]
fn attach_adds_a_database() {
    let database = database("ATTACH DATABASE ':memory:' AS aux; CREATE TABLE aux.t(x);");
    assert_eq!(schema_names(&database), ["aux", "main"]);
    let table = stored(&database, Some("aux"), "t").expect("aux.t names it");
    assert_eq!(table.table_schema(), Some("aux"));
    assert!(!table.is_temporary());
    assert!(stored(&database, None, "t").is_none());
}

#[test]
fn attach_refuses_a_name_in_use() {
    for sql in [
        "ATTACH ':memory:' AS main;",
        "ATTACH ':memory:' AS TEMP;",
        "ATTACH ':memory:' AS aux; ATTACH ':memory:' AS AUX;",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::DatabaseAlreadyInUse { .. })),
            "{sql}: {:?}",
            parse(sql)
        );
    }
}

#[test]
fn a_database_nothing_attached_is_unknown() {
    for sql in ["CREATE TABLE aux.t(x);", "CREATE TABLE public.t(x);", "CREATE TABLE pg_temp.t(x);"]
    {
        assert!(parse(sql).is_err(), "{sql}");
    }
}

#[test]
fn database_names_compare_without_case() {
    let database = database(
        "ATTACH ':memory:' AS aux;
         CREATE TABLE MAIN.m(x);
         CREATE TABLE \"Aux\".a(x);
         CREATE TABLE \"TEMP\".t(x);",
    );
    assert!(stored(&database, None, "m").is_some());
    assert!(stored(&database, Some("aux"), "a").is_some());
    assert!(resolved(&database, "t").is_temporary());
}

#[test]
fn an_unqualified_name_searches_temp_then_main_then_attach_order() {
    let database = database(
        "ATTACH ':memory:' AS a1;
         ATTACH ':memory:' AS a2;
         CREATE TABLE a2.u(x);
         CREATE TABLE a1.u(x);
         CREATE TABLE a1.v(x);
         CREATE TABLE v(x);
         CREATE TABLE w(x);
         CREATE TEMP TABLE w(x);",
    );
    let path: Vec<(&str, bool)> = database.search_path().collect();
    assert_eq!(path, [("temp", false), ("main", false), ("a1", false), ("a2", false)]);
    assert_eq!(resolved(&database, "u").table_schema(), Some("a1"));
    assert_eq!(resolved(&database, "v").table_schema(), None);
    assert_eq!(resolved(&database, "w").table_schema(), Some("temp"));
}

/// What dieselite feeds `ParserDB` for a connection with an attached `aux`:
/// each stored statement with only its created name qualified.
const ATTACHED: &str = "ATTACH DATABASE '/path/aux.db' AS aux;
     CREATE TABLE main.p(id INTEGER PRIMARY KEY, main_only);
     CREATE TABLE main.c(main_only);
     CREATE TABLE aux.p(id INTEGER PRIMARY KEY);
     CREATE TABLE aux.c(pid REFERENCES p(id));";

#[test]
fn a_foreign_key_references_its_own_database() {
    let database = database(ATTACHED);
    let child = stored(&database, Some("aux"), "c").expect("aux.c exists");
    let foreign_key =
        child.foreign_keys(&database).expect("the keys resolve").next().expect("one foreign key");
    let parent = foreign_key.referenced_table(&database).expect("the parent resolves");
    assert_eq!(parent.table_schema(), Some("aux"));
}

#[test]
fn an_index_indexes_a_table_of_its_own_database() {
    let database = database(&format!("{ATTACHED} CREATE INDEX aux.i ON c(pid);"));
    let index = database.indexes().next().expect("one index");
    let Ok(Relation::Table(table)) = index.relation(&database) else {
        panic!("the index is on a table");
    };
    assert_eq!(table.table_schema(), Some("aux"));
    assert_eq!(table.table_name(), "c");
}

#[test]
fn an_unqualified_index_indexes_a_main_or_temporary_table() {
    // SQLite looks for `main.c` and reports `no such table: main.c`.
    assert!(
        parse("ATTACH ':memory:' AS aux; CREATE TABLE aux.c(z); CREATE INDEX i ON c(z);").is_err()
    );
    let database = database(
        "ATTACH ':memory:' AS aux;
         CREATE TABLE aux.t(z);
         CREATE TEMP TABLE t(y);
         CREATE INDEX i ON t(y);",
    );
    let index = database.indexes().next().expect("one index");
    let Ok(Relation::Table(table)) = index.relation(&database) else {
        panic!("the index is on a table");
    };
    assert!(table.is_temporary());
}

#[test]
fn a_trigger_fires_on_a_table_of_its_own_database() {
    let database = database(&format!(
        "{ATTACHED} CREATE TRIGGER aux.tr AFTER INSERT ON c BEGIN DELETE FROM p; END;"
    ));
    let trigger = database.triggers().next().expect("one trigger");
    let Ok(Relation::Table(table)) = trigger.relation(&database) else {
        panic!("the trigger is on a table");
    };
    assert_eq!(table.table_schema(), Some("aux"));
}

#[test]
fn an_unqualified_trigger_fires_on_a_main_or_temporary_table() {
    // SQLite looks for `main.c` and reports `no such table: main.c`.
    assert!(
        parse(
            "ATTACH ':memory:' AS aux;
             CREATE TABLE aux.c(z);
             CREATE TRIGGER tr AFTER INSERT ON c BEGIN SELECT 1; END;"
        )
        .is_err()
    );
    let database = database(
        "CREATE TABLE t(x);
         CREATE TEMP TABLE t(y);
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;",
    );
    let trigger = database.triggers().next().expect("one trigger");
    let Ok(Relation::Table(table)) = trigger.relation(&database) else {
        panic!("the trigger is on a table");
    };
    assert!(table.is_temporary());
}

#[test]
fn a_view_reads_its_own_database() {
    let database = database(&format!("{ATTACHED} CREATE VIEW aux.v AS SELECT * FROM c;"));
    let view = database.views().next().expect("one view");
    assert_eq!(view.view_schema(), Some("aux"));
    let definition = view.definition().to_string();
    assert!(definition.contains("aux.c"), "{definition}");
    assert!(definition.contains("pid") && !definition.contains("main_only"), "{definition}");
}

#[test]
fn a_main_view_reads_main_past_a_temporary_table() {
    let database = database(
        "CREATE TABLE t(x);
         CREATE TEMP TABLE t(y);
         CREATE VIEW v AS SELECT * FROM t;",
    );
    let view = database.views().next().expect("one view");
    assert!(!view.is_temporary());
    let definition = view.definition().to_string();
    assert!(definition.contains('x') && !definition.contains('y'), "{definition}");
}

#[test]
fn a_temporary_object_reads_every_database() {
    let database = database(&format!(
        "{ATTACHED}
         CREATE VIEW temp.tv AS SELECT * FROM aux.c;
         CREATE TEMP TRIGGER tt AFTER INSERT ON aux.c BEGIN SELECT * FROM main.p; END;"
    ));
    assert!(database.views().next().expect("one view").is_temporary());
    let trigger = database.triggers().next().expect("one trigger");
    let Ok(Relation::Table(table)) = trigger.relation(&database) else {
        panic!("the trigger is on a table");
    };
    assert_eq!(table.table_schema(), Some("aux"));
}

#[test]
fn a_permanent_object_refuses_another_database() {
    for sql in [
        // `view v cannot reference objects in database main`
        "CREATE VIEW aux.v AS SELECT * FROM main.p;",
        "CREATE VIEW aux.v AS SELECT (SELECT count(*) FROM main.p) FROM c;",
        "CREATE VIEW main.v AS SELECT * FROM aux.c;",
        "CREATE VIEW v AS SELECT * FROM temp.t;",
        // `trigger tr cannot reference objects in database main`
        "CREATE TRIGGER aux.tr AFTER INSERT ON main.c BEGIN SELECT 1; END;",
        "CREATE TRIGGER aux.tr AFTER INSERT ON c BEGIN SELECT * FROM main.p; END;",
        "CREATE TRIGGER aux.tr AFTER INSERT ON c BEGIN INSERT INTO p SELECT * FROM main.p; END;",
    ] {
        let script = format!("{ATTACHED} CREATE TEMP TABLE t(x); {sql}");
        assert!(
            matches!(parse(&script), Err(Error::CrossDatabaseReference { .. })),
            "{sql}: {:?}",
            parse(&script)
        );
    }
}

/// The table and column a three-part `reference` in `SELECT <reference> FROM
/// t` names, comparing names under `case`.
fn column_named(database: &ParserDB, reference: &str, case: IdentifierCase) -> (String, String) {
    let parsed = |sql: &str| {
        let mut statements =
            sqlparser::parser::Parser::parse_sql(&SQLiteDialect {}, sql).expect("SQLite parses it");
        let Some(sqlparser::ast::Statement::Query(query)) = statements.pop() else {
            panic!("one query")
        };
        query
    };
    let query = parsed(&format!("SELECT {reference} FROM t"));
    let scope = ColumnScope::from_query(&query, database, case).expect("the scope builds");
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        panic!("a plain SELECT")
    };
    let sqlparser::ast::SelectItem::UnnamedExpr(expression) = &select.projection[0] else {
        panic!("an expression")
    };
    let Some(ColumnDefinition::Base { table, column, .. }) =
        scope.resolve_column_definition(expression).expect("the reference resolves")
    else {
        panic!("{reference} names a column of t")
    };
    (table.table_name().to_string(), column.column_name().to_string())
}

#[test]
fn a_column_qualified_by_main_reads_an_unqualified_table() {
    let expected = (String::from("t"), String::from("x"));
    let folded = database("CREATE TABLE t(x);");
    assert_eq!(column_named(&folded, "main.t.x", IdentifierCase::Folded), expected);
    let as_written = parse_as_written("CREATE TABLE t(x);").expect("SQLite accepts it");
    assert_eq!(column_named(&as_written, "MAIN.t.x", IdentifierCase::AsWritten), expected);
}

#[test]
fn database_names_compare_as_the_caller_compares_names() {
    // PostgreSQL's rule keeps the case of a quoted name, a database's too.
    for sql in [
        "ATTACH ':memory:' AS \"Aux\"; CREATE TABLE aux.a(x);",
        "CREATE TABLE t(y); CREATE INDEX \"Main\".i ON t(y);",
    ] {
        assert!(parse_as_written(sql).is_err(), "{sql}");
        assert!(parse(sql).is_ok(), "{sql}");
    }
    let database = parse_as_written(
        "ATTACH ':memory:' AS \"Aux\"; CREATE TABLE \"Aux\".a(x); CREATE TABLE MAIN.m(y);",
    )
    .expect("PostgreSQL's rule accepts the script");
    for target in [
        TargetName::new("a", false).with_schema("Aux", true),
        TargetName::new("m", false).with_schema("main", false),
    ] {
        assert!(
            database.table_by_target(target, IdentifierCase::AsWritten).expect("one").is_some()
        );
    }
}

#[test]
fn an_unqualified_index_or_trigger_takes_no_attached_table() {
    for sql in [
        // `trigger tr cannot reference objects in database aux`
        "CREATE TRIGGER tr AFTER INSERT ON aux.c BEGIN SELECT 1; END;",
        // A syntax error in SQLite, which takes no database before the table.
        "CREATE INDEX i ON aux.c(pid);",
    ] {
        let script = format!("{ATTACHED} {sql}");
        assert!(
            matches!(parse(&script), Err(Error::CrossDatabaseReference { .. })),
            "{sql}: {:?}",
            parse(&script)
        );
    }
}

#[test]
fn a_trigger_on_a_temporary_table_is_temporary() {
    let database = database(&format!(
        "{ATTACHED}
         CREATE TEMP TABLE t(x);
         CREATE TRIGGER tr AFTER INSERT ON temp.t BEGIN SELECT * FROM aux.c; END;"
    ));
    let trigger = database.triggers().next().expect("one trigger");
    let Ok(Relation::Table(table)) = trigger.relation(&database) else {
        panic!("the trigger is on a table");
    };
    assert!(table.is_temporary());
}

#[test]
fn an_altered_table_reads_its_own_database() {
    for altered in ["aux.c", "c"] {
        let database = database(&format!(
            "ATTACH ':memory:' AS aux;
             CREATE TABLE p(id);
             CREATE TABLE aux.p(k INTEGER PRIMARY KEY);
             CREATE TABLE aux.c(x);
             ALTER TABLE {altered} ADD COLUMN q REFERENCES p(k);"
        ));
        let child = stored(&database, Some("aux"), "c").expect("aux.c exists");
        let foreign_key = child
            .foreign_keys(&database)
            .expect("the keys resolve")
            .next()
            .expect("one foreign key");
        let parent = foreign_key.referenced_table(&database).expect("the parent resolves");
        assert_eq!(parent.table_schema(), Some("aux"), "{altered}");
    }
}

#[test]
fn altering_a_missing_table_is_refused() {
    let error = parse("ATTACH ':memory:' AS aux; ALTER TABLE c ADD COLUMN q;")
        .expect_err("no database holds `c`");
    assert!(
        matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "c"),
        "{error:?}"
    );
}

#[test]
fn an_attachment_outlives_a_rollback() {
    for sql in [
        "BEGIN; ATTACH ':memory:' AS aux; ROLLBACK;",
        "BEGIN; ATTACH ':memory:' AS aux; CREATE TABLE aux.t(x); ROLLBACK;",
        "BEGIN; ATTACH ':memory:' AS aux;",
    ] {
        let database = database(sql);
        assert_eq!(schema_names(&database), ["aux", "main"], "{sql}");
        let path: Vec<(&str, bool)> = database.search_path().collect();
        assert_eq!(path, [("temp", false), ("main", false), ("aux", false)], "{sql}");
        assert_eq!(database.tables().count(), 0, "{sql}");
    }
    assert!(parse("BEGIN; ATTACH ':memory:' AS aux; ROLLBACK; CREATE TABLE aux.t(x);").is_ok());
}

#[test]
fn both_spellings_of_main_hold_one_trigger_name() {
    // `trigger tr already exists`
    assert!(
        parse(
            "CREATE TABLE t(x);
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;
             CREATE TRIGGER tr AFTER INSERT ON main.t BEGIN SELECT 1; END;"
        )
        .is_err()
    );
}

#[test]
fn postgresql_keeps_its_schemas() {
    let database = ParserDB::parse::<PostgreSqlDialect>("CREATE TEMP TABLE t(x int);")
        .expect("PostgreSQL accepts the script");
    assert_eq!(schema_names(&database), ["public"]);
    assert_eq!(database.tables().next().expect("one table").table_schema(), Some("pg_temp"));
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE main.t(x int);"),
        Err(Error::SchemaNotFoundForRelation { .. })
    ));
}
