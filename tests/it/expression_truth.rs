//! Table-scoped truth analysis for policy and check expressions.
#![expect(
    clippy::unwrap_used,
    clippy::unreachable,
    reason = "Tests require parsed fixtures and exhaustive expected policy names."
)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::Expr,
    dialect::{GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    parser::Parser,
};

fn expression(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

const fn never_raising(outcomes: Outcomes) -> ExpressionTruth {
    ExpressionTruth { outcomes, may_raise: false }
}

const UNDECIDED: Outcomes = Outcomes { truth: true, falsity: true, null: false };

fn truth_in(schema: &str, table: &str, sql: &str) -> Result<ExpressionTruth, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(schema)?;
    let table =
        db.table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)?.unwrap();
    Ok(table.expression_truth(&db, &expression(sql), IdentifierCase::AsWritten)?)
}

fn truth(sql: &str) -> Result<ExpressionTruth, Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE app.invoices (id INT NOT NULL, tenant_id INT, \"ID\" INT);
         CREATE TABLE elsewhere (id INT);",
    )?;
    let table = db
        .table_by_target(
            TargetName::new("invoices", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )?
        .unwrap();
    Ok(table.expression_truth(&db, &expression(sql), IdentifierCase::AsWritten)?)
}

#[test]
fn proven_truths() -> Result<(), Error> {
    for sql in [
        "true",
        "NOT false",
        "1 = 1",
        "1.0 = 1",
        "01 = 1",
        "1 = 1.0",
        "'1' = 1",
        "true = true",
        "'tenant' = 'tenant'",
        "false OR true",
        "true AND true",
        "NULL OR true",
        "id > 5 OR id <= 5",
        "id IS NOT NULL",
        "ID IS NOT NULL",
        "(id) IS NOT NULL",
        "invoices.id IS NOT NULL",
        "app.invoices.id IS NOT NULL",
        "tenant_id IS NULL OR tenant_id IS NOT NULL",
        "tenant_id IS NOT NULL OR tenant_id IS NULL",
        "NOT (tenant_id = 7 AND false)",
        "(tenant_id = 7 OR true) AND id IS NOT NULL",
        "NOT NOT ((tenant_id) IS NULL OR tenant_id IS NOT NULL)",
    ] {
        assert_eq!(truth(sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    Ok(())
}

#[test]
fn proven_falsities() -> Result<(), Error> {
    for sql in [
        "false",
        "NOT true",
        "1 = 0",
        "true = false",
        "false OR false",
        "true AND false",
        "NULL AND false",
        "'a' = 'A'",
        "id IS NULL",
        "NOT (id IS NOT NULL)",
        "id > 10 AND id < 5",
        "id BETWEEN 1 AND 10 AND id IN (20, 30)",
        "id IS NOT NULL AND id <> id",
    ] {
        assert_eq!(truth(sql)?, never_raising(Outcomes::FALSE), "{sql}");
    }
    Ok(())
}

#[test]
fn null_outcomes_are_told_apart() -> Result<(), Error> {
    for sql in [
        "NULL",
        "NULL = NULL",
        "NULL = 1",
        "1 = NULL",
        "NOT (NULL = NULL)",
        "NULL AND true",
        "NULL OR false",
    ] {
        assert_eq!(truth(sql)?, never_raising(Outcomes::NULL), "{sql}");
    }
    assert_eq!(truth("id NOT IN (1, NULL)")?, never_raising(Outcomes::NOT_TRUE));
    // A `CHECK` admits every row this answers, a policy only those where
    // `tenant_id` is not `NULL`.
    assert_eq!(truth("tenant_id > 5 OR tenant_id <= 5")?, never_raising(Outcomes::NOT_FALSE));
    for sql in ["tenant_id = 7", "tenant_id > 7"] {
        assert_eq!(truth(sql)?, never_raising(Outcomes::ANY), "{sql}");
    }
    Ok(())
}

#[test]
fn overflow_always_raises() -> Result<(), Error> {
    assert_eq!(
        truth("2147483647 + 1 > 0")?,
        ExpressionTruth { outcomes: Outcomes::NONE, may_raise: true },
    );
    Ok(())
}

#[test]
fn unresolved_and_nullable_references_stay_undecided() -> Result<(), Error> {
    for sql in [
        "tenant_id IS NULL",
        "tenant_id IS NOT NULL",
        "unknown_column IS NOT NULL",
        "\"ID\" IS NOT NULL",
        "elsewhere.id IS NOT NULL",
        "other.invoices.id IS NOT NULL",
        "missing IS NULL OR missing IS NOT NULL",
        "elsewhere.id IS NULL OR elsewhere.id IS NOT NULL",
        "NOT (tenant_id IS NOT NULL OR false)",
        "id IS NULL OR tenant_id IS NOT NULL",
        "tenant_id IS NULL OR \"ID\" IS NOT NULL",
    ] {
        let truth = truth(sql)?;
        assert!(truth.outcomes.truth && truth.outcomes.falsity, "{sql}: {truth:?}");
    }
    Ok(())
}

#[test]
fn numeric_literals_keep_their_precision() -> Result<(), Error> {
    let max = u128::MAX;
    assert_eq!(truth(&format!("0{max} = {max}"))?, never_raising(Outcomes::TRUE));
    assert_eq!(truth(&format!("{max} = {}", max - 1))?, never_raising(Outcomes::FALSE));
    for sql in [
        "0 = 340282366920938463463374607431768211456",
        "340282366920938463463374607431768211456 = 0",
    ] {
        assert_eq!(truth(sql)?, never_raising(Outcomes::FALSE), "{sql}");
    }
    Ok(())
}

#[test]
fn volatile_calls_that_may_return_null_stay_undecided() -> Result<(), Error> {
    assert_eq!(truth("random() IS NULL OR random() IS NOT NULL")?, never_raising(UNDECIDED));
    Ok(())
}

#[test]
fn calls_in_checks_resolve_where_the_evaluator_reads_copies() -> Result<(), Error> {
    let domain = "CREATE DOMAIN short AS TEXT CHECK (length(VALUE) BETWEEN 1 AND 50);
         CREATE TABLE invoices (code short NOT NULL);";
    assert_eq!(truth_in(domain, "invoices", "length(code) > 0")?, never_raising(Outcomes::TRUE));
    let table = "CREATE TABLE invoices (code TEXT NOT NULL, CHECK (length(code) BETWEEN 1 AND 5));";
    assert_eq!(truth_in(table, "invoices", "length(code) > 0")?, never_raising(Outcomes::TRUE));
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (code TEXT NOT NULL, CHECK (length(code) BETWEEN 1 AND 5 AND code <> ''));",
    )?;
    let check = db.tables().next().unwrap().check_constraints(&db)?.next().unwrap();
    assert!(!check.is_tautology(&db)?);
    assert!(!check.is_negation(&db)?);
    Ok(())
}

#[test]
fn calls_on_a_domain_run_the_overload_declared_for_it() -> Result<(), Error> {
    let schema = "CREATE DOMAIN email AS TEXT;
         CREATE FUNCTION label(email) RETURNS TEXT LANGUAGE sql IMMUTABLE CALLED ON NULL INPUT AS 'SELECT ''none''';
         CREATE FUNCTION label(TEXT) RETURNS TEXT LANGUAGE sql IMMUTABLE STRICT AS 'SELECT $1';
         CREATE TABLE invoices (contact email);";
    let truth = truth_in(schema, "invoices", "label(contact) IS NULL OR contact IS NOT NULL")?;
    assert!(truth.outcomes.falsity, "{truth:?}");
    Ok(())
}

#[test]
fn policy_expressions_can_be_analyzed_independently() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (id INT NOT NULL, tenant_id INT);
         CREATE POLICY public_rows ON invoices USING (NOT false);
         CREATE POLICY tenant_rows ON invoices USING (tenant_id = 7);
         CREATE POLICY unchecked_writes ON invoices USING (tenant_id = 7) WITH CHECK (1 = 1);
         CREATE POLICY denied ON invoices USING (false);
         CREATE POLICY only_check ON invoices WITH CHECK (NOT false);
         CREATE POLICY nullable_writes ON invoices USING (NOT false) WITH CHECK (tenant_id IS NOT NULL);",
    )?;
    for policy in db.policies() {
        let table = policy.table(&db)?;
        let using = policy
            .using_expression(&db)?
            .map(|expr| table.expression_truth(&db, expr, IdentifierCase::AsWritten))
            .transpose()?
            .map(|truth| truth.outcomes);
        let check = policy
            .check_expression(&db)?
            .map(|expr| table.expression_truth(&db, expr, IdentifierCase::AsWritten))
            .transpose()?
            .map(|truth| truth.outcomes);
        let expected = match policy.name() {
            "public_rows" => (Some(Outcomes::TRUE), None),
            "tenant_rows" => (Some(Outcomes::ANY), None),
            "unchecked_writes" => (Some(Outcomes::ANY), Some(Outcomes::TRUE)),
            "denied" => (Some(Outcomes::FALSE), None),
            "only_check" => (None, Some(Outcomes::TRUE)),
            "nullable_writes" => (Some(Outcomes::TRUE), Some(UNDECIDED)),
            _ => unreachable!(),
        };
        assert_eq!((using, check), expected, "{}", policy.name());
    }
    Ok(())
}

#[test]
fn checks_are_judged_by_the_rows_they_admit() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (
            id INT NOT NULL,
            tenant_id INT,
            CHECK (NULL = NULL), CHECK (1 = 1.0), CHECK (NOT false),
            CHECK (invoices.id IS NOT NULL), CHECK (2147483647 + id > 0),
            CHECK (tenant_id < 0 AND tenant_id > 0), CHECK (id < 0 AND id > 0)
         );",
    )?;
    let table = db.tables().next().unwrap();
    let results = table
        .check_constraints(&db)?
        .map(|check| Ok((check.is_tautology(&db)?, check.is_negation(&db)?)))
        .collect::<Result<Vec<_>, Error>>()?;
    assert_eq!(
        results,
        [
            (true, false),
            (true, false),
            (true, false),
            (true, false),
            (false, false),
            (false, false),
            (false, true),
        ]
    );
    Ok(())
}

#[test]
fn table_checks_seed_expressions_but_not_each_other() -> Result<(), Error> {
    let schema = "CREATE TABLE invoices (tenant_id INT, CHECK (tenant_id > 0));";
    assert_eq!(truth_in(schema, "invoices", "tenant_id < 0")?, never_raising(Outcomes::NOT_TRUE));
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (id INT NOT NULL, CHECK (id > 0), CHECK (id < 0));",
    )?;
    let table = db.tables().next().unwrap();
    let checks: Vec<_> = table.check_constraints(&db)?.collect();
    for check in &checks {
        assert!(!check.is_negation(&db)?, "{}", check.expression(&db));
        assert_eq!(check.truth_under_other_checks(&db)?, never_raising(Outcomes::FALSE));
    }
    // The two checks together admit no row, so no expression takes an
    // outcome on one.
    assert_eq!(
        table.expression_truth(&db, &expression("id = 1"), IdentifierCase::AsWritten)?,
        never_raising(Outcomes::NONE)
    );
    Ok(())
}

#[test]
fn a_check_written_twice_is_redundant_beside_its_twin_only() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE invoices (tenant_id INT, CHECK (tenant_id > 0), CHECK (tenant_id > 0));",
    )?;
    let table = db.tables().next().unwrap();
    for check in table.check_constraints(&db)? {
        assert!(!check.is_tautology(&db)?);
        assert_eq!(check.truth_under_other_checks(&db)?, never_raising(Outcomes::NOT_FALSE));
    }
    Ok(())
}

#[test]
fn checks_postgresql_does_not_enforce_on_every_row_seed_nothing() -> Result<(), Error> {
    for schema in [
        "CREATE TABLE invoices (tenant_id INT);
         ALTER TABLE invoices ADD CONSTRAINT positive CHECK (tenant_id > 0) NOT VALID;",
        "CREATE TABLE invoices (tenant_id INT, CHECK (tenant_id > 0) NOT ENFORCED);",
        "CREATE TABLE invoices (tenant_id INT, CHECK (tenant_id > 0) NO INHERIT);",
    ] {
        assert_eq!(
            truth_in(schema, "invoices", "tenant_id < 0")?,
            never_raising(Outcomes::ANY),
            "{schema}"
        );
    }
    Ok(())
}

#[test]
fn checks_that_may_answer_differently_later_seed_nothing() -> Result<(), Error> {
    let functions =
        "CREATE FUNCTION fixed() RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE FUNCTION today() RETURNS boolean LANGUAGE sql STABLE AS 'SELECT true';";
    let seeded = format!(
        "{functions} CREATE TABLE invoices (tenant_id INT, CHECK (tenant_id > 0 AND fixed()));"
    );
    assert_eq!(truth_in(&seeded, "invoices", "tenant_id < 0")?, never_raising(Outcomes::NOT_TRUE));
    for check in [
        "tenant_id > 0 AND today()",
        "tenant_id > 0 AND random() < 2",
        "tenant_id > 0 AND CURRENT_DATE IS NOT NULL",
    ] {
        let table = format!("{functions} CREATE TABLE invoices (tenant_id INT, CHECK ({check}));");
        assert_eq!(
            truth_in(&table, "invoices", "tenant_id < 0")?,
            never_raising(Outcomes::ANY),
            "{check}"
        );
        let domain = format!(
            "{functions} CREATE DOMAIN positive AS INT CHECK ({});
             CREATE TABLE invoices (tenant_id positive);",
            check.replace("tenant_id", "VALUE")
        );
        assert_eq!(
            truth_in(&domain, "invoices", "tenant_id < 0")?,
            never_raising(Outcomes::ANY),
            "{check}"
        );
    }
    Ok(())
}

#[test]
fn check_calls_resolve_under_the_path_the_check_was_created_with() -> Result<(), Error> {
    let schema = "CREATE SCHEMA pinned;
         CREATE FUNCTION pinned.ok() RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE FUNCTION public.ok() RETURNS boolean LANGUAGE sql VOLATILE AS 'SELECT true';
         SET search_path = pinned, public;
         CREATE TABLE public.invoices (tenant_id INT, CHECK (ok() IS NULL OR ok() IS NOT NULL));
         SET search_path = public;";
    let db = ParserDB::parse::<PostgreSqlDialect>(schema)?;
    let check = db.tables().next().unwrap().check_constraints(&db)?.next().unwrap();
    assert!(check.is_tautology(&db)?);
    Ok(())
}

#[test]
fn checks_whose_calls_the_database_path_reads_otherwise_seed_nothing() -> Result<(), Error> {
    let schema = "CREATE SCHEMA pinned;
         CREATE FUNCTION pinned.ok() RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE FUNCTION public.ok() RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         SET search_path = pinned, public;
         CREATE TABLE public.invoices (tenant_id INT, CHECK (tenant_id > 0 AND ok()));
         SET search_path = public;";
    for sql in ["tenant_id > 0 AND ok()", "tenant_id < 0"] {
        assert_eq!(truth_in(schema, "invoices", sql)?, never_raising(Outcomes::ANY), "{sql}");
    }
    Ok(())
}

#[test]
fn checks_whose_calls_the_read_check_resolves_otherwise_seed_nothing() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA pinned;
         CREATE FUNCTION public.f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE FUNCTION pinned.f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 0';
         CREATE TABLE public.invoices (n INT, CONSTRAINT public_positive CHECK (f() > 0));
         SET search_path = pinned, public;
         ALTER TABLE public.invoices ADD CONSTRAINT pinned_positive CHECK (f() > 0);
         SET search_path = public;",
    )?;
    let table = db.tables().next().unwrap();
    for check in table.check_constraints(&db)? {
        assert_eq!(check.truth_under_other_checks(&db)?, never_raising(Outcomes::ANY));
    }
    Ok(())
}

#[test]
fn numeric_columns_may_hold_nan_above_every_number() -> Result<(), Error> {
    let schema = "CREATE TABLE invoices (amount NUMERIC(3, 1) NOT NULL);";
    assert_eq!(truth_in(schema, "invoices", "amount > -100")?, never_raising(Outcomes::TRUE));
    assert_eq!(truth_in(schema, "invoices", "amount < 100")?, never_raising(UNDECIDED));
    Ok(())
}

#[test]
fn integer_columns_stay_within_their_width() -> Result<(), Error> {
    let schema = "CREATE TABLE invoices (small SMALLINT NOT NULL, big BIGINT NOT NULL);";
    assert_eq!(truth_in(schema, "invoices", "small <= 32767")?, never_raising(Outcomes::TRUE));
    assert_eq!(truth_in(schema, "invoices", "small > 40000")?, never_raising(Outcomes::FALSE));
    assert_eq!(
        truth_in(schema, "invoices", "big > 9223372036854775807")?,
        never_raising(Outcomes::FALSE)
    );
    Ok(())
}

#[test]
fn dates_timestamps_and_arrays_parse_the_literals_they_meet() -> Result<(), Error> {
    let schema = "CREATE TABLE invoices (
            issued DATE NOT NULL,
            stamped TIMESTAMP NOT NULL,
            lines INT[] NOT NULL
         );";
    let raising = ExpressionTruth { outcomes: Outcomes::NONE, may_raise: true };
    for sql in ["issued > 'nonsense'", "stamped > 'nonsense'", "lines = 'nonsense'"] {
        assert_eq!(truth_in(schema, "invoices", sql)?, raising, "{sql}");
    }
    assert_eq!(truth_in(schema, "invoices", "issued = issued")?, never_raising(Outcomes::TRUE));
    let element = truth_in(schema, "invoices", "lines[1] > 2147483647")?;
    assert!(!element.outcomes.truth, "{element:?}");
    Ok(())
}

#[test]
fn catalog_functions_fold_their_constant_arguments() -> Result<(), Error> {
    for sql in [
        "btrim('  a ') = 'a'",
        "octet_length('ab') = 2",
        "abs(-3) = 3",
        "concat('a', NULL, 'b') = 'ab'",
        "concat_ws('-', 'a', 'b') = 'a-b'",
    ] {
        assert_eq!(truth(sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    Ok(())
}

#[test]
fn count_never_returns_null() -> Result<(), Error> {
    assert_eq!(truth("count(tenant_id) IS NOT NULL")?, never_raising(Outcomes::TRUE));
    Ok(())
}

#[test]
fn names_no_reader_resolves_leave_the_expression_undecided() -> Result<(), Error> {
    let schema =
        "CREATE FUNCTION label(INT) RETURNS INT LANGUAGE sql IMMUTABLE STRICT AS 'SELECT 1';
         CREATE FUNCTION label(TEXT) RETURNS INT LANGUAGE sql IMMUTABLE STRICT AS 'SELECT 1';
         CREATE TABLE invoices (id INT NOT NULL);";
    for sql in ["a.b.c.label(1) IS NULL", "label(missing) IS NOT NULL", "$1 IS NOT NULL"] {
        let truth = truth_in(schema, "invoices", sql)?;
        assert!(truth.outcomes.truth && truth.outcomes.falsity, "{sql}: {truth:?}");
    }
    Ok(())
}

#[test]
fn stable_calls_answer_alike_within_a_statement() -> Result<(), Error> {
    let schema = "CREATE FUNCTION stable_level() RETURNS INT LANGUAGE sql STABLE AS 'SELECT 1';
         CREATE FUNCTION volatile_level() RETURNS INT LANGUAGE sql VOLATILE AS 'SELECT 1';
         CREATE TABLE invoices (id INT);";
    assert_eq!(
        truth_in(schema, "invoices", "stable_level() IS NULL OR stable_level() IS NOT NULL")?,
        never_raising(Outcomes::TRUE)
    );
    assert_eq!(
        truth_in(schema, "invoices", "volatile_level() IS NULL OR volatile_level() IS NOT NULL")?,
        never_raising(UNDECIDED)
    );
    Ok(())
}

#[test]
fn checks_writing_coalesce_or_nullif_seed() -> Result<(), Error> {
    for check in ["n > 0 AND coalesce(n, 1) IS NOT NULL", "n > 0 AND nullif(n, 5) IS NOT NULL"] {
        let schema = format!("CREATE TABLE invoices (n INT NOT NULL, CHECK ({check}));");
        assert_eq!(truth_in(&schema, "invoices", "n <= 0")?, never_raising(Outcomes::FALSE));
    }
    Ok(())
}

#[test]
fn domain_checks_seed_their_columns() -> Result<(), Error> {
    let schema = "CREATE DOMAIN positive AS INT CHECK (VALUE > 0);
         CREATE DOMAIN small_positive AS positive CHECK (VALUE < 10);
         CREATE TABLE invoices (amount positive, required small_positive NOT NULL);";
    assert_eq!(truth_in(schema, "invoices", "amount <= 0")?, never_raising(Outcomes::NOT_TRUE));
    assert_eq!(
        truth_in(schema, "invoices", "required BETWEEN 1 AND 9")?,
        never_raising(Outcomes::TRUE)
    );
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE DOMAIN positive AS INT CHECK (VALUE > 0);
         CREATE TABLE invoices (amount positive NOT NULL, CHECK (amount > 0));",
    )?;
    let check = db.tables().next().unwrap().check_constraints(&db)?.next().unwrap();
    assert!(check.is_tautology(&db)?);
    Ok(())
}

#[test]
fn text_comparisons_follow_the_collation() -> Result<(), Error> {
    let schema =
        "CREATE COLLATION ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
         CREATE TABLE notes (
            plain TEXT NOT NULL,
            icu TEXT COLLATE \"und-x-icu\" NOT NULL,
            icu_too TEXT COLLATE \"und-x-icu\" NOT NULL,
            bytes TEXT COLLATE \"C\" NOT NULL,
            custom TEXT COLLATE ci NOT NULL,
            short VARCHAR(3) NOT NULL,
            named_default TEXT COLLATE \"default\" NOT NULL
         );";
    for sql in [
        "plain = 'a' AND plain = 'A'",
        "named_default = 'a' AND named_default = 'A'",
        "icu = 'a' AND icu = 'A'",
        "plain < short AND short < plain",
        "icu < icu_too AND icu_too < icu",
        "icu < plain AND plain < icu",
        "bytes = 'a' AND bytes < 'B'",
    ] {
        assert_eq!(truth_in(schema, "notes", sql)?, never_raising(Outcomes::FALSE), "{sql}");
    }
    for sql in ["custom = 'a' AND custom = 'A'", "plain = 'a' AND plain < 'B'"] {
        let undecided = truth_in(schema, "notes", sql)?;
        assert!(undecided.outcomes.falsity && undecided.outcomes.truth, "{sql}: {undecided:?}");
    }
    assert_eq!(truth_in(schema, "notes", "length(short) > 3")?, never_raising(Outcomes::FALSE));
    Ok(())
}

#[test]
fn the_c_collation_is_the_one_current_user_carries() -> Result<(), Error> {
    let schema = "CREATE TABLE notes (
            bytes TEXT COLLATE \"C\" NOT NULL,
            posix TEXT COLLATE \"POSIX\" NOT NULL,
            icu TEXT COLLATE \"und-x-icu\" NOT NULL
         );";
    assert!(!truth_in(schema, "notes", "bytes = current_user")?.may_raise);
    for sql in ["posix = current_user", "icu = current_user"] {
        assert!(truth_in(schema, "notes", sql)?.may_raise, "{sql}");
    }
    Ok(())
}

#[test]
fn other_dialects_keep_only_shared_semantics() -> Result<(), Error> {
    let db = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE invoices (
            id INT NOT NULL,
            paid BOOLEAN NOT NULL,
            rate FLOAT NOT NULL,
            issued DATE NOT NULL
         );",
    )?;
    let table = db.tables().next().unwrap();
    let truth = |sql| table.expression_truth(&db, &expression(sql), IdentifierCase::AsWritten);
    assert_eq!(truth("id > 5 OR id <= 5")?, never_raising(Outcomes::TRUE));
    assert_eq!(truth("paid OR NOT paid")?, never_raising(Outcomes::TRUE));
    assert_eq!(truth("id IS NULL")?, never_raising(Outcomes::FALSE));
    assert_eq!(truth("rate IS NULL OR issued IS NULL")?, never_raising(Outcomes::FALSE));
    let text = truth("'a' = 'A'")?;
    assert!(text.outcomes.truth && text.outcomes.falsity, "{text:?}");
    Ok(())
}

#[test]
fn sqlite_calls_are_not_folded_with_postgresql_semantics() -> Result<(), Error> {
    let db = ParserDB::parse::<SQLiteDialect>("CREATE TABLE invoices (id INT NOT NULL);")?;
    let table = db.tables().next().unwrap();
    let truth =
        table.expression_truth(&db, &expression("abs(-3) = 3"), IdentifierCase::AsWritten)?;
    assert!(truth.outcomes.truth && truth.outcomes.falsity, "{truth:?}");
    Ok(())
}

#[test]
fn mysql_text_may_compare_without_case() -> Result<(), Error> {
    let db = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE notes (
            plain TEXT NOT NULL,
            encoded TEXT CHARACTER SET utf8mb4 NOT NULL,
            named TEXT COLLATE utf8mb4_general_ci NOT NULL
         );",
    )?;
    let table = db.tables().next().unwrap();
    for column in ["plain", "encoded", "named"] {
        let sql = format!("{column} = 'a' AND {column} = 'A'");
        let truth = table.expression_truth(&db, &expression(&sql), IdentifierCase::AsWritten)?;
        assert!(truth.outcomes.truth, "{sql}: {truth:?}");
    }
    Ok(())
}

#[test]
fn callers_choose_identifier_comparison() -> Result<(), Error> {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (\"ID\" INT NOT NULL);")?;
    let table = db.tables().next().unwrap();
    let expr = expression("id IS NOT NULL");
    let truth = |case| table.expression_truth(&db, &expr, case).map(|truth| truth.outcomes);
    assert_eq!(truth(IdentifierCase::AsWritten)?, UNDECIDED);
    assert_eq!(truth(IdentifierCase::Folded)?, Outcomes::TRUE);
    assert_eq!(truth(IdentifierCase::Exact)?, UNDECIDED);
    Ok(())
}

#[test]
fn inherited_columns_keep_their_nullability() -> Result<(), Error> {
    let schema = "CREATE TABLE parent (id INT NOT NULL);
         CREATE TABLE child (tenant_id INT) INHERITS (parent);";
    assert_eq!(truth_in(schema, "child", "child.id IS NOT NULL")?, never_raising(Outcomes::TRUE));
    Ok(())
}

#[test]
fn generated_columns_hold_what_their_expression_evaluates_to() -> Result<(), Error> {
    let schema = "CREATE TABLE ownables (
         id INT NOT NULL,
         kind TEXT GENERATED ALWAYS AS ('note') STORED NOT NULL,
         label TEXT GENERATED ALWAYS AS ('note') STORED,
         computed TEXT GENERATED ALWAYS AS ('note') VIRTUAL,
         doubled INT GENERATED ALWAYS AS (id * 2) STORED,
         flag BOOLEAN GENERATED ALWAYS AS (true) STORED
     );";
    for sql in [
        "kind = 'project'",
        "label = 'folder'",
        "computed <> 'note'",
        "doubled IS NULL",
        "NOT flag",
    ] {
        assert_eq!(truth_in(schema, "ownables", sql)?, never_raising(Outcomes::FALSE), "{sql}");
    }
    for sql in ["kind = 'note'", "label IS NOT NULL", "computed = 'note'", "flag"] {
        assert_eq!(truth_in(schema, "ownables", sql)?, never_raising(Outcomes::TRUE), "{sql}");
    }
    Ok(())
}

#[test]
fn generated_values_the_column_type_rewrites_narrow_nothing() -> Result<(), Error> {
    // PostgreSQL 18 stores 'ab', 1.3 and 2.
    let schema = "CREATE TABLE rewritten (
         short VARCHAR(2) GENERATED ALWAYS AS ('ab  ') STORED,
         price NUMERIC(5, 1) GENERATED ALWAYS AS (1.25) STORED,
         code INT GENERATED ALWAYS AS (1.6) STORED
     );";
    for sql in ["short = 'ab'", "price = 1.3", "code = 2"] {
        assert!(truth_in(schema, "rewritten", sql)?.outcomes.truth, "{sql}");
    }
    Ok(())
}

#[test]
fn generated_columns_and_checks_narrow_each_other() -> Result<(), Error> {
    let schema = "CREATE TABLE ownables (
         id INT NOT NULL CHECK (id BETWEEN 1 AND 3),
         doubled INT GENERATED ALWAYS AS (id * 2) STORED,
         kind TEXT GENERATED ALWAYS AS ('note') STORED CHECK (kind = 'note')
     );";
    assert_eq!(truth_in(schema, "ownables", "doubled > 6")?, never_raising(Outcomes::FALSE));
    let db = ParserDB::parse::<PostgreSqlDialect>(schema)?;
    let table = db.tables().next().unwrap();
    let kind_check = table
        .check_constraints(&db)?
        .find(|check| check.expression(&db).to_string() == "kind = 'note'")
        .unwrap();
    assert!(kind_check.is_tautology(&db)?);
    Ok(())
}

#[test]
fn generated_calls_resolve_under_the_path_the_column_was_created_with() -> Result<(), Error> {
    // PostgreSQL 18 stores 5 and reads `abs(-5)` as 7 under the later path.
    let schema = "CREATE SCHEMA app;
         CREATE FUNCTION app.abs(n INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 7';
         CREATE TABLE shadowed (k INT GENERATED ALWAYS AS (abs(-5)) STORED);
         SET search_path TO app, pg_catalog;";
    assert_eq!(truth_in(schema, "shadowed", "k = 5")?, never_raising(Outcomes::TRUE));
    Ok(())
}

#[test]
fn enum_columns_narrow_to_the_labels_their_table_admits() -> Result<(), Error> {
    let schema = "CREATE SCHEMA authz;
         CREATE TYPE authz.resource_kind AS ENUM ('project', 'note', 'folder');
         CREATE TABLE generated (
             kind authz.resource_kind GENERATED ALWAYS AS ('note'::authz.resource_kind) STORED
         );
         CREATE TABLE checked (kind authz.resource_kind NOT NULL CHECK (kind = 'project'));
         CREATE TABLE listed (
             kind authz.resource_kind NOT NULL CHECK (kind IN ('project', 'folder'))
         );";
    for (table, sql, outcomes) in [
        ("generated", "kind = 'note'", Outcomes::TRUE),
        ("generated", "kind = 'project'", Outcomes::FALSE),
        ("generated", "kind = 'folder'", Outcomes::FALSE),
        ("checked", "kind = 'project'", Outcomes::TRUE),
        ("checked", "kind = 'note'", Outcomes::FALSE),
        ("listed", "kind = 'note'", Outcomes::FALSE),
        ("listed", "kind = 'folder'", UNDECIDED),
        ("checked", "kind < 'folder'", Outcomes::TRUE),
    ] {
        assert_eq!(truth_in(schema, table, sql)?, never_raising(outcomes), "{table}: {sql}");
    }
    assert!(truth_in(schema, "listed", "kind = 'bogus'")?.may_raise);
    Ok(())
}

#[test]
fn an_enum_is_one_type_however_its_name_is_written() -> Result<(), Error> {
    let schema = "CREATE SCHEMA authz;
         CREATE TYPE authz.resource_kind AS ENUM ('project', 'note');
         SET search_path TO authz, public;
         CREATE TABLE public.generated (
             kind authz.resource_kind GENERATED ALWAYS AS ('note'::resource_kind) STORED
         );";
    for (sql, outcomes) in [
        ("kind = 'project'", Outcomes::FALSE),
        ("kind = 'note'::authz.resource_kind", Outcomes::TRUE),
        ("kind = 'note'::resource_kind", Outcomes::TRUE),
    ] {
        assert_eq!(truth_in(schema, "generated", sql)?, never_raising(outcomes), "{sql}");
    }
    Ok(())
}

#[test]
fn an_absent_table_is_an_error_even_for_a_literal() -> Result<(), Error> {
    let owned = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (id INT);")?;
    let elsewhere = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE other (id INT);")?;
    let table = owned.tables().next().unwrap();
    assert!(matches!(
        table.expression_truth(&elsewhere, &expression("true"), IdentifierCase::AsWritten),
        Err(sql_traits::errors::LookupError::ObjectNotInDatabase { .. }),
    ));
    Ok(())
}

#[test]
fn check_truth_analysis_reports_absent_constraint_metadata() -> Result<(), Error> {
    let owned =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (id INT, CHECK (NOT false));")?;
    let elsewhere = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE invoices (id INT);")?;
    let table = owned.tables().next().unwrap();
    let check = table.check_constraints(&owned)?.next().unwrap();
    for result in [
        check.is_tautology(&elsewhere),
        check.is_negation(&elsewhere),
        check.truth_under_other_checks(&elsewhere).map(|truth| truth.outcomes.truth),
    ] {
        assert!(
            matches!(result, Err(sql_traits::errors::LookupError::ObjectNotInDatabase { .. }),)
        );
    }
    Ok(())
}
