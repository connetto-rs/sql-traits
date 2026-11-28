//! Tests that a data-modifying statement answers each value it stores in a
//! column with the conversion PostgreSQL 18 applies under
//! `COERCION_ASSIGNMENT`.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{SetExpr, Statement},
    dialect::{PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

use crate::support::coercion_steps::steps;

const SCHEMA: &str = "CREATE DOMAIN positive AS INT4 CHECK (VALUE > 0);
     CREATE TABLE t (
         a INT4 PRIMARY KEY, b INT8, n NUMERIC, s TEXT, d DATE, v VARCHAR(5), p positive,
         tags VARCHAR(3)[], iv INTERVAL(0)
     );
     CREATE VIEW w AS SELECT a, b FROM t;";

fn schema() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

fn parse(sql: &str) -> Statement {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("statement parses").remove(0)
}

/// What `context` answers for each value `statement` stores, as `column:
/// type <- steps`, `column: refused` or `column: undetermined`.
fn labels(
    db: &ParserDB,
    context: &ExpressionContext<'_, '_, ParserDB>,
    statement: &Statement,
) -> Vec<String> {
    let assignments =
        context.assignments(statement).expect("values type").expect("the target is known");
    assignments
        .iter()
        .map(|assignment| {
            let (column, _) = assignment.column();
            match assignment.resolution() {
                AssignmentResolution::Converted(evidence) => {
                    let plan = steps(db, evidence.conversion());
                    format!("{column}: {} <- {plan}", evidence.target().name())
                }
                AssignmentResolution::Refused => format!("{column}: refused"),
                AssignmentResolution::Undetermined => format!("{column}: undetermined"),
            }
        })
        .collect()
}

/// What each value of the statement `sql` stores answers.
fn stored(db: &ParserDB, sql: &str) -> Vec<String> {
    let statement = parse(sql);
    let context = ExpressionContext::for_statement(
        &statement,
        &ExpressionScope::Empty,
        db,
        IdentifierCase::Folded,
    )
    .expect("statement resolves");
    labels(db, &context, &statement)
}

/// The worked examples of the request, as PostgreSQL 18 plans them.
#[test]
fn a_stored_value_converts_to_its_columns_type() {
    let db = schema();
    assert_eq!(stored(&db, "UPDATE t SET a = b"), ["a: int4 <- function int8->int4"]);
    assert_eq!(stored(&db, "UPDATE t SET a = 5"), ["a: int4 <- "]);
    assert_eq!(stored(&db, "INSERT INTO t (d) VALUES ('2026-01-01')"), ["d: date <- input date"]);
}

/// Assignment applies the assignment casts an operand never takes, reaches a
/// string type through text where no cast does, and refuses the rest.
#[test]
fn assignment_casts_and_text_conversion_apply_where_explicit_casts_do_not() {
    let db = schema();
    assert_eq!(
        stored(&db, "INSERT INTO t (a) VALUES (2.5)"),
        ["a: int4 <- function numeric->int4"]
    );
    assert_eq!(stored(&db, "INSERT INTO t (s) VALUES (1)"), ["s: text <- text io text"]);
    // `bool` reaches `int4` only by an explicit cast.
    assert_eq!(stored(&db, "INSERT INTO t (a) VALUES (true)"), ["a: refused"]);
}

/// A column's modifier adds the length coercion of its type, element by
/// element for an array, unless the value already carries it, and a domain
/// column checks the domain.
#[test]
fn the_columns_modifier_and_domain_end_the_conversion() {
    let db = schema();
    assert_eq!(
        stored(&db, "UPDATE t SET v = s"),
        ["v: varchar <- relabel text->varchar,length varchar->varchar 9"]
    );
    assert_eq!(stored(&db, "UPDATE t SET v = v"), ["v: varchar <- "]);
    assert_eq!(
        stored(&db, "INSERT INTO t (v) VALUES ('abc')"),
        ["v: varchar <- input varchar,length varchar->varchar 9"]
    );
    assert_eq!(
        stored(&db, "INSERT INTO t (tags) VALUES ('{a}')"),
        ["tags: _varchar <- input _varchar,array[length varchar->varchar 7]"]
    );
    assert_eq!(stored(&db, "INSERT INTO t (p) VALUES (5)"), ["p: positive <- check positive"]);
    // The input function of `interval` reads a literal under the column's
    // modifier, which a converted value then lacks.
    assert_eq!(
        stored(&db, "INSERT INTO t (iv) VALUES ('1 day')"),
        ["iv: interval <- input interval"]
    );
    assert_eq!(
        stored(&db, "UPDATE t SET iv = INTERVAL '1 day'"),
        ["iv: interval <- length interval->interval 2147418112"]
    );
}

/// Every row of an `INSERT`'s `VALUES` stores its own cells, the relation's
/// columns in order when the statement lists none, and `DEFAULT` stores
/// nothing a value stands for.
#[test]
fn each_inserted_row_stores_its_own_cells() {
    let db = schema();
    assert_eq!(
        stored(&db, "INSERT INTO t (n) VALUES (1), (2.5)"),
        ["n: numeric <- function int4->numeric", "n: numeric <- "]
    );
    assert_eq!(
        stored(&db, "INSERT INTO t VALUES (DEFAULT, 1)"),
        ["b: int8 <- function int4->int8"]
    );
}

/// An `INSERT ... SELECT` stores each output of its query, an untyped literal
/// read by the column's input function.
#[test]
fn an_inserted_query_stores_its_outputs() {
    let db = schema();
    let statement = parse("INSERT INTO t (d, a) SELECT '2026-01-01', b FROM t");
    let context = ExpressionContext::for_statement(
        &statement,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::Folded,
    )
    .expect("statement resolves");
    assert_eq!(
        labels(&db, &context, &statement),
        ["d: date <- input date", "a: int4 <- function int8->int4"]
    );
    let assignments = context.assignments(&statement).expect("values type").expect("known");
    let positions: Vec<usize> = assignments
        .iter()
        .map(|assignment| {
            match assignment.value() {
                AssignedValue::Output { position, .. } => position,
                AssignedValue::Expression(_) => panic!("an output"),
            }
        })
        .collect();
    assert_eq!(positions, [0, 1]);
    assert_eq!(assignments[0].value_type(), &OperandType::UntypedLiteral);
}

/// `ON CONFLICT DO UPDATE`, multi-column `SET` items and both actions of a
/// `MERGE` store their values like an `UPDATE` and an `INSERT`.
#[test]
fn conflict_merge_and_multi_column_set_store_their_values() {
    let db = schema();
    assert_eq!(
        stored(&db, "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET b = excluded.a"),
        ["a: int4 <- ", "b: int8 <- function int4->int8"]
    );
    let both = ["a: int4 <- function int8->int4", "n: numeric <- function int4->numeric"];
    assert_eq!(stored(&db, "UPDATE t SET (a, n) = (SELECT b, a FROM t)"), both);
    assert_eq!(stored(&db, "UPDATE t SET (a, n) = ROW(b, a)"), both);
    assert_eq!(
        stored(
            &db,
            "MERGE INTO t USING t AS src ON t.a = src.a
             WHEN MATCHED THEN UPDATE SET b = src.a
             WHEN NOT MATCHED THEN INSERT (a, n) VALUES (src.b, src.a)",
        ),
        [
            "b: int8 <- function int4->int8",
            "a: int4 <- function int8->int4",
            "n: numeric <- function int4->numeric"
        ]
    );
}

/// A plain view stores values in its own columns, and a data-modifying CTE
/// answers like a statement of its own.
#[test]
fn views_and_data_modifying_ctes_store_their_values() {
    let db = schema();
    assert_eq!(stored(&db, "UPDATE w SET a = b"), ["a: int4 <- function int8->int4"]);
    let statement = parse("WITH x AS (UPDATE t SET a = b RETURNING a) SELECT * FROM x");
    let context = ExpressionContext::for_statement(
        &statement,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::Folded,
    )
    .expect("statement resolves");
    let Statement::Query(query) = &statement else { panic!("a query") };
    let with = query.with.as_ref().expect("a WITH");
    let SetExpr::Update(update) = with.cte_tables[0].query.body.as_ref() else {
        panic!("an UPDATE")
    };
    assert_eq!(labels(&db, &context, update), ["a: int4 <- function int8->int4"]);
    // A statement of another query is refused.
    assert!(matches!(
        context.assignments(&parse("UPDATE t SET a = b")),
        Err(LookupError::ExpressionNotInQuery { .. })
    ));
}

/// Each shape a statement writes values in answers its values, and a shape
/// storing none, a `DELETE` included, answers none.
#[test]
fn every_written_shape_answers_its_values() {
    let db = schema();
    let both = ["a: int4 <- function int8->int4", "n: numeric <- function int4->numeric"];
    assert_eq!(stored(&db, "UPDATE t SET (a, n) = (b, a)"), both);
    let none: [String; 0] = [];
    assert_eq!(stored(&db, "UPDATE t SET (a, n) = ROW(t.*)"), none);
    assert_eq!(stored(&db, "UPDATE t SET (a, n) = b"), none);
    assert_eq!(stored(&db, "INSERT INTO t DEFAULT VALUES"), none);
    assert_eq!(
        stored(
            &db,
            "MERGE INTO t USING t AS src ON t.a = src.a
             WHEN MATCHED AND t.b > 0 THEN DELETE
             WHEN MATCHED THEN UPDATE SET b = src.a",
        ),
        ["b: int8 <- function int4->int8"]
    );
    let statement = parse("DELETE FROM t");
    let context = ExpressionContext::for_statement(
        &statement,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::Folded,
    )
    .expect("statement resolves");
    assert!(context.assignments(&statement).expect("nothing to type").is_none());
}

/// A value stored in a field of a column, or of a type or into a type the
/// facts do not settle, is undetermined, and a type no cast reaches outside
/// the string types is refused.
#[test]
fn missing_facts_leave_the_assignment_undetermined() {
    let db = schema();
    let statement = parse("UPDATE t SET v = s, a.f = 1");
    let context = ExpressionContext::for_statement(
        &statement,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::Folded,
    )
    .expect("statement resolves");
    let assignments = context.assignments(&statement).expect("values type").expect("known");
    let varchar = TargetName::new("varchar", false).with_schema("pg_catalog", false);
    assert_eq!(
        assignments[0].column_type(),
        &OperandType::Known { type_name: varchar, typmod: Some(9) }
    );
    assert_eq!(assignments[1].column_type(), &OperandType::Unknown);
    assert!(matches!(assignments[1].resolution(), AssignmentResolution::Undetermined));
    assert_eq!(stored(&db, "UPDATE t SET a = no_such_function(1)"), ["a: undetermined"]);
    assert_eq!(stored(&db, "UPDATE t SET a = d"), ["a: refused"]);

    let extension = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE e (a INT4, h hstore[]);")
        .expect("schema parses");
    assert_eq!(stored(&extension, "UPDATE e SET h = a"), ["h: undetermined"]);

    // A database answering no casts proves no length coercion.
    let sqlite =
        ParserDB::parse::<SQLiteDialect>("CREATE TABLE t (v VARCHAR(5));").expect("schema parses");
    assert_eq!(stored(&sqlite, "UPDATE t SET v = 'x'"), ["v: undetermined"]);
}
