//! The names a function body leaves to the search path in force when it
//! runs, each with what it reached.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::{BigQueryDialect, MsSqlDialect, PostgreSqlDialect};

use crate::support::relation_labels::relation_label;

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn function<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
}

/// Each unqualified name the body of `name` writes, with what it reached,
/// such as `function helper: public.helper` or `relation gone: nothing`.
fn unqualified_in(database: &ParserDB, name: &str) -> Result<Vec<String>, LookupError> {
    Ok(function(database, name)
        .unqualified_names(database)?
        .into_iter()
        .map(|unqualified| {
            match unqualified {
                UnqualifiedName::Relation { name, relation } => {
                    let reached = relation.map_or_else(|| "nothing".to_owned(), relation_label);
                    format!("relation {name}: {reached}")
                }
                UnqualifiedName::Function { name, functions } => {
                    let reached: Vec<String> = functions
                        .iter()
                        .map(|called| {
                            let target = called.target_name();
                            format!("{}.{}", target.schema().unwrap_or("public"), called.name())
                        })
                        .collect();
                    let reached =
                        if reached.is_empty() { "nothing".to_owned() } else { reached.join(", ") };
                    format!("function {name}: {reached}")
                }
            }
        })
        .collect())
}

const SHADOWED: &str = "
    CREATE TABLE unqualified_items (v integer);
    CREATE TABLE fixed_items (v integer);
    CREATE FUNCTION helper(x integer) RETURNS integer LANGUAGE sql RETURN x;
    CREATE FUNCTION qualified_helper(x integer) RETURNS integer LANGUAGE sql RETURN x;
    CREATE FUNCTION sql_names(x integer) RETURNS integer LANGUAGE sql AS $body$
      WITH local_rows AS (SELECT v FROM unqualified_items)
      SELECT helper(r.v) + public.qualified_helper(f.v)
      FROM local_rows r CROSS JOIN public.fixed_items f LIMIT 1
    $body$;
    CREATE FUNCTION procedural_names(x integer) RETURNS integer LANGUAGE plpgsql AS $body$
    DECLARE local_value integer := x;
    BEGIN
      WITH local_rows AS (SELECT v FROM unqualified_items)
      SELECT helper(r.v) + public.qualified_helper(f.v) INTO local_value
      FROM local_rows r CROSS JOIN public.fixed_items f LIMIT 1;
      RETURN local_value;
    END
    $body$;
";

/// A caller's temporary `unqualified_items` shadows the table both bodies
/// read on PostgreSQL 18, and its own `helper` could shadow the call.
#[test]
fn a_body_answers_the_names_a_caller_s_path_could_redirect() {
    let database = postgres(SHADOWED);
    let expected =
        ["relation unqualified_items: table unqualified_items", "function helper: public.helper"];
    assert_eq!(unqualified_in(&database, "sql_names").unwrap(), expected);
    assert_eq!(unqualified_in(&database, "procedural_names").unwrap(), expected);
}

#[test]
fn a_built_in_call_is_unqualified_and_an_expression_form_is_no_call() {
    let database = postgres(
        "CREATE TABLE items (name TEXT);
         CREATE FUNCTION lowered() RETURNS SETOF TEXT LANGUAGE sql AS $$
             SELECT COALESCE(lower(name), '') FROM public.items
             WHERE EXISTS (SELECT 1 FROM public.items)
         $$;",
    );
    let names = unqualified_in(&database, "lowered").unwrap();
    assert!(!names.is_empty(), "lower is written unqualified");
    assert!(
        names.iter().all(|name| name.starts_with("function lower: pg_catalog.lower")),
        "{names:?}"
    );
}

#[test]
fn a_name_reaching_nothing_still_depends_on_the_path() {
    let database = postgres(
        "CREATE FUNCTION dangling() RETURNS INT LANGUAGE sql
             AS $$ SELECT missing_call(count(*)) FROM missing_rows $$;",
    );
    assert_eq!(
        unqualified_in(&database, "dangling").unwrap(),
        [
            "relation missing_rows: nothing",
            "function missing_call: nothing",
            "function count: pg_catalog.count"
        ],
    );
}

/// `FROM f(1)` calls `f`, so a table of the same name is no relation name.
#[test]
fn a_table_function_is_a_call_and_not_a_relation() {
    let database = postgres(
        "CREATE TABLE produced (a INT);
         CREATE FUNCTION produced(n INT) RETURNS SETOF INT LANGUAGE sql AS 'SELECT n';
         CREATE FUNCTION consumer() RETURNS SETOF INT LANGUAGE sql
             AS 'SELECT * FROM produced(1)';",
    );
    assert_eq!(
        unqualified_in(&database, "consumer").unwrap(),
        ["function produced: public.produced"]
    );
}

#[test]
fn only_the_view_the_body_names_counts_and_not_its_definition() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE VIEW open_docs AS SELECT id FROM docs;
         CREATE FUNCTION bare() RETURNS BIGINT LANGUAGE sql AS 'SELECT count(*) FROM open_docs';
         CREATE FUNCTION qualified() RETURNS BIGINT LANGUAGE sql
             AS 'SELECT 1 FROM public.open_docs';",
    );
    assert_eq!(
        unqualified_in(&database, "bare").unwrap(),
        ["relation open_docs: view open_docs", "function count: pg_catalog.count"],
    );
    assert_eq!(unqualified_in(&database, "qualified").unwrap(), Vec::<String>::new());
}

#[test]
fn a_body_bound_at_creation_leaves_no_name_to_the_path() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION one() RETURNS INT LANGUAGE sql RETURN 1;
         CREATE FUNCTION returned() RETURNS BIGINT LANGUAGE sql
             RETURN (SELECT count(*) + one() FROM docs);
         CREATE FUNCTION atomic() RETURNS BIGINT LANGUAGE sql
             BEGIN ATOMIC SELECT count(*) + one() FROM docs; END;",
    );
    assert_eq!(unqualified_in(&database, "returned").unwrap(), Vec::<String>::new());
    assert_eq!(unqualified_in(&database, "atomic").unwrap(), Vec::<String>::new());
}

/// A name after a straight-line `SET search_path` is still left to a path,
/// the one the body set.
#[test]
fn a_set_path_resolves_the_names_after_it() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.rows (a INT);
         CREATE TABLE public.rows (a INT);
         CREATE FUNCTION moved() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
             SET search_path = app;
             DELETE FROM rows;
         END $$;
         CREATE FUNCTION pinned() RETURNS VOID LANGUAGE sql SET search_path = ''
             AS 'DELETE FROM rows';",
    );
    let moved = function(&database, "moved").unqualified_names(&database).unwrap();
    let [UnqualifiedName::Relation { name, relation: Some(Relation::Table(table)) }] =
        moved.as_slice()
    else {
        panic!("one table name, got {moved:?}");
    };
    assert_eq!((name.name(), table.table_schema()), ("rows", Some("app")));
    // The emptied path reaches no table, and the name still depends on it.
    assert_eq!(unqualified_in(&database, "pinned").unwrap(), ["relation rows: nothing"]);
}

#[test]
fn a_path_a_branch_may_change_leaves_later_names_unresolvable() {
    let database = postgres(
        "CREATE TABLE rows (a INT);
         CREATE FUNCTION branched(flag BOOLEAN) RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
             IF flag THEN SET search_path = ''; END IF;
             DELETE FROM rows;
         END $$;
         CREATE FUNCTION dynamic() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'DELETE FROM rows'; END $$;",
    );
    for name in ["branched", "dynamic"] {
        assert!(
            matches!(
                function(&database, name).unqualified_names(&database),
                Err(LookupError::FunctionBodyUnresolvable { .. })
            ),
            "{name}",
        );
    }
}

#[test]
fn quoting_is_kept_as_written() {
    let database = postgres(
        r#"CREATE TABLE "Rows" (a INT);
         CREATE FUNCTION quoted() RETURNS BIGINT LANGUAGE sql AS 'SELECT 1 FROM "Rows"';"#,
    );
    let names = function(&database, "quoted").unqualified_names(&database).unwrap();
    let [unqualified] = names.as_slice() else { panic!("one name, got {names:?}") };
    assert_eq!(unqualified.name(), &TargetName::new("Rows", true));
}

/// A name written again reaching the same objects is answered once, and one
/// reaching other objects after a `SET search_path` is answered again.
#[test]
fn a_name_is_answered_once_for_each_object_it_reaches() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.items (v INT);
         CREATE TABLE public.items (v INT);
         CREATE FUNCTION helper() RETURNS INT LANGUAGE sql RETURN 1;
         CREATE FUNCTION repeated() RETURNS VOID LANGUAGE sql AS $$
             SELECT helper() FROM items, gone;
             SELECT helper() FROM items, gone;
             SET search_path = app;
             DELETE FROM items;
         $$;",
    );
    let names = function(&database, "repeated").unqualified_names(&database).unwrap();
    let reached: Vec<String> = names
        .iter()
        .map(|unqualified| {
            match unqualified {
                UnqualifiedName::Relation { name, relation: Some(Relation::Table(table)) } => {
                    let schema = table.table_schema().unwrap_or("public");
                    format!("{name}: {schema}.{}", table.table_name())
                }
                UnqualifiedName::Relation { name, relation } => format!("{name}: {relation:?}"),
                UnqualifiedName::Function { name, functions } => {
                    format!("{name}(): {} reached", functions.len())
                }
            }
        })
        .collect();
    assert_eq!(
        reached,
        ["items: public.items", "gone: None", "helper(): 1 reached", "items: app.items"]
    );
}

#[test]
fn an_expression_or_query_body_answers_its_names() {
    let sql_server = ParserDB::parse::<MsSqlDialect>(
        "CREATE TABLE items (v INT);
         CREATE FUNCTION helper() RETURNS INT AS BEGIN RETURN 1; END;
         CREATE FUNCTION listed() RETURNS TABLE AS RETURN SELECT helper() AS h FROM items;",
    )
    .unwrap();
    assert_eq!(
        unqualified_in(&sql_server, "listed").unwrap(),
        ["relation items: table items", "function helper: public.helper"],
    );

    let big_query = ParserDB::parse::<BigQueryDialect>(
        "CREATE FUNCTION helper() RETURNS INT64 AS (1);
         CREATE FUNCTION counted() RETURNS INT64 AS (helper() + 1);",
    )
    .unwrap();
    assert_eq!(unqualified_in(&big_query, "counted").unwrap(), ["function helper: public.helper"]);
}
