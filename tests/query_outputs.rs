//! Integration tests for the ordered outputs of a recorded query.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking on unexpected outputs"
)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Ident, Query, SelectItem, SetExpr, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

const SCHEMA: &str = "
    CREATE TABLE halves(h halfvec(3));
    CREATE TABLE items(id INT PRIMARY KEY, h halfvec(3), v vector(3));
    CREATE TABLE a(id INT, x INT);
    CREATE TABLE b(id INT);
";

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

fn query(sql: &str) -> Query {
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .unwrap_or_else(|error| panic!("statement `{sql}` parses: {error}"));
    match statements.pop().expect("one statement") {
        Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}

fn describe(definition: &ColumnDefinition<'_, '_, '_, ParserDB>) -> String {
    match definition {
        ColumnDefinition::Base { table, column } => {
            format!("{}.{}", table.table_name(), column.column_name())
        }
        ColumnDefinition::Expression { expression, .. } => format!("expression {expression}"),
        ColumnDefinition::SetOperation { operator, left, right } => {
            format!(
                "{operator}({}, {})",
                describe(&left.definition()),
                describe(&right.definition())
            )
        }
        ColumnDefinition::RecursiveUnion { anchor, recursive } => {
            format!(
                "recursive({}, {})",
                describe(&anchor.definition()),
                describe(&recursive.definition())
            )
        }
        ColumnDefinition::Opaque => "opaque".to_owned(),
    }
}

fn outputs_of(sql: &str, pick: fn(&Query) -> &Query) -> Vec<String> {
    let database = database();
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let outputs = scope.scope_for_query(pick(&query)).expect("query is recorded").outputs();
    let entries = outputs.len();
    let described = outputs
        .map(|output| {
            match output {
                QueryOutput::Column { definition, .. } => describe(&definition),
                QueryOutput::Unexpanded => "unexpanded".to_owned(),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(described.len(), entries, "`{sql}` reports its entry count");
    described
}

fn outputs(sql: &str) -> Vec<String> {
    outputs_of(sql, |query| query)
}

/// Renders each output name, double-quoting a quoted one, with `?` for an
/// unmodeled name and `*` for an unexpanded wildcard.
fn names_of(sql: &str, pick: impl Fn(&Query) -> &Query) -> Vec<String> {
    let database = database();
    let query = query(sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    scope
        .scope_for_query(pick(&query))
        .expect("query is recorded")
        .outputs()
        .map(|output| {
            match output {
                QueryOutput::Column { name: Some((name, true)), .. } => format!("\"{name}\""),
                QueryOutput::Column { name: Some((name, false)), .. } => name.to_owned(),
                QueryOutput::Column { name: None, .. } => "?".to_owned(),
                QueryOutput::Unexpanded => "*".to_owned(),
            }
        })
        .collect()
}

fn names(sql: &str) -> Vec<String> {
    names_of(sql, |query| query)
}

fn cte_body(index: usize) -> impl Fn(&Query) -> &Query {
    move |query| &query.with.as_ref().expect("a WITH clause").cte_tables[index].query
}

fn first_subquery(query: &Query) -> &Query {
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a SELECT body") };
    let (SelectItem::UnnamedExpr(expression) | SelectItem::ExprWithAlias { expr: expression, .. }) =
        &select.projection[0]
    else {
        panic!("expected an expression projection")
    };
    let Expr::BinaryOp { left, .. } = expression else { panic!("expected a distance operator") };
    let Expr::Subquery(subquery) = left.as_ref() else { panic!("expected a scalar subquery") };
    subquery
}

#[test]
fn a_scalar_subquery_wildcard_exposes_the_stored_column() {
    let sql = "SELECT (SELECT * FROM halves LIMIT 1) <-> '[1,2,3]'::vector(3) AS distance";
    assert_eq!(outputs_of(sql, first_subquery), ["halves.h"]);
    let sql = "SELECT (SELECT * FROM halves LIMIT 1) <-> v FROM items";
    assert_eq!(outputs_of(sql, first_subquery), ["halves.h"]);
}

#[test]
fn outputs_follow_the_select_list_with_wildcards_expanded_in_place() {
    assert_eq!(
        outputs("SELECT x, *, b.*, id * 2 AS doubled FROM a JOIN b ON a.id = b.id"),
        ["a.x", "a.id", "a.x", "b.id", "b.id", "expression id * 2"]
    );
    assert_eq!(outputs("WITH c AS (SELECT id FROM a) SELECT * FROM c"), ["a.id"]);
    assert_eq!(outputs("SELECT d.* FROM (SELECT x AS y FROM a) AS d"), ["a.x"]);
}

#[test]
fn expression_outputs_keep_their_defining_scope() {
    let database = database();
    let query = query("SELECT id + x FROM a");
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let mut outputs = scope.scope_for_query(&query).expect("query is recorded").outputs();
    let Some(QueryOutput::Column {
        definition: ColumnDefinition::Expression { scope: defining, .. },
        ..
    }) = outputs.next()
    else {
        panic!("expected an expression output")
    };
    let id = defining
        .resolve_column_definition(&Expr::Identifier(Ident::new("id")))
        .expect("lookup succeeds")
        .expect("id resolves");
    assert_eq!(describe(&id), "a.id");
    assert!(outputs.next().is_none());
}

#[test]
fn an_unenumerable_wildcard_stays_one_entry_of_unknown_width() {
    assert_eq!(
        outputs("SELECT b.id, *, b.id AS again FROM b, generate_series(1, 2) AS g"),
        ["b.id", "unexpanded", "b.id"]
    );
}

#[test]
fn set_operations_pair_positions_only_when_every_width_is_known() {
    assert_eq!(outputs("SELECT id FROM a UNION ALL SELECT id FROM b"), ["UNION(a.id, b.id)"]);
    assert_eq!(
        outputs("SELECT id FROM b UNION SELECT * FROM generate_series(1, 2) AS g"),
        ["opaque"]
    );
    assert_eq!(
        outputs("SELECT * FROM generate_series(1, 2) AS g UNION SELECT id FROM b"),
        ["unexpanded"]
    );
    assert_eq!(outputs("VALUES (1, 2), (3, 4)"), ["opaque", "opaque"]);
}

#[test]
fn recursive_queries_expose_their_anchor_and_recursive_definitions() {
    let sql =
        "WITH RECURSIVE r(n) AS (SELECT id FROM b UNION ALL SELECT n + 1 FROM r) SELECT * FROM r";
    let [output] = outputs(sql).try_into().expect("one output");
    assert!(output.starts_with("recursive(b.id, "), "{output}");
}

#[test]
fn folded_wildcards_preserve_order_and_expression_scopes() {
    use sqlparser::dialect::SQLiteDialect;

    let database =
        ParserDB::parse::<SQLiteDialect>("CREATE TABLE docs(id INT);").expect("schema parses");
    let Statement::Query(query) = Parser::parse_sql(
        &SQLiteDialect {},
        "SELECT [D].*, [D].[VALUE] + 1 FROM (SELECT [ID] AS [VaLuE] FROM [DOCS]) AS [d]",
    )
    .expect("query parses")
    .remove(0) else {
        panic!("expected a query")
    };
    let scope =
        ColumnScope::from_query(&query, &database, IdentifierCase::Folded).expect("scope builds");
    let mut outputs = scope.scope_for_query(&query).expect("query is recorded").outputs();
    let Some(QueryOutput::Column { definition: first, .. }) = outputs.next() else {
        panic!("expected the expanded column")
    };
    assert_eq!(describe(&first), "docs.id");
    let Some(QueryOutput::Column {
        definition: ColumnDefinition::Expression { expression, scope: defining },
        ..
    }) = outputs.next()
    else {
        panic!("expected an expression output")
    };
    let Expr::BinaryOp { left, .. } = expression else { panic!("expected a binary expression") };
    let (definition, binding) =
        defining.resolve_column_binding(left).expect("lookup succeeds").expect("column resolves");
    assert_eq!(describe(&definition), "docs.id");
    assert_eq!(binding.relation(), Some(("d", true)));
    assert_eq!(binding.exposed(), Some(("VaLuE", true)));
    assert!(outputs.next().is_none());
}

#[test]
fn output_names_follow_aliases_through_nested_wildcards() {
    let sql = "WITH c AS (SELECT id AS alias, x FROM a), d AS (SELECT * FROM c) SELECT * FROM d";
    assert_eq!(names(sql), ["alias", "x"]);
    assert_eq!(outputs(sql), ["a.id", "a.x"]);
    assert_eq!(names("SELECT a.id, x AS \"Value\", * FROM a"), ["id", "\"Value\"", "id", "x"]);
}

#[test]
fn declared_column_lists_rename_a_prefix_where_the_relation_is_read() {
    let sql = "WITH c(stable) AS (SELECT id, x FROM a), d AS (SELECT * FROM c) SELECT * FROM d";
    assert_eq!(names(sql), ["stable", "x"]);
    assert_eq!(names_of(sql, cte_body(0)), ["id", "x"]);
    assert_eq!(names_of(sql, cte_body(1)), ["stable", "x"]);
    assert_eq!(names("SELECT d.* FROM (SELECT id, x FROM a) AS d(k)"), ["k", "x"]);
}

#[test]
fn set_operations_and_values_keep_their_leading_names() {
    assert_eq!(names("SELECT id AS l FROM a UNION ALL SELECT id AS r FROM b"), ["l"]);
    assert_eq!(names("VALUES (1, 2)"), ["\"column1\"", "\"column2\""]);
    let sql =
        "WITH RECURSIVE r(n) AS (SELECT id FROM b UNION ALL SELECT n + 1 FROM r) SELECT * FROM r";
    assert_eq!(names(sql), ["n"]);
    assert_eq!(names_of(sql, cte_body(0)), ["id"]);
}

#[test]
fn unexpanded_wildcards_and_unmodeled_names_stay_unknown() {
    assert_eq!(
        names("SELECT b.id, *, 1 + 1, b.id AS again FROM b, generate_series(1, 2) AS g"),
        ["id", "*", "\"?column?\"", "again"]
    );
    assert_eq!(names("SELECT '1'::tsvector FROM b"), ["?"]);
}
