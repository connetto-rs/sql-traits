//! The relations and functions a PL/pgSQL body reaches, read from the SQL it
//! embeds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

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

/// Each table `name` reads, as `schema.table`.
fn read_by(database: &ParserDB, name: &str) -> Result<Vec<String>, LookupError> {
    Ok(function(database, name)
        .referenced_tables(database)?
        .into_iter()
        .map(|table| format!("{}.{}", table.table_schema().unwrap_or("public"), table.table_name()))
        .collect())
}

/// Each relation `name` names directly, labelled with its kind.
fn named_by(database: &ParserDB, name: &str) -> Result<Vec<String>, LookupError> {
    Ok(function(database, name)
        .referenced_relations(database)?
        .into_iter()
        .map(relation_label)
        .collect())
}

/// Each function `name` calls that the schema declares, as `schema.name`.
fn declared_called_by(database: &ParserDB, name: &str) -> Result<Vec<String>, LookupError> {
    Ok(function(database, name)
        .referenced_functions(database)?
        .into_iter()
        .map(|called| {
            format!("{}.{}", called.target_name().schema().unwrap_or("public"), called.name())
        })
        .filter(|called| !called.starts_with("pg_catalog."))
        .collect())
}

#[test]
fn a_definer_updating_a_protected_table_reads_it() {
    let database = postgres(
        "CREATE TABLE photos (content_id BYTEA PRIMARY KEY, content_state TEXT);
         ALTER TABLE photos ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION connetto_set_content_state(p_file_id BYTEA, p_new_state TEXT, p_caller TEXT)
             RETURNS BYTEA LANGUAGE plpgsql SECURITY DEFINER SET search_path TO ''
             AS $$ BEGIN
                 UPDATE public.photos SET content_state = p_new_state WHERE content_id = p_file_id;
                 RETURN p_file_id;
             END; $$;",
    );
    assert_eq!(read_by(&database, "connetto_set_content_state").unwrap(), ["public.photos"]);
}

/// Every place PL/pgSQL runs SQL from is read: `PERFORM`, a `FOR` query, a
/// cursor, a nested block with its handler and `RETURN QUERY`.
#[test]
fn every_embedded_statement_is_read_in_body_order() {
    let database = postgres(
        "CREATE TABLE pp_docs (id INT);
         CREATE TABLE pp_log (id INT);
         CREATE TABLE unread (id INT);
         CREATE FUNCTION pp_all() RETURNS SETOF INT LANGUAGE plpgsql AS $$
         DECLARE r RECORD; c CURSOR FOR SELECT id FROM pp_docs; n INT;
         BEGIN
           PERFORM 1 FROM pp_docs;
           FOR r IN SELECT id FROM pp_docs LOOP INSERT INTO pp_log VALUES (r.id); END LOOP;
           OPEN c; FETCH c INTO n; CLOSE c;
           BEGIN UPDATE pp_docs SET id = id; EXCEPTION WHEN others THEN DELETE FROM pp_log; END;
           RETURN QUERY SELECT id FROM pp_docs;
         END $$;",
    );
    assert_eq!(read_by(&database, "pp_all").unwrap(), ["public.pp_docs", "public.pp_log"]);
}

#[test]
fn expressions_holding_sublinks_are_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE TABLE audit (id INT);
         CREATE TABLE archive (id INT);
         CREATE TABLE quota (id INT);
         CREATE FUNCTION checked() RETURNS INT LANGUAGE plpgsql AS $$
         DECLARE n INT := (SELECT count(*) FROM quota);
         BEGIN
           IF EXISTS (SELECT 1 FROM docs) THEN n := (SELECT count(*) FROM audit); END IF;
           RETURN (SELECT count(*) FROM archive);
         END $$;",
    );
    assert_eq!(
        read_by(&database, "checked").unwrap(),
        ["public.quota", "public.docs", "public.audit", "public.archive"]
    );
}

/// `CASE`, every kind of loop and a block with a handler each run their
/// SQL where the body reaches them, nested here inside an outer loop.
#[test]
fn every_control_structure_is_read_in_body_order() {
    let tables = [
        "subject", "matched", "other", "guarded", "caught", "bound", "upper", "step", "argument",
        "cursored", "items", "exits",
    ];
    let schema = tables.map(|table| format!("CREATE TABLE {table} (id INT);")).join("\n");
    let database = postgres(&format!(
        "{schema}
         CREATE FUNCTION structured() RETURNS VOID LANGUAGE plpgsql AS $$
         DECLARE c CURSOR (k INT) FOR SELECT id FROM cursored WHERE id = k; r RECORD; x INT; n INT := 0;
         BEGIN
           FOR i IN 1..2 LOOP
             CASE (SELECT count(*) FROM subject)
               WHEN 0 THEN PERFORM 1 FROM matched;
               ELSE PERFORM 1 FROM other;
             END CASE;
             BEGIN PERFORM 1 FROM guarded; EXCEPTION WHEN others THEN PERFORM 1 FROM caught; END;
             WHILE n < (SELECT count(*) FROM bound) LOOP n := n + 1; END LOOP;
             FOR j IN 1..(SELECT count(*) FROM upper) BY (SELECT count(*) FROM step) LOOP NULL; END LOOP;
             FOR r IN c((SELECT max(id) FROM argument)) LOOP NULL; END LOOP;
             FOREACH x IN ARRAY (SELECT array_agg(id) FROM items) LOOP NULL; END LOOP;
             LOOP EXIT WHEN (SELECT count(*) FROM exits) >= 0; END LOOP;
           END LOOP;
         END $$;"
    ));
    let expected: Vec<String> = tables.iter().map(|table| format!("public.{table}")).collect();
    assert_eq!(read_by(&database, "structured").unwrap(), expected);
}

#[test]
fn unqualified_names_resolve_under_the_function_path() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION own_path() RETURNS BIGINT LANGUAGE plpgsql SET search_path = app
             AS $$ BEGIN RETURN (SELECT count(*) FROM docs); END $$;
         CREATE FUNCTION session_path() RETURNS BIGINT LANGUAGE plpgsql
             AS $$ BEGIN RETURN (SELECT count(*) FROM docs); END $$;
         CREATE FUNCTION emptied() RETURNS BIGINT LANGUAGE plpgsql SET search_path = ''
             AS $$ BEGIN RETURN (SELECT count(*) FROM docs); END $$;",
    );
    assert_eq!(read_by(&database, "own_path").unwrap(), ["app.docs"]);
    assert_eq!(read_by(&database, "session_path").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "emptied").unwrap(), Vec::<String>::new());
}

#[test]
fn a_path_set_in_the_body_reaches_the_statements_after_it() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION switched() RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM 1 FROM docs;
           SET LOCAL search_path = app;
           RETURN (SELECT count(*) FROM docs);
         END $$;",
    );
    assert_eq!(read_by(&database, "switched").unwrap(), ["public.docs", "app.docs"]);
}

#[test]
fn a_path_set_at_run_time_hides_later_unqualified_names() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION bare_after() RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM set_config('search_path', 'other', true);
           RETURN (SELECT count(*) FROM docs);
         END $$;
         CREATE FUNCTION qualified_after() RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM set_config('search_path', 'other', true);
           RETURN (SELECT count(*) FROM public.docs);
         END $$;",
    );
    assert!(matches!(
        read_by(&database, "bare_after"),
        Err(LookupError::FunctionBodyUnresolvable { function, .. }) if function == "bare_after"
    ));
    assert_eq!(read_by(&database, "qualified_after").unwrap(), ["public.docs"]);
}

/// A path set in a branch or a loop may or may not be in force when a later
/// name runs, and a loop runs the statements before its `SET` again after it.
#[test]
fn a_path_set_off_the_straight_line_hides_the_names_it_may_reach() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION branched(p BOOLEAN) RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           IF p THEN SET LOCAL search_path = app; END IF;
           RETURN (SELECT count(*) FROM docs);
         END $$;
         CREATE FUNCTION looped() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           FOR i IN 1..2 LOOP PERFORM 1 FROM docs; SET LOCAL search_path = app; END LOOP;
         END $$;
         CREATE FUNCTION qualified() RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           FOR i IN 1..2 LOOP PERFORM 1 FROM public.docs; SET LOCAL search_path = app; END LOOP;
           RETURN (SELECT count(*) FROM app.docs);
         END $$;",
    );
    for name in ["branched", "looped"] {
        assert!(
            matches!(
                read_by(&database, name),
                Err(LookupError::FunctionBodyUnresolvable { function, .. }) if function == name
            ),
            "{name}"
        );
    }
    assert_eq!(read_by(&database, "qualified").unwrap(), ["public.docs", "app.docs"]);
}

/// A `SET` buried in nested structures still hides what it may reach, a
/// `WHILE` condition runs again after its loop's `SET`, and a `CASE` branch
/// sets its path for only some runs.
#[test]
fn a_nested_or_repeated_path_change_hides_later_names() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION buried(p BOOLEAN) RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           FOR i IN 1..2 LOOP
             PERFORM 1 FROM docs;
             IF p THEN
               CASE WHEN p THEN
                 WHILE p LOOP BEGIN PERFORM set_config('search_path', 'app', true); END; END LOOP;
               END CASE;
             END IF;
           END LOOP;
         END $$;
         CREATE FUNCTION conditioned() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           WHILE EXISTS (SELECT 1 FROM docs) LOOP SET LOCAL search_path = app; END LOOP;
         END $$;
         CREATE FUNCTION cased(n INT) RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           CASE n WHEN 1 THEN SET LOCAL search_path = app; ELSE NULL; END CASE;
           RETURN (SELECT count(*) FROM docs);
         END $$;",
    );
    assert_eq!(
        read_by(&database, "buried"),
        Err(LookupError::FunctionBodyUnresolvable {
            function: "buried".to_string(),
            statement: "SELECT set_config('search_path', 'app', true)".to_string(),
        })
    );
    for name in ["conditioned", "cased"] {
        assert_eq!(
            read_by(&database, name),
            Err(LookupError::FunctionBodyUnresolvable {
                function: name.to_string(),
                statement: "SET LOCAL search_path = app".to_string(),
            }),
            "{name}"
        );
    }
}

/// A `SET` PostgreSQL refuses is reported wherever it sits.
#[test]
fn an_invalid_path_in_a_branch_is_reported() {
    let database = postgres(
        "CREATE FUNCTION refused(p BOOLEAN) RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN IF p THEN SET search_path = a.b; END IF; END $$;",
    );
    assert!(matches!(read_by(&database, "refused"), Err(LookupError::InvalidObjectName { .. })));
}

/// An `ELSIF` is tried only once the branch before it was not taken, so a
/// `SET` in that branch has not run.
#[test]
fn a_later_condition_reads_under_the_path_before_the_branches() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION tried(p BOOLEAN) RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           IF p THEN SET LOCAL search_path = app;
           ELSIF EXISTS (SELECT 1 FROM docs) THEN PERFORM 1 FROM docs;
           END IF;
         END $$;",
    );
    assert_eq!(read_by(&database, "tried").unwrap(), ["public.docs"]);
}

/// A handler runs after the block's subtransaction rolled back, taking every
/// `SET` the block made with it.
#[test]
fn a_handler_reads_under_the_path_its_block_started_with() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION guarded() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           SET LOCAL search_path = app;
           PERFORM 1 FROM docs;
         EXCEPTION WHEN others THEN
           PERFORM 1 FROM docs;
         END $$;",
    );
    assert_eq!(read_by(&database, "guarded").unwrap(), ["app.docs", "public.docs"]);
}

/// A nested block without handlers runs straight through, while one with
/// handlers leaves either its own path or its handler's behind.
#[test]
fn a_nested_block_passes_its_path_on_only_without_handlers() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION nested() RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN SET LOCAL search_path = app; END;
           RETURN (SELECT count(*) FROM docs);
         END $$;
         CREATE FUNCTION caught() RETURNS BIGINT LANGUAGE plpgsql AS $$
         BEGIN
           BEGIN SET LOCAL search_path = app; EXCEPTION WHEN others THEN NULL; END;
           RETURN (SELECT count(*) FROM docs);
         END $$;",
    );
    assert_eq!(read_by(&database, "nested").unwrap(), ["app.docs"]);
    assert_eq!(
        read_by(&database, "caught"),
        Err(LookupError::FunctionBodyUnresolvable {
            function: "caught".to_string(),
            statement: "SET LOCAL search_path = app".to_string(),
        })
    );
}

/// Variables, records, cursors and parameters live in the body, so a name
/// shared with a table reaches the table only where SQL reads a relation.
#[test]
fn declared_names_and_parameters_are_never_relations() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE TABLE audit (id INT);
         CREATE TABLE archive (id INT);
         CREATE FUNCTION shadowed(archive INT) RETURNS INT LANGUAGE plpgsql AS $$
         DECLARE docs INT; audit CURSOR FOR SELECT 1;
         BEGIN
           OPEN audit; FETCH audit INTO docs; CLOSE audit;
           RETURN docs + archive;
         END $$;",
    );
    assert_eq!(read_by(&database, "shadowed").unwrap(), Vec::<String>::new());
}

#[test]
fn sql_built_at_run_time_cannot_be_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION executed(t TEXT) RETURNS VOID LANGUAGE plpgsql
             AS $$ BEGIN EXECUTE 'DELETE FROM ' || quote_ident(t); END $$;
         CREATE FUNCTION returned(t TEXT) RETURNS SETOF INT LANGUAGE plpgsql
             AS $$ BEGIN RETURN QUERY EXECUTE 'SELECT id FROM ' || quote_ident(t); END $$;
         CREATE FUNCTION opened(t TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
             DECLARE c refcursor;
             BEGIN OPEN c FOR EXECUTE 'SELECT id FROM ' || quote_ident(t); CLOSE c; END $$;
         CREATE FUNCTION looped(t TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
             DECLARE r RECORD;
             BEGIN FOR r IN EXECUTE 'SELECT id FROM ' || quote_ident(t) LOOP END LOOP; END $$;
         CREATE FUNCTION branched(p BOOLEAN, t TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
             BEGIN PERFORM 1 FROM docs; IF p THEN EXECUTE t; END IF; END $$;
         CREATE FUNCTION nested(p BOOLEAN, t TEXT) RETURNS VOID LANGUAGE plpgsql AS $$
             DECLARE r RECORD;
             BEGIN IF p THEN FOR r IN EXECUTE t LOOP END LOOP; END IF; END $$;
         CREATE FUNCTION selected() RETURNS VOID LANGUAGE plpgsql
             AS $$ BEGIN EXECUTE command FROM commands; END $$;",
    );
    let hidden_by = |name: &str| {
        match read_by(&database, name) {
            Err(LookupError::FunctionBodyUnresolvable { function, statement })
                if function == name =>
            {
                statement
            }
            other => panic!("{name} answered {other:?}"),
        }
    };
    assert_eq!(hidden_by("executed"), "EXECUTE 'DELETE FROM ' || quote_ident(t)");
    assert_eq!(hidden_by("returned"), "EXECUTE 'SELECT id FROM ' || quote_ident(t)");
    assert_eq!(hidden_by("opened"), "EXECUTE 'SELECT id FROM ' || quote_ident(t)");
    assert_eq!(hidden_by("looped"), "EXECUTE 'SELECT id FROM ' || quote_ident(t)");
    assert_eq!(hidden_by("branched"), "EXECUTE t");
    assert_eq!(hidden_by("nested"), "EXECUTE t");
    // A command read from rows shows the whole query that reads it.
    assert_eq!(hidden_by("selected"), "EXECUTE (SELECT command FROM commands)");
}

#[test]
fn a_malformed_body_is_reported() {
    let database = postgres(
        "CREATE FUNCTION broken() RETURNS INT LANGUAGE plpgsql AS $$ BEGIN RETURN; $$;
         CREATE FUNCTION unquoted() RETURNS INT LANGUAGE plpgsql RETURN 1;",
    );
    for name in ["broken", "unquoted"] {
        assert!(
            matches!(
                read_by(&database, name),
                Err(LookupError::FunctionBodyUnparsable { function, .. }) if function == name
            ),
            "{name}"
        );
    }
}

#[test]
fn relations_are_named_before_views_expand() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE VIEW open_docs AS SELECT id FROM docs;
         CREATE FUNCTION reader() RETURNS BIGINT LANGUAGE plpgsql
             AS $$ BEGIN RETURN (SELECT count(*) FROM open_docs); END $$;",
    );
    assert_eq!(named_by(&database, "reader").unwrap(), ["view open_docs"]);
    assert_eq!(read_by(&database, "reader").unwrap(), ["public.docs"]);
}

#[test]
fn calls_in_embedded_statements_are_resolved() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.helper() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION helper() RETURNS INT LANGUAGE sql AS 'SELECT 2';
         CREATE FUNCTION caller() RETURNS INT LANGUAGE plpgsql SET search_path = app
             AS $$ BEGIN PERFORM helper(); RETURN 1; END $$;
         CREATE FUNCTION guarded() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           SET LOCAL search_path = app;
           PERFORM helper();
         EXCEPTION WHEN others THEN
           PERFORM helper();
         END $$;
         CREATE FUNCTION looped() RETURNS VOID LANGUAGE plpgsql AS $$
         BEGIN
           LOOP PERFORM helper(); SET LOCAL search_path = app; END LOOP;
         END $$;",
    );
    assert_eq!(declared_called_by(&database, "caller").unwrap(), ["app.helper"]);
    // The handler calls under the path the block started with.
    assert_eq!(declared_called_by(&database, "guarded").unwrap(), ["app.helper", "public.helper"]);
    assert!(matches!(
        declared_called_by(&database, "looped"),
        Err(LookupError::FunctionBodyUnresolvable { statement, .. })
            if statement == "SET LOCAL search_path = app"
    ));
}

#[test]
fn a_body_in_another_procedural_language_cannot_be_read() {
    let database =
        postgres("CREATE FUNCTION scripted() RETURNS INT LANGUAGE plperl AS $$ return 1; $$;");
    let scripted = function(&database, "scripted");
    let not_sql = || {
        LookupError::FunctionBodyNotSql {
            function: "scripted".to_string(),
            language: "plperl".to_string(),
        }
    };
    assert_eq!(scripted.referenced_tables(&database).map(|_| ()), Err(not_sql()));
    assert_eq!(scripted.referenced_relations(&database).map(|_| ()), Err(not_sql()));
    assert_eq!(scripted.referenced_functions(&database).map(|_| ()), Err(not_sql()));
}
