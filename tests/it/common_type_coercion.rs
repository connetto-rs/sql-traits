//! Tests that the sites where values meet at one type answer the type
//! PostgreSQL 18's `select_common_type` picks and how each value converts.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, SelectItem, SetExpr, Statement, TableFactor, Values},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use crate::support::coercion_steps::steps;

const SCHEMA: &str = "CREATE DOMAIN positive AS INT4 CHECK (VALUE > 0);
     CREATE DOMAIN ints AS INT4[];
     CREATE TABLE t (
         a INT4, b INT8, n NUMERIC, s TEXT, d DATE, v VARCHAR(5), c CHAR(3), p positive
     );";

fn schema() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

fn parse(sql: &str) -> Statement {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("statement parses").remove(0)
}

fn context_of<'q>(
    db: &'q ParserDB,
    statement: &'q Statement,
) -> ExpressionContext<'q, 'q, ParserDB> {
    ExpressionContext::for_statement(statement, &ExpressionScope::Empty, db, IdentifierCase::Folded)
        .expect("statement resolves")
}

/// The first output expression of the query `statement`.
fn projected(statement: &Statement) -> &Expr {
    let Statement::Query(query) = statement else { panic!("a query") };
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT") };
    let SelectItem::UnnamedExpr(expression) = &select.projection[0] else { panic!("an output") };
    expression
}

/// The `VALUES` list the first `FROM` item of the query `statement` reads.
fn derived_values(statement: &Statement) -> &Values {
    let Statement::Query(query) = statement else { panic!("a query") };
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT") };
    let TableFactor::Derived { subquery, .. } = &select.from[0].relation else {
        panic!("a derived table")
    };
    let SetExpr::Values(values) = subquery.body.as_ref() else { panic!("a VALUES list") };
    values
}

/// The common type and each input's steps, as `type: steps | steps`.
fn label(db: &ParserDB, application: &CommonTypeApplication<'_, '_, ParserDB>) -> String {
    match application.resolution() {
        CommonTypeResolution::Selected { common, inputs } => {
            let plans: Vec<String> =
                inputs.iter().map(|input| steps(db, input.conversion())).collect();
            format!("{}: {}", common.name(), plans.join(" | "))
        }
        CommonTypeResolution::Refused => "refused".to_owned(),
        CommonTypeResolution::Undetermined => "undetermined".to_owned(),
    }
}

/// The type a type answer names, with its modifier.
fn typed(answer: &OperandType<'_>) -> String {
    match answer {
        OperandType::Known { type_name, typmod: Some(typmod) } => {
            format!("{}({typmod})", type_name.name())
        }
        OperandType::Known { type_name, typmod: None } => type_name.name().to_owned(),
        OperandType::UntypedLiteral => "untyped".to_owned(),
        OperandType::Unknown => "unknown".to_owned(),
    }
}

/// What the first output of `SELECT {expression} FROM t` answers, its
/// common type label and its type.
fn met(db: &ParserDB, expression: &str) -> (String, String) {
    let statement = parse(&format!("SELECT {expression} FROM t"));
    let context = context_of(db, &statement);
    let node = projected(&statement);
    let application = context.common_type(node).expect("inputs type").expect("a common-type site");
    (label(db, &application), typed(&context.type_of(node).expect("the node types")))
}

/// The worked examples of the request.
#[test]
fn results_and_arguments_meet_at_their_common_type() {
    let db = schema();
    assert_eq!(
        met(&db, "CASE WHEN true THEN a ELSE n END"),
        ("numeric: function int4->numeric | ".to_owned(), "numeric".to_owned())
    );
    assert_eq!(met(&db, "COALESCE(s, 'x')"), ("text:  | input text".to_owned(), "text".to_owned()));
    assert_eq!(
        met(&db, "GREATEST(a, b)"),
        ("int8: function int4->int8 | ".to_owned(), "int8".to_owned())
    );
}

/// A `CASE` weighs its `ELSE` result first, so of `varchar` and `bpchar`,
/// which convert to each other implicitly, the `ELSE` one wins, and a
/// missing `ELSE` is a `NULL` meeting the others.
#[test]
fn a_case_weighs_its_else_first_and_supplies_a_missing_one() {
    let db = schema();
    assert_eq!(
        met(&db, "CASE WHEN true THEN v ELSE c END"),
        ("bpchar: relabel varchar->bpchar | ".to_owned(), "bpchar".to_owned())
    );
    assert_eq!(met(&db, "COALESCE(v, c)").1, "varchar");
    let statement = parse("SELECT CASE WHEN true THEN 'x' END FROM t");
    let context = context_of(&db, &statement);
    let application =
        context.common_type(projected(&statement)).expect("inputs type").expect("a CASE");
    assert_eq!(label(&db, &application), "text: input text | input text");
    assert!(matches!(
        application.inputs(),
        [CommonTypeInput::Expression(_), CommonTypeInput::ImplicitElse]
    ));
}

/// Values of no common type, and an empty array, are refused.
#[test]
fn values_without_a_common_type_are_refused() {
    let db = schema();
    assert_eq!(met(&db, "COALESCE(a, d)"), ("refused".to_owned(), "unknown".to_owned()));
    assert_eq!(met(&db, "ARRAY[]"), ("refused".to_owned(), "unknown".to_owned()));
    // Two values of one domain keep it, a domain and its base meet at the base.
    assert_eq!(met(&db, "COALESCE(p, p)"), ("positive:  | ".to_owned(), "positive".to_owned()));
    assert_eq!(met(&db, "COALESCE(p, a)"), ("int4: base int4 | ".to_owned(), "int4".to_owned()));
}

/// The elements of an array meet at its element type, arrays building a
/// multidimensional one meet as arrays, and a cast written over the array
/// converts them itself.
#[test]
fn array_elements_meet_unless_a_cast_types_them() {
    let db = schema();
    assert_eq!(
        met(&db, "ARRAY[a, 2.5]"),
        ("numeric: function int4->numeric | ".to_owned(), "_numeric".to_owned())
    );
    assert_eq!(
        met(&db, "ARRAY[ARRAY[a], ARRAY[b]]"),
        ("_int8: array[function int4->int8] | ".to_owned(), "_int8".to_owned())
    );
    let statement = parse("SELECT ARRAY[a, 2.5]::INT8[] FROM t");
    let context = context_of(&db, &statement);
    let Expr::Cast { expr: array, .. } = projected(&statement) else { panic!("a cast") };
    assert!(context.common_type(array).expect("a node of the query").is_none());
    assert_eq!(typed(&context.type_of(array).expect("the array types")), "_int8");
}

/// The result keeps the modifier every value carries unconverted.
#[test]
fn the_result_keeps_a_modifier_every_value_carries() {
    let db = schema();
    assert_eq!(met(&db, "COALESCE(v, v)").1, "varchar(9)");
    assert_eq!(met(&db, "CASE WHEN true THEN v ELSE v END").1, "varchar(9)");
    assert_eq!(met(&db, "ARRAY[v, v]").1, "_varchar(9)");
    assert_eq!(met(&db, "COALESCE(v, 'x')").1, "varchar");
    assert_eq!(met(&db, "CASE WHEN true THEN v END").1, "varchar");
}

/// Each column of a `VALUES` list meets across its rows, which types the
/// column, but the `VALUES` of an `INSERT` stores its cells one by one.
#[test]
fn values_columns_meet_across_rows() {
    let db = schema();
    let statement = parse("SELECT x, y FROM (VALUES (1, 'a'), (2.5, 'b')) AS v(x, y)");
    let context = context_of(&db, &statement);
    let columns = context
        .values_common_types(derived_values(&statement))
        .expect("cells type")
        .expect("a VALUES list");
    let labels: Vec<String> = columns.iter().map(|column| label(&db, column)).collect();
    assert_eq!(labels, ["numeric: function int4->numeric | ", "text: input text | input text"]);
    assert_eq!(typed(&context.type_of(projected(&statement)).expect("x types")), "numeric");
    // A column whose rows share no type takes none.
    let statement = parse("SELECT x FROM (VALUES (1), (DATE '2026-01-01')) AS v(x)");
    let context = context_of(&db, &statement);
    assert_eq!(typed(&context.type_of(projected(&statement)).expect("x types")), "unknown");

    let statement = parse("INSERT INTO t (a) VALUES (1), (2)");
    let context = context_of(&db, &statement);
    let Statement::Insert(insert) = &statement else { panic!("an INSERT") };
    let source = insert.source.as_deref().expect("a source");
    let SetExpr::Values(values) = source.body.as_ref() else { panic!("a VALUES list") };
    assert!(context.values_common_types(values).expect("a list of the query").is_none());

    // A `VALUES` with a clause of a query is one, its literal resolved as
    // `text`, which no assignment converts to `date`.
    let statement = parse("INSERT INTO t (d) VALUES ('2026-01-01') ORDER BY 1");
    let context = context_of(&db, &statement);
    let assignments =
        context.assignments(&statement).expect("values type").expect("the target is known");
    assert!(matches!(assignments[0].resolution(), AssignmentResolution::Refused));
}

/// Only the sites PostgreSQL resolves a common type at answer, a cast over
/// an array answering for the array's type whatever it names.
#[test]
fn only_common_type_sites_answer() {
    let db = schema();
    assert_eq!(met(&db, "(COALESCE(a, b))").1, "int8");
    for expression in ["a", "COALESCE()", "COALESCE(x => 1)"] {
        let statement = parse(&format!("SELECT {expression} FROM t"));
        let context = context_of(&db, &statement);
        assert!(context.common_type(projected(&statement)).expect("a node").is_none());
    }
    // `NULLIF` takes exactly two values, and no function `nullif` takes one.
    let statement = parse("SELECT NULLIF(a) FROM t");
    let context = context_of(&db, &statement);
    assert_eq!(typed(&context.type_of(projected(&statement)).expect("a node")), "unknown");
    let cast_array = |sql: &str| {
        let statement = parse(&format!("SELECT {sql} FROM t"));
        let context = context_of(&db, &statement);
        let Expr::Cast { expr: array, .. } = projected(&statement) else { panic!("a cast") };
        let application = context.common_type(array).expect("a node");
        (
            application.map(|application| label(&db, &application)),
            typed(&context.type_of(array).expect("the array types")),
        )
    };
    assert_eq!(cast_array("ARRAY[a, 2.5]::ints"), (None, "_int4".to_owned()));
    assert_eq!(
        cast_array("ARRAY[a, 2.5]::TEXT"),
        (Some("numeric: function int4->numeric | ".to_owned()), "_numeric".to_owned())
    );
    assert_eq!(
        cast_array("ARRAY[a]::no_such[]"),
        (Some("undetermined".to_owned()), "unknown".to_owned())
    );
}

/// Values PostgreSQL refuses to build into an array or to list as `VALUES`
/// are refused, and values whose types or facts are missing undetermined.
#[test]
fn missing_facts_leave_the_common_type_undetermined() {
    let db = schema();
    assert_eq!(met(&db, "ARRAY[NULL::void]").0, "refused");
    assert_eq!(met(&db, "COALESCE(no_such_function(1), a)").0, "undetermined");
    let extension = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a INT4, h hstore);")
        .expect("schema parses");
    assert_eq!(met(&extension, "ARRAY[h]"), ("undetermined".to_owned(), "unknown".to_owned()));
    assert_eq!(met(&extension, "COALESCE(h, a)").0, "undetermined");
    let statement = parse("SELECT * FROM (VALUES (1), (2, 3)) AS v");
    let context = context_of(&db, &statement);
    assert!(context.values_common_types(derived_values(&statement)).expect("a list").is_none());
}
