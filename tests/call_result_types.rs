//! Tests that a call written in any statement resolves to the function
//! PostgreSQL runs and answers the type it returns.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::LookupError, prelude::*, utils::common_type_name};
use sqlparser::{
    ast::{Expr, Function, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn db(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema parses")
}

/// The call `expression` writes.
fn call(expression: &str) -> Function {
    let Statement::Query(query) =
        Parser::parse_sql(&PostgreSqlDialect {}, &format!("SELECT {expression}"))
            .expect("query parses")
            .remove(0)
    else {
        panic!("not a query")
    };
    let SetExpr::Select(select) = *query.body else { panic!("not a select") };
    match select.projection.into_iter().next() {
        Some(SelectItem::UnnamedExpr(Expr::Function(call))) => call,
        _ => panic!("not a call"),
    }
}

/// What `expression` returns given its argument types, under PostgreSQL's
/// identifier rule.
fn returns(db: &ParserDB, expression: &str, types: &[Option<&str>]) -> Option<String> {
    db.call_result_type_name(&call(expression), types, IdentifierCase::AsWritten)
        .expect("call resolves")
        .map(std::borrow::Cow::into_owned)
}

const SCHEMA: &str = "CREATE TABLE t (b BYTEA);
     CREATE FUNCTION f() RETURNS BYTEA AS 'SELECT ''\\x01''::bytea' LANGUAGE sql;";

/// A call, its argument types and the type it returns.
type Case<'a> = (&'a str, &'a [Option<&'a str>], Option<&'a str>);

/// The calls the request names answer the type PostgreSQL gives them.
#[test]
fn builtin_and_declared_calls_answer_their_result_types() {
    let db = db(SCHEMA);
    let bytea = Some("BYTEA");
    let literal = Some("unknown");
    let int = Some("INT");
    let cases: &[Case<'_>] = &[
        ("decode('0102', 'hex')", &[literal, literal], Some("BYTEA")),
        ("f()", &[], Some("BYTEA")),
        ("gen_random_uuid()", &[], Some("UUID")),
        ("coalesce(b, NULL)", &[bytea, literal], Some("BYTEA")),
        ("nullif(b, b)", &[bytea, bytea], Some("BYTEA")),
        ("min(b)", &[bytea], Some("BYTEA")),
        ("max(b)", &[bytea], Some("BYTEA")),
        ("lag(b)", &[bytea], Some("BYTEA")),
        ("lag(b, 1)", &[bytea, int], Some("BYTEA")),
        ("lag(b, 1, NULL)", &[bytea, int, literal], Some("BYTEA")),
        ("lead(b)", &[bytea], Some("BYTEA")),
        ("first_value(b)", &[bytea], Some("BYTEA")),
        ("last_value(b)", &[bytea], Some("BYTEA")),
        ("nth_value(b, 2)", &[bytea, int], Some("BYTEA")),
        ("array_agg(b)", &[bytea], Some("BYTEA[]")),
        // An array aggregates into an array of the same type.
        ("array_agg(b)", &[Some("INT[]")], Some("INT[]")),
        ("count(b)", &[bytea], Some("BIGINT")),
        ("count(*)", &[], Some("BIGINT")),
        ("sum(i)", &[int], Some("BIGINT")),
        ("sum(i)", &[Some("BIGINT")], Some("NUMERIC")),
        ("avg(i)", &[Some("REAL")], Some("DOUBLE PRECISION")),
        ("random(1, 2.5)", &[int, Some("NUMERIC")], Some("NUMERIC")),
        ("lower(v)", &[Some("VARCHAR(10)")], Some("TEXT")),
    ];
    for (expression, types, expected) in cases {
        assert_eq!(returns(&db, expression, types).as_deref(), *expected, "{expression} {types:?}");
    }
}

/// `anycompatible` takes PostgreSQL's common type, and a call the server
/// refuses answers nothing.
#[test]
fn compatible_arguments_take_their_common_type() {
    let db = db("");
    let cases: &[(&[Option<&str>], Option<&str>)] = &[
        (&[Some("INT"), Some("BIGINT")], Some("BIGINT")),
        (&[Some("BIGINT"), Some("INT")], Some("BIGINT")),
        (&[Some("INT"), Some("NUMERIC")], Some("NUMERIC")),
        // `double precision` is the numeric category's preferred type.
        (&[Some("FLOAT8"), Some("NUMERIC")], Some("FLOAT8")),
        (&[Some("DATE"), Some("TIMESTAMPTZ")], Some("TIMESTAMPTZ")),
        (&[Some("TEXT"), Some("VARCHAR")], Some("TEXT")),
        (&[Some("unknown"), Some("unknown")], Some("TEXT")),
        (&[Some("BYTEA"), Some("TEXT")], None),
        (&[Some("UUID"), Some("TEXT")], None),
        (&[Some("INT"), None], None),
    ];
    for (types, expected) in cases {
        assert_eq!(returns(&db, "coalesce(x, y)", types).as_deref(), *expected, "{types:?}");
        assert_eq!(common_type_name(types).as_deref(), *expected, "{types:?}");
    }
}

/// An argument whose type nobody knows leaves the result open only when the
/// result depends on it.
#[test]
fn an_unknown_argument_type_decides_only_what_depends_on_it() {
    let db = db(SCHEMA);
    assert_eq!(
        returns(&db, "decode(x, 'hex')", &[None, Some("unknown")]).as_deref(),
        Some("BYTEA")
    );
    assert_eq!(returns(&db, "count(x)", &[None]).as_deref(), Some("BIGINT"));
    assert_eq!(returns(&db, "array_agg(x)", &[None]), None);
    assert_eq!(returns(&db, "min(x)", &[None]), None);
    // Every `sum` overload taking the argument returns a different type.
    assert_eq!(returns(&db, "sum(x)", &[None]), None);
}

/// Overloads are told apart by their argument types, and a name that reaches
/// several alike is refused.
#[test]
fn overloads_are_chosen_by_argument_type() -> Result<(), LookupError> {
    let db = db("CREATE FUNCTION g(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION g(x BYTEA) RETURNS BYTEA LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION g(x TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT x';");
    let case = IdentifierCase::AsWritten;
    let g = call("g(x)");
    let chosen = |types: &[Option<&str>]| -> Result<Option<String>, LookupError> {
        Ok(db
            .resolve_call(&g, types, case)?
            .and_then(|function| function.argument_type_names(&db).next())
            .map(std::borrow::Cow::into_owned))
    };
    assert_eq!(chosen(&[Some("BYTEA")])?.as_deref(), Some("BYTEA"));
    // A smaller integer converts to `int` and to nothing else here.
    assert_eq!(chosen(&[Some("SMALLINT")])?.as_deref(), Some("INT"));
    // An untyped literal leans towards the string category.
    assert_eq!(chosen(&[Some("unknown")])?.as_deref(), Some("TEXT"));
    assert_eq!(chosen(&[Some("UUID")])?, None);
    assert_eq!(chosen(&[None])?, None);
    assert!(matches!(
        db.resolve_call(&g, &[], case),
        Err(LookupError::ArgumentTypeCount { arguments: 1, types: 0, .. })
    ));
    Ok(())
}

/// Defaults, `VARIADIC` and named notation decide which functions take a
/// call before any type is weighed.
#[test]
fn arity_and_named_notation_select_the_reached_functions() -> Result<(), LookupError> {
    let db =
        db("CREATE FUNCTION h(a INT, b TEXT DEFAULT 'x') RETURNS TEXT LANGUAGE sql AS 'SELECT b';
         CREATE FUNCTION v(VARIADIC xs INT[]) RETURNS INT LANGUAGE sql AS 'SELECT 1';");
    let case = IdentifierCase::AsWritten;
    let reached = |expression: &str| -> Result<usize, LookupError> {
        Ok(db.functions_reached_by(&call(expression), case)?.count())
    };
    assert_eq!(reached("h(1)")?, 1);
    assert_eq!(reached("h(1, 'y')")?, 1);
    assert_eq!(reached("h(b => 'y', a => 1)")?, 1);
    assert_eq!(reached("h(b => 'y')")?, 0);
    assert_eq!(reached("h(c => 1)")?, 0);
    assert_eq!(reached("h()")?, 0);
    assert_eq!(reached("v(1, 2, 3)")?, 1);
    assert_eq!(reached("v()")?, 0);
    assert_eq!(
        returns(&db, "h(b => 'y', a => 1)", &[Some("unknown"), Some("INT")]).as_deref(),
        Some("TEXT")
    );
    // Each argument a `VARIADIC` parameter collects takes its element type.
    assert_eq!(returns(&db, "v(1, 2)", &[Some("INT"), Some("UUID")]), None);
    assert_eq!(returns(&db, "v(1, 2)", &[Some("INT"), Some("SMALLINT")]).as_deref(), Some("INT"));
    Ok(())
}

/// A qualified call reaches its own schema only, and a name no static reader
/// resolves is refused.
#[test]
fn a_qualified_call_reaches_its_own_schema() {
    let db = db("CREATE SCHEMA app;
         CREATE FUNCTION app.decode(x TEXT, y TEXT) RETURNS TEXT LANGUAGE sql AS 'SELECT x';");
    let bare = [Some("TEXT"), Some("TEXT")];
    assert_eq!(returns(&db, "decode(x, y)", &bare).as_deref(), Some("BYTEA"));
    assert_eq!(returns(&db, "app.decode(x, y)", &bare).as_deref(), Some("TEXT"));
    assert_eq!(returns(&db, "pg_catalog.decode(x, y)", &bare).as_deref(), Some("BYTEA"));
    assert!(matches!(
        db.call_result_type_name(&call("a.b.decode(x, y)"), &bare, IdentifierCase::AsWritten),
        Err(LookupError::InvalidObjectName { .. })
    ));
}

/// A set-returning function and one whose result is a row column return the
/// column's type for each row.
#[test]
fn row_results_return_their_column_type() {
    let db = db("CREATE FUNCTION ids() RETURNS SETOF BYTEA LANGUAGE sql AS 'SELECT NULL::bytea';
         CREATE FUNCTION one() RETURNS TABLE (id UUID) LANGUAGE sql AS 'SELECT gen_random_uuid()';
         CREATE FUNCTION two(OUT a INT, OUT b TEXT) LANGUAGE sql AS 'SELECT 1, ''x''';
         CREATE FUNCTION same(x anyelement) RETURNS anyarray LANGUAGE sql AS 'SELECT ARRAY[x]';");
    assert_eq!(returns(&db, "ids()", &[]).as_deref(), Some("BYTEA"));
    assert_eq!(returns(&db, "one()", &[]).as_deref(), Some("UUID"));
    assert_eq!(returns(&db, "two()", &[]).as_deref(), Some("record"));
    assert_eq!(returns(&db, "same(x)", &[Some("UUID")]).as_deref(), Some("UUID[]"));
}

/// Every name an answer can take parses back with the PostgreSQL dialect into
/// the type it names, custom types and arrays included.
#[test]
fn every_answered_name_parses_back_as_its_type() {
    let db = db(SCHEMA);
    let parses = |name: &str| {
        let mut parser = Parser::new(&PostgreSqlDialect {}).try_with_sql(name).expect("tokenizes");
        let parsed = parser.parse_data_type().expect("parses as a type");
        assert_eq!(parsed.to_string().to_lowercase(), name.to_lowercase(), "{name}");
    };
    for function in db.functions() {
        if let Some(result) = function.result_type_name(&db) {
            parses(&result);
        }
    }
    for (expression, types) in [
        ("array_agg(x)", [Some("vector(3)")]),
        ("array_agg(x)", [Some("geometry")]),
        ("array_agg(x)", [Some("app.\"Mood\"")]),
        ("min(x)", [Some("vector(3)")]),
        ("array_agg(x)", [Some("TIMESTAMP(3) WITH TIME ZONE")]),
    ] {
        let answer = returns(&db, expression, &types).expect("the call has a result type");
        parses(&answer);
    }
    assert_eq!(returns(&db, "array_agg(x)", &[Some("vector(3)")]).as_deref(), Some("vector(3)[]"));
    assert_eq!(
        returns(&db, "coalesce(x, NULL)", &[Some("geometry"), Some("unknown")]).as_deref(),
        Some("geometry")
    );
}

/// Calls of the functions `uuid-ossp` creates answer their result types.
#[test]
fn uuid_ossp_calls_return_uuid() {
    let created = db("CREATE EXTENSION \"uuid-ossp\";");
    for (expression, types) in [
        ("uuid_generate_v4()", &[][..]),
        ("uuid_generate_v1mc()", &[]),
        ("uuid_nil()", &[]),
        ("uuid_generate_v5(uuid_ns_url(), 'x')", &[Some("UUID"), Some("unknown")]),
    ] {
        assert_eq!(returns(&created, expression, types).as_deref(), Some("UUID"), "{expression}");
    }
    let in_schema = db("CREATE SCHEMA ext; CREATE EXTENSION \"uuid-ossp\" WITH SCHEMA ext;");
    assert_eq!(returns(&in_schema, "uuid_generate_v4()", &[]), None);
    assert_eq!(returns(&in_schema, "ext.uuid_generate_v4()", &[]).as_deref(), Some("UUID"));
}
