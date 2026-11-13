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

/// A reference that reaches no column takes no type.
#[test]
fn unresolved_references_take_no_type() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE shares (weight INT);").unwrap();
    let query = query("SELECT nope FROM shares");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let [missing] = projected(&query)[..] else { unreachable!() };
    assert_eq!(context.type_of(missing).unwrap(), OperandType::Unknown);
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

/// A reference binds as its clause binds it: an `ORDER BY` or `DISTINCT ON`
/// name reaches a select-list output first, a `GROUP BY` name the input
/// first, a join condition only the items joined so far, and a whole-row
/// reference binds no column.
#[test]
fn references_bind_as_their_clause_binds_them() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (x TEXT); CREATE TABLE a (id INT); CREATE TABLE b (id INT);
         CREATE TABLE c (z INT);",
    )
    .unwrap();

    let ordered = query("SELECT 1 AS x FROM t ORDER BY x");
    let context = ExpressionContext::new(&ordered, &db, IdentifierCase::AsWritten).unwrap();
    let key = &ordered.order_by.as_ref().unwrap();
    let sqlparser::ast::OrderByKind::Expressions(keys) = &key.kind else { unreachable!() };
    let (_, binding) = context.reference(&keys[0].expr).unwrap().unwrap();
    assert_eq!(binding.entry(), None);
    assert_eq!(context.type_of(&keys[0].expr).unwrap(), pg("int4"));

    let grouped = query("SELECT length(x) AS x FROM t GROUP BY x");
    let context = ExpressionContext::new(&grouped, &db, IdentifierCase::AsWritten).unwrap();
    let SetExpr::Select(select) = grouped.body.as_ref() else { unreachable!() };
    let sqlparser::ast::GroupByExpr::Expressions(keys, _) = &select.group_by else {
        unreachable!()
    };
    assert_eq!(context.type_of(&keys[0]).unwrap(), pg("text"));

    let distinct = query("SELECT DISTINCT ON (x) 1 AS x FROM t");
    let context = ExpressionContext::new(&distinct, &db, IdentifierCase::AsWritten).unwrap();
    let SetExpr::Select(select) = distinct.body.as_ref() else { unreachable!() };
    let Some(sqlparser::ast::Distinct::On(keys)) = &select.distinct else { unreachable!() };
    assert_eq!(context.reference(&keys[0]).unwrap().unwrap().1.entry(), None);
    assert_eq!(context.type_of(&keys[0]).unwrap(), pg("int4"));

    let whole_row = query("SELECT 1 FROM t WHERE t.* IS NULL");
    let context = ExpressionContext::new(&whole_row, &db, IdentifierCase::AsWritten).unwrap();
    let SetExpr::Select(select) = whole_row.body.as_ref() else { unreachable!() };
    let Some(Expr::IsNull(row)) = &select.selection else { unreachable!() };
    assert!(matches!(row.as_ref(), Expr::QualifiedWildcard(..)));
    assert!(context.reference(row).unwrap().is_none());
    assert_eq!(context.type_of(row).unwrap(), OperandType::Unknown);

    let joined = query("SELECT 1 FROM a JOIN b ON z = 1 CROSS JOIN c");
    let context = ExpressionContext::new(&joined, &db, IdentifierCase::AsWritten).unwrap();
    let SetExpr::Select(select) = joined.body.as_ref() else { unreachable!() };
    let sqlparser::ast::JoinOperator::Join(sqlparser::ast::JoinConstraint::On(Expr::BinaryOp {
        left: z,
        ..
    })) = &select.from[0].joins[0].join_operator
    else {
        unreachable!()
    };
    assert!(context.reference(z).unwrap().is_none());
    assert_eq!(context.type_of(z).unwrap(), OperandType::Unknown);
}

const OPERANDS: &str = "CREATE TABLE t (i INT4, s INT2, v VARCHAR(5), n NUMERIC(6, 2), a INT4[]);";

/// The operator names each node applies, in order, `None` for a node whose
/// applications the context does not derive.
fn applied<DB: DatabaseLike>(
    context: &ExpressionContext<'_, '_, DB>,
    nodes: &[&Expr],
) -> Vec<Option<Vec<String>>> {
    nodes
        .iter()
        .map(|node| {
            context.operators(node).unwrap().map(|applications| {
                applications.iter().map(|application| application.name().to_string()).collect()
            })
        })
        .collect()
}

/// An operator node takes the result type of the operator PostgreSQL 18
/// selects for it, as a view over the same expressions reports.
#[test]
fn operators_take_the_selected_operators_result_type() {
    let db = ParserDB::parse::<PostgreSqlDialect>(OPERANDS).unwrap();
    let query = query(
        "SELECT i > 1.5, n + 1, v || 'x', a || 5, i = s, -i, +1, v NOT LIKE 'a%', v ILIKE 'a',
                v IS DISTINCT FROM 'x', v IS NOT DISTINCT FROM 'x', i OPERATOR(pg_catalog.+) 1
         FROM t",
    );
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let nodes = projected(&query);
    let types: Vec<_> = nodes.iter().map(|node| context.type_of(node).unwrap()).collect();
    let Expr::BinaryOp { left: array, .. } = nodes[3] else { unreachable!() };
    let int4_array = context.type_of(array).unwrap();
    assert_eq!(
        types,
        [
            pg("bool"),
            pg("numeric"),
            pg("text"),
            int4_array,
            pg("bool"),
            pg("int4"),
            pg("int4"),
            pg("bool"),
            pg("bool"),
            pg("bool"),
            pg("bool"),
            pg("int4"),
        ]
    );
    let names = |names: &[&str]| Some(names.iter().map(ToString::to_string).collect());
    assert_eq!(
        applied(&context, &nodes),
        [
            names(&[">"]),
            names(&["+"]),
            names(&["||"]),
            names(&["||"]),
            names(&["="]),
            names(&["-"]),
            names(&["+"]),
            names(&["!~~"]),
            names(&["~~*"]),
            names(&["="]),
            names(&["="]),
            names(&["pg_catalog.+"]),
        ]
    );

    // `i > 1.5` converts `i` to `numeric` by a cast function.
    let Expr::BinaryOp { left: i, right: literal, .. } = nodes[0] else { unreachable!() };
    let applications = context.operators(nodes[0]).unwrap().unwrap();
    assert!(applications[0].left().is_some_and(|left| core::ptr::eq(left, i.as_ref())));
    assert!(core::ptr::eq(applications[0].right(), literal.as_ref()));
    let OperatorResolution::Selected(greater) = applications[0].resolution() else {
        unreachable!()
    };
    assert!(matches!(
        greater.left().map(OperandEvidence::conversion),
        Some([CoercionStep::Function(_)])
    ));
    assert_eq!(greater.result_type().map(TargetName::name), Some("bool"));

    // The prefix `-` over a column applies to that column alone.
    let applications = context.operators(nodes[5]).unwrap().unwrap();
    assert!(applications[0].left().is_none());
}

/// A `-` over a numeric literal folds into the constant, as PostgreSQL's
/// grammar does, so it selects no operator and the sign decides the type.
#[test]
fn negated_numeric_literals_fold_into_constants() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let query = query(
        "SELECT -2147483648, -(2147483648), - -1, -9223372036854775808, -1.5, -0x80000000,
                -2147483649",
    );
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let nodes = projected(&query);
    let types: Vec<_> = nodes.iter().map(|node| context.type_of(node).unwrap()).collect();
    assert_eq!(
        types,
        [pg("int4"), pg("int4"), pg("int4"), pg("int8"), pg("numeric"), pg("int4"), pg("int8"),]
    );
    assert!(
        applied(&context, &nodes).iter().all(|names| names.as_ref().is_some_and(Vec::is_empty))
    );
}

/// `NULLIF` applies `=` and takes its first operand's type as `=` converts
/// it, keeping the operand's modifier only when nothing converts it.
#[test]
fn nullif_takes_its_first_operand_as_equality_converts_it() {
    let db = ParserDB::parse::<PostgreSqlDialect>(OPERANDS).unwrap();
    let query = query("SELECT NULLIF(i, 1.5), NULLIF(v, 'x'), NULLIF(n, 1), NULLIF(s, 1) FROM t");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let nodes = projected(&query);
    let types: Vec<_> = nodes.iter().map(|node| context.type_of(node).unwrap()).collect();
    let numeric_6_2 = OperandType::Known {
        type_name: TargetName::new("numeric", false).with_schema("pg_catalog", false),
        typmod: Some((6 << 16 | 2) + 4),
    };
    assert_eq!(types, [pg("numeric"), pg("text"), numeric_6_2, pg("int2")]);
    let equals = Some(Vec::from(["=".to_owned()]));
    assert_eq!(applied(&context, &nodes), [equals.clone(), equals.clone(), equals.clone(), equals]);
}

/// The forms whose operators a later slice derives answer `None`, never an
/// empty list that would claim they apply none.
#[test]
fn forms_not_yet_derived_answer_none() {
    let db = ParserDB::parse::<PostgreSqlDialect>(OPERANDS).unwrap();
    let query = query(
        "SELECT i IN (1, 2), i BETWEEN 1 AND 2, v LIKE 'a' ESCAPE '!', (1, 2) = (1, 2),
                ROW(1, 2) = ROW(1, 2), i > 0 AND i < 9, NOT true, length(v),
                pg_catalog.length(v), 1 OPERATOR(a.b.+) 1, 5 !
         FROM t",
    );
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let nodes = projected(&query);
    assert!(applied(&context, &nodes).iter().all(Option::is_none));
    assert!(nodes.iter().all(|node| context.type_of(node).unwrap() == OperandType::Unknown));
}

/// Every operator symbol PostgreSQL's dialect writes is looked up by that
/// symbol, prefix and binary alike, and `OPERATOR(op)` by the name inside.
#[test]
fn operators_are_looked_up_by_the_symbol_written() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let binary = [
        "+", "-", "*", "/", "%", "||", ">", "<", ">=", "<=", "=", "<>", "<=>", "|", "&", "^", "#",
        "<<", ">>", "&&", "~", "~*", "!~", "!~*", "~~", "~~*", "!~~", "!~~*", "^@", "->", "->>",
        "#>", "#>>", "@@", "@>", "<@", "#-", "@?", "?", "?&", "?|", "##", "<->", "&<", "&>", "<<|",
        "|>>", "&<|", "|&>", "<^", ">^", "?#", "?-", "?-|", "?||", "@", "~=", "%%",
    ];
    let prefix = ["-", "+", "~", "@", "|/", "||/", "!!", "@-@", "@@", "#", "?-", "?|"];
    let sql = binary
        .iter()
        .map(|symbol| format!("1 {symbol} 1"))
        .chain(prefix.iter().map(|symbol| format!("{symbol} x")))
        .chain([String::from("1 OPERATOR(+) 1")])
        .collect::<Vec<_>>();
    let query = query(&format!("SELECT {}", sql.join(", ")));
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let written: Vec<_> = binary
        .iter()
        .chain(&prefix)
        .chain(&["+"])
        .map(|symbol| Some(vec![(*symbol).to_owned()]))
        .collect();
    assert_eq!(applied(&context, &projected(&query)), written);
}

/// An operator no overload takes leaves its node untyped, and the node still
/// reports the operator it applies.
#[test]
fn unresolved_operators_leave_their_nodes_untyped() {
    let db = ParserDB::parse::<PostgreSqlDialect>(OPERANDS).unwrap();
    let query = query("SELECT -true, v IS DISTINCT FROM a, NULLIF(v, a) FROM t");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    for node in projected(&query) {
        assert_eq!(context.type_of(node).unwrap(), OperandType::Unknown, "{node}");
        let applications = context.operators(node).unwrap().unwrap();
        assert!(matches!(applications[0].resolution(), OperatorResolution::NotFound), "{node}");
    }
}

/// A parenthesized node applies what its content applies, and a column
/// reference or a `COLLATE` applies no operator of its own.
#[test]
fn parentheses_answer_for_their_content() {
    let db = ParserDB::parse::<PostgreSqlDialect>(OPERANDS).unwrap();
    let query = query("SELECT (i > 1), i, v COLLATE \"C\" FROM t");
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let nodes = projected(&query);
    let greater = Some(Vec::from([">".to_owned()]));
    assert_eq!(applied(&context, &nodes), [greater, Some(Vec::new()), Some(Vec::new())]);
}

/// `IS [NOT] DISTINCT FROM` an undecorated `NULL`, parenthesized or not, is a
/// null test on the other operand, which applies no `=` even for a type that
/// has none, while a typed `NULL` still compares through `=`.
#[test]
fn distinct_from_a_bare_null_applies_no_operator() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (i INT4, x XML);").unwrap();
    let query = query(
        "SELECT NULL IS DISTINCT FROM i, i IS NOT DISTINCT FROM (NULL), NULL IS DISTINCT FROM NULL,
                x IS DISTINCT FROM NULL, (1, 2) IS DISTINCT FROM NULL, i IS DISTINCT FROM NULL::int4
         FROM t",
    );
    let context = ExpressionContext::new(&query, &db, IdentifierCase::AsWritten).unwrap();
    let nodes = projected(&query);
    assert!(nodes.iter().all(|node| context.type_of(node).unwrap() == pg("bool")));
    let none = Some(Vec::new());
    assert_eq!(
        applied(&context, &nodes),
        [none.clone(), none.clone(), none.clone(), none.clone(), none, Some(vec!["=".to_owned()])]
    );
}
