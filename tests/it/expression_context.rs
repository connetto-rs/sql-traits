//! The type and the binding each expression node of a query takes, as
//! PostgreSQL 18 derives them.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::unreachable,
    reason = "Fixtures must hold the nodes they navigate to."
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{BinaryOperator, Expr, Query, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn query(sql: &str) -> Query {
    match Parser::parse_sql(&PostgreSqlDialect {}, sql).unwrap().remove(0) {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}

/// The expressions a query's outermost `SELECT` projects.
fn projected(query: &Query) -> Vec<&Expr> {
    let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    select
        .projection
        .iter()
        .map(|item| {
            match item {
                SelectItem::UnnamedExpr(expression)
                | SelectItem::ExprWithAlias { expr: expression, .. } => expression,
                other => panic!("expected an expression, got {other:?}"),
            }
        })
        .collect()
}

fn pg(name: &str) -> OperandType<'_> {
    OperandType::Known {
        type_name: TargetName::new(name, false).with_schema("pg_catalog", false),
        typmod: None,
    }
}

/// Literals, typed strings and casts take the type PostgreSQL 18 reports
/// for them with `pg_typeof`, and a cast its modifier.
#[test]
fn literals_and_casts_take_their_postgres_types() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let query = query(
        "SELECT 'a', 1, 2147483648, 9223372036854775808, 1.5, 1e3, true, NULL,
                'x'::varchar(5), CAST(1 AS int8), DATE '2024-01-01', ('x'), 'a' COLLATE \"C\",
                X'1F', B'01', N'n', $1, 0x1F, 0o17, 0b101, 1_000, 0xFFFFFFFFF,
                18446744073709551616, 'x'::geometry(Point, 4326), 'x'::nosuchtype[]",
    );
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let types: Vec<_> =
        projected(&query).into_iter().map(|node| context.type_of(node).unwrap()).collect();
    let varchar5 = OperandType::Known {
        type_name: TargetName::new("varchar", false).with_schema("pg_catalog", false),
        typmod: Some(9),
    };
    assert_eq!(
        types,
        [
            OperandType::UntypedLiteral,
            pg("int4"),
            pg("int8"),
            pg("numeric"),
            pg("numeric"),
            pg("numeric"),
            pg("bool"),
            OperandType::UntypedLiteral,
            varchar5,
            pg("int8"),
            pg("date"),
            OperandType::UntypedLiteral,
            OperandType::UntypedLiteral,
            pg("bit"),
            pg("bit"),
            pg("bpchar"),
            OperandType::Unknown,
            pg("int4"),
            pg("int4"),
            pg("int4"),
            pg("int4"),
            pg("int8"),
            pg("numeric"),
            // A modifier the model cannot encode, and a type the database
            // cannot name, leave the cast's type unknown.
            OperandType::Unknown,
            OperandType::Unknown,
        ]
    );
}

/// A column reference binds in its own scope, so a table read again in a
/// nested `SELECT` gives the inner reference its own `FROM` item, and takes
/// the column's type and modifier.
#[test]
fn references_bind_and_type_in_their_own_scope() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE paper_shares (paper_id INT, viewer TEXT, weight INT, code VARCHAR(5));",
    )
    .unwrap();
    let query = query(
        "SELECT 1 FROM paper_shares AS s
         WHERE s.weight > (SELECT avg(weight) FROM paper_shares) AND s.code = 'a'",
    );
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let SetExpr::Select(outer) = query.body.as_ref() else { unreachable!() };
    let Some(Expr::BinaryOp { left: comparison, op: BinaryOperator::And, right: code_test }) =
        &outer.selection
    else {
        unreachable!()
    };
    let Expr::BinaryOp { left: outer_weight, right: subquery, .. } = comparison.as_ref() else {
        unreachable!()
    };
    let Expr::Subquery(subquery) = subquery.as_ref() else { unreachable!() };
    let SetExpr::Select(inner) = subquery.body.as_ref() else { unreachable!() };
    let SelectItem::UnnamedExpr(Expr::Function(avg)) = &inner.projection[0] else { unreachable!() };
    let sqlparser::ast::FunctionArguments::List(arguments) = &avg.args else { unreachable!() };
    let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(inner_weight)) =
        &arguments.args[0]
    else {
        unreachable!()
    };
    let Expr::BinaryOp { left: code, .. } = code_test.as_ref() else { unreachable!() };

    let (_, outer_binding) = context.reference(outer_weight).unwrap().unwrap();
    let (_, inner_binding) = context.reference(inner_weight).unwrap().unwrap();
    let (outer_entry, inner_entry) =
        (outer_binding.entry().unwrap(), inner_binding.entry().unwrap());
    assert!(core::ptr::eq(outer_entry.factor(), core::ptr::from_ref(&outer.from[0].relation)));
    assert!(core::ptr::eq(inner_entry.factor(), core::ptr::from_ref(&inner.from[0].relation)));
    assert_eq!(context.type_of(outer_weight).unwrap(), pg("int4"));
    assert_eq!(context.type_of(inner_weight).unwrap(), pg("int4"));
    assert_eq!(
        context.type_of(code).unwrap(),
        OperandType::Known {
            type_name: TargetName::new("varchar", false).with_schema("pg_catalog", false),
            typmod: Some(9),
        }
    );
    // A node that is no column reference binds nothing.
    assert!(context.reference(comparison).unwrap().is_none());
}

/// A reference that reaches no column, and an expression this context does
/// not type, take no type.
#[test]
fn unresolved_and_untyped_nodes_take_no_type() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE shares (weight INT);").unwrap();
    let query = query("SELECT nope, weight + 1 FROM shares");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let [missing, sum] = projected(&query)[..] else { unreachable!() };
    assert_eq!(context.type_of(missing).unwrap(), OperandType::Unknown);
    assert_eq!(context.type_of(sum).unwrap(), OperandType::Unknown);
}

/// An output of a derived table takes the type of the expression defining
/// it, and a domain column the domain's type.
#[test]
fn derived_outputs_and_domains_take_their_definition_types() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE DOMAIN d3 AS VARCHAR(3); CREATE TABLE t (x d3);",
    )
    .unwrap();
    let derived = query("SELECT d.v FROM (SELECT 'x'::varchar(5) AS v) AS d");
    let context = ExpressionContext::new(&derived, &db, IdentifierCase::AsWritten).unwrap();
    assert_eq!(
        context.type_of(projected(&derived)[0]).unwrap(),
        OperandType::Known {
            type_name: TargetName::new("varchar", false).with_schema("pg_catalog", false),
            typmod: Some(9),
        }
    );

    let domain = query("SELECT x FROM t");
    let context = ExpressionContext::new(&domain, &db, IdentifierCase::AsWritten).unwrap();
    assert_eq!(
        context.type_of(projected(&domain)[0]).unwrap(),
        OperandType::Known {
            type_name: TargetName::new("d3", true).with_schema("public", true),
            typmod: None,
        }
    );
}

/// A column whose modifier the model cannot encode leaves its type unknown.
#[test]
fn an_unencodable_modifier_leaves_the_type_unknown() {
    let db =
        ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE g (p geometry(Point, 4326));").unwrap();
    let query = query("SELECT p FROM g");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    assert_eq!(context.type_of(projected(&query)[0]).unwrap(), OperandType::Unknown);
}

/// A node of another query is refused, even one equal to a node of this one.
#[test]
fn a_node_of_another_query_is_refused() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let held = query("SELECT 1");
    let other = query("SELECT 1");
    let context = ExpressionContext::new(&held, &db, IdentifierCase::AsWritten).unwrap();
    assert!(matches!(
        context.type_of(projected(&other)[0]),
        Err(LookupError::ExpressionNotInQuery { .. })
    ));
    assert!(matches!(
        context.reference(projected(&other)[0]),
        Err(LookupError::ExpressionNotInQuery { .. })
    ));
}
