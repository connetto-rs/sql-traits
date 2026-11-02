//! Tests that a function's language and argument names are reachable.
//!
//! A body means nothing without the language it is written in, and a call
//! cannot be expanded into a body without knowing what the body calls its
//! arguments. Both are recorded when the input is parsed, and both used to be
//! unreachable, which left a caller reading a body as SQL it may not be and
//! substituting into names it had to guess.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

/// Reads the written names out of collected argument targets, borrowing from
/// the collected values instead of from the iterator item.
fn names_of<'a>(arguments: &'a [Option<TargetName<'_>>]) -> Vec<Option<&'a str>> {
    arguments.iter().map(|name| name.as_ref().map(TargetName::name)).collect()
}

#[test]
fn the_language_clause_is_reported_as_written_and_as_stored() {
    let database = db("CREATE FUNCTION one() RETURNS INT LANGUAGE SQL AS 'SELECT 1';
         CREATE FUNCTION two() RETURNS INT LANGUAGE plpgsql AS 'BEGIN RETURN 2; END';");

    let one = database
        .function_by_target(TargetName::new("one", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("one exists");
    assert_eq!(one.language(), Some("SQL"));
    assert!(!one.language_is_quoted());
    assert_eq!(one.stored_language().as_deref(), Some("sql"));

    let two = database
        .function_by_target(TargetName::new("two", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("two exists");
    assert_eq!(two.language(), Some("plpgsql"));
    assert_eq!(two.stored_language().as_deref(), Some("plpgsql"));
}

/// PostgreSQL refuses `LANGUAGE "SQL"`, because the language it stores is named
/// `sql`, so the quoted spelling has to stay distinguishable from the folded
/// one rather than being read as the same language.
#[test]
fn a_quoted_language_name_keeps_its_case() {
    let database = db(r#"CREATE FUNCTION one() RETURNS INT LANGUAGE "SQL" AS 'SELECT 1';"#);
    let one = database
        .function_by_target(TargetName::new("one", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("one exists");

    assert_eq!(one.language(), Some("SQL"));
    assert!(one.language_is_quoted());
    assert_eq!(one.stored_language().as_deref(), Some("SQL"));
}

/// The clause is required by PostgreSQL, so a function without one comes from
/// input the server would reject, and the reader says so rather than inventing
/// a language.
#[test]
fn an_omitted_language_clause_names_no_language() {
    let database =
        ParserDB::parse::<GenericDialect>("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';")
            .expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.language(), None);
    assert_eq!(function.stored_language(), None);
}

#[test]
fn arguments_report_the_names_they_are_declared_under() {
    let database = db("CREATE FUNCTION is_member(doc_id INT, role TEXT) RETURNS BOOL
             LANGUAGE sql AS 'SELECT true';");
    let function = database
        .function_by_target(TargetName::new("is_member", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("is_member exists");

    let arguments: Vec<_> = function.argument_names(&database).collect();
    let names: Vec<Option<&str>> = names_of(&arguments);
    assert_eq!(names, vec![Some("doc_id"), Some("role")]);

    let stored: Vec<Option<String>> = function
        .stored_argument_names(&database)
        .map(|name| name.map(|name| name.to_string()))
        .collect();
    assert_eq!(stored, vec![Some("doc_id".to_owned()), Some("role".to_owned())]);
}

/// A quoted argument name is reached case-sensitively in the body, so folding
/// it would point a substitution at a name the body never writes.
#[test]
fn a_quoted_argument_name_keeps_its_case() {
    let database = db(r#"CREATE FUNCTION f("DocId" INT, "Role" TEXT) RETURNS BOOL LANGUAGE sql
             AS 'SELECT true';"#);
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    let arguments: Vec<_> = function.argument_names(&database).collect();
    let names: Vec<Option<&str>> = names_of(&arguments);
    assert_eq!(names, vec![Some("DocId"), Some("Role")]);
    assert!(
        function
            .argument_names(&database)
            .all(|name| name.expect("the argument is named").name_is_quoted())
    );

    let stored: Vec<Option<String>> = function
        .stored_argument_names(&database)
        .map(|name| name.map(|name| name.to_string()))
        .collect();
    assert_eq!(stored, vec![Some("DocId".to_owned()), Some("Role".to_owned())]);
}

/// An argument declared as a bare type has no name for a body to reach, and
/// only `$1` gets at it.
#[test]
fn an_argument_declared_as_a_bare_type_has_no_name() {
    let database = db("CREATE FUNCTION f(INT, TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.argument_names(&database).collect::<Vec<_>>(), vec![None, None]);
    assert_eq!(function.stored_argument_names(&database).collect::<Vec<_>>(), vec![None, None]);
}

/// The two readers are used together to decide what a call substitutes, so a
/// position naming one argument must name the same argument's type.
#[test]
fn argument_names_line_up_with_argument_types() {
    let database = db("CREATE FUNCTION f(a INT, TEXT, INOUT c BOOL) RETURNS BOOL LANGUAGE sql
             AS 'SELECT true';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    let arguments: Vec<_> = function.argument_names(&database).collect();
    let names: Vec<Option<&str>> = names_of(&arguments);
    let types: Vec<String> =
        function.argument_type_names(&database).map(|name| name.to_string()).collect();

    assert_eq!(names, vec![Some("a"), None, Some("c")]);
    assert_eq!(types, vec!["INT".to_owned(), "TEXT".to_owned(), "BOOLEAN".to_owned()]);
}

/// `VARIADIC` is a mode rather than a name, so the argument it marks is unnamed
/// unless the input also names it.
#[test]
fn a_variadic_argument_is_named_only_when_the_input_names_it() {
    let database = db("CREATE FUNCTION f(VARIADIC items INT[]) RETURNS BOOL LANGUAGE sql
             AS 'SELECT true';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");
    assert_eq!(
        function.argument_names(&database).collect::<Vec<_>>(),
        vec![Some(TargetName::new("items", false))]
    );

    // A registered builtin resides in the catalog schema, so the lookup names
    // it.
    let builtin = database
        .function_by_target(
            TargetName::new("coalesce", false).with_schema("pg_catalog", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("coalesce is registered");
    assert_eq!(builtin.argument_names(&database).collect::<Vec<_>>(), vec![None]);
}

/// A replacement keeps the same catalog entry, so the arguments and language it
/// declares are the ones a later reader must see.
#[test]
fn a_replaced_function_reports_the_new_declaration() {
    let database = db("CREATE FUNCTION f(old_id INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';
         CREATE OR REPLACE FUNCTION f(new_id INT) RETURNS BOOL LANGUAGE plpgsql
             AS 'BEGIN RETURN true; END';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.stored_language().as_deref(), Some("plpgsql"));
    assert_eq!(
        function.argument_names(&database).collect::<Vec<_>>(),
        vec![Some(TargetName::new("new_id", false))]
    );
}

/// The two body readers are disjoint: one answers the string spellings, the
/// other the `RETURN` spelling PostgreSQL 14 added, so a caller wanting the
/// body asks both and a `RETURN` body is no longer invisible.
#[test]
fn a_return_body_is_an_expression_and_a_string_body_is_text() {
    let database = db("CREATE FUNCTION added(a INT, b INT) RETURNS INT LANGUAGE sql RETURN a + b;
         CREATE FUNCTION quoted(a INT) RETURNS INT LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION dollar(a INT) RETURNS INT LANGUAGE sql AS $$SELECT a$$;
         CREATE FUNCTION constant() RETURNS TEXT LANGUAGE sql RETURN 'text';");

    let added = database
        .function_by_target(TargetName::new("added", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("added exists");
    assert_eq!(added.body(), None);
    assert_eq!(added.body_expression().map(ToString::to_string).as_deref(), Some("a + b"));

    let quoted = database
        .function_by_target(TargetName::new("quoted", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("quoted exists");
    assert_eq!(quoted.body(), Some("SELECT a"));
    assert_eq!(quoted.body_expression(), None);

    let dollar = database
        .function_by_target(TargetName::new("dollar", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("dollar exists");
    assert_eq!(dollar.body(), Some("SELECT a"));
    assert_eq!(dollar.body_expression(), None);

    // A returned string constant is the expression, not body text to parse.
    let constant = database
        .function_by_target(TargetName::new("constant", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("constant exists");
    assert_eq!(constant.body(), None);
    assert_eq!(constant.body_expression().map(ToString::to_string).as_deref(), Some("'text'"));
}

/// An `OUT` argument occupies a position like any other, and the two readers
/// are only usable together if they agree on that.
#[test]
fn out_and_default_carrying_arguments_keep_their_positions() {
    let database = db("CREATE FUNCTION f(IN a INT DEFAULT 1, OUT b TEXT, INOUT c BOOL)
             RETURNS INT LANGUAGE sql AS 'SELECT 1';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    let arguments: Vec<_> = function.argument_names(&database).collect();
    let names: Vec<Option<&str>> = names_of(&arguments);
    let types: Vec<String> =
        function.argument_type_names(&database).map(|name| name.to_string()).collect();

    assert_eq!(names, vec![Some("a"), Some("b"), Some("c")]);
    assert_eq!(types, vec!["INT".to_owned(), "TEXT".to_owned(), "BOOLEAN".to_owned()]);
}

/// Folding an unquoted identifier lowercases only its ASCII letters, which is
/// what PostgreSQL does on a UTF8 database: `RÔLE` is stored as `rÔle`,
/// measured on 18.4. A reader that lowercased the rest would look for a name
/// the server does not hold.
#[test]
fn a_non_ascii_argument_name_folds_the_way_postgresql_stores_it() {
    let database = db("CREATE FUNCTION f(RÔLE TEXT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(
        function.stored_argument_names(&database).next().expect("one argument").as_deref(),
        Some("rÔle")
    );
}

/// The readers answer off the parsed statement, so a dialect that parses the
/// same `CREATE FUNCTION` answers the same way.
#[test]
fn the_readers_answer_under_another_dialect() {
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE FUNCTION f(doc_id INT) RETURNS BOOL LANGUAGE sql AS 'SELECT true';",
    )
    .expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.stored_language().as_deref(), Some("sql"));
    assert_eq!(
        function.argument_names(&database).collect::<Vec<_>>(),
        vec![Some(TargetName::new("doc_id", false))]
    );
}

#[test]
fn configuration_parameters_follow_create_and_alter_order() {
    let database = db("CREATE FUNCTION created() RETURNS INT LANGUAGE sql
             SET search_path TO a, pg_temp SET work_mem TO '64kB' AS 'SELECT 1';
         CREATE FUNCTION altered() RETURNS INT LANGUAGE sql
             SET search_path TO a, pg_temp SET work_mem TO '64kB' AS 'SELECT 1';
         ALTER FUNCTION altered() SET search_path TO b, pg_temp
             SET statement_timeout TO '1s';
         ALTER FUNCTION altered() RESET work_mem SET search_path TO c;
         CREATE FUNCTION default_path() RETURNS INT LANGUAGE sql
             SET search_path TO DEFAULT AS 'SELECT 1';
         CREATE FUNCTION current_path() RETURNS INT LANGUAGE sql
             SET search_path FROM CURRENT AS 'SELECT 1';");
    let parameters = |name: &str| {
        database
            .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function exists")
            .configuration_parameters()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };

    assert_eq!(parameters("created"), ["SET search_path = a, pg_temp", "SET work_mem = '64kB'"]);
    assert_eq!(parameters("altered"), ["SET search_path = c", "SET statement_timeout = '1s'"]);
    assert_eq!(parameters("default_path"), ["SET search_path = DEFAULT"]);
    // PostgreSQL stores the path the clause read, as `pg_proc.proconfig` shows.
    assert_eq!(parameters("current_path"), ["SET search_path = public"]);
}

#[test]
fn configuration_parameter_prefixes_remain_distinct() {
    let database = db("CREATE FUNCTION f() RETURNS INT LANGUAGE sql
             SET a.b.c TO x SET other.b.c TO y AS 'SELECT 1';
         ALTER FUNCTION f() RESET a.b.c;");
    let parameters = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
        .configuration_parameters()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert_eq!(parameters, ["SET other.b.c = y"]);
}
