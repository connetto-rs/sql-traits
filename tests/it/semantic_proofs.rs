//! Canonical text of `CHECK` and policy expressions read through
//! [`SemanticProofs`] against the text PostgreSQL 18's `pg_get_constraintdef`
//! and `pg_get_expr` deparse them into.
#![expect(
    clippy::unwrap_used,
    clippy::unreachable,
    reason = "Fixtures must parse and hold the nodes they navigate to."
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{ast::Expr, dialect::PostgreSqlDialect, parser::Parser};
use sqlparser_canonicalize::{Canonicalizer, SemanticContext};

const DDL: &str = "CREATE TYPE mood AS ENUM ('happy', 'sad');
CREATE DOMAIN dt AS TEXT;
CREATE DOMAIN dv AS VARCHAR(5);
CREATE TABLE u (x TEXT);
CREATE TABLE t (
    id INT PRIMARY KEY, s TEXT, i INT4, v VARCHAR(5), c CHAR(3), b INT8, h INT2, d dt, e dv,
    \"Mixed\" TEXT, m mood, n NUMERIC, day DATE
);";

fn parse(sql: &str) -> Expr {
    Parser::new(&PostgreSqlDialect {}).try_with_sql(sql).unwrap().parse_expr().unwrap()
}

fn table(db: &ParserDB) -> &<ParserDB as DatabaseLike>::Table {
    db.table_by_target(TargetName::new("t", false), IdentifierCase::Folded).unwrap().unwrap()
}

/// The canonical text of `check`, read as a check constraint of `t`.
fn canonical(db: &ParserDB, check: &str) -> String {
    let expression = parse(check);
    let scope = ExpressionScope::Table(table(db));
    let proofs =
        SemanticProofs::for_expression(&expression, &scope, db, IdentifierCase::Folded).unwrap();
    Canonicalizer::new(&PostgreSqlDialect {}).normalize_with_context(&proofs.view()).unwrap()
}

const USER_TYPES: &str = "CREATE SCHEMA app;
CREATE TYPE mood AS ENUM ('happy', 'sad');
CREATE TYPE app.tone AS ENUM ('low', 'high');
CREATE TYPE \"Mood\" AS ENUM ('a');
CREATE TYPE \"user\" AS ENUM ('a');
CREATE TYPE status AS ENUM ('a');
CREATE TYPE app.mood AS ENUM ('x');
CREATE TABLE t (m mood, ma mood[], o app.tone, q \"Mood\", u \"user\", st status);";

/// The canonical text of `check`, read as a check constraint of `t` by a
/// session whose `search_path` is `path`.
fn canonical_on_path(db: &ParserDB, check: &str, path: &[(&str, bool)]) -> String {
    let expression = parse(check);
    let scope = ExpressionScope::Table(table(db));
    let case = IdentifierCase::Folded;
    let proofs =
        SemanticProofs::for_expression_deparsed_on_path(&expression, &scope, db, case, path)
            .unwrap();
    Canonicalizer::new(&PostgreSqlDialect {}).normalize_with_context(&proofs.view()).unwrap()
}

#[test]
fn user_types_read_as_the_reading_session_deparses_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>(USER_TYPES).unwrap();
    let public: &[(&str, bool)] = &[("public", false)];
    let app_first: &[(&str, bool)] = &[("app", false), ("public", false)];
    let empty: &[(&str, bool)] = &[];
    for (path, declared, deparsed) in [
        (public, "m <> 'sad'", "(m <> 'sad'::mood)"),
        (public, "m IN ('happy', 'sad')", "(m = ANY (ARRAY['happy'::mood, 'sad'::mood]))"),
        (public, "o = 'low'", "(o = 'low'::app.tone)"),
        (public, "q = 'a'", "(q = 'a'::\"Mood\")"),
        (public, "u = 'a'", "(u = 'a'::\"user\")"),
        (public, "st = 'a'", "(st = 'a'::status)"),
        (public, "ma = '{happy}'", "(ma = '{happy}'::mood[])"),
        (public, "o IN ('low', 'high')", "(o = ANY (ARRAY['low'::app.tone, 'high'::app.tone]))"),
        (public, "m::text = 'happy'", "((m)::text = 'happy'::text)"),
        (public, "m = 'happy'::text::mood", "(m = ('happy'::text)::mood)"),
        (app_first, "m <> 'sad'", "(m <> 'sad'::public.mood)"),
        (
            app_first,
            "m IN ('happy', 'sad')",
            "(m = ANY (ARRAY['happy'::public.mood, 'sad'::public.mood]))",
        ),
        (app_first, "o = 'low'", "(o = 'low'::tone)"),
        (app_first, "q = 'a'", "(q = 'a'::\"Mood\")"),
        (app_first, "ma = '{happy}'", "(ma = '{happy}'::public.mood[])"),
        (app_first, "o IN ('low', 'high')", "(o = ANY (ARRAY['low'::tone, 'high'::tone]))"),
        (empty, "m <> 'sad'", "(m <> 'sad'::public.mood)"),
        (empty, "o = 'low'", "(o = 'low'::app.tone)"),
        (empty, "q = 'a'", "(q = 'a'::public.\"Mood\")"),
        (empty, "u = 'a'", "(u = 'a'::public.\"user\")"),
        (empty, "st = 'a'", "(st = 'a'::public.status)"),
        (empty, "ma = '{happy}'", "(ma = '{happy}'::public.mood[])"),
        (empty, "m::text = 'happy'", "((m)::text = 'happy'::text)"),
    ] {
        assert_eq!(
            canonical_on_path(&db, declared, path),
            canonical_on_path(&db, deparsed, path),
            "{declared} on {path:?}"
        );
    }
}

#[test]
fn a_catalog_type_an_earlier_schema_hides_keeps_its_spelling() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TYPE text AS ENUM ('a'); CREATE TABLE t (s pg_catalog.text);",
    )
    .unwrap();
    let scope = ExpressionScope::Table(table(&db));
    let expression = parse("s <> ''");
    let case = IdentifierCase::Folded;
    for (path, proved) in
        [(&[("public", false)][..], true), (&[("public", false), ("pg_catalog", false)][..], false)]
    {
        let proofs =
            SemanticProofs::for_expression_deparsed_on_path(&expression, &scope, &db, case, path)
                .unwrap();
        let view = proofs.view();
        let proved_now = view.operator_coercions(&expression).unwrap().is_some();
        assert_eq!(proved_now, proved, "{path:?}");
    }
}

#[test]
fn declared_checks_read_as_postgresql_deparses_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    assert_eq!(canonical(&db, "s <> ''"), "(s != CAST('' AS TEXT))");
    assert_eq!(
        canonical(&db, "s IN ('a', 'b')"),
        "s = ANY(ARRAY[CAST('a' AS TEXT), CAST('b' AS TEXT)])"
    );
    assert_eq!(canonical(&db, "i = '7'"), "(i = 7)");
    for (declared, deparsed) in [
        ("s <> ''", "(s <> ''::text)"),
        ("s IN ('a', 'b')", "(s = ANY (ARRAY['a'::text, 'b'::text]))"),
        ("i = '7'", "(i = 7)"),
        ("i = '007'", "(i = 7)"),
        ("i = ' 7'", "(i = 7)"),
        ("t.s <> ''", "(s <> ''::text)"),
        ("public.t.s <> ''", "(s <> ''::text)"),
        ("\"Mixed\" <> ''", "(\"Mixed\" <> ''::text)"),
        ("v = 'a'", "((v)::text = 'a'::text)"),
        (
            "v IN ('a', 'b')",
            "((v)::text = ANY ((ARRAY['a'::character varying, 'b'::character varying])::text[]))",
        ),
        ("i = '-7'", "(i = '-7'::integer)"),
        ("b = '7'", "(b = '7'::bigint)"),
        ("h = '07'", "(h = '7'::smallint)"),
        ("i = 1.5", "((i)::numeric = 1.5)"),
        ("c IN ('a', 'b')", "(c = ANY (ARRAY['a'::bpchar, 'b'::bpchar]))"),
        ("s IN ('a')", "(s = 'a'::text)"),
        ("i IN ('1', '2')", "(i = ANY (ARRAY[1, 2]))"),
        ("i IN (1, 2)", "(i = ANY (ARRAY[1, 2]))"),
        ("s = NULL", "(s = NULL::text)"),
        ("d = 'x'", "((d)::text = 'x'::text)"),
        ("e = 'a'", "((e)::text = 'a'::text)"),
        ("d IN ('a', 'b')", "((d)::text = ANY (ARRAY['a'::text, 'b'::text]))"),
        ("s NOT IN ('a', 'b')", "(s <> ALL (ARRAY['a'::text, 'b'::text]))"),
        (
            "s IN ('a', \"Mixed\", 'b')",
            "((s = ANY (ARRAY['a'::text, 'b'::text])) OR (s = \"Mixed\"))",
        ),
        ("s IN ('a', 'b') AND i > 0", "(((s = ANY (ARRAY['a'::text, 'b'::text])) AND (i > 0)))"),
        // A minus sign over a number is the one constant the grammar folds.
        ("i > -1", "(i > '-1'::integer)"),
        ("b > -1", "(b > '-1'::integer)"),
        ("h > -1", "(h > '-1'::integer)"),
        ("i > - (1)", "(i > '-1'::integer)"),
        ("i > -(-(1))", "(i > 1)"),
        ("i > -0", "(i > 0)"),
        ("i > -(-2147483648)", "(i > '2147483648'::bigint)"),
        ("b = -9223372036854775808", "(b = '-9223372036854775808'::bigint)"),
        ("b > -9223372036854775809", "((b)::numeric > '-9223372036854775809'::numeric)"),
        ("i IN (-1, -2)", "(i = ANY (ARRAY['-1'::integer, '-2'::integer]))"),
        ("h IN (-1, 2)", "(h = ANY (ARRAY['-1'::integer, 2]))"),
        ("n > -1.5", "(n > '-1.5'::numeric)"),
        ("n > -.5", "(n > '-0.5'::numeric)"),
        ("n > -00.50", "(n > '-0.50'::numeric)"),
        ("n > -5.", "(n > '-5'::numeric)"),
        ("n > -(-5.)", "(n > '5'::numeric)"),
        ("n > -(-1.5)", "(n > 1.5)"),
        ("n > -0.00", "(n > 0.00)"),
        ("n > -99999999999999999999", "(n > '-99999999999999999999'::numeric)"),
        // A conversion goes under a `COLLATE`, which a type without a
        // collation drops.
        ("s = 'a' COLLATE \"C\"", "(s = ('a'::text COLLATE \"C\"))"),
        ("v = 'a' COLLATE \"C\"", "((v)::text = ('a'::text COLLATE \"C\"))"),
        ("v COLLATE \"C\" = 'a'", "(((v)::text COLLATE \"C\") = 'a'::text)"),
        ("c = 'a' COLLATE \"C\"", "(c = ('a'::bpchar COLLATE \"C\"))"),
        ("d = 'x' COLLATE \"C\"", "((d)::text = ('x'::text COLLATE \"C\"))"),
        ("d COLLATE \"C\" = 'x'", "(((d)::text COLLATE \"C\") = 'x'::text)"),
        ("i = '1' COLLATE \"C\"", "(i = 1)"),
        ("i = '-1' COLLATE \"C\"", "(i = '-1'::integer)"),
        ("b = '7' COLLATE \"C\"", "(b = '7'::bigint)"),
        ("day = '2020-01-01' COLLATE \"C\"", "(day = '2020-01-01'::date)"),
        ("i = ('7')", "(i = 7)"),
        ("i IN ('1' COLLATE \"C\", '2')", "(i = ANY (ARRAY[1, 2]))"),
        (
            "v IN ('a' COLLATE \"C\", 'b')",
            "((v)::text = ANY ((ARRAY[('a'::character varying COLLATE \"C\"), \
             'b'::character varying])::text[]))",
        ),
        // A policy, whose subquery keeps the qualified names it writes.
        (
            "EXISTS (SELECT 1 FROM u WHERE u.x = t.s AND u.x <> '')",
            "(EXISTS ( SELECT 1 FROM u WHERE ((u.x = t.s) AND (u.x <> ''::text))))",
        ),
    ] {
        assert_eq!(canonical(&db, declared), canonical(&db, deparsed), "{declared}");
    }
}

/// Every other binary operator, `LIKE`, `ILIKE` and `SIMILAR TO` read as the
/// operators and escape functions they become, and `BETWEEN` as its
/// comparisons with each bound.
#[test]
fn operators_matches_and_ranges_read_as_postgresql_deparses_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    for (declared, deparsed) in [
        ("s || 'x' <> ''", "(((s || 'x'::text) <> ''::text))"),
        ("v || 'x' <> ''", "((((v)::text || 'x'::text) <> ''::text))"),
        ("i + '1' > 0", "(((i + 1) > 0))"),
        ("n / 2 > 1", "(((n / (2)::numeric) > (1)::numeric))"),
        (
            "2 ^ i > 1",
            "((((2)::double precision ^ (i)::double precision) > (1)::double precision))",
        ),
        ("i % 2 = 0", "(((i % 2) = 0))"),
        ("s ~ '^a'", "((s ~ '^a'::text))"),
        ("s !~* '^a'", "((s !~* '^a'::text))"),
        ("s LIKE 'a%'", "((s ~~ 'a%'::text))"),
        ("s NOT LIKE 'a%'", "((s !~~ 'a%'::text))"),
        ("s ILIKE 'a%'", "((s ~~* 'a%'::text))"),
        ("s NOT ILIKE 'a%'", "((s !~~* 'a%'::text))"),
        ("s LIKE 'a!%' ESCAPE '!'", "((s ~~ like_escape('a!%'::text, '!'::text)))"),
        ("s SIMILAR TO 'a%'", "((s ~ similar_to_escape('a%'::text)))"),
        ("s NOT SIMILAR TO 'a%' ESCAPE '!'", "((s !~ similar_to_escape('a%'::text, '!'::text)))"),
        ("v LIKE 'a%'", "(((v)::text ~~ 'a%'::text))"),
        ("c LIKE 'a%'", "((c ~~ 'a%'::text))"),
        ("v SIMILAR TO 'a%'", "(((v)::text ~ similar_to_escape('a%'::text)))"),
        ("s LIKE s || '%'", "((s ~~ (s || '%'::text)))"),
        ("i BETWEEN 1 AND 10", "(((i >= 1) AND (i <= 10)))"),
        ("s BETWEEN 'a' AND 'z'", "(((s >= 'a'::text) AND (s <= 'z'::text)))"),
        ("i NOT BETWEEN '1' AND '10'", "(((i < 1) OR (i > 10)))"),
        ("v BETWEEN 'a' AND 'z'", "((((v)::text >= 'a'::text) AND ((v)::text <= 'z'::text)))"),
        ("n BETWEEN 1 AND 2.5", "(((n >= (1)::numeric) AND (n <= 2.5)))"),
        ("i BETWEEN -1 AND 1", "(((i >= '-1'::integer) AND (i <= 1)))"),
    ] {
        assert_eq!(canonical(&db, declared), canonical(&db, deparsed), "{declared}");
    }
}

/// `IN` list elements of another type than the array PostgreSQL builds read
/// as the casts the deparse writes, mixed fixed and varying character types
/// included, with the blanks of each constant kept as written.
#[test]
fn converted_list_elements_read_as_postgresql_deparses_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    for (declared, deparsed) in [
        ("c IN ('ab ', 'x'::text)", "(c = ANY (ARRAY['ab '::bpchar, ('x'::text)::bpchar]))"),
        (
            "c IN ('ab '::text, 'x'::text)",
            "(c = ANY (ARRAY[('ab '::text)::bpchar, ('x'::text)::bpchar]))",
        ),
        (
            "c IN ('ab '::text, 'ab '::text)",
            "(c = ANY (ARRAY[('ab '::text)::bpchar, ('ab '::text)::bpchar]))",
        ),
        ("c IN ('ab '::text)", "((c)::text = 'ab '::text)"),
        ("s IN ('ab ', 'x')", "(s = ANY (ARRAY['ab '::text, 'x'::text]))"),
        (
            "c IN ('x'::character(2), 'ab '::character varying)",
            "(c = ANY (ARRAY['x'::character(2), ('ab '::character varying)::bpchar]))",
        ),
        (
            "s IN ('ab '::character(3), 'x')",
            "(s = ANY (ARRAY[('ab '::character(3))::text, 'x'::text]))",
        ),
        (
            "s IN ('ab '::character varying, 'x'::character(2))",
            "(s = ANY (ARRAY[('ab '::character varying)::text, ('x'::character(2))::text]))",
        ),
        (
            "d IN ('a'::text, 'b'::character varying)",
            "((d)::text = ANY (ARRAY['a'::text, ('b'::character varying)::text]))",
        ),
        ("i IN (1, 2.5)", "((i)::numeric = ANY (ARRAY[(1)::numeric, 2.5]))"),
    ] {
        assert_eq!(canonical(&db, declared), canonical(&db, deparsed), "{declared}");
    }
}

/// `ANY`, `SOME` and `ALL` comparisons with arrays read as the conversions
/// PostgreSQL applies to the tested operand, the array and its elements.
#[test]
fn quantified_comparisons_read_as_postgresql_deparses_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    for (declared, deparsed) in [
        (
            "c = ANY (ARRAY['ab '::varchar, 'x'])",
            "(c = ANY ((ARRAY['ab '::character varying, 'x'::character varying])::bpchar[]))",
        ),
        (
            "c = ANY (ARRAY['x'::char(2), 'ab '::varchar])",
            "(c = ANY (ARRAY['x'::character(2), ('ab '::character varying)::bpchar]))",
        ),
        ("c = ANY (ARRAY['ab '::text])", "((c)::text = ANY (ARRAY['ab '::text]))"),
        ("c::text = ANY (ARRAY['ab ', 'x'])", "((c)::text = ANY (ARRAY['ab '::text, 'x'::text]))"),
        ("s = ANY (ARRAY['ab ', 'x'])", "(s = ANY (ARRAY['ab '::text, 'x'::text]))"),
        ("s = SOME (ARRAY['a'])", "(s = ANY (ARRAY['a'::text]))"),
        ("v = ANY (ARRAY['a', 'b'])", "((v)::text = ANY (ARRAY['a'::text, 'b'::text]))"),
        (
            "s <> ALL (ARRAY['a'::varchar, 'b'])",
            "(s <> ALL ((ARRAY['a'::character varying, 'b'::character varying])::text[]))",
        ),
        ("s = ANY (ARRAY['a', 'b']::text[])", "(s = ANY (ARRAY['a'::text, 'b'::text]))"),
        ("s = ANY ((ARRAY['a'])::text[])", "(s = ANY (ARRAY['a'::text]))"),
    ] {
        assert_eq!(canonical(&db, declared), canonical(&db, deparsed), "{declared}");
    }
}

#[test]
fn unproved_operations_and_lists_keep_their_spelling() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    let scope = ExpressionScope::Table(table(&db));
    let proved = |check: &str| {
        let expression = parse(check);
        let proofs =
            SemanticProofs::for_expression(&expression, &scope, &db, IdentifierCase::Folded)
                .unwrap();
        let view = proofs.view();
        (
            view.operator_coercions(&expression).unwrap().is_some(),
            view.pattern_coercions(&expression).unwrap().is_some(),
            view.range_coercions(&expression).unwrap().is_some(),
            view.membership_coercions(&expression).unwrap().is_some(),
            view.quantified_coercions(&expression).unwrap().is_some(),
        )
    };
    // No `=` takes `text` and `int4`. A cast to `mood` is spelled
    // as the deparsing session's `search_path` reaches the type, a converted
    // negative constant as a cast of its quoted cast, and a negated exponent
    // as the digits `numeric_in` expands it into. No `~~` takes `int4`, and
    // no `<=` takes `text` and `int4`.
    for check in [
        "s = 1",
        "m = 'happy'",
        "n = -1",
        "n > -1e3",
        "s IN (1)",
        "i IN (1, $1)",
        "n IN (-1, 2)",
        "n IN (-1e3, -2e3)",
        "i LIKE 'a%'",
        "s SIMILAR TO 1",
        "s LIKE 'a' ESCAPE 1",
        "s BETWEEN 'a' AND 1",
        "s LIKE ANY (ARRAY['a%'])",
        "s = ANY (ARRAY[1])",
        "s = ANY ('{a,b}')",
        "c = ANY (ARRAY['ab ']::char(2)[])",
        "n = ANY (ARRAY[-1, 2.5])",
        "s = ANY (ARRAY['a', 1]::text[])",
        "s = ANY (ARRAY[NULL::mood])",
        "m BETWEEN 'happy' AND 'sad'",
    ] {
        assert_eq!(proved(check), (false, false, false, false, false), "{check}");
    }
}

#[test]
fn a_trigger_condition_keeps_its_row_qualifications() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    let scope = ExpressionScope::Trigger(table(&db));
    let expression = parse("NEW.s <> ''");
    let proofs =
        SemanticProofs::for_expression(&expression, &scope, &db, IdentifierCase::Folded).unwrap();
    let view = proofs.view();
    let Expr::BinaryOp { left, .. } = &expression else { unreachable!() };
    assert!(view.column_reference(left).unwrap().is_none());
    assert!(view.operator_coercions(&expression).unwrap().is_some());
}

#[test]
fn a_missing_column_keeps_its_spelling() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    let scope = ExpressionScope::Table(table(&db));
    let expression = parse("t.missing <> ''");
    let proofs =
        SemanticProofs::for_expression(&expression, &scope, &db, IdentifierCase::Folded).unwrap();
    let view = proofs.view();
    let Expr::BinaryOp { left, .. } = &expression else { unreachable!() };
    assert!(view.column_reference(left).unwrap().is_none());
    assert!(view.operator_coercions(&expression).unwrap().is_none());
}

#[test]
fn a_lookup_error_typing_an_operand_is_returned() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    let scope = ExpressionScope::Table(table(&db));
    let expression = parse("a.b.c.f() = 1");
    let proofs = SemanticProofs::for_expression(&expression, &scope, &db, IdentifierCase::Folded);
    assert!(matches!(proofs, Err(LookupError::InvalidObjectName { .. })), "{proofs:?}");
}

#[test]
fn a_node_of_another_expression_is_refused() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    let scope = ExpressionScope::Table(table(&db));
    let expression = parse("s <> ''");
    let proofs =
        SemanticProofs::for_expression(&expression, &scope, &db, IdentifierCase::Folded).unwrap();
    let view = proofs.view();
    let copy = expression.clone();
    assert!(view.operator_coercions(&copy).is_err());
    assert!(view.pattern_coercions(&copy).is_err());
    assert!(view.range_coercions(&copy).is_err());
    assert!(view.column_reference(&copy).is_err());
    assert!(view.membership_coercions(&copy).is_err());
}
