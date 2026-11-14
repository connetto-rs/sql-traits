//! Tests that a one-argument call named after a type, which no function
//! takes exactly, is the cast PostgreSQL 18 reads it as.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{Expr, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// A zero UUID, as an untyped literal.
const ZERO: &str = "'00000000-0000-0000-0000-000000000000'";

/// What `call`, written over the columns of `t` in the database `schema`
/// builds, resolves to, and the type it has.
fn read(schema: &str, call: &str) -> (String, Option<String>) {
    let db = parse(schema).expect("the schema parses");
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::Folded)
        .expect("one table answers")
        .expect("`t` exists");
    let expression = Parser::new(&PostgreSqlDialect {})
        .try_with_sql(call)
        .and_then(|mut parser| parser.parse_expr())
        .expect("the call parses");
    let scope = ExpressionScope::Table(table);
    let context =
        ExpressionContext::for_expression(&expression, &scope, &db, IdentifierCase::Folded)
            .expect("the call is typed");
    let application = context.call(&expression).expect("the call is read").expect("a plain call");
    let resolution = match application.resolution() {
        CallResolution::Cast(target) => format!("cast to {target}"),
        CallResolution::Selected(selected) => {
            format!("runs {}", selected.function().target_name())
        }
        CallResolution::NotFound => "not found".to_owned(),
        CallResolution::Ambiguous(_) => "ambiguous".to_owned(),
        CallResolution::UnboundPolymorphic(_) => "unbound".to_owned(),
        CallResolution::Undetermined => "undetermined".to_owned(),
    };
    let typed = match context.type_of(&expression).expect("the call has a type or none") {
        OperandType::Known { type_name, .. } => Some(type_name.to_string()),
        OperandType::UntypedLiteral | OperandType::Unknown => None,
    };
    (resolution, typed)
}

/// Asserts that `call` over `schema` is a cast to `target`, of that type.
#[track_caller]
fn assert_cast(schema: &str, call: &str, target: &str) {
    assert_eq!(
        read(schema, call),
        (format!("cast to {target}"), Some(target.to_owned())),
        "{call} over {schema}"
    );
}

/// An untyped literal passed to a function named after a type is read by
/// that type's input function, whether no function of the name exists or
/// several take other types.
#[test]
fn an_untyped_literal_is_cast() {
    let schema = "CREATE TABLE t (a INT4);";
    assert_cast(schema, &format!("uuid({ZERO})"), "pg_catalog.uuid");
    assert_cast(schema, "int8('5')", "pg_catalog.int8");
}

/// A typed argument is cast where the explicit conversion keeps its binary
/// form, as from a type to itself or to a domain over its type, goes
/// through text, or is a cast the catalog runs through text.
#[test]
fn a_typed_argument_is_cast_in_binary_or_through_text() {
    let schema = "CREATE TYPE mood AS ENUM ('a');
                  CREATE DOMAIN code AS VARCHAR(3);
                  CREATE TABLE t (a INT4, m mood, s TEXT, v VARCHAR(3), j JSON);";
    assert_cast(schema, "text(a)", "pg_catalog.text");
    assert_cast(schema, "pg_catalog.text(a)", "pg_catalog.text");
    assert_cast(schema, "text(m)", "pg_catalog.text");
    assert_cast(schema, "text(s)", "pg_catalog.text");
    assert_cast(schema, "code(s)", "\"public\".\"code\"");
    assert_cast(schema, "code(v)", "\"public\".\"code\"");
    assert_cast(schema, "jsonb(j)", "pg_catalog.jsonb");
}

/// A function taking the argument type exactly runs instead, and an
/// untyped literal, which no declared type matches exactly, is still cast.
#[test]
fn an_exact_function_runs_instead() {
    let schema = "CREATE FUNCTION uuid(text) RETURNS UUID LANGUAGE sql IMMUTABLE
                      AS 'SELECT gen_random_uuid()';
                  CREATE TABLE t (a TEXT);";
    assert_eq!(read(schema, "uuid(a)").0, "runs uuid");
    assert_cast(schema, &format!("uuid({ZERO})"), "pg_catalog.uuid");
}

/// No cast is read where the explicit conversion runs a cast function,
/// converts an array element by element, or does not exist, from a row type
/// to a string, to a row type, or with the argument passed by name.
#[test]
fn other_calls_select_among_functions() {
    let schema = "CREATE TYPE mood AS ENUM ('a');
                  CREATE TYPE pair AS (x INT4, y INT4);
                  CREATE TABLE t (a INT4, b BOOL, p pair, s TEXT, arr INT4[]);";
    // `boolean` reaches `character` by the cast function `text(boolean)`.
    for call in ["bpchar(b)", "_text(arr)", "mood(a)", "text(p)", "pair(s)", "text(x => a)"] {
        assert_eq!(read(schema, call), ("not found".to_owned(), None), "{call}");
    }
    // A field of a composite column has no type the model knows, so the
    // call may be a cast or a function.
    assert_eq!(read(schema, "text((p).x)"), ("undetermined".to_owned(), None));
}

/// The type a call written in any statement returns is the type it is cast
/// to, and no function runs.
#[test]
fn the_cast_answers_the_call_result_type() {
    let db = parse("").expect("an empty schema");
    let Statement::Query(query) =
        Parser::parse_sql(&PostgreSqlDialect {}, "SELECT int8('5')").expect("parses").remove(0)
    else {
        panic!("a query")
    };
    let SetExpr::Select(select) = *query.body else { panic!("a select") };
    let SelectItem::UnnamedExpr(Expr::Function(call)) = &select.projection[0] else {
        panic!("a call")
    };
    let case = IdentifierCase::Folded;
    assert_eq!(
        db.call_result_type_name(call, &[Some("unknown")], case).expect("typed").as_deref(),
        Some("int8")
    );
    assert!(db.resolve_call(call, &[Some("unknown")], case).expect("read").is_none());
    // Whether a function takes an argument of unknown type exactly is open.
    assert_eq!(db.call_result_type_name(call, &[None], case).expect("typed"), None);
}

/// A call read as a cast holds no function, so the function of the same
/// name drops, while a call running it holds it.
#[test]
fn a_cast_holds_no_function() {
    let function = "CREATE FUNCTION uuid(text) RETURNS UUID LANGUAGE sql IMMUTABLE
                        AS 'SELECT gen_random_uuid()';";
    parse(&format!(
        "{function} CREATE TABLE d (x UUID DEFAULT uuid({ZERO})); DROP FUNCTION uuid(text);"
    ))
    .expect("the default casts the literal");
    let result = parse(&format!(
        "{function} CREATE TABLE d (t TEXT, CONSTRAINT ck CHECK (uuid(t) IS NOT NULL));
         DROP FUNCTION uuid(text);"
    ));
    assert!(
        matches!(&result, Err(Error::FunctionReferenced { function_name }) if function_name == "uuid"),
        "got {result:?}"
    );
}
