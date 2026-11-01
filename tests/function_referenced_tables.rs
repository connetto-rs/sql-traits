//! The tables a function body reads, resolved under the `search_path` the
//! body runs with.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::Statement,
    dialect::{BigQueryDialect, GenericDialect, HiveDialect, MsSqlDialect, PostgreSqlDialect},
    parser::Parser,
};

fn postgres(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

fn read_by(database: &ParserDB, function: &str) -> Result<Vec<String>, LookupError> {
    let function = database
        .function_by_target(TargetName::new(function, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    Ok(function
        .referenced_tables(database)?
        .into_iter()
        .map(|table| format!("{}.{}", table.table_schema().unwrap_or("public"), table.table_name()))
        .collect())
}

const TWO_DOCS: &str = "
    CREATE SCHEMA app;
    CREATE TABLE app.docs (id INT);
    CREATE TABLE public.docs (id INT);
";

#[test]
fn an_emptied_path_reaches_only_qualified_names() {
    let database = postgres(
        "CREATE TABLE owner_admins (owner uuid);
         CREATE TABLE audit (owner uuid);
         CREATE FUNCTION qualified(o uuid) RETURNS boolean LANGUAGE sql SECURITY DEFINER
             SET search_path = ''
             AS $$ SELECT EXISTS (SELECT 1 FROM public.owner_admins WHERE owner = o) $$;
         CREATE FUNCTION unqualified(o uuid) RETURNS boolean LANGUAGE sql SECURITY DEFINER
             SET search_path = ''
             AS $$ SELECT EXISTS (SELECT 1 FROM owner_admins WHERE owner = o) $$;",
    );
    assert_eq!(read_by(&database, "qualified").unwrap(), ["public.owner_admins"]);
    // The schema's own path would carry this name into `public`, the
    // function's empty one carries it nowhere.
    assert_eq!(read_by(&database, "unqualified").unwrap(), Vec::<String>::new());
}

#[test]
fn the_function_path_decides_over_the_schema_path() {
    let database = postgres(&format!(
        "{TWO_DOCS}
         CREATE FUNCTION own_path() RETURNS INT LANGUAGE sql SET search_path TO app
             AS 'SELECT count(*) FROM docs';
         CREATE FUNCTION session_path() RETURNS INT LANGUAGE sql
             AS 'SELECT count(*) FROM docs';"
    ));
    assert_eq!(read_by(&database, "own_path").unwrap(), ["app.docs"]);
    assert_eq!(read_by(&database, "session_path").unwrap(), ["public.docs"]);
}

#[test]
fn the_path_follows_later_alterations() {
    let database = postgres(&format!(
        "{TWO_DOCS}
         CREATE FUNCTION altered() RETURNS INT LANGUAGE sql AS 'SELECT count(*) FROM docs';
         ALTER FUNCTION altered() SET search_path = app;
         CREATE FUNCTION reset() RETURNS INT LANGUAGE sql SET search_path = app
             AS 'SELECT count(*) FROM docs';
         ALTER FUNCTION reset() RESET search_path;
         CREATE FUNCTION defaulted() RETURNS INT LANGUAGE sql SET search_path = app
             AS 'SELECT count(*) FROM docs';
         ALTER FUNCTION defaulted() SET search_path TO DEFAULT;"
    ));
    assert_eq!(read_by(&database, "altered").unwrap(), ["app.docs"]);
    assert_eq!(read_by(&database, "reset").unwrap(), ["public.docs"]);
    // `TO DEFAULT` drops the function's own setting, as `RESET` does.
    assert_eq!(read_by(&database, "defaulted").unwrap(), ["public.docs"]);
}

#[test]
fn from_current_keeps_the_path_of_its_statement() {
    let database = postgres(&format!(
        "{TWO_DOCS}
         SET search_path = app;
         CREATE FUNCTION created() RETURNS INT LANGUAGE sql SET search_path FROM CURRENT
             AS 'SELECT count(*) FROM docs';
         CREATE FUNCTION altered() RETURNS INT LANGUAGE sql AS 'SELECT count(*) FROM docs';
         ALTER FUNCTION altered() SET search_path FROM CURRENT;
         SET search_path = public;"
    ));
    assert_eq!(read_by(&database, "created").unwrap(), ["app.docs"]);
    assert_eq!(read_by(&database, "altered").unwrap(), ["app.docs"]);
    let created = database
        .function_by_target(TargetName::new("created", false), IdentifierCase::AsWritten)
        .unwrap()
        .unwrap();
    // PostgreSQL stores the path the clause read, never the clause.
    assert_eq!(
        created.configuration_parameters().iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["SET search_path = app"]
    );
}

#[test]
fn the_temporary_schema_comes_first_unless_the_path_places_it() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TEMP TABLE docs (id INT);
         CREATE FUNCTION temp_first() RETURNS INT LANGUAGE sql SET search_path = app
             AS 'SELECT count(*) FROM docs';
         CREATE FUNCTION temp_last() RETURNS INT LANGUAGE sql SET search_path = app, pg_temp
             AS 'SELECT count(*) FROM docs';",
    );
    let temporary = |function: &str| {
        let function = database
            .function_by_target(TargetName::new(function, false), IdentifierCase::AsWritten)
            .unwrap()
            .unwrap();
        function
            .referenced_tables(&database)
            .unwrap()
            .into_iter()
            .map(TableLike::is_temporary)
            .collect::<Vec<_>>()
    };
    assert_eq!(temporary("temp_first"), [true]);
    assert_eq!(temporary("temp_last"), [false]);
}

#[test]
fn every_statement_and_return_expression_is_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE TABLE audit (id INT);
         CREATE TABLE archive (id INT);
         CREATE FUNCTION touch(d INT) RETURNS INT LANGUAGE sql AS $$
             INSERT INTO audit VALUES (d);
             WITH archive AS (SELECT 1 AS id) SELECT count(*) FROM docs, archive;
         $$;
         CREATE FUNCTION counted() RETURNS BIGINT LANGUAGE sql
             RETURN (SELECT count(*) FROM archive);
         CREATE FUNCTION atomic(d INT) RETURNS BIGINT LANGUAGE sql BEGIN ATOMIC
             INSERT INTO audit VALUES (d);
             UPDATE docs SET id = d WHERE id IN (SELECT id FROM archive);
             RETURN (SELECT count(*) FROM docs);
         END;",
    );
    // The `WITH` item shadows the table of the same name.
    assert_eq!(read_by(&database, "touch").unwrap(), ["public.audit", "public.docs"]);
    assert_eq!(read_by(&database, "counted").unwrap(), ["public.archive"]);
    assert_eq!(
        read_by(&database, "atomic").unwrap(),
        ["public.audit", "public.docs", "public.archive"]
    );
}

#[test]
fn a_procedure_call_in_a_body_cannot_be_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION calls() RETURNS INT LANGUAGE sql
             AS 'CALL elsewhere(); SELECT count(*)::int FROM docs';",
    );
    assert_eq!(
        read_by(&database, "calls"),
        Err(LookupError::FunctionBodyUnresolvable {
            function: "calls".to_string(),
            statement: "CALL elsewhere()".to_string(),
        })
    );
}

/// A `RETURN` or `BEGIN ATOMIC` body is bound when the function is created,
/// so the path the function sets for its own runs does not reach it.
#[test]
fn a_bound_body_reads_what_it_reached_at_creation() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE TABLE docs (id INT);
         CREATE TABLE app.docs (id INT);
         CREATE FUNCTION counted() RETURNS BIGINT LANGUAGE sql SET search_path = app
             RETURN (SELECT count(*) FROM docs);
         CREATE FUNCTION atomic() RETURNS BIGINT LANGUAGE sql SET search_path = app
             BEGIN ATOMIC SELECT count(*) FROM docs; END;",
    );
    assert_eq!(read_by(&database, "counted").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "atomic").unwrap(), ["public.docs"]);
}

/// A block body stores a temporary relation by its bare name, which reads
/// back as the temporary relation.
#[test]
fn a_block_body_reads_the_temporary_relation_it_bound() {
    let database = postgres(
        "CREATE TEMP TABLE scratch (id INT);
         CREATE FUNCTION atomic() RETURNS BIGINT LANGUAGE sql
             BEGIN ATOMIC SELECT count(*) FROM scratch; END;",
    );
    let function = database
        .function_by_target(TargetName::new("atomic", false), IdentifierCase::AsWritten)
        .unwrap()
        .unwrap();
    let temporary: Vec<bool> = function
        .referenced_tables(&database)
        .unwrap()
        .into_iter()
        .map(TableLike::is_temporary)
        .collect();
    assert_eq!(temporary, [true]);
}

#[test]
fn a_body_without_a_language_clause_reads_as_sql() {
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION unstated() RETURNS INT AS 'SELECT count(*) FROM docs';",
    )
    .unwrap();
    assert_eq!(read_by(&database, "unstated").unwrap(), ["public.docs"]);
}

#[test]
fn a_body_in_another_language_cannot_be_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION procedural() RETURNS INT LANGUAGE plpgsql
             AS $$ BEGIN RETURN (SELECT count(*) FROM docs); END $$;
         CREATE FUNCTION native() RETURNS INT LANGUAGE c AS 'module', 'symbol';",
    );
    assert_eq!(
        read_by(&database, "procedural"),
        Err(LookupError::FunctionBodyNotSql {
            function: "procedural".to_string(),
            language: "plpgsql".to_string(),
        })
    );
    assert!(matches!(
        read_by(&database, "native"),
        Err(LookupError::FunctionBodyNotSql { language, .. }) if language == "c"
    ));
}

#[test]
fn a_malformed_body_is_reported() {
    let database =
        postgres("CREATE FUNCTION broken() RETURNS INT LANGUAGE sql AS 'SELECT FROM WHERE (';");
    assert!(matches!(
        read_by(&database, "broken"),
        Err(LookupError::FunctionBodyUnparsable { function, .. }) if function == "broken"
    ));
}

#[test]
fn bodies_the_input_already_parsed_are_read() {
    let sql_server = ParserDB::parse::<MsSqlDialect>(
        "CREATE TABLE docs (id INT);
         CREATE TABLE audit (id INT);
         CREATE FUNCTION counted() RETURNS INT AS BEGIN RETURN (SELECT COUNT(*) FROM docs); END;
         CREATE FUNCTION listed() RETURNS TABLE AS RETURN SELECT id FROM audit;",
    )
    .unwrap();
    assert_eq!(read_by(&sql_server, "counted").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&sql_server, "listed").unwrap(), ["public.audit"]);

    let big_query = ParserDB::parse::<BigQueryDialect>(
        "CREATE TABLE docs (id INT64);
         CREATE FUNCTION counted() RETURNS INT64 AS ((SELECT COUNT(*) FROM docs));
         CREATE FUNCTION described() RETURNS INT64 OPTIONS (description = 'count')
             AS ((SELECT COUNT(*) FROM docs));",
    )
    .unwrap();
    assert_eq!(read_by(&big_query, "counted").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&big_query, "described").unwrap(), ["public.docs"]);
}

#[test]
fn a_multi_table_delete_list_is_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE TABLE audit (id INT);
         CREATE FUNCTION purge() RETURNS INT LANGUAGE sql
             AS 'DELETE docs, audit FROM docs JOIN audit ON docs.id = audit.id';
         CREATE FUNCTION malformed() RETURNS INT LANGUAGE sql AS 'DELETE a.b.c FROM docs';",
    );
    let mut read = read_by(&database, "purge").unwrap();
    read.sort();
    assert_eq!(read, ["public.audit", "public.docs"]);
    assert!(matches!(
        read_by(&database, "malformed"),
        Err(LookupError::InvalidObjectName { object_name, .. }) if object_name == "a.b.c"
    ));
}

#[test]
fn from_current_keeps_an_emptied_path() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         SET search_path = '';
         CREATE FUNCTION public.isolated() RETURNS INT LANGUAGE sql SET search_path FROM CURRENT
             AS 'SELECT count(*) FROM public.docs';
         SET search_path = public;
         CREATE FUNCTION bare() RETURNS INT LANGUAGE sql AS 'SELECT count(*) FROM docs';
         ALTER FUNCTION bare() SET search_path FROM CURRENT;",
    );
    let isolated = database
        .function_by_target(TargetName::new("isolated", false), IdentifierCase::AsWritten)
        .unwrap()
        .unwrap();
    // PostgreSQL spells the empty schema name `""` in `pg_proc.proconfig`.
    assert_eq!(
        isolated.configuration_parameters().iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["SET search_path = \"\""]
    );
    assert_eq!(read_by(&database, "isolated").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "bare").unwrap(), ["public.docs"]);
}

#[test]
fn code_outside_the_schema_cannot_be_read() {
    let unavailable = |database: &ParserDB, function: &str| {
        assert_eq!(
            read_by(database, function),
            Err(LookupError::FunctionBodyUnavailable { function: function.to_string() })
        );
    };
    let declared =
        ParserDB::parse::<GenericDialect>("CREATE FUNCTION declared() RETURNS INT;").unwrap();
    unavailable(&declared, "declared");

    let remote = ParserDB::parse::<BigQueryDialect>(
        "CREATE FUNCTION remote() RETURNS INT64 REMOTE WITH CONNECTION us.c
             OPTIONS (endpoint = 'https://example.com');",
    )
    .unwrap();
    unavailable(&remote, "remote");

    let jar = ParserDB::parse::<HiveDialect>(
        "CREATE FUNCTION jarred AS 'com.example.Jarred' USING JAR 'udf.jar';",
    )
    .unwrap();
    unavailable(&jar, "jarred");
}

#[test]
fn the_function_path_reads_entries_as_postgresql_does() {
    let database = postgres(
        "CREATE SCHEMA \"App\"; CREATE TABLE \"App\".docs (id INT);
         CREATE SCHEMA app; CREATE TABLE app.docs (id INT);
         CREATE SCHEMA \"7\"; CREATE TABLE \"7\".docs (id INT);
         CREATE FUNCTION exact() RETURNS INT LANGUAGE sql SET search_path = 'App'
             AS 'SELECT count(*) FROM docs';
         CREATE FUNCTION numbered() RETURNS INT LANGUAGE sql SET search_path = 007
             AS 'SELECT count(*) FROM docs';",
    );
    assert_eq!(read_by(&database, "exact").unwrap(), ["App.docs"]);
    assert_eq!(read_by(&database, "numbered").unwrap(), ["7.docs"]);
}

#[test]
fn a_function_path_postgresql_rejects_is_refused() {
    for sql in [
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql SET search_path = a.b AS 'SELECT 1';",
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
         ALTER FUNCTION f() SET search_path = app, a + b;",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(sql);
        assert!(
            matches!(
                result,
                Err(sql_traits::errors::Error::IdentifierLookupError(
                    LookupError::InvalidObjectName { .. }
                ))
            ),
            "{sql}: {result:?}"
        );
    }
}

#[test]
fn a_node_never_ingested_reports_a_path_postgresql_rejects() {
    let database = postgres("CREATE TABLE docs (id INT);");
    let mut statements = Parser::parse_sql(
        &PostgreSqlDialect {},
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql SET search_path = a.b
             AS 'SELECT count(*) FROM docs'",
    )
    .unwrap();
    let Some(Statement::CreateFunction(function)) = statements.pop() else {
        panic!("expected CREATE FUNCTION");
    };
    assert!(matches!(
        function.referenced_tables(&database),
        Err(LookupError::InvalidObjectName { object_name, .. }) if object_name == "a.b"
    ));
}

/// PostgreSQL 18 analyzes each statement of a string body when it runs, so a
/// `SET` or `RESET` of `search_path` reaches the statements after it.
#[test]
fn a_path_set_in_the_body_reaches_later_statements() {
    let database = postgres(&format!(
        "{TWO_DOCS}
         CREATE SCHEMA other; CREATE TABLE other.docs (id INT);
         CREATE FUNCTION switched() RETURNS INT LANGUAGE sql AS $$
             SELECT count(*) FROM docs; SET LOCAL search_path = other; SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION session_scope() RETURNS INT LANGUAGE sql SET search_path = app AS $$
             SET SESSION search_path TO other; SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION reset_one() RETURNS INT LANGUAGE sql SET search_path = app AS $$
             RESET search_path; SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION reset_all() RETURNS INT LANGUAGE sql SET search_path = app AS $$
             RESET ALL; SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION set_default() RETURNS INT LANGUAGE sql SET search_path = app AS $$
             SET search_path TO DEFAULT; SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION other_parameter() RETURNS INT LANGUAGE sql SET search_path = app AS $$
             SET work_mem = '4MB'; RESET statement_timeout; SELECT count(*)::int FROM docs
         $$;
         SET search_path = app;"
    ));
    assert_eq!(read_by(&database, "switched").unwrap(), ["app.docs", "other.docs"]);
    assert_eq!(read_by(&database, "session_scope").unwrap(), ["other.docs"]);
    // A reset restores the server's starting path, never the function's
    // clause or the caller's path.
    assert_eq!(read_by(&database, "reset_one").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "reset_all").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "set_default").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "other_parameter").unwrap(), ["app.docs"]);
}

#[test]
fn a_body_running_code_outside_it_cannot_be_read() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION calls() RETURNS INT LANGUAGE sql AS $$
             SELECT count(*) FROM docs; CALL elsewhere(); SELECT 1
         $$;
         CREATE FUNCTION executes() RETURNS INT LANGUAGE sql AS $$
             EXECUTE prepared; SELECT 1
         $$;",
    );
    for (function, statement) in [("calls", "CALL elsewhere()"), ("executes", "EXECUTE prepared")] {
        assert_eq!(
            read_by(&database, function),
            Err(LookupError::FunctionBodyUnresolvable {
                function: function.to_string(),
                statement: statement.to_string(),
            })
        );
    }
}

/// `set_config` changes `search_path` while the body runs, so an unqualified
/// name after it reaches a schema no static reader can know.
#[test]
fn a_path_set_at_run_time_hides_later_unqualified_names() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION bare_after() RETURNS INT LANGUAGE sql AS $$
             SELECT set_config('search_path', 'other', true); SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION qualified_after() RETURNS INT LANGUAGE sql AS $$
             SELECT pg_catalog.set_config('SEARCH_PATH', 'other', true);
             SELECT count(*)::int FROM public.docs
         $$;
         CREATE FUNCTION bare_before() RETURNS TEXT LANGUAGE sql AS $$
             SELECT count(*) FROM docs; SELECT set_config('search_path', 'other', true)
         $$;
         CREATE FUNCTION other_setting() RETURNS INT LANGUAGE sql AS $$
             SELECT set_config('work_mem', '4MB', true), current_user; SELECT count(*)::int FROM docs
         $$;
         CREATE FUNCTION computed_setting(name TEXT) RETURNS INT LANGUAGE sql AS $$
             SELECT set_config(name, 'other', true); SELECT count(*)::int FROM docs
         $$;",
    );
    assert!(matches!(
        read_by(&database, "bare_after"),
        Err(LookupError::FunctionBodyUnresolvable { statement, .. })
            if statement == "SELECT set_config('search_path', 'other', true)"
    ));
    assert_eq!(read_by(&database, "qualified_after").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "bare_before").unwrap(), ["public.docs"]);
    assert_eq!(read_by(&database, "other_setting").unwrap(), ["public.docs"]);
    assert!(matches!(
        read_by(&database, "computed_setting"),
        Err(LookupError::FunctionBodyUnresolvable { statement, .. })
            if statement == "SELECT set_config(name, 'other', true)"
    ));
}

fn named_by(database: &ParserDB, function: &str) -> Result<Vec<String>, LookupError> {
    let function = database
        .function_by_target(TargetName::new(function, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists");
    Ok(function
        .referenced_relations(database)?
        .into_iter()
        .map(|relation| {
            match relation {
                Relation::Table(table) => format!("table {}", table.table_name()),
                Relation::ForeignTable(table) => format!("foreign table {}", table.table_name()),
                Relation::View(view) => format!("view {}", view.view_name()),
                Relation::MaterializedView(view) => {
                    format!("materialized view {}", view.view_name())
                }
                Relation::Sequence(sequence) => format!("sequence {}", sequence.sequence_name()),
            }
        })
        .collect())
}

/// A body reading a view reads the view's base tables, while a materialized
/// view holds its own rows.
#[test]
fn a_body_reads_through_views_to_their_tables() {
    let database = postgres(
        "CREATE TABLE docs (id INT);
         CREATE TABLE audit (id INT);
         CREATE VIEW open_docs AS SELECT id FROM docs;
         CREATE MATERIALIZED VIEW counted AS SELECT id FROM audit;
         CREATE FUNCTION reader() RETURNS BIGINT LANGUAGE sql AS $$
             SELECT count(*) FROM open_docs, counted
         $$;",
    );
    assert_eq!(
        named_by(&database, "reader").unwrap(),
        ["view open_docs", "materialized view counted"]
    );
    assert_eq!(read_by(&database, "reader").unwrap(), ["public.docs"]);
}

/// The function's own path and body-level `SET` decide which relation a name
/// reaches, whatever its kind.
#[test]
fn relations_resolve_under_the_path_the_body_runs_with() {
    let database = postgres(
        "CREATE SCHEMA app;
         CREATE VIEW app.docs AS SELECT 1 AS id;
         CREATE TABLE public.docs (id INT);
         CREATE FUNCTION own_path() RETURNS BIGINT LANGUAGE sql SET search_path = app
             AS 'SELECT count(*) FROM docs';
         CREATE FUNCTION switched() RETURNS BIGINT LANGUAGE sql AS $$
             SET LOCAL search_path = app; SELECT count(*) FROM docs
         $$;",
    );
    assert_eq!(named_by(&database, "own_path").unwrap(), ["view docs"]);
    assert_eq!(named_by(&database, "switched").unwrap(), ["view docs"]);
}

#[test]
fn relations_report_the_same_unknowable_bodies() {
    let database = postgres(
        "CREATE FUNCTION procedural() RETURNS INT LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;",
    );
    assert!(matches!(
        named_by(&database, "procedural"),
        Err(LookupError::FunctionBodyNotSql { language, .. }) if language == "plpgsql"
    ));
}
