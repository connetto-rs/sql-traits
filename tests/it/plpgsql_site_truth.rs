//! Whether a path reaches each statement and expression of a PL/pgSQL body,
//! and what its expressions evaluate to under the guarantees reaching it.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test fails loudly on a broken premise"
)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Query, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

const fn never_raising(outcomes: Outcomes) -> ExpressionTruth {
    ExpressionTruth { outcomes, raising: Raising::Never }
}

fn database(schema: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(schema).expect("schema parses")
}

/// The database holding `run(n integer)`, returning `text` and running
/// `body`.
fn function(body: &str) -> ParserDB {
    database(&format!(
        "CREATE FUNCTION run(n integer) RETURNS text LANGUAGE plpgsql AS $$ {body} $$;"
    ))
}

fn named<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateFunction {
    database
        .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("function exists")
}

/// The one expression the `SELECT` of a site selects.
fn selected<'s>(site: &'s PlPgSqlSite<'_, '_, ParserDB>) -> &'s Expr {
    let query: &Query = match site.sql() {
        EmbeddedSql::Expression(query) => query,
        EmbeddedSql::Statement(Statement::Query(query)) => query,
        EmbeddedSql::Statement(statement) => panic!("{statement} selects nothing"),
    };
    let SetExpr::Select(select) = &*query.body else { panic!("a SELECT") };
    let [SelectItem::UnnamedExpr(expression)] = select.projection.as_slice() else {
        panic!("one expression")
    };
    expression
}

const CONDITIONS: fn(&PlPgSqlSiteRole) -> bool = |role| {
    matches!(
        role,
        PlPgSqlSiteRole::IfCondition { .. }
            | PlPgSqlSiteRole::ElsifCondition { .. }
            | PlPgSqlSiteRole::CaseWhen { .. }
            | PlPgSqlSiteRole::WhileCondition
            | PlPgSqlSiteRole::ExitWhen
            | PlPgSqlSiteRole::ContinueWhen
            | PlPgSqlSiteRole::AssertCondition
    )
};

/// Each condition of the body of `function`, with its role and value, and
/// no other site answering a value as a condition.
fn conditions(
    database: &ParserDB,
    function: &str,
) -> Vec<(String, PlPgSqlSiteRole, Option<ExpressionTruth>)> {
    let body = named(database, function).plpgsql_sites(database).unwrap();
    let mut conditions = Vec::new();
    for site in body.sites().unwrap() {
        let condition = site.condition(IdentifierCase::Folded).unwrap();
        if CONDITIONS(&site.role()) {
            conditions.push((selected(&site).to_string(), site.role(), condition));
        } else {
            assert_eq!(condition, None, "{:?} is no condition", site.role());
        }
    }
    conditions
}

fn parsed(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

#[test]
fn a_condition_after_a_constant_assignment_is_always_true() {
    let database = function(
        "DECLARE x int := 5; BEGIN IF x > 0 THEN RETURN 'then'; ELSE RETURN 'else'; END IF; END",
    );
    assert_eq!(
        conditions(&database, "run"),
        [(
            "x > 0".to_owned(),
            PlPgSqlSiteRole::IfCondition { followed: true },
            Some(never_raising(Outcomes::TRUE)),
        )]
    );
}

#[test]
fn an_expression_under_a_null_test_is_never_true() {
    let database = function("BEGIN IF n IS NULL THEN RETURN n > 3; END IF; RETURN 'set'; END");
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let returned = sites
        .iter()
        .find(|site| selected(site).to_string() == "n > 3")
        .expect("the RETURN inside the IF");
    assert_eq!(returned.role(), PlPgSqlSiteRole::Expression);
    assert_eq!(
        returned.truth_under(selected(returned), IdentifierCase::Folded).unwrap(),
        Some(never_raising(Outcomes::NULL))
    );
}

#[test]
fn a_parameter_reads_as_its_type_and_an_elsif_knows_what_follows_it() {
    let database = function(
        "BEGIN IF n > 0 THEN RETURN 'positive'; ELSIF n > 5 THEN RETURN 'never'; END IF; \
         RETURN 'other'; END",
    );
    assert_eq!(
        conditions(&database, "run"),
        [
            (
                "n > 0".to_owned(),
                PlPgSqlSiteRole::IfCondition { followed: true },
                Some(never_raising(Outcomes::ANY)),
            ),
            (
                "n > 5".to_owned(),
                PlPgSqlSiteRole::ElsifCondition { followed: false },
                Some(never_raising(Outcomes::NOT_TRUE)),
            ),
        ]
    );
}

#[test]
fn a_site_after_a_return_is_not_reached() {
    let database = function("BEGIN RETURN 'done'; PERFORM n + 1; END");
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let reached: Vec<(PlPgSqlSiteRole, bool)> = sites
        .iter()
        .map(|site| (site.role(), site.reached(IdentifierCase::Folded).unwrap()))
        .collect();
    assert_eq!(reached, [(PlPgSqlSiteRole::Expression, true), (PlPgSqlSiteRole::Statement, false)]);
    assert_eq!(sites[1].truth_under(selected(&sites[1]), IdentifierCase::Folded).unwrap(), None);
}

#[test]
fn an_expression_of_no_site_is_refused() {
    let database = function("BEGIN RETURN n > 3; END");
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    assert!(sites[0].truth_under(&parsed("n > 3"), IdentifierCase::Folded).is_err());
}

#[test]
fn a_case_without_else_tells_whether_its_end_is_reached_unmatched() {
    let database = database(
        "CREATE FUNCTION simple() RETURNS text LANGUAGE plpgsql AS $$ \
         DECLARE x int := 2; BEGIN CASE x WHEN 1 THEN RETURN 'one'; WHEN 2, 3 THEN RETURN 'two'; \
         END CASE; END $$; \
         CREATE FUNCTION missed() RETURNS text LANGUAGE plpgsql AS $$ \
         DECLARE x int := 4; BEGIN CASE x WHEN 1 THEN RETURN 'one'; END CASE; END $$; \
         CREATE FUNCTION searched(n int) RETURNS text LANGUAGE plpgsql AS $$ \
         BEGIN CASE WHEN n > 0 THEN RETURN 'positive'; WHEN n <= 0 THEN RETURN 'other'; \
         WHEN n IS NULL THEN RETURN 'null'; END CASE; END $$; \
         CREATE FUNCTION otherwise(n int) RETURNS text LANGUAGE plpgsql AS $$ \
         BEGIN CASE n WHEN 1 THEN RETURN 'one'; ELSE RETURN 'other'; END CASE; END $$;",
    );
    let unmatched = |function: &str| -> Vec<(PlPgSqlSiteRole, Option<bool>)> {
        let body = named(&database, function).plpgsql_sites(&database).unwrap();
        body.sites()
            .unwrap()
            .iter()
            .filter(|site| {
                !matches!(site.role(), PlPgSqlSiteRole::Default | PlPgSqlSiteRole::Expression)
            })
            .map(|site| (site.role(), site.unmatched(IdentifierCase::Folded).unwrap()))
            .collect()
    };
    assert_eq!(
        unmatched("simple"),
        [
            (PlPgSqlSiteRole::CaseSubject, Some(false)),
            (PlPgSqlSiteRole::CaseValue, None),
            (PlPgSqlSiteRole::CaseValue, None),
        ]
    );
    assert_eq!(
        unmatched("missed"),
        [(PlPgSqlSiteRole::CaseSubject, Some(true)), (PlPgSqlSiteRole::CaseValue, None),]
    );
    assert_eq!(
        unmatched("searched"),
        [
            (PlPgSqlSiteRole::CaseWhen { followed: true }, Some(false)),
            (PlPgSqlSiteRole::CaseWhen { followed: true }, None),
            (PlPgSqlSiteRole::CaseWhen { followed: false }, None),
        ]
    );
    assert_eq!(
        unmatched("otherwise"),
        [(PlPgSqlSiteRole::CaseSubject, None), (PlPgSqlSiteRole::CaseValue, None),]
    );
    assert_eq!(conditions(&database, "searched")[2].2, Some(never_raising(Outcomes::TRUE)));
}

#[test]
fn loop_exit_and_assert_conditions_carry_their_roles() {
    let database = function(
        "DECLARE i int := 0; BEGIN WHILE i < 3 LOOP i := i + 1; EXIT WHEN i > 10; \
         CONTINUE WHEN i = 2; END LOOP; ASSERT n IS NOT NULL; RETURN 'done'; END",
    );
    let roles: Vec<(String, PlPgSqlSiteRole)> =
        conditions(&database, "run").into_iter().map(|(text, role, _)| (text, role)).collect();
    assert_eq!(
        roles,
        [
            ("i < 3".to_owned(), PlPgSqlSiteRole::WhileCondition),
            ("i > 10".to_owned(), PlPgSqlSiteRole::ExitWhen),
            ("i = 2".to_owned(), PlPgSqlSiteRole::ContinueWhen),
            ("n IS NOT NULL".to_owned(), PlPgSqlSiteRole::AssertCondition),
        ]
    );
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let assigned: Vec<PlPgSqlSiteRole> = body
        .sites()
        .unwrap()
        .iter()
        .map(PlPgSqlSite::role)
        .filter(|role| matches!(role, PlPgSqlSiteRole::Assigned | PlPgSqlSiteRole::Default))
        .collect();
    assert_eq!(assigned, [PlPgSqlSiteRole::Default, PlPgSqlSiteRole::Assigned]);
}

#[test]
fn a_strict_function_never_starts_with_a_null_argument() {
    let database = database(
        "CREATE FUNCTION run(n integer) RETURNS text LANGUAGE plpgsql STRICT AS $$ \
         BEGIN IF n IS NULL THEN RETURN 'null'; END IF; RETURN 'set'; END $$;",
    );
    assert_eq!(conditions(&database, "run")[0].2, Some(never_raising(Outcomes::FALSE)));
}

#[test]
fn a_statement_site_reads_a_lone_row_select() {
    let database = function(
        "DECLARE x int; BEGIN SELECT 4 INTO x; IF x = 4 THEN RETURN 'four'; END IF; \
         RETURN 'other'; END",
    );
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    assert!(matches!(sites[0].sql(), EmbeddedSql::Statement(Statement::Query(_))));
    assert_eq!(sites[0].role(), PlPgSqlSiteRole::Statement);
    assert_eq!(conditions(&database, "run")[0].2, Some(never_raising(Outcomes::TRUE)));
}

const TRIGGER_SCHEMA: &str = "CREATE DOMAIN positive AS integer CHECK (VALUE > 0); \
     CREATE TABLE items (amount integer NOT NULL CHECK (amount > 0), tag positive, \
     low integer, high integer, CHECK (low < high)); \
     CREATE FUNCTION audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
     IF NEW.amount > 0 THEN PERFORM 1; END IF; \
     IF NEW.tag > 0 THEN PERFORM 2; END IF; \
     IF NEW.low < NEW.high THEN PERFORM 3; END IF; \
     RETURN NULL; END $$;";

/// The value of each condition of `audit` when `trigger` executes it.
fn trigger_conditions(trigger: &str) -> Vec<Option<ExpressionTruth>> {
    let database = database(&format!("{TRIGGER_SCHEMA} {trigger}"));
    conditions(&database, "audit").into_iter().map(|(.., truth)| truth).collect()
}

#[test]
fn an_after_row_trigger_reads_new_under_the_column_guarantees() {
    assert_eq!(
        trigger_conditions(
            "CREATE TRIGGER audited AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit();"
        ),
        [
            Some(never_raising(Outcomes::TRUE)),
            Some(never_raising(Outcomes::NOT_FALSE)),
            Some(never_raising(Outcomes::ANY)),
        ]
    );
}

#[test]
fn a_before_row_trigger_reads_new_before_the_table_constraints_hold() {
    assert_eq!(
        trigger_conditions(
            "CREATE TRIGGER audited BEFORE INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit();"
        ),
        [
            Some(never_raising(Outcomes::ANY)),
            Some(never_raising(Outcomes::NOT_FALSE)),
            Some(never_raising(Outcomes::ANY)),
        ]
    );
}

#[test]
fn new_is_null_where_a_delete_or_a_statement_fires_the_trigger() {
    let null = Some(never_raising(Outcomes::NULL));
    assert_eq!(
        trigger_conditions(
            "CREATE TRIGGER audited AFTER DELETE ON items FOR EACH ROW EXECUTE FUNCTION audit();"
        ),
        [null, null, null]
    );
    assert_eq!(
        trigger_conditions(
            "CREATE TRIGGER audited AFTER INSERT ON items FOR EACH STATEMENT \
             EXECUTE FUNCTION audit();"
        ),
        [null, null, null]
    );
    assert_eq!(
        trigger_conditions(
            "CREATE TRIGGER audited AFTER INSERT OR DELETE ON items FOR EACH ROW \
             EXECUTE FUNCTION audit();"
        )[0],
        Some(never_raising(Outcomes::NOT_FALSE))
    );
}

/// A field of a row no trigger types has no known type, so comparing it may
/// raise.
#[test]
fn a_trigger_function_no_trigger_executes_reads_new_by_its_type() {
    let untyped = Some(ExpressionTruth { outcomes: Outcomes::ANY, raising: Raising::Sometimes });
    assert_eq!(trigger_conditions(""), [untyped, untyped, untyped]);
}
