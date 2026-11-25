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

/// A call reads as in any query, `abs` raising on the least integer, and a
/// name no variable answers is one value wherever the condition reads it.
#[test]
fn a_call_and_a_name_no_variable_answers_read_as_in_any_query() {
    let database = database(
        "CREATE TABLE log (v integer); \
         CREATE FUNCTION run(n integer) RETURNS text LANGUAGE plpgsql STRICT AS $$ BEGIN \
         INSERT INTO log VALUES (n); \
         IF nowhere IS NULL AND nowhere IS NOT NULL THEN RETURN 'never'; END IF; \
         IF abs(n) IS NOT NULL THEN RETURN 'set'; END IF; \
         RETURN 'other'; END $$;",
    );
    let truths: Vec<Option<ExpressionTruth>> =
        conditions(&database, "run").into_iter().map(|(.., truth)| truth).collect();
    assert_eq!(
        truths,
        [
            Some(never_raising(Outcomes::FALSE)),
            Some(ExpressionTruth { outcomes: Outcomes::TRUE, raising: Raising::Sometimes }),
        ]
    );
}

#[test]
fn an_out_parameter_starts_null_and_a_trigger_variable_set() {
    let database = database(
        "CREATE FUNCTION split(OUT o integer) LANGUAGE plpgsql AS $$ \
         BEGIN IF o IS NULL THEN o := 1; END IF; END $$; \
         CREATE FUNCTION fired() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF TG_OP IS NULL THEN RETURN NULL; END IF; RETURN NEW; END $$;",
    );
    assert_eq!(conditions(&database, "split")[0].2, Some(never_raising(Outcomes::TRUE)));
    assert_eq!(conditions(&database, "fired")[0].2, Some(never_raising(Outcomes::FALSE)));
}

/// A row read into a variable of a table's type holds the table's columns,
/// and reading a column the table does not have raises.
#[test]
fn a_row_variable_reads_the_columns_of_its_table() {
    let database = database(
        "CREATE TABLE items (amount integer); CREATE TABLE pairs (a integer, b integer); \
         CREATE TYPE pair AS (a integer, b integer); \
         CREATE FUNCTION run() RETURNS text LANGUAGE plpgsql AS $$ \
         DECLARE r items; p pair; BEGIN \
         SELECT * INTO r FROM items; SELECT a, b INTO p FROM pairs; \
         IF r.amount > 0 THEN RETURN 'positive'; END IF; \
         IF p.a > 0 THEN RETURN 'unset'; END IF; \
         IF r.missing > 0 THEN RETURN 'unknown'; END IF; \
         RETURN 'other'; END $$;",
    );
    let truths: Vec<Option<ExpressionTruth>> =
        conditions(&database, "run").into_iter().map(|(.., truth)| truth).collect();
    assert_eq!(
        truths,
        [
            Some(never_raising(Outcomes::ANY)),
            Some(never_raising(Outcomes::ANY)),
            Some(ExpressionTruth { outcomes: Outcomes::NONE, raising: Raising::Always }),
        ]
    );
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

#[test]
fn a_function_several_triggers_execute_joins_their_rows() {
    assert_eq!(
        trigger_conditions(
            "CREATE TABLE others (amount integer, tag integer, low integer, high integer); \
             CREATE TRIGGER audited AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION audit(); \
             CREATE TRIGGER copied AFTER INSERT ON others FOR EACH ROW EXECUTE FUNCTION audit();"
        ),
        [Some(never_raising(Outcomes::ANY)); 3]
    );
}

/// A view has no column guarantees, so a field of the row of a trigger on it
/// has no known type.
#[test]
fn a_trigger_on_a_view_reads_new_by_nothing() {
    assert_eq!(
        trigger_conditions(
            "CREATE VIEW shown AS SELECT amount, tag, low, high FROM items; \
             CREATE TRIGGER audited INSTEAD OF INSERT ON shown FOR EACH ROW \
             EXECUTE FUNCTION audit();"
        )[0],
        Some(never_raising(Outcomes::ANY))
    );
}

/// The value of each condition of `whole` when `trigger` executes it.
fn whole_conditions(trigger: &str) -> Vec<Option<ExpressionTruth>> {
    let database = database(&format!(
        "{TRIGGER_SCHEMA} CREATE FUNCTION whole() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         IF NEW IS NULL THEN PERFORM 1; END IF; \
         IF NEW.amount.inner IS NULL THEN PERFORM 2; END IF; \
         IF NEW.missing IS NULL THEN PERFORM 3; END IF; \
         RETURN NULL; END $$; {trigger}"
    ));
    conditions(&database, "whole").into_iter().map(|(.., truth)| truth).collect()
}

#[test]
fn new_is_null_where_no_row_fires_and_a_column_it_lacks_raises() {
    let known = Outcomes { truth: true, falsity: true, null: false };
    assert_eq!(
        whole_conditions(
            "CREATE TRIGGER audited AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION whole();"
        ),
        [
            Some(never_raising(Outcomes::FALSE)),
            Some(ExpressionTruth { outcomes: known, raising: Raising::Sometimes }),
            Some(ExpressionTruth { outcomes: Outcomes::NONE, raising: Raising::Always }),
        ]
    );
    assert_eq!(
        whole_conditions(
            "CREATE TRIGGER audited AFTER INSERT ON items FOR EACH STATEMENT \
             EXECUTE FUNCTION whole();"
        )[0],
        Some(never_raising(Outcomes::TRUE))
    );
}

#[test]
fn a_type_copied_from_nothing_fails_the_walk() {
    let database = function("DECLARE x nowhere.col%TYPE; BEGIN x := 1; RETURN 'a'; END");
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    assert!(sites[0].reached(IdentifierCase::Folded).is_err());
}

/// A site formats its body after a truth read without the walk it ran.
#[test]
fn a_site_formats_its_body_after_a_truth_read() {
    let database = function("BEGIN IF n > 0 THEN RETURN 'positive'; END IF; RETURN 'other'; END");
    let body = named(&database, "run").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    assert!(sites[0].reached(IdentifierCase::Folded).unwrap());
    let formatted = format!("{:?}", sites[0]);
    assert!(formatted.contains("SiteFlow { names: "), "{formatted}");
    assert!(formatted.contains("block: "), "{formatted}");
    assert!(formatted.contains("flow: "), "{formatted}");
}

/// The truth of the comparison the `IS NULL` test tests in the `WHERE` of
/// the `SELECT INTO` of `body`, the body of `nulls(y int)` over `r`.
fn raising_comparison(body: &str) -> Option<ExpressionTruth> {
    let database = database(&format!(
        "CREATE TABLE r (a int NOT NULL, c int); \
         CREATE FUNCTION nulls(y int) RETURNS bigint LANGUAGE plpgsql AS $$ {body} $$;"
    ));
    let body = named(&database, "nulls").plpgsql_sites(&database).unwrap();
    let sites = body.sites().unwrap();
    let (site, query) = sites
        .iter()
        .find_map(|site| {
            match site.sql() {
                EmbeddedSql::Statement(Statement::Query(query)) => Some((site, query)),
                _ => None,
            }
        })
        .expect("the SELECT INTO");
    let SetExpr::Select(select) = &*query.body else { panic!("a SELECT") };
    let Some(Expr::IsNull(tested)) = &select.selection else { panic!("an IS NULL test") };
    let Expr::Nested(compared) = &**tested else { panic!("a parenthesized comparison") };
    site.truth_under(compared, IdentifierCase::Folded).unwrap()
}

/// PostgreSQL plans a parameter, a variable and a field of a row variable
/// as constants in a custom plan, and folds a strict operator over a `NULL`
/// one to `NULL` before the operand dividing by zero runs.
#[test]
fn a_comparison_over_a_body_name_may_fold_before_raising() {
    let folded = |body: &str| raising_comparison(body).map(|truth| truth.raising);
    assert_eq!(
        folded(
            "DECLARE n bigint; BEGIN \
             SELECT count(*) INTO n FROM r WHERE (a / 0 > y) IS NULL; RETURN n; END"
        ),
        Some(Raising::Sometimes)
    );
    assert_eq!(
        folded(
            "DECLARE n bigint; v int; BEGIN \
             SELECT count(*) INTO n FROM r WHERE (a / 0 > v) IS NULL; RETURN n; END"
        ),
        Some(Raising::Sometimes)
    );
    assert_eq!(
        folded(
            "DECLARE n bigint; row r; BEGIN \
             SELECT count(*) INTO n FROM r WHERE (a / 0 > row.c) IS NULL; RETURN n; END"
        ),
        Some(Raising::Sometimes)
    );
    assert_eq!(
        folded(
            "DECLARE n bigint; BEGIN \
             SELECT count(*) INTO n FROM r WHERE (a / 0 > c) IS NULL; RETURN n; END"
        ),
        Some(Raising::Always)
    );
}
