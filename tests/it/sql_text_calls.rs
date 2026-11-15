//! The calls of a function body to the `pg_catalog` functions that run SQL
//! text one of their arguments gives, read as dynamic-execution sites.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails loudly on a broken premise"
)]

use sql_traits::prelude::*;
use sqlparser::{ast::Expr, dialect::PostgreSqlDialect};

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn function<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
}

/// Each site of `name` as the function it calls and the command it runs.
fn calls(database: &ParserDB, name: &str) -> Vec<(String, String)> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    body.sites()
        .unwrap()
        .iter()
        .map(|site| {
            let called = site.function().expect("a call site");
            (called.target_name().to_string(), site.command().to_string())
        })
        .collect()
}

/// How a test spells the type an expression takes.
fn rendered(operand: &OperandType<'_>) -> String {
    match operand {
        OperandType::Known { type_name, typmod: Some(typmod) } => format!("{type_name}({typmod})"),
        OperandType::Known { type_name, typmod: None } => type_name.to_string(),
        OperandType::UntypedLiteral => "unknown literal".to_owned(),
        OperandType::Unknown => "?".to_owned(),
    }
}

/// The type the right operand of the concatenation each site of `name`
/// runs takes, in the context of the site.
fn concatenated_types(database: &ParserDB, name: &str) -> Vec<String> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    body.sites()
        .unwrap()
        .iter()
        .map(|site| {
            let context = site.context(IdentifierCase::Folded).unwrap();
            let Expr::BinaryOp { right, .. } = site.command() else { panic!("a concatenation") };
            rendered(&context.type_of(right).unwrap())
        })
        .collect()
}

const BODIES: &str = "CREATE TABLE items (name text);
    CREATE FUNCTION returned(p text) RETURNS xml LANGUAGE sql
     RETURN query_to_xml('SELECT count(*) FROM items WHERE name = ' || p, false, false, '');
    CREATE FUNCTION quoted(p text) RETURNS xml LANGUAGE sql AS $$
     SELECT query_to_xmlschema('SELECT * FROM items WHERE name = ' || p, false, false, '')
    $$;
    CREATE FUNCTION atomic(p text) RETURNS bigint LANGUAGE sql BEGIN ATOMIC
     SELECT query_to_xml_and_xmlschema('SELECT 1 WHERE ' || p, true, true, 'urn:x');
     SELECT count(*) FROM ts_stat('SELECT body FROM docs WHERE ' || p, 'ab');
    END;
    CREATE FUNCTION rewritten(p text) RETURNS tsquery LANGUAGE sql
     RETURN ts_rewrite('a & c'::tsquery, 'SELECT target, substitute FROM rewrites WHERE ' || p);";

/// A `RETURN`, a quoted and a `BEGIN ATOMIC` SQL body each run the text
/// their calls pass, a `ts_stat` in `FROM` included, and the command is the
/// argument the overload runs, the second one of `ts_rewrite(tsquery, text)`.
#[test]
fn sql_bodies_run_the_text_their_catalog_calls_pass() {
    let database = postgres(BODIES);
    assert_eq!(
        calls(&database, "returned"),
        [(
            "pg_catalog.query_to_xml".to_owned(),
            "'SELECT count(*) FROM items WHERE name = ' || p".to_owned()
        )]
    );
    assert_eq!(
        calls(&database, "quoted"),
        [(
            "pg_catalog.query_to_xmlschema".to_owned(),
            "'SELECT * FROM items WHERE name = ' || p".to_owned()
        )]
    );
    assert_eq!(
        calls(&database, "atomic"),
        [
            (
                "pg_catalog.query_to_xml_and_xmlschema".to_owned(),
                "'SELECT 1 WHERE ' || p".to_owned()
            ),
            ("pg_catalog.ts_stat".to_owned(), "'SELECT body FROM docs WHERE ' || p".to_owned()),
        ]
    );
    assert_eq!(
        calls(&database, "rewritten"),
        [(
            "pg_catalog.ts_rewrite".to_owned(),
            "'SELECT target, substitute FROM rewrites WHERE ' || p".to_owned()
        )]
    );
    for name in ["returned", "quoted", "atomic", "rewritten"] {
        for typed in concatenated_types(&database, name) {
            assert_eq!(typed, "pg_catalog.text", "{name}");
        }
    }
}

/// A call PostgreSQL resolves to another function of the name runs no text:
/// one qualified with another schema, one whose arguments a user overload
/// takes better, and the `ts_rewrite` overload taking three queries.
#[test]
fn calls_resolving_elsewhere_are_no_site() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.query_to_xml(text, boolean, boolean, text) RETURNS xml
          LANGUAGE sql RETURN NULL;
         CREATE FUNCTION query_to_xml(q integer) RETURNS xml LANGUAGE sql RETURN NULL;
         CREATE FUNCTION qualified(p text) RETURNS xml LANGUAGE sql
          RETURN app.query_to_xml('SELECT ' || p, false, false, '');
         CREATE FUNCTION overloaded(p integer) RETURNS xml LANGUAGE sql
          RETURN query_to_xml(p);
         CREATE FUNCTION three(p tsquery) RETURNS tsquery LANGUAGE sql
          RETURN ts_rewrite(p, 'a'::tsquery, 'b'::tsquery);
         CREATE FUNCTION unqualified(p text) RETURNS xml LANGUAGE sql
          RETURN query_to_xml('SELECT ' || p, false, false, '');",
    );
    for name in ["qualified", "overloaded", "three"] {
        assert_eq!(calls(&database, name), [], "{name}");
    }
    assert_eq!(
        calls(&database, "unqualified"),
        [("pg_catalog.query_to_xml".to_owned(), "'SELECT ' || p".to_owned())]
    );
}

/// The text a call inside a query builds reads that query's columns, and a
/// column wins over a parameter of the same name, as PostgreSQL binds it.
#[test]
fn a_column_of_the_enclosing_query_binds_before_a_parameter() {
    let database = postgres(
        "CREATE TABLE docs (name varchar(10), body tsvector);
         CREATE FUNCTION per_row(name text) RETURNS SETOF xml LANGUAGE sql
          RETURN (SELECT query_to_xml('SELECT ' || d.name, false, false, '') FROM docs d);
         CREATE FUNCTION bare(name text) RETURNS SETOF xml LANGUAGE sql AS $$
          SELECT query_to_xml('SELECT ' || name, false, false, '') FROM docs
         $$;
         CREATE FUNCTION parameter(name text) RETURNS xml LANGUAGE sql
          RETURN query_to_xml('SELECT ' || name, false, false, '');",
    );
    assert_eq!(concatenated_types(&database, "per_row"), ["pg_catalog.varchar(14)"]);
    assert_eq!(concatenated_types(&database, "bare"), ["pg_catalog.varchar(14)"]);
    assert_eq!(concatenated_types(&database, "parameter"), ["pg_catalog.text"]);
}

/// An `EXECUTE`-family site names no function and keeps the PL/pgSQL
/// variables in reach.
#[test]
fn execute_sites_name_no_function() {
    let database = postgres(
        "CREATE FUNCTION built(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN EXECUTE 'SELECT ' || p; END $$;",
    );
    let body = function(&database, "built").dynamic_execution_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let [site] = sites.as_slice() else { panic!("one site") };
    assert!(site.function().is_none());
    assert!(site.plpgsql_scope().is_some());
}

/// Each site of `name` as the function a call site calls, `None` for an
/// `EXECUTE`-family site, and the command it runs.
fn sites(database: &ParserDB, name: &str) -> Vec<(Option<String>, String)> {
    let body = function(database, name).dynamic_execution_sites(database).unwrap();
    body.sites()
        .unwrap()
        .iter()
        .map(|site| {
            let called = site.function().map(|called| called.target_name().to_string());
            (called, site.command().to_string())
        })
        .collect()
}

/// A PL/pgSQL body runs the text its calls pass from a default, `PERFORM`,
/// an assignment, a condition, an embedded statement, the command and the
/// `USING` arguments of an `EXECUTE`, the arguments of an `OPEN` and the
/// cursor's query it opens, and `RETURN`, in the order the body evaluates
/// them.
#[test]
fn plpgsql_bodies_run_the_text_their_catalog_calls_pass() {
    let database = postgres(
        "CREATE TABLE items (name text);
         CREATE FUNCTION everywhere(p text) RETURNS xml LANGUAGE plpgsql AS $$
         DECLARE
          d xml := query_to_xml('SELECT 0 WHERE ' || p, false, false, '');
          c CURSOR (k xml) FOR SELECT k, ts_stat('SELECT ' || p);
          v xml;
         BEGIN
          PERFORM ts_stat('SELECT 1 WHERE ' || p);
          v := query_to_xmlschema('SELECT 2 WHERE ' || p, false, false, '');
          IF query_to_xml('SELECT 3 WHERE ' || p, false, false, '') IS NULL THEN
           INSERT INTO items SELECT query_to_xml('SELECT 4 WHERE ' || p, false, false, '')::text;
          END IF;
          EXECUTE query_to_xml('SELECT 5 WHERE ' || p, false, false, '')::text
           USING ts_rewrite('a'::tsquery, 'SELECT 6 WHERE ' || p);
          OPEN c(query_to_xml('SELECT 7 WHERE ' || p, false, false, ''));
          RETURN query_to_xml_and_xmlschema('SELECT 8 WHERE ' || p, false, false, '');
         END $$;",
    );
    let call = |function: &str, command: &str| {
        (Some(format!("pg_catalog.{function}")), command.to_owned())
    };
    assert_eq!(
        sites(&database, "everywhere"),
        [
            call("query_to_xml", "'SELECT 0 WHERE ' || p"),
            call("ts_stat", "'SELECT 1 WHERE ' || p"),
            call("query_to_xmlschema", "'SELECT 2 WHERE ' || p"),
            call("query_to_xml", "'SELECT 3 WHERE ' || p"),
            call("query_to_xml", "'SELECT 4 WHERE ' || p"),
            call("query_to_xml", "'SELECT 5 WHERE ' || p"),
            call("ts_rewrite", "'SELECT 6 WHERE ' || p"),
            (None, "query_to_xml('SELECT 5 WHERE ' || p, false, false, '')::TEXT".to_owned()),
            call("query_to_xml", "'SELECT 7 WHERE ' || p"),
            call("ts_stat", "'SELECT ' || p"),
            call("query_to_xml_and_xmlschema", "'SELECT 8 WHERE ' || p"),
        ]
    );
}

/// A call in a PL/pgSQL body resolves under the path in force where it
/// runs and with the arguments its variables give, so one a `SET` sends to
/// another schema's function or one a user overload takes better runs no
/// text.
#[test]
fn plpgsql_calls_resolve_where_the_body_runs_them() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.query_to_xml(text, boolean, boolean, text) RETURNS xml
          LANGUAGE sql RETURN NULL;
         CREATE FUNCTION query_to_xml(q integer) RETURNS xml LANGUAGE sql RETURN NULL;
         CREATE FUNCTION overloaded(n integer) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN PERFORM query_to_xml(n); END $$;
         CREATE FUNCTION pathed(p text) RETURNS void LANGUAGE plpgsql AS $$
         BEGIN
          PERFORM query_to_xml('SELECT 1 WHERE ' || p, false, false, '');
          SET search_path = app, pg_catalog;
          PERFORM query_to_xml('SELECT 2 WHERE ' || p, false, false, '');
         END $$;",
    );
    assert_eq!(sites(&database, "overloaded"), []);
    assert_eq!(
        sites(&database, "pathed"),
        [(Some("pg_catalog.query_to_xml".to_owned()), "'SELECT 1 WHERE ' || p".to_owned())]
    );
}

/// The text a call in a PL/pgSQL body builds reads the variables in reach
/// where the body evaluates it, a loop's index, a declared variable and a
/// cursor's parameter in the cursor's query.
#[test]
fn plpgsql_calls_read_the_variables_in_reach() {
    let database = postgres(
        "CREATE FUNCTION scoped(p text) RETURNS void LANGUAGE plpgsql AS $$
         DECLARE
          v name;
          c CURSOR (k varchar) FOR SELECT ts_stat('SELECT ' || k);
         BEGIN
          FOR i IN 1 .. 2 LOOP
           PERFORM ts_stat('SELECT ' || i);
          END LOOP;
          PERFORM ts_stat('SELECT ' || v);
          OPEN c('x');
         END $$;",
    );
    assert_eq!(
        concatenated_types(&database, "scoped"),
        ["pg_catalog.int4", "pg_catalog.name", "pg_catalog.varchar"]
    );
}
