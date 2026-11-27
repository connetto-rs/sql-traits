//! Expression truth over the queries an expression nests and the bodies of
//! the `LANGUAGE sql` functions it calls.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "Tests require parsed fixtures of a known shape."
)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::{Expr, SetExpr, Statement},
    dialect::{
        ClickHouseDialect, Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect,
        PostgreSqlDialect, SQLiteDialect, SnowflakeDialect,
    },
    parser::Parser,
};

const SCHEMA: &str = "CREATE TABLE caps (o INT NOT NULL, role INT NOT NULL);
     CREATE TABLE positive (n INT NOT NULL CHECK (n >= 1));
     CREATE FUNCTION one() RETURNS INT LANGUAGE sql STABLE AS $$ SELECT 1 $$;
     CREATE FUNCTION returned_one() RETURNS INT LANGUAGE sql STABLE RETURN 1;
     CREATE FUNCTION inverse(x INT) RETURNS INT LANGUAGE sql STABLE AS $$ SELECT 10 / x $$;
     CREATE FUNCTION ratio(a INT, b INT) RETURNS INT LANGUAGE sql STABLE AS $$ SELECT a / b $$;
     CREATE FUNCTION capped(min_role INT) RETURNS SETOF INT LANGUAGE sql STABLE STRICT
         AS $$ SELECT o FROM caps WHERE role >= min_role $$;
     CREATE FUNCTION capped2(min_role INT) RETURNS TABLE (o INT, k INT) LANGUAGE sql STABLE
         STRICT AS $$ SELECT o, role FROM caps WHERE role >= min_role $$;
     CREATE FUNCTION quotient(a INT, b INT DEFAULT 0) RETURNS INT LANGUAGE sql STABLE
         AS $$ SELECT a / b $$;
     CREATE TABLE t (o INT NOT NULL, k INT NOT NULL, maybe INT);
     GRANT SELECT ON caps, positive TO PUBLIC;
     CREATE ROLE keeper;
     CREATE ROLE bypasser BYPASSRLS;
     CREATE TABLE hidden (n INT NOT NULL);
     ALTER TABLE hidden OWNER TO keeper;
     CREATE FUNCTION hidden_count() RETURNS BIGINT LANGUAGE sql STABLE SECURITY DEFINER
         AS $$ SELECT count(*) FROM hidden $$;
     ALTER FUNCTION hidden_count() OWNER TO keeper;
     CREATE FUNCTION invoked_hidden_count() RETURNS BIGINT LANGUAGE sql STABLE
         AS $$ SELECT count(*) FROM hidden $$;
     CREATE TABLE guarded (n INT NOT NULL);
     GRANT SELECT ON guarded TO PUBLIC;
     ALTER TABLE guarded ENABLE ROW LEVEL SECURITY;
     CREATE POLICY divides ON guarded FOR SELECT USING (10 / n > 0);
     CREATE FUNCTION guarded_count() RETURNS BIGINT LANGUAGE sql STABLE SECURITY DEFINER
         AS $$ SELECT count(*) FROM guarded $$;
     ALTER FUNCTION guarded_count() OWNER TO bypasser;
     CREATE TABLE screened (n INT NOT NULL);
     GRANT SELECT ON screened TO PUBLIC;
     ALTER TABLE screened ENABLE ROW LEVEL SECURITY;
     CREATE POLICY compares ON screened FOR SELECT USING (n > 0);
     CREATE FUNCTION all_caps() RETURNS SETOF caps LANGUAGE sql STABLE
         AS $$ SELECT caps.* FROM caps $$;
     CREATE FUNCTION mismatched() RETURNS TABLE (a INT, b INT) LANGUAGE sql STABLE
         AS $$ SELECT 1 $$;
     CREATE FUNCTION nothing() RETURNS INT LANGUAGE sql STABLE AS $$ SELECT 1 WHERE false $$;
     CREATE FUNCTION no_rows() RETURNS SETOF INT LANGUAGE sql STABLE
         AS $$ SELECT 1 WHERE false $$;
     CREATE FUNCTION broken() RETURNS INT LANGUAGE sql STABLE AS $$ SELECT x FROM nowhere $$;
     CREATE FUNCTION two_statements() RETURNS INT LANGUAGE sql STABLE
         AS $$ SELECT 1; SELECT 2 $$;
     CREATE FUNCTION garbled() RETURNS INT LANGUAGE sql STABLE AS $$ SELECT ( $$;
     CREATE FUNCTION counted(x INT) RETURNS BIGINT LANGUAGE sql STABLE
         AS $$ SELECT count(x) FROM caps $$;
     CREATE TABLE loop_a (n INT NOT NULL);
     CREATE TABLE loop_b (n INT NOT NULL);
     GRANT SELECT ON loop_a, loop_b TO PUBLIC;
     ALTER TABLE loop_a ENABLE ROW LEVEL SECURITY;
     ALTER TABLE loop_b ENABLE ROW LEVEL SECURITY;
     CREATE POLICY a_reads_b ON loop_a FOR SELECT USING (EXISTS (SELECT 1 FROM loop_b));
     CREATE POLICY b_reads_a ON loop_b FOR SELECT USING (EXISTS (SELECT 1 FROM loop_a));";

const UNDECIDED: Outcomes = Outcomes { truth: true, falsity: true, null: false };

const fn never_raising(outcomes: Outcomes) -> ExpressionTruth {
    ExpressionTruth { outcomes, raising: Raising::Never }
}

const fn sometimes_raising(outcomes: Outcomes) -> ExpressionTruth {
    ExpressionTruth { outcomes, raising: Raising::Sometimes }
}

fn expression(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

fn truth(sql: &str) -> Result<ExpressionTruth, Error> {
    truth_assuming(sql, None)
}

fn truth_assuming(sql: &str, assumed: Option<&str>) -> Result<ExpressionTruth, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let table =
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    let assumed = assumed.map(expression);
    let assumptions: Vec<Assumption<'_, ParserDB>> = assumed
        .iter()
        .map(|expression| Assumption::Expression { expression, outcomes: Outcomes::TRUE })
        .collect();
    Ok(table.truth_under(&db, &expression(sql), IdentifierCase::AsWritten, &assumptions)?)
}

#[test]
fn queries_without_rows_to_read() -> Result<(), Error> {
    for (sql, outcomes) in [
        ("(SELECT 1) IS NULL", Outcomes::FALSE),
        ("(SELECT 1) = 1", Outcomes::TRUE),
        ("EXISTS (SELECT 1)", Outcomes::TRUE),
        ("NOT EXISTS (SELECT 1)", Outcomes::FALSE),
        ("EXISTS (SELECT 1 WHERE false)", Outcomes::FALSE),
        ("(SELECT 1 WHERE false) IS NULL", Outcomes::TRUE),
        ("1 IN (SELECT 1)", Outcomes::TRUE),
        ("3 IN (VALUES (1), (2))", Outcomes::FALSE),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn queries_reading_tables() -> Result<(), Error> {
    for (sql, outcomes) in [
        ("o IN (SELECT o FROM caps)", UNDECIDED),
        ("o IN (SELECT 10 / n FROM positive)", UNDECIDED),
        ("o IN (SELECT 10 / role FROM caps WHERE role >= 1)", UNDECIDED),
        ("EXISTS (SELECT 1 FROM caps)", UNDECIDED),
        ("EXISTS (SELECT 1 FROM caps WHERE caps.o = t.o)", UNDECIDED),
        ("(SELECT o FROM caps LIMIT 1) IS NULL", UNDECIDED),
        ("(SELECT count(*) FROM caps) IS NULL", Outcomes::FALSE),
        ("(SELECT t.o) IS NULL", Outcomes::FALSE),
        ("(SELECT t.maybe) IS NULL", UNDECIDED),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn queries_that_may_raise() -> Result<(), Error> {
    for sql in [
        // `role` may be zero.
        "o IN (SELECT 10 / role FROM caps)",
        // More than one row raises in a scalar subquery.
        "(SELECT o FROM caps) IS NULL",
        // The correlated `t.maybe` may be zero.
        "EXISTS (SELECT 1 FROM caps WHERE 10 / t.maybe > 0)",
        // The sample percentage must lie between 0 and 100.
        "EXISTS (SELECT 1 FROM caps TABLESAMPLE BERNOULLI (200))",
    ] {
        assert_eq!(truth(sql)?.raising, Raising::Sometimes, "{sql}");
    }
    Ok(())
}

#[test]
fn sql_function_bodies() -> Result<(), Error> {
    for (sql, outcomes) in [
        ("one() IS NULL", Outcomes::FALSE),
        ("one() = 1", Outcomes::TRUE),
        ("returned_one() IS NULL", Outcomes::FALSE),
        ("inverse(5) IS NULL", Outcomes::FALSE),
        ("o IN (SELECT capped(2))", UNDECIDED),
        ("o IN (SELECT * FROM capped(2))", UNDECIDED),
        ("o IN (SELECT c FROM capped(2) AS c)", UNDECIDED),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    assert_eq!(truth("inverse(o) IS NULL")?, sometimes_raising(Outcomes::FALSE));
    assert_eq!(truth("inverse(0) IS NULL")?.raising, Raising::Always);
    // PostgreSQL binds `b` to 0 here, so the call divides by zero.
    assert_ne!(truth("ratio(b => 0, a => 5) IS NULL")?.raising, Raising::Never);
    Ok(())
}

#[test]
fn nested_queries_read_the_caller_row() -> Result<(), Error> {
    let truth = truth_assuming("(SELECT t.maybe) IS NULL", Some("maybe IS NOT NULL"))?;
    assert_eq!(truth, never_raising(Outcomes::FALSE));
    let truth = truth_assuming("EXISTS (SELECT 1 WHERE t.maybe > 0)", Some("maybe > 1"))?;
    assert_eq!(truth, never_raising(Outcomes::TRUE));
    Ok(())
}

#[test]
fn joins() -> Result<(), Error> {
    for (sql, outcomes) in [
        ("o IN (SELECT o FROM caps JOIN positive ON caps.o = positive.n)", UNDECIDED),
        ("o IN (SELECT a.o FROM caps AS a JOIN caps AS b USING (o))", UNDECIDED),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    // `role` may be zero on a row the join tests.
    let sql = "EXISTS (SELECT 1 FROM caps LEFT JOIN positive ON 10 / caps.role > 0)";
    assert_eq!(truth(sql)?.raising, Raising::Sometimes);
    // The `RIGHT JOIN` extends with `NULL` the rows the inner join matched.
    let sql = "(SELECT c.o FROM caps AS c JOIN positive AS p ON c.o = 1
        RIGHT JOIN positive AS q ON true LIMIT 1) = 1";
    assert!(truth(sql)?.outcomes.null);
    Ok(())
}

#[test]
fn passing_arguments_may_raise() -> Result<(), Error> {
    // The input function of `INT` rejects `'abc'`.
    assert_ne!(truth("inverse('abc') IS NULL")?.raising, Raising::Never);
    // The default of `b` divides by zero.
    assert_ne!(truth("quotient(5) IS NULL")?.raising, Raising::Never);
    assert_eq!(truth("quotient(5, 1) IS NULL")?, never_raising(Outcomes::FALSE));
    Ok(())
}

#[test]
fn aggregates() -> Result<(), Error> {
    // A sort key calling an aggregate makes the query return one row.
    let sql = "EXISTS (SELECT 1 FROM caps WHERE false ORDER BY count(*))";
    assert_eq!(truth(sql)?, never_raising(Outcomes::TRUE));
    // `count(t.o)` belongs to the outer level, where PostgreSQL refuses it.
    let sql = "EXISTS (SELECT count(t.o) FROM caps WHERE false)";
    assert_ne!(truth(sql)?.raising, Raising::Never);
    // The one row is returned when the `WHERE` drops every `caps` row, so it
    // holds the values of `t` the `WHERE` rules out.
    let sql = "(SELECT t.o FROM caps WHERE t.o > 5 ORDER BY count(*)) > 5";
    assert!(truth(sql)?.outcomes.falsity);
    let sql = "(SELECT 10 / t.maybe FROM caps WHERE t.maybe <> 0 ORDER BY count(*)) IS NULL";
    assert_eq!(truth(sql)?.raising, Raising::Sometimes);
    // A call resolving to no function may be an aggregate, so the query
    // stays unread.
    let sql = "(SELECT t.o FROM caps WHERE t.o > 5 ORDER BY count(o IS NULL)) > 5";
    assert!(truth(sql)?.outcomes.falsity);
    // An aggregate in a nested query whose arguments read only columns of
    // `c` belongs to the query reading `c`, which then returns one row.
    for sql in [
        "EXISTS (SELECT (SELECT max(c.o)) FROM caps AS c WHERE false)",
        "EXISTS (SELECT (SELECT max(c.o) FROM caps AS d) FROM caps AS c WHERE false)",
        "EXISTS (SELECT 1 FROM caps AS c WHERE false ORDER BY (SELECT max(c.o)))",
        "EXISTS (SELECT (SELECT count(*) FILTER (WHERE c.o > 0)) FROM caps AS c WHERE false)",
        "EXISTS (SELECT DISTINCT ON ((SELECT max(c.o))) 1 FROM caps AS c WHERE false)",
    ] {
        assert!(truth(sql)?.outcomes.truth, "{sql}");
    }
    let sql = "(SELECT (SELECT count(c.o)) FROM caps AS c WHERE false) IS NULL";
    assert!(truth(sql)?.outcomes.falsity);
    let sql = "EXISTS (SELECT DISTINCT ON (max(c.o)) 1 FROM caps AS c WHERE false)";
    assert_eq!(truth(sql)?.outcomes, Outcomes::TRUE);
    // A window function belongs to the query it is written in.
    let sql = "EXISTS (SELECT (SELECT count(c.o) OVER ()) FROM caps AS c WHERE false)";
    assert_eq!(truth(sql)?.outcomes, Outcomes::FALSE);
    // The filter reads only `t`, so `count(*)` belongs to the outer level.
    let sql = "EXISTS (SELECT count(*) FILTER (WHERE t.o > 0) FROM caps WHERE false)";
    assert_ne!(truth(sql)?.raising, Raising::Never);
    // Reading a column of `d`, the aggregate belongs to the query reading `d`.
    let sql = "EXISTS (SELECT (SELECT max(d.o) FROM caps AS d) FROM caps AS c WHERE false)";
    assert_eq!(truth(sql)?.outcomes, Outcomes::FALSE);
    Ok(())
}

#[test]
fn privileges_and_policies_of_the_reading_role() -> Result<(), Error> {
    for sql in [
        // Only `keeper` may read `hidden`.
        "EXISTS (SELECT 1 FROM hidden)",
        "invoked_hidden_count() IS NULL",
        // The policy of `guarded` divides by `n`.
        "EXISTS (SELECT 1 FROM guarded)",
    ] {
        assert_eq!(truth(sql)?.raising, Raising::Sometimes, "{sql}");
    }
    for (sql, outcomes) in [
        ("EXISTS (SELECT 1 FROM screened)", UNDECIDED),
        // The bodies run as `keeper`, who owns `hidden`, and as `bypasser`.
        ("hidden_count() IS NULL", Outcomes::FALSE),
        ("guarded_count() IS NULL", Outcomes::FALSE),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn reads_run_as_each_acting_role_of_the_deployment() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let table =
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    let read = |sql: &str, acting: &[&str]| -> Result<ExpressionTruth, Error> {
        let deployment = Deployment::new("authenticator", acting.iter().copied(), []).unwrap();
        let assumptions = [Assumption::Deployment(&deployment)];
        Ok(table.truth_under(&db, &expression(sql), IdentifierCase::AsWritten, &assumptions)?)
    };
    // `keeper` owns `hidden`, and `bypasser` skips the policy of `guarded`.
    assert_eq!(read("EXISTS (SELECT 1 FROM hidden)", &["keeper"])?, never_raising(UNDECIDED));
    assert_eq!(read("EXISTS (SELECT 1 FROM guarded)", &["bypasser"])?, never_raising(UNDECIDED));
    for (sql, acting) in [
        // `bypasser` may not read `hidden`.
        ("EXISTS (SELECT 1 FROM hidden)", ["keeper", "bypasser"].as_slice()),
        // `keeper` owns `hidden` but not `guarded`, whose policy divides by
        // `n`.
        ("EXISTS (SELECT 1 FROM guarded)", ["keeper", "bypasser"].as_slice()),
        // A role the schema does not declare may be subject to every policy.
        ("EXISTS (SELECT 1 FROM guarded)", ["ghost"].as_slice()),
    ] {
        assert_eq!(read(sql, acting)?.raising, Raising::Sometimes, "{sql} as {acting:?}");
    }
    Ok(())
}

#[test]
fn shapes_the_read_follows() -> Result<(), Error> {
    for (sql, outcomes) in [
        ("EXISTS (SELECT DISTINCT o FROM caps)", UNDECIDED),
        ("o IN (SELECT a.o FROM caps AS a LEFT JOIN caps AS b USING (o))", UNDECIDED),
        ("EXISTS (SELECT 1 FROM caps CROSS JOIN positive)", UNDECIDED),
        ("EXISTS (SELECT *, 1 FROM caps LEFT JOIN positive ON true)", UNDECIDED),
        ("(SELECT count(CAST(o AS TEXT)) FROM caps) IS NULL", Outcomes::FALSE),
        ("EXISTS (SELECT 1 FROM one())", Outcomes::TRUE),
        ("(SELECT x FROM one() AS x) = 1", Outcomes::TRUE),
        // A strict function returns no row for a `NULL` argument.
        ("EXISTS (SELECT 1 FROM capped(NULL))", Outcomes::FALSE),
        ("o IN (SELECT * FROM capped(t.maybe))", UNDECIDED),
        ("EXISTS (SELECT 1 FROM all_caps())", UNDECIDED),
        ("EXISTS (SELECT 1 FROM caps LEFT JOIN positive ON true CROSS JOIN capped2(1))", UNDECIDED),
        ("inverse(1) IS NULL OR inverse(2) IS NULL", Outcomes::FALSE),
        ("nothing() IS NULL", Outcomes::TRUE),
        ("(SELECT x FROM no_rows() AS x) IS NULL", Outcomes::TRUE),
        // A set without rows leaves the expression no value to test.
        ("no_rows() IS NULL", Outcomes::NONE),
        ("counted(1) IS NULL", Outcomes::FALSE),
        ("EXISTS (SELECT 1 FROM caps OFFSET 0)", UNDECIDED),
        ("EXISTS (SELECT 1 FROM caps LIMIT 0)", Outcomes::FALSE),
        ("EXISTS (SELECT 1 FROM caps LIMIT 5)", UNDECIDED),
        (
            "EXISTS (SELECT 1 FROM screened) OR EXISTS (SELECT 1 FROM screened WHERE n > 1)",
            UNDECIDED,
        ),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn shapes_the_read_leaves_raising() -> Result<(), Error> {
    for sql in [
        "EXISTS (SELECT 1 FROM nowhere)",
        "EXISTS (SELECT 1 FROM pg_catalog.pg_class)",
        "EXISTS (SELECT 1 FROM no_such_function())",
        "EXISTS (SELECT 1 FROM caps GROUP BY o)",
        "(SELECT count((SELECT 1)) FROM caps) IS NULL",
        // PostgreSQL refuses rows of differing lengths.
        "EXISTS (VALUES (1), (1, 2))",
        "broken() IS NULL",
        "two_statements() IS NULL",
        // A body that does not parse.
        "garbled() IS NULL",
        // The body returns one column where two are declared.
        "EXISTS (SELECT 1 FROM mismatched())",
        // A negative count raises.
        "EXISTS (SELECT 1 FROM caps OFFSET t.k)",
        "EXISTS (SELECT 1 FROM caps LIMIT t.k)",
        // Each policy reads the table the other guards.
        "EXISTS (SELECT 1 FROM loop_a)",
    ] {
        assert_ne!(truth(sql)?.raising, Raising::Never, "{sql}");
    }
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let table =
        db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    let deployment = Deployment::new("authenticator", ["keeper", "ghost"], []).unwrap();
    let assumptions = [Assumption::Deployment(&deployment)];
    let read = |sql: &str| {
        table.truth_under(&db, &expression(sql), IdentifierCase::AsWritten, &assumptions)
    };
    assert_eq!(read("EXISTS (SELECT 1 FROM loop_a)")?.raising, Raising::Sometimes);
    assert_eq!(read("EXISTS (SELECT 1 FROM screened)")?, never_raising(UNDECIDED));
    Ok(())
}

#[test]
fn reads_under_other_engines() -> Result<(), Error> {
    let schema = "CREATE TABLE caps (o INT NOT NULL, role INT NOT NULL);
         CREATE TABLE t (o INT NOT NULL);";
    let read =
        |db: &ParserDB, sql: &str, dialect: &dyn Dialect| -> Result<ExpressionTruth, Error> {
            let table = db
                .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
                .unwrap();
            let expression = Parser::new(dialect).try_with_sql(sql).unwrap().parse_expr().unwrap();
            Ok(table.truth_under(db, &expression, IdentifierCase::AsWritten, &[])?)
        };
    // SQLite checks no privilege on the tables a query reads.
    let sqlite = ParserDB::parse::<SQLiteDialect>(schema)?;
    let sql = "EXISTS (SELECT 1 FROM caps)";
    assert_eq!(read(&sqlite, sql, &SQLiteDialect {})?, never_raising(UNDECIDED));
    let generic = ParserDB::parse::<GenericDialect>(&format!(
        "{schema} GRANT SELECT ON caps TO PUBLIC; ALTER TABLE caps ENABLE ROW LEVEL SECURITY;
         CREATE POLICY positive ON caps FOR SELECT USING (role > 0);"
    ))?;
    for (sql, dialect) in [
        ("EXISTS (SELECT 1 FROM caps WHERE role > 0)", &GenericDialect {} as &dyn Dialect),
        ("EXISTS (SELECT 1 FROM caps ORDER BY ALL)", &DuckDbDialect {}),
    ] {
        assert_eq!(read(&generic, sql, dialect)?, never_raising(UNDECIDED), "{sql}");
    }
    // Clauses the read does not follow.
    for (sql, dialect) in [
        ("EXISTS (SELECT 1 FROM caps ORDER BY o WITH FILL)", &ClickHouseDialect {} as &dyn Dialect),
        ("EXISTS (SELECT 1 FROM caps ORDER BY o INTERPOLATE)", &ClickHouseDialect {}),
        ("EXISTS (SELECT 1 FROM caps LIMIT 1 BY o)", &ClickHouseDialect {}),
        ("EXISTS (SELECT 1 FROM caps LIMIT 1, 2)", &MySqlDialect {}),
        ("upper(SELECT 'a') IS NULL", &SnowflakeDialect {}),
        ("EXISTS (SELECT 1 FROM caps CROSS APPLY caps AS other)", &MsSqlDialect {}),
    ] {
        assert_ne!(read(&generic, sql, dialect)?.raising, Raising::Never, "{sql}");
    }
    Ok(())
}

#[test]
fn helper_shapes() -> Result<(), Error> {
    for (sql, outcomes) in [
        ("o = ANY (ARRAY(SELECT capped(2)))", Outcomes::ANY),
        ("o IN ((SELECT 1), (SELECT one()))", UNDECIDED),
        ("(SELECT one()) = 1", Outcomes::TRUE),
        ("(SELECT o FROM capped2(2) LIMIT 1) IS NULL", UNDECIDED),
        ("o IN (SELECT o FROM capped2(2))", UNDECIDED),
        ("k IN (SELECT k FROM capped2(2) WHERE o > 0)", UNDECIDED),
    ] {
        assert_eq!(truth(sql)?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn site_reads_nested_queries() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA)?;
    let sql = "SELECT 1 FROM t WHERE o IN (SELECT * FROM capped(2)) AND EXISTS (SELECT 1)";
    let Statement::Query(query) = Parser::parse_sql(&PostgreSqlDialect {}, sql)?.remove(0) else {
        panic!("a query")
    };
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("a SELECT body") };
    let Some(Expr::BinaryOp { left, right, .. }) = &select.selection else {
        panic!("a binary WHERE")
    };
    let context = ExpressionContext::new(&query, &db, IdentifierCase::Folded)?;
    assert_eq!(context.truth_under(left, &[])?, never_raising(UNDECIDED));
    assert_eq!(context.truth_under(right, &[])?, never_raising(Outcomes::TRUE));
    Ok(())
}
