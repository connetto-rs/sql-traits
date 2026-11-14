//! Tests that a stored expression one of whose operators or calls selects
//! nothing is refused where PostgreSQL 18 refuses it: when the statement
//! creating it runs, and when `ALTER COLUMN ... TYPE` parses it again.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, Unresolved},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Comparison functions over `int4` and `int8`.
const FUNCTIONS: &str = "SET search_path = public, app;
     CREATE SCHEMA app;
     CREATE FUNCTION app.lt8(int8, int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
     CREATE FUNCTION app.lt84(int8, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
     CREATE FUNCTION app.lt48(int4, int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';";

/// `<<<` over two `int8`.
const INT8: &str = "CREATE OPERATOR app.<<< (LEFTARG = int8, RIGHTARG = int8, FUNCTION = app.lt8);";

/// `<<<` over `int8` and `int4`, and over `int4` and `int8`, which leave
/// two `int2` operands without a best match.
const MIXED: &str =
    "CREATE OPERATOR app.<<< (LEFTARG = int8, RIGHTARG = int4, FUNCTION = app.lt84);
     CREATE OPERATOR app.<<< (LEFTARG = int4, RIGHTARG = int8, FUNCTION = app.lt48);";

/// `f` over `int4`.
const F: &str = "CREATE FUNCTION f(int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';";

/// Asserts that `sql` ends refused because `operator` over `operands`
/// selects nothing for `reason`.
#[track_caller]
fn assert_operator_refused(sql: &str, operator: &str, operands: &[&str], reason: Unresolved) {
    let result = parse(sql);
    assert!(
        matches!(
            &result,
            Err(Error::UnresolvedOperator { operator_name, operand_types, reason: refused })
                if &**operator_name == operator
                    && operand_types.iter().map(AsRef::as_ref).eq(operands.iter().copied())
                    && *refused == reason
        ),
        "{sql}\ngot {result:?}"
    );
}

/// Each stored expression PostgreSQL types when the statement creating it
/// runs is refused when an operator in it takes no operand types.
#[test]
fn a_created_expression_is_refused() {
    for created in [
        "CREATE TABLE d (a text CHECK (a > 0));",
        "CREATE TABLE d (a text); ALTER TABLE d ADD CONSTRAINT ck CHECK (a > 0);",
        "CREATE TABLE d (a text, b bool DEFAULT ('x'::text > 0));",
        "CREATE TABLE d (a text, b bool); ALTER TABLE d ALTER COLUMN b SET DEFAULT ('x'::text > 0);",
        "CREATE TABLE d (a text, b bool GENERATED ALWAYS AS (a > 0) STORED);",
        "CREATE TABLE d (a text); CREATE INDEX ON d ((a > 0));",
        "CREATE TABLE d (a text); CREATE VIEW v AS SELECT a > 0 AS x FROM d;",
        "CREATE TABLE d (a text); CREATE POLICY p ON d USING (a > 0);",
        "CREATE TABLE d (a text);
         CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TRIGGER tg BEFORE INSERT ON d FOR EACH ROW WHEN (NEW.a > 0) EXECUTE FUNCTION trg();",
        "CREATE TABLE d (a text); CREATE RULE r AS ON INSERT TO d WHERE NEW.a > 0 DO INSTEAD NOTHING;",
        "CREATE DOMAIN dm AS text CHECK (VALUE > 0);",
        "CREATE TABLE d (a text) PARTITION BY RANGE ((a > 0));",
    ] {
        assert_operator_refused(created, ">", &["text", "int4"], Unresolved::DoesNotExist);
    }
}

/// A column type change parses each expression reading the column again
/// over the new type, refusing one an operator no longer takes.
#[test]
fn a_type_change_refuses_an_operator_the_new_type_lacks() {
    assert_operator_refused(
        "CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (a > 0));
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;",
        ">",
        &["text", "int4"],
        Unresolved::DoesNotExist,
    );
    assert_operator_refused(
        &format!(
            "{FUNCTIONS} {INT8} CREATE TABLE d (a INT8, CONSTRAINT ck CHECK (a <<< 1));
             ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;"
        ),
        "<<<",
        &["text", "int8"],
        Unresolved::DoesNotExist,
    );
}

/// An operand or an argument the old selection converted keeps its cast
/// through the type change, so the expression still types.
#[test]
fn a_type_change_keeps_a_converted_operand_typed() {
    parse(&format!(
        "{FUNCTIONS} {INT8} CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (a <<< 1));
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;"
    ))
    .expect("the deparse casts `a` to `int8`");
    parse(
        "CREATE FUNCTION f8(int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (f8(a)));
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;",
    )
    .expect("the deparse casts the argument to `int8`");
}

/// Retyping every column an expression reads in one statement parses it
/// again over all the new types at once.
#[test]
fn a_statement_retyping_both_operands_keeps_the_expression_typed() {
    parse(
        "CREATE TABLE d (a INT4, b INT4, CONSTRAINT ck CHECK (a > b));
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text,
             ALTER COLUMN b TYPE text USING b::text;",
    )
    .expect("`text > text` exists");
}

/// Each refusal reads as PostgreSQL's, naming the operator or the function
/// with the types it weighed.
#[test]
fn a_refusal_names_what_was_weighed() {
    let message = |sql: &str| parse(sql).expect_err(sql).to_string();
    assert_eq!(
        message("CREATE TABLE d (a text CHECK (a > 0));"),
        "Operator `>` over (text, int4) does not exist."
    );
    assert_eq!(
        message(&format!("{FUNCTIONS} {MIXED} CREATE TABLE d (a INT2, CHECK (a <<< 1::int2));")),
        "Operator `<<<` over (int2, int2) is not unique."
    );
    assert_eq!(
        message(
            "CREATE FUNCTION p(anyelement) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT true';
             CREATE TABLE d (a INT2 CHECK (p('x')));"
        ),
        "Function `p(unknown)` cannot bind a polymorphic type to `unknown`."
    );
}

/// A call two functions take equally well, and one whose polymorphic
/// parameter meets only an untyped literal, are refused as PostgreSQL
/// refuses them, and so is an operator whose polymorphic operands meet only
/// untyped literals.
#[test]
fn an_ambiguous_or_unbound_selection_is_refused() {
    let refused_call = |sql: &str, reason: Unresolved| {
        let result = parse(sql);
        assert!(
            matches!(&result, Err(Error::UnresolvedCall { reason: refused, .. }) if *refused == reason),
            "{sql}\ngot {result:?}"
        );
    };
    refused_call(
        "CREATE FUNCTION h(int8, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE FUNCTION h(int4, int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE TABLE d (a INT2 CHECK (h(a, a)));",
        Unresolved::NotUnique,
    );
    refused_call(
        "CREATE FUNCTION p(anyelement) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE TABLE d (a INT2 CHECK (p('x')));",
        Unresolved::PolymorphicUnknown,
    );
    assert_operator_refused(
        "CREATE FUNCTION pp(anyelement, anyelement) RETURNS BOOL LANGUAGE sql IMMUTABLE
             AS 'SELECT true';
         CREATE OPERATOR <<<< (LEFTARG = anyelement, RIGHTARG = anyelement, FUNCTION = pp);
         CREATE TABLE d (a INT2 CHECK ('x' <<<< 'y'));",
        "<<<<",
        &["unknown", "unknown"],
        Unresolved::PolymorphicUnknown,
    );
}

/// Operands two operators take equally well are refused as PostgreSQL
/// refuses them, though an expression stored before the second one keeps
/// its selection through later rebuilds.
#[test]
fn an_ambiguous_operator_is_refused_only_when_written() {
    assert_operator_refused(
        &format!("{FUNCTIONS} {MIXED} CREATE TABLE d (a INT2, CHECK (a <<< 1::int2));"),
        "<<<",
        &["int2", "int2"],
        Unresolved::NotUnique,
    );
    parse(&format!(
        "{FUNCTIONS} {INT8} CREATE TABLE d (a INT2, CONSTRAINT ck CHECK (a <<< 1::int2));
         {MIXED}
         ALTER TABLE d ADD COLUMN b INT;
         ALTER TABLE d RENAME COLUMN a TO c;"
    ))
    .expect("the check keeps `<<<(int8, int8)`");
}

/// A call no function takes is refused when written and when a type
/// change parses it again.
#[test]
fn an_unresolved_call_is_refused() {
    for sql in [
        format!("{F} CREATE TABLE d (a text CHECK (f(a)));"),
        format!(
            "{F} CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (f(a)));
             ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;"
        ),
        // The new default's call is typed, though an older check calls the
        // same name with as many arguments.
        format!(
            "{F} CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (f(a)));
             ALTER TABLE d ADD COLUMN b BOOL DEFAULT f('x'::text);"
        ),
    ] {
        let result = parse(&sql);
        assert!(
            matches!(
                &result,
                Err(Error::UnresolvedCall { function_name, argument_types, reason })
                    if function_name == "f"
                        && argument_types.iter().map(AsRef::as_ref).eq(["text"])
                        && *reason == Unresolved::DoesNotExist
            ),
            "{sql}\ngot {result:?}"
        );
    }
}

/// A call a rebuild carries is not typed again, so the path it was written
/// under keeps holding it.
#[test]
fn a_carried_call_keeps_the_path_it_was_written_under() {
    parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.g(int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         SET search_path = app, public;
         CREATE TABLE public.d (a INT4, CONSTRAINT ck CHECK (g(a)));
         SET search_path = public;
         ALTER TABLE d ADD COLUMN b INT;
         ALTER TABLE d RENAME COLUMN a TO c;",
    )
    .expect("the check keeps `app.g`");
}

/// An installed extension may supply the operator, so nothing is refused
/// while one is.
#[test]
fn an_extension_may_supply_the_operator() {
    parse("CREATE EXTENSION pg_trgm; CREATE TABLE d (a text CHECK (a % 'x'));")
        .expect("`pg_trgm` declares `%` over `text`");
}

/// With an extension installed nothing is refused, and a call parsed again
/// still keeps the function its argument was converted to.
#[test]
fn an_extension_leaves_a_converted_argument_pinned() {
    let result = parse(
        "CREATE EXTENSION pg_trgm;
         CREATE FUNCTION f8(int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (f8(a)));
         CREATE FUNCTION f8(text) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT true';
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;
         DROP FUNCTION f8(int8);",
    );
    assert!(
        matches!(&result, Err(Error::FunctionReferenced { function_name, .. }) if function_name == "f8"),
        "got {result:?}"
    );
}
