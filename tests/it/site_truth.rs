//! Truth read where a query, a function body or a PL/pgSQL statement
//! evaluates an expression, and over a domain's `VALUE`.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "Tests require parsed fixtures of a known shape."
)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::{
    ast::{
        BinaryOperator, DataType, Distinct, Expr, JoinConstraint, JoinOperator, OrderByKind, Query,
        Select, SelectItem, SetExpr, Statement,
    },
    dialect::{PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

use crate::support::query_parsing::query_with;

const SCHEMA: &str = "CREATE TABLE t (x INT NOT NULL, y INT);
     CREATE TABLE u (x INT NOT NULL, z INT);
     CREATE DOMAIN d AS INT CHECK (VALUE > 5 AND VALUE > 3);";

const UNDECIDED: Outcomes = Outcomes { truth: true, falsity: true, null: false };

const fn never_raising(outcomes: Outcomes) -> ExpressionTruth {
    ExpressionTruth { outcomes, raising: Raising::Never }
}

fn expression(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

/// The two sides of the `AND` the `WHERE` or, without one, the `HAVING` of
/// the outer `SELECT` of `query` writes, or the whole condition twice.
fn sides(select: &sqlparser::ast::Select) -> (&Expr, &Expr) {
    let condition = select.selection.as_ref().or(select.having.as_ref()).unwrap();
    match condition {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => (left, right),
        condition => (condition, condition),
    }
}

/// The truth of the right side of the `AND` in the condition of `sql`, read
/// with the left side assumed true, or of the whole condition with nothing
/// assumed when it is no `AND`.
fn query_truth(schema: &str, sql: &str) -> Result<ExpressionTruth, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(schema)?;
    let query = query_with(&PostgreSqlDialect {}, sql);
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT body") };
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    let (assumed, term) = sides(select);
    let assumptions = if core::ptr::eq(assumed, term) {
        Vec::new()
    } else {
        vec![Assumption::Expression { expression: assumed, outcomes: Outcomes::TRUE }]
    };
    Ok(context.truth_under(term, &assumptions)?)
}

#[test]
fn an_alias_reads_the_column_it_qualifies() -> Result<(), Error> {
    for sql in [
        "SELECT 1 FROM t AS a WHERE a.x > 5 AND a.x > 3",
        "SELECT 1 FROM t AS a WHERE x > 5 AND a.x > 3",
        "SELECT 1 FROM t AS a WHERE a.x > 5 AND x > 3",
        "SELECT 1 FROM t WHERE t.x > 5 AND x > 3",
        "SELECT 1 FROM t AS a JOIN u ON a.y = u.z WHERE a.x > 5 AND a.x > 3",
    ] {
        assert_eq!(query_truth(SCHEMA, sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    Ok(())
}

#[test]
fn columns_of_other_readings_are_other_values() -> Result<(), Error> {
    for sql in [
        "SELECT 1 FROM t, u WHERE u.x > 5 AND t.x > 3",
        "SELECT 1 FROM t AS a, t AS b WHERE a.x > 5 AND b.x > 3",
    ] {
        assert_eq!(query_truth(SCHEMA, sql)?, never_raising(UNDECIDED), "{sql}");
    }
    Ok(())
}

#[test]
fn assumptions_admitting_no_row_leave_no_outcome() -> Result<(), Error> {
    let sql = "SELECT 1 FROM t AS a WHERE (a.x > 5 AND x < 3) AND a.y = 1";
    assert_eq!(query_truth(SCHEMA, sql)?, never_raising(Outcomes::NONE));
    Ok(())
}

#[test]
fn not_null_holds_on_rows_no_outer_join_extends() -> Result<(), Error> {
    assert_eq!(
        query_truth(SCHEMA, "SELECT 1 FROM t AS a WHERE a.x IS NOT NULL")?,
        never_raising(Outcomes::TRUE)
    );
    for sql in [
        "SELECT 1 FROM u LEFT JOIN t AS a ON true WHERE a.x IS NOT NULL",
        "SELECT 1 FROM t AS a RIGHT JOIN u ON true WHERE a.x IS NOT NULL",
        "SELECT 1 FROM u LEFT JOIN (t AS a JOIN t AS b ON true) ON true WHERE a.x IS NOT NULL",
    ] {
        assert_eq!(query_truth(SCHEMA, sql)?, never_raising(UNDECIDED), "{sql}");
    }
    Ok(())
}

/// The truth of the `ON` of the last join `sql` writes in its `FROM`, with
/// nothing assumed.
fn on_truth(schema: &str, sql: &str) -> Result<ExpressionTruth, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(schema)?;
    let query = query_with(&PostgreSqlDialect {}, sql);
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT body") };
    let join = select.from[0].joins.last().unwrap();
    let (JoinOperator::Join(JoinConstraint::On(on))
    | JoinOperator::Inner(JoinConstraint::On(on))
    | JoinOperator::Left(JoinConstraint::On(on))
    | JoinOperator::Right(JoinConstraint::On(on))) = &join.join_operator
    else {
        panic!("a join with an ON")
    };
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    Ok(context.truth_under(on, &[])?)
}

#[test]
fn a_join_condition_reads_its_sides_before_later_joins_extend_them() -> Result<(), Error> {
    for sql in [
        "SELECT 1 FROM u LEFT JOIN t AS a ON a.x IS NOT NULL",
        "SELECT 1 FROM t AS a RIGHT JOIN u ON a.x IS NOT NULL",
        "SELECT 1 FROM u JOIN t AS a ON true LEFT JOIN t AS b ON a.x IS NOT NULL",
    ] {
        assert_eq!(on_truth(SCHEMA, sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    for sql in [
        "SELECT 1 FROM u LEFT JOIN t AS a ON true LEFT JOIN t AS b ON a.x IS NOT NULL",
        "SELECT 1 FROM t AS a RIGHT JOIN u ON true JOIN t AS b ON a.x IS NOT NULL",
        "SELECT 1 FROM (u LEFT JOIN t AS a ON true) JOIN t AS b ON a.x IS NOT NULL",
    ] {
        assert_eq!(on_truth(SCHEMA, sql)?, never_raising(UNDECIDED), "{sql}");
    }
    Ok(())
}

#[test]
fn table_checks_hold_over_each_reading() -> Result<(), Error> {
    let schema = "CREATE TABLE t (x INT NOT NULL, CHECK (x > 5));
                  CREATE TABLE u (x INT NOT NULL, z INT);";
    for sql in [
        "SELECT * FROM t AS a WHERE x > 3",
        "SELECT * FROM t AS a WHERE a.x > 3",
        "SELECT * FROM t AS a JOIN t AS b ON true WHERE b.x > 3",
        "SELECT x FROM t GROUP BY x HAVING x > 3",
    ] {
        assert_eq!(query_truth(schema, sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    assert_eq!(
        on_truth(schema, "SELECT * FROM u LEFT JOIN t AS a ON a.x > 3")?,
        never_raising(Outcomes::TRUE)
    );
    for sql in [
        "SELECT * FROM u LEFT JOIN t AS a ON true WHERE a.x > 3",
        "SELECT x FROM t GROUP BY ROLLUP (x) HAVING x > 3",
    ] {
        let truth = query_truth(schema, sql)?;
        assert_ne!(truth.outcomes, Outcomes::TRUE, "{sql}");
        assert!(truth.outcomes.falsity || truth.outcomes.null, "{sql}");
    }
    Ok(())
}

#[test]
fn a_domain_column_carries_its_checks() -> Result<(), Error> {
    let schema = format!("{SCHEMA} CREATE TABLE w (v d);");
    assert_eq!(
        query_truth(&schema, "SELECT 1 FROM w AS a WHERE a.v > 3")?,
        never_raising(Outcomes::NOT_FALSE)
    );
    Ok(())
}

#[test]
fn a_parameter_is_one_value_of_its_type() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION f(p INT) RETURNS BOOL LANGUAGE sql RETURN true;",
    )?;
    let function = db.functions().find(|function| function.name() == "f").unwrap();
    for sql in [
        "SELECT 1 WHERE p > 5 AND f.p > 3",
        "SELECT 1 WHERE $1 > 5 AND p > 3",
        "SELECT 1 WHERE f.p > 5 AND $1 > 3",
    ] {
        let body = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0);
        let context = ExpressionContext::for_statement(
            &body,
            &ExpressionScope::FunctionBody(function),
            &db,
            IdentifierCase::Folded,
        )?;
        let (assumed, term) = sides(select_of(query_of(&body)));
        let assumption = Assumption::Expression { expression: assumed, outcomes: Outcomes::TRUE };
        assert_eq!(
            context.truth_under(term, &[assumption])?,
            never_raising(Outcomes::TRUE),
            "{sql}"
        );
    }
    Ok(())
}

/// Inlining or a custom plan binds a parameter of a SQL function as a
/// constant, which PostgreSQL folds a strict operator over to `NULL` before
/// the operand dividing by zero runs, while a column is read on each row.
#[test]
fn a_comparison_over_a_parameter_may_fold_before_raising() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE FUNCTION f(p INT) RETURNS BIGINT LANGUAGE sql RETURN 1;"
    ))?;
    let function = db.functions().find(|function| function.name() == "f").unwrap();
    for (operand, raising) in
        [("p", Raising::Sometimes), ("$1", Raising::Sometimes), ("y", Raising::Always)]
    {
        let sql = format!("SELECT count(*) FROM t WHERE x / 0 > {operand}");
        let body = Parser::parse_sql(&PostgreSqlDialect {}, &sql)?.remove(0);
        let context = ExpressionContext::for_statement(
            &body,
            &ExpressionScope::FunctionBody(function),
            &db,
            IdentifierCase::Folded,
        )?;
        let condition = select_of(query_of(&body)).selection.as_ref().unwrap();
        assert_eq!(context.truth_under(condition, &[])?.raising, raising, "{sql}");
    }
    Ok(())
}

#[test]
fn a_variable_declared_not_null_is_never_null() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION run() RETURNS void LANGUAGE plpgsql AS $$
         DECLARE v int NOT NULL := 6; n int; s text;
         BEGIN
          s := CASE WHEN v IS NOT NULL AND n IS NOT NULL THEN 'SELECT 1' END;
          EXECUTE s;
         END $$;",
    )?;
    let run =
        db.function_by_target(TargetName::new("run", false), IdentifierCase::Folded)?.unwrap();
    let body = run.dynamic_execution_sites(&db)?;
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded)?;
    let Reaching::Definitions(definitions) = reads[0].reaching() else { panic!("s is reached") };
    let DefinitionSource::Value(value) = definitions[0].source() else { panic!("a value") };
    let value: &Expr = value;
    let scope = definitions[0].scope().unwrap();
    let scope = ExpressionScope::PlPgSql(&scope);
    let context = ExpressionContext::for_expression(value, &scope, &db, IdentifierCase::Folded)?;
    let Expr::Case { conditions, .. } = value else { panic!("a CASE") };
    let Expr::BinaryOp { left, right, .. } = &conditions[0].condition else { panic!("an AND") };
    assert_eq!(context.truth_under(left, &[])?, never_raising(Outcomes::TRUE));
    assert_eq!(context.truth_under(right, &[])?, never_raising(UNDECIDED));
    Ok(())
}

#[test]
fn a_domain_value_carries_the_checks_of_the_domains_below_it() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{SCHEMA} CREATE DOMAIN e AS d CHECK (VALUE < 9);"
    ))?;
    let domain = |name: &str| db.domains().find(|domain| domain.name() == name).unwrap();
    let assumed = expression("VALUE > 5");
    let assumption = Assumption::Expression { expression: &assumed, outcomes: Outcomes::TRUE };
    let term = expression("VALUE > 3");
    assert_eq!(
        domain("d").truth_under(&db, &term, IdentifierCase::Folded, &[assumption])?,
        never_raising(Outcomes::TRUE)
    );
    assert_eq!(
        domain("d").truth_under(&db, &term, IdentifierCase::Folded, &[])?,
        never_raising(Outcomes::ANY),
        "the domain's own checks are what a caller reads"
    );
    assert_eq!(
        domain("e").truth_under(&db, &term, IdentifierCase::Folded, &[])?,
        never_raising(Outcomes::NOT_FALSE)
    );
    Ok(())
}

/// The `SELECT` body of `query`.
fn select_of(query: &Query) -> &Select {
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT body") };
    select
}

/// The expression the select item `item` writes.
fn item_expression(item: &SelectItem) -> &Expr {
    let SelectItem::UnnamedExpr(expression) = item else { panic!("an expression item") };
    expression
}

/// Selects a term of a statement and, when any, the expression assumed true
/// beside it.
type Pick = for<'s> fn(&'s Statement) -> (&'s Expr, Option<&'s Expr>);

/// The term `pick` selects in `sql`, a statement read with nothing in scope,
/// with the expression `pick` selects beside it, when any, assumed true.
fn statement_truth(schema: &str, sql: &str, pick: Pick) -> Result<ExpressionTruth, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(schema)?;
    let statement = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0);
    let context = ExpressionContext::for_statement(
        &statement,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::Folded,
    )?;
    let (term, assumed) = pick(&statement);
    let assumptions: Vec<_> = assumed
        .map(|expression| Assumption::Expression { expression, outcomes: Outcomes::TRUE })
        .into_iter()
        .collect();
    Ok(context.truth_under(term, &assumptions)?)
}

/// The query `statement` is.
fn query_of(statement: &Statement) -> &Query {
    let Statement::Query(query) = statement else { panic!("a query") };
    query
}

/// The `WHERE` of the subquery `EXISTS` writes in `expression`.
fn exists_condition(expression: &Expr) -> &Expr {
    let Expr::Exists { subquery, .. } = expression else { panic!("an EXISTS") };
    select_of(subquery).selection.as_ref().unwrap()
}

#[test]
fn grouping_set_rows_reach_nested_references() -> Result<(), Error> {
    let schema = "CREATE TABLE t (x INT NOT NULL);";
    let built: [(&str, Pick); 3] = [
        (
            "SELECT x FROM t GROUP BY ROLLUP (x) HAVING EXISTS (SELECT 1 WHERE t.x IS NULL)",
            |statement| {
                (exists_condition(select_of(query_of(statement)).having.as_ref().unwrap()), None)
            },
        ),
        ("SELECT x, (SELECT t.x IS NULL) FROM t GROUP BY ROLLUP (x)", |statement| {
            let item = item_expression(&select_of(query_of(statement)).projection[1]);
            let Expr::Subquery(subquery) = item else { panic!("a subquery") };
            (item_expression(&select_of(subquery).projection[0]), None)
        }),
        ("SELECT DISTINCT ON (t.x IS NULL) x FROM t GROUP BY ROLLUP (x)", |statement| {
            let Some(Distinct::On(keys)) = &select_of(query_of(statement)).distinct else {
                panic!("DISTINCT ON")
            };
            (&keys[0], None)
        }),
    ];
    for (sql, pick) in built {
        assert!(statement_truth(schema, sql, pick)?.outcomes.truth, "{sql}");
    }
    let sql = "SELECT x FROM t WHERE EXISTS (SELECT 1 WHERE t.x IS NULL) GROUP BY ROLLUP (x)";
    let truth = statement_truth(schema, sql, |statement| {
        (exists_condition(select_of(query_of(statement)).selection.as_ref().unwrap()), None)
    })?;
    assert_eq!(truth, never_raising(Outcomes::FALSE));
    Ok(())
}

#[test]
fn a_returning_list_reads_the_written_row() -> Result<(), Error> {
    let schema = "CREATE TABLE t (x INT NOT NULL);";
    for sql in [
        "UPDATE t SET x = x + 10 WHERE x < 5 RETURNING x < 5",
        "UPDATE t SET x = x + 10 WHERE x < 5 RETURNING t.x < 5",
    ] {
        let truth = statement_truth(schema, sql, |statement| {
            let Statement::Update(update) = statement else { panic!("an UPDATE") };
            let returning = update.returning.as_ref().unwrap();
            (item_expression(&returning[0]), update.selection.as_ref())
        })?;
        assert!(truth.outcomes.falsity, "{sql}");
    }
    let truth = statement_truth(schema, "DELETE FROM t RETURNING new.x IS NULL", |statement| {
        let Statement::Delete(delete) = statement else { panic!("a DELETE") };
        (item_expression(&delete.returning.as_ref().unwrap()[0]), None)
    })?;
    assert!(truth.outcomes.truth);
    Ok(())
}

#[test]
fn a_trigger_row_holds_no_constraint_yet() -> Result<(), Error> {
    let db =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (x INT NOT NULL CHECK (x > 0));")?;
    let table = db.table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?.unwrap();
    for (scope, sql, outcomes) in [
        (ExpressionScope::Trigger(table), "NEW.x IS NULL OR NEW.x <= 0", UNDECIDED),
        (ExpressionScope::Table(table), "x IS NULL OR x <= 0", Outcomes::FALSE),
    ] {
        let term = expression(sql);
        let context =
            ExpressionContext::for_expression(&term, &scope, &db, IdentifierCase::Folded)?;
        assert_eq!(context.truth_under(&term, &[])?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn a_domain_check_holds_only_on_values_not_null() -> Result<(), Error> {
    let schema = "CREATE DOMAIN nn AS INT CHECK (VALUE IS NOT NULL);
                  CREATE TABLE t (id INT);
                  CREATE TABLE w (id INT, v nn);";
    let sql = "SELECT 1 FROM t LEFT JOIN w ON t.id = w.id WHERE w.v IS NULL";
    assert!(query_truth(schema, sql)?.outcomes.truth);
    Ok(())
}

const FUNCTIONS: &str = "CREATE FUNCTION h(i INT) RETURNS INT LANGUAGE sql IMMUTABLE RETURN i;
     CREATE FUNCTION h(i BIGINT) RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;";

#[test]
fn a_call_at_a_site_runs_the_functions_it_resolves_to() -> Result<(), Error> {
    let schema = format!("{SCHEMA} {FUNCTIONS}");
    for sql in [
        "SELECT 1 FROM t AS a WHERE abs(a.x) IS NOT NULL",
        "SELECT 1 FROM t AS a WHERE h(NULL) > 5 AND h(NULL) > 3",
    ] {
        assert_eq!(query_truth(&schema, sql)?.outcomes, Outcomes::TRUE, "{sql}");
    }
    let sql = "SELECT 1 FROM t AS a WHERE nowhere(a.x) IS NOT NULL";
    assert_ne!(query_truth(&schema, sql)?.outcomes, Outcomes::TRUE);
    Ok(())
}

#[test]
fn a_subquery_at_a_site_leaves_the_rest_read() -> Result<(), Error> {
    let sql = "SELECT 1 FROM t AS a
               WHERE a.x > 5 AND (a.x > 3 OR EXISTS (SELECT 1 FROM u WHERE u.x = a.x))";
    assert_eq!(query_truth(SCHEMA, sql)?.outcomes, Outcomes::TRUE);
    Ok(())
}

#[test]
fn a_column_of_a_derived_relation_is_one_value() -> Result<(), Error> {
    for sql in [
        "SELECT 1 FROM (SELECT x FROM t) AS s WHERE s.x > 5 AND s.x > 3",
        "SELECT 1 FROM (SELECT x + 1 AS y FROM t) AS s WHERE s.y > 5 AND y > 3",
        "SELECT 1 FROM t JOIN u USING (x) WHERE x > 5 AND x > 3",
        "SELECT 1 FROM nowhere AS n WHERE n.q > 5 AND n.q > 3",
    ] {
        assert_eq!(query_truth(SCHEMA, sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    let sql = "SELECT 1 FROM (SELECT x FROM t) AS s, (SELECT x FROM t) AS r
               WHERE s.x > 5 AND r.x > 3";
    assert_eq!(query_truth(SCHEMA, sql)?, never_raising(Outcomes::ANY));
    Ok(())
}

#[test]
fn an_assumption_read_through_a_copy_narrows_the_site() -> Result<(), Error> {
    let schema = format!("{SCHEMA} {FUNCTIONS}");
    let sql = "SELECT 1 FROM t AS a WHERE a.x BETWEEN 6 AND 9 AND a.x > 3";
    assert_eq!(query_truth(&schema, sql)?, never_raising(Outcomes::TRUE));
    // `h` runs a body the read does not hold, which may raise.
    let sql = "SELECT 1 FROM t AS a WHERE h(NULL) BETWEEN 6 AND 9 AND h(NULL) > 3";
    let truth = query_truth(&schema, sql)?;
    assert_eq!(truth.outcomes, Outcomes::TRUE);
    assert_eq!(truth.raising, Raising::Sometimes);
    Ok(())
}

#[test]
fn an_expression_outside_the_site_is_refused() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let query = query_with(&PostgreSqlDialect {}, "SELECT 1 FROM t AS a WHERE a.x > 3");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    assert!(matches!(
        context.truth_under(&expression("a.x > 3"), &[]),
        Err(LookupError::ExpressionNotInQuery { .. })
    ));
    Ok(())
}

#[test]
fn sorting_and_grouping_keys_are_read_at_their_site() -> Result<(), Error> {
    let sql = "SELECT x FROM t ORDER BY x > 5 AND x > 3";
    let truth = statement_truth(SCHEMA, sql, |statement| {
        let Some(OrderByKind::Expressions(keys)) =
            query_of(statement).order_by.as_ref().map(|order| &order.kind)
        else {
            panic!("an ORDER BY")
        };
        let (assumed, term) = sides_of(&keys[0].expr);
        (term, Some(assumed))
    })?;
    assert_eq!(truth, never_raising(Outcomes::TRUE));
    let schema = "CREATE TABLE b (flag BOOL NOT NULL);";
    let sql = "SELECT flag FROM b GROUP BY ROLLUP (flag) ORDER BY flag";
    let truth = statement_truth(schema, sql, |statement| {
        let Some(OrderByKind::Expressions(keys)) =
            query_of(statement).order_by.as_ref().map(|order| &order.kind)
        else {
            panic!("an ORDER BY")
        };
        (&keys[0].expr, None)
    })?;
    assert!(truth.outcomes.null, "the grand total row sorts a NULL flag");
    let sql = "SELECT x FROM t GROUP BY ALL HAVING x > 5 AND x > 3";
    assert_eq!(query_truth(SCHEMA, sql)?, never_raising(Outcomes::TRUE));
    Ok(())
}

/// The two sides of the `AND` `expression` writes.
fn sides_of(expression: &Expr) -> (&Expr, &Expr) {
    let Expr::BinaryOp { left, op: BinaryOperator::And, right } = expression else {
        panic!("an AND")
    };
    (left, right)
}

#[test]
fn a_placeholder_outside_a_function_body_is_one_value() -> Result<(), Error> {
    let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE t (x INT);")?;
    let query =
        query_with(&SQLiteDialect {}, "SELECT 1 FROM t WHERE ?1 IS NULL AND ?1 IS NOT NULL");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    let (assumed, term) = sides(select_of(&query));
    let assumption = Assumption::Expression { expression: assumed, outcomes: Outcomes::TRUE };
    assert_eq!(context.truth_under(term, &[assumption])?, never_raising(Outcomes::FALSE));
    Ok(())
}

#[test]
fn a_domain_value_at_a_site_is_one_value_of_the_base_type() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let condition = expression("VALUE > 5 AND VALUE > 3");
    let (assumed, term) = sides_of(&condition);
    let scope = ExpressionScope::DomainValue(&DataType::Int(None));
    let context =
        ExpressionContext::for_expression(&condition, &scope, &db, IdentifierCase::Folded)?;
    let assumption = Assumption::Expression { expression: assumed, outcomes: Outcomes::TRUE };
    assert_eq!(context.truth_under(term, &[assumption])?, never_raising(Outcomes::TRUE));
    Ok(())
}

#[test]
fn a_domain_of_another_catalog_is_refused() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let other = ParserDB::parse::<PostgreSqlDialect>("CREATE DOMAIN o AS INT;")?;
    let domain = other.domains().next().unwrap();
    assert!(
        domain.truth_under(&db, &expression("VALUE > 3"), IdentifierCase::Folded, &[]).is_err()
    );
    Ok(())
}

#[test]
fn a_variable_field_and_a_positional_variable_are_one_value_each() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (x INT NOT NULL, y INT);
         CREATE FUNCTION run(p INT) RETURNS void LANGUAGE plpgsql AS $$
         DECLARE r t; s text;
         BEGIN
          s := CASE WHEN r.y > 5 AND r.y > 3 AND $1 > 5 AND p > 3 THEN 'SELECT 1' END;
          EXECUTE s;
         END $$;",
    )?;
    let run =
        db.function_by_target(TargetName::new("run", false), IdentifierCase::Folded)?.unwrap();
    let body = run.dynamic_execution_sites(&db)?;
    let sites = body.sites().unwrap();
    let reads = sites[0].reads(IdentifierCase::Folded)?;
    let Reaching::Definitions(definitions) = reads[0].reaching() else { panic!("s is reached") };
    let DefinitionSource::Value(value) = definitions[0].source() else { panic!("a value") };
    let value: &Expr = value;
    let scope = definitions[0].scope().unwrap();
    let scope = ExpressionScope::PlPgSql(&scope);
    let context = ExpressionContext::for_expression(value, &scope, &db, IdentifierCase::Folded)?;
    let Expr::Case { conditions, .. } = value else { panic!("a CASE") };
    let (rest, p_term) = sides_of(&conditions[0].condition);
    let (rest, p_assumed) = sides_of(rest);
    let (r_assumed, r_term) = sides_of(rest);
    for (assumed, term) in [(r_assumed, r_term), (p_assumed, p_term)] {
        let assumption = Assumption::Expression { expression: assumed, outcomes: Outcomes::TRUE };
        assert_eq!(context.truth_under(term, &[assumption])?, never_raising(Outcomes::TRUE));
    }
    Ok(())
}

#[test]
fn a_name_no_scope_resolves_is_one_untyped_value() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let condition = expression("q IS NULL AND q IS NOT NULL");
    let (assumed, term) = sides_of(&condition);
    let context = ExpressionContext::for_expression(
        &condition,
        &ExpressionScope::Empty,
        &db,
        IdentifierCase::Folded,
    )?;
    let assumption = Assumption::Expression { expression: assumed, outcomes: Outcomes::TRUE };
    assert_eq!(context.truth_under(term, &[assumption])?, never_raising(Outcomes::FALSE));
    Ok(())
}

#[test]
fn a_reference_the_site_cannot_bind_is_an_error() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let query = query_with(&PostgreSqlDialect {}, "SELECT 1 FROM t, u WHERE x > 3");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    assert!(context.truth_under(select_of(&query).selection.as_ref().unwrap(), &[]).is_err());
    Ok(())
}

#[test]
fn a_check_assumed_holds_only_on_the_table_it_constrains() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (x INT NOT NULL);
         CREATE TABLE u (x INT NOT NULL, CHECK (x > 5));
         ALTER TABLE t ADD CONSTRAINT big CHECK (x > 5) NOT VALID;",
    )?;
    let check = |table: &str| {
        let table =
            db.table_by_target(TargetName::new(table, false), IdentifierCase::Folded)?.unwrap();
        Ok::<_, Error>(table.check_constraints(&db)?.next().unwrap())
    };
    let query = query_with(&PostgreSqlDialect {}, "SELECT 1 FROM t AS a WHERE a.x > 3");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    let term = select_of(&query).selection.as_ref().unwrap();
    assert_eq!(context.truth_under(term, &[])?, never_raising(UNDECIDED));
    assert_eq!(
        context.truth_under(term, &[Assumption::Check(check("t")?)])?,
        never_raising(Outcomes::TRUE)
    );
    assert_eq!(
        context.truth_under(term, &[Assumption::Check(check("u")?)])?,
        never_raising(UNDECIDED)
    );
    Ok(())
}

#[test]
fn generated_columns_and_checks_with_calls_hold_at_a_site() -> Result<(), Error> {
    let schema = "CREATE TABLE t (
                      x INT NOT NULL CHECK (x > abs(-5)),
                      k INT GENERATED ALWAYS AS (x * 2) STORED
                  );";
    for sql in ["SELECT 1 FROM t AS a WHERE a.x > 3", "SELECT 1 FROM t AS a WHERE a.k > 10"] {
        assert_eq!(query_truth(schema, sql)?.outcomes, Outcomes::TRUE, "{sql}");
    }
    Ok(())
}
