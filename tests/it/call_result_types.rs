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

/// A call written without parentheses passes nothing, and a subquery passes
/// one argument.
#[test]
fn bare_and_subquery_calls_count_their_arguments() -> Result<(), LookupError> {
    let db = db("CREATE FUNCTION \"array\"(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';");
    assert_eq!(returns(&db, "current_date", &[]).as_deref(), Some("DATE"));
    assert_eq!(
        db.functions_reached_by(&call("ARRAY(SELECT 1)"), IdentifierCase::AsWritten)?.count(),
        1
    );
    Ok(())
}

/// Positional arguments must come first, and a parameter is named at most
/// once and only by an identifier.
#[test]
fn named_notation_maps_every_argument_once() -> Result<(), LookupError> {
    let db =
        db("CREATE FUNCTION h(a INT, b TEXT DEFAULT 'x') RETURNS TEXT LANGUAGE sql AS 'SELECT b';");
    let case = IdentifierCase::AsWritten;
    let reached = |call: &Function| -> Result<usize, LookupError> {
        Ok(db.functions_reached_by(call, case)?.count())
    };
    assert_eq!(reached(&call("h(1, b => 'y')"))?, 1);
    assert_eq!(reached(&call("h(a => 1, 2)"))?, 0);
    assert_eq!(reached(&call("h(1, a => 2)"))?, 0);

    let mut by_expression = call("h(a => 1)");
    let sqlparser::ast::FunctionArguments::List(list) = &mut by_expression.args else {
        panic!("an argument list")
    };
    let Some(sqlparser::ast::FunctionArg::ExprNamed { name, .. }) = list.args.first_mut() else {
        panic!("a named argument")
    };
    *name = Expr::value(sqlparser::ast::Value::SingleQuotedString("a".to_owned()));
    assert_eq!(reached(&by_expression)?, 0);
    assert_eq!(db.call_result_type_name(&by_expression, &[Some("INT")], case)?, None);
    let h = db.functions().find(|function| function.name() == "h").expect("declared");
    assert_eq!(h.call_result_type_name(&by_expression, &[Some("INT")], &db)?, None);
    Ok(())
}

/// The `anyelement` family binds one type, and `anyarray` the array over it.
#[test]
fn the_element_family_binds_one_type() {
    let db = db("CREATE FUNCTION first_of(xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT xs[1]';
         CREATE FUNCTION wrap(x anyelement) RETURNS anyarray LANGUAGE sql AS 'SELECT ARRAY[x]';
         CREATE FUNCTION both(x anyelement, xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION same(a anyelement, b anyelement) RETURNS anyelement LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION firsts(VARIADIC xs anyarray) RETURNS anyelement LANGUAGE sql AS 'SELECT xs[1]';
         CREATE FUNCTION ranged(x anyelement) RETURNS anyrange LANGUAGE sql AS 'SELECT NULL';");
    let cases: &[Case<'_>] = &[
        ("first_of(xs)", &[Some("INT[]")], Some("INT")),
        ("first_of(xs)", &[Some("INT[][]")], Some("INT")),
        ("first_of(xs)", &[Some("_int4")], Some("int4")),
        ("first_of(xs)", &[Some("TEXT")], None),
        ("first_of(NULL)", &[Some("unknown")], None),
        ("wrap(x)", &[Some("UUID")], Some("UUID[]")),
        // PostgreSQL has no array type over an array type.
        ("wrap(x)", &[Some("INT[]")], None),
        ("both(x, xs)", &[Some("INT"), Some("INT[]")], Some("INT")),
        ("both(NULL, xs)", &[Some("unknown"), Some("INT[]")], Some("INT")),
        ("both(x, xs)", &[Some("INT"), Some("TEXT[]")], None),
        ("same(a, NULL)", &[Some("INT"), Some("unknown")], Some("INT")),
        ("same(a, b)", &[Some("INT"), Some("BIGINT")], None),
        ("firsts(1, 2)", &[Some("INT"), Some("INT")], Some("INT")),
        ("firsts(1, 2)", &[Some("INT"), Some("BIGINT")], None),
        ("ranged(x)", &[Some("INT")], None),
        ("rank(x)", &[Some("TEXT")], Some("BIGINT")),
    ];
    for (expression, types, expected) in cases {
        assert_eq!(returns(&db, expression, types).as_deref(), *expected, "{expression} {types:?}");
    }
}

/// The `anycompatible` family binds the common type, and its array forms the
/// array over it.
#[test]
fn the_compatible_family_binds_the_common_type() {
    let db = db("CREATE FUNCTION pair(a anycompatible, b anycompatible) RETURNS anycompatiblearray
             LANGUAGE sql AS 'SELECT ARRAY[a, b]';
         CREATE FUNCTION extend(xs anycompatiblearray, y anycompatible) RETURNS anycompatiblearray
             LANGUAGE sql AS 'SELECT xs || y';
         CREATE FUNCTION scalar(x anycompatiblenonarray) RETURNS anycompatible
             LANGUAGE sql AS 'SELECT x';");
    let cases: &[Case<'_>] = &[
        ("pair(a, b)", &[Some("INT"), Some("BIGINT")], Some("BIGINT[]")),
        ("pair(NULL, NULL)", &[Some("unknown"), Some("unknown")], Some("TEXT[]")),
        ("pair(a, b)", &[Some("INT"), None], None),
        ("extend(xs, y)", &[Some("INT[]"), Some("BIGINT")], Some("BIGINT[]")),
        ("extend(xs, y)", &[Some("INT"), Some("BIGINT")], None),
        ("extend(NULL, y)", &[Some("unknown"), Some("INT")], Some("INT[]")),
        ("extend(xs, y)", &[None, Some("INT")], None),
        ("scalar(x)", &[Some("INT")], Some("INT")),
        ("scalar(x)", &[Some("INT[]")], None),
        ("coalesce(x, y)", &[Some("INT[]"), Some("BIGINT[]")], Some("BIGINT[]")),
        ("coalesce(x, y, z)", &[Some("INT"), Some("INT"), Some("BIGINT")], Some("BIGINT")),
        ("coalesce(x, y)", &[Some("geometry"), Some("vector(3)")], None),
    ];
    for (expression, types, expected) in cases {
        assert_eq!(returns(&db, expression, types).as_deref(), *expected, "{expression} {types:?}");
    }
}

/// Overloads taking the arguments by conversion are told apart as
/// `func_select_candidate` tells them apart, or the call is refused.
#[test]
fn overload_heuristics_follow_func_select_candidate() {
    let db = db("CREATE FUNCTION exact(a INT, b BIGINT) RETURNS INT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION exact(a BIGINT, b BIGINT) RETURNS BIGINT LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION prefer(x NUMERIC) RETURNS NUMERIC LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION prefer(x FLOAT8) RETURNS FLOAT8 LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION tie(x BIGINT) RETURNS BIGINT LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION tie(x NUMERIC) RETURNS NUMERIC LANGUAGE sql AS 'SELECT x';
         CREATE FUNCTION gasp(a INT, b INT) RETURNS INT LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION gasp(a INT, b UUID) RETURNS UUID LANGUAGE sql AS 'SELECT b';
         CREATE FUNCTION wide(a BIGINT, b BIGINT) RETURNS BIGINT LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION wide(a NUMERIC, b NUMERIC) RETURNS NUMERIC LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION wide(a BIGINT, b BIGINT, c BIGINT) RETURNS BIGINT LANGUAGE sql AS 'SELECT a';
         CREATE FUNCTION wide(a NUMERIC, b NUMERIC, c NUMERIC) RETURNS NUMERIC LANGUAGE sql AS 'SELECT a';");
    let cases: &[Case<'_>] = &[
        // The most exact matches win.
        ("exact(a, b)", &[Some("INT"), Some("INT")], Some("INT")),
        // A preferred type of the argument's category wins.
        ("prefer(x)", &[Some("INT")], Some("FLOAT8")),
        ("tie(x)", &[Some("SMALLINT")], None),
        // The literal takes the one type the other arguments share.
        ("gasp(a, '5')", &[Some("INT"), Some("unknown")], Some("INT")),
        ("wide(a, '5')", &[Some("INT"), Some("unknown")], None),
        // The other arguments share no one type for the literal to take.
        ("wide(a, b, '5')", &[Some("SMALLINT"), Some("INT"), Some("unknown")], None),
    ];
    for (expression, types, expected) in cases {
        assert_eq!(returns(&db, expression, types).as_deref(), *expected, "{expression} {types:?}");
    }
}

/// A function on the path taking the same arguments as one in a schema
/// searched earlier is hidden.
#[test]
fn an_earlier_schema_hides_a_function_of_the_same_arguments() -> Result<(), LookupError> {
    let db = db("CREATE FUNCTION lower(x TEXT) RETURNS INT LANGUAGE sql AS 'SELECT 1';");
    let reached: Vec<String> = db
        .functions_reached_by(&call("lower(x)"), IdentifierCase::AsWritten)?
        .map(|function| function.target_name().to_string())
        .collect();
    assert_eq!(reached, ["pg_catalog.lower"]);
    assert_eq!(returns(&db, "lower(x)", &[Some("TEXT")]).as_deref(), Some("TEXT"));
    Ok(())
}

/// One function answers what a call of it returns, or nothing when it does
/// not take the call.
#[test]
fn a_function_answers_for_a_call_of_it() -> Result<(), LookupError> {
    let db = db("CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';");
    let f = db.functions().find(|function| function.name() == "f").expect("declared");
    let one = call("f(x)");
    let answer = |call: &Function, types: &[Option<&str>]| -> Result<Option<String>, LookupError> {
        Ok(f.call_result_type_name(call, types, &db)?.map(std::borrow::Cow::into_owned))
    };
    assert_eq!(answer(&one, &[Some("SMALLINT")])?.as_deref(), Some("INT"));
    assert_eq!(answer(&one, &[Some("BIGINT")])?, None);
    assert_eq!(answer(&call("f(x, y)"), &[Some("INT"), Some("INT")])?, None);
    assert!(matches!(answer(&one, &[]), Err(LookupError::ArgumentTypeCount { .. })));
    Ok(())
}

/// A named table result yields one column's type or a record, as the anonymous
/// one does.
#[test]
fn a_named_table_result_returns_its_column_type() {
    let db = ParserDB::parse::<sqlparser::dialect::MsSqlDialect>(
        "CREATE FUNCTION one() RETURNS @r TABLE (id INT) AS BEGIN RETURN; END;
         CREATE FUNCTION two() RETURNS @r TABLE (id INT, name TEXT) AS BEGIN RETURN; END;",
    )
    .expect("schema parses");
    let result = |name: &str| {
        db.functions()
            .find(|function| function.name() == name)
            .and_then(|function| function.result_type_name(&db))
            .map(std::borrow::Cow::into_owned)
    };
    assert_eq!(result("one").as_deref(), Some("INT"));
    assert_eq!(result("two").as_deref(), Some("record"));
}
