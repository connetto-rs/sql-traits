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
    \"Mixed\" TEXT, m mood
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
        // A policy, whose subquery keeps the qualified names it writes.
        (
            "EXISTS (SELECT 1 FROM u WHERE u.x = t.s AND u.x <> '')",
            "(EXISTS ( SELECT 1 FROM u WHERE ((u.x = t.s) AND (u.x <> ''::text))))",
        ),
    ] {
        assert_eq!(canonical(&db, declared), canonical(&db, deparsed), "{declared}");
    }
}

#[test]
fn unproved_comparisons_and_lists_keep_their_spelling() {
    let db = ParserDB::parse::<PostgreSqlDialect>(DDL).unwrap();
    let scope = ExpressionScope::Table(table(&db));
    // No `=` takes `text` and `int4`, and PostgreSQL casts `1` into the
    // `numeric` array, which no proof spells. A cast to `mood` is spelled
    // as the deparsing session's `search_path` reaches the type.
    for (check, comparison) in
        [("s = 1", true), ("m = 'happy'", true), ("s IN (1)", false), ("i IN (1, 2.5)", false)]
    {
        let expression = parse(check);
        let proofs =
            SemanticProofs::for_expression(&expression, &scope, &db, IdentifierCase::Folded)
                .unwrap();
        let view = proofs.view();
        if comparison {
            assert!(view.comparison_coercions(&expression).unwrap().is_none(), "{check}");
        } else {
            assert!(view.membership_coercions(&expression).unwrap().is_none(), "{check}");
        }
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
    assert!(view.comparison_coercions(&expression).unwrap().is_some());
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
    assert!(view.comparison_coercions(&expression).unwrap().is_none());
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
    assert!(view.comparison_coercions(&copy).is_err());
    assert!(view.column_reference(&copy).is_err());
    assert!(view.membership_coercions(&copy).is_err());
}
