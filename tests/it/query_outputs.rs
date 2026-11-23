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

use crate::support::{query_parsing::query_with, values_labels::values_label};

const SCHEMA: &str = "
    CREATE TABLE halves(h halfvec(3));
    CREATE TABLE items(id INT PRIMARY KEY, h halfvec(3), v vector(3));
    CREATE TABLE a(id INT, x INT);
    CREATE TABLE b(id INT);
    CREATE TYPE role AS ENUM ('reader', 'writer', 'admin');
    CREATE TABLE docs(id INT, tags TEXT[], body TSVECTOR);
";

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

fn query(sql: &str) -> Query {
    query_with(&PostgreSqlDialect {}, sql)
}

fn describe(definition: &ColumnDefinition<'_, '_, '_, ParserDB>) -> String {
    match definition {
        ColumnDefinition::Base { table, column, .. } => {
            format!("{}.{}", table.table_name(), column.column_name())
        }
        ColumnDefinition::ArrayElement { array, .. } => format!("element of {array}"),
        ColumnDefinition::Ordinality => "ordinality".to_owned(),
        ColumnDefinition::FunctionResult { function, .. } => {
            format!("result of {}", function.name())
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
        ColumnDefinition::Values { rows } => values_label(*rows, describe),
        ColumnDefinition::Parameter { ordinal, .. } => format!("parameter {ordinal}"),
        ColumnDefinition::Variable(variable) => format!("variable {}", variable.name()),
        ColumnDefinition::VariableField(variable) => format!("field of {}", variable.name()),
        ColumnDefinition::Opaque => "opaque".to_owned(),
    }
}

fn outputs_of(sql: &str, pick: fn(&Query) -> &Query) -> Vec<String> {
    outputs_in(&database(), sql, pick)
}

fn outputs_in(database: &ParserDB, sql: &str, pick: fn(&Query) -> &Query) -> Vec<String> {
    let query = query(sql);
    let scope =
        ColumnScope::from_query(&query, database, IdentifierCase::AsWritten).expect("scope builds");
    let outputs = scope.scope_for_query(pick(&query)).expect("query is recorded").outputs();
    let entries = outputs.len();
    let described = outputs
        .map(|output| {
            match output {
                QueryOutput::Column { definition, .. } => describe(&definition),
                QueryOutput::Unexpanded { .. } => "unexpanded".to_owned(),
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
                QueryOutput::Unexpanded { .. } => "*".to_owned(),
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
    assert_eq!(
        outputs("VALUES (1, 2), (3, 4)"),
        ["values(expression 1, expression 3)", "values(expression 2, expression 4)"]
    );
}

// PostgreSQL resolves a `VALUES` column's type across every row, so each
// column carries the cell of every row, read where the list sits, whether
// the list is the whole query, a `FROM` item or a set operation arm.
#[test]
fn values_columns_carry_the_cell_of_every_row() {
    for (sql, expected) in [
        (
            "VALUES ('reader'::role), ('writer')",
            "values(expression 'reader'::role, expression 'writer')",
        ),
        (
            "SELECT r FROM (VALUES (NULL), (2::BIGINT)) AS t(r)",
            "values(expression NULL, expression 2::BIGINT)",
        ),
        ("SELECT column1 FROM (VALUES (1), (2.5))", "values(expression 1, expression 2.5)"),
        (
            "SELECT id FROM a UNION SELECT v.r FROM (VALUES (1)) AS v(r)",
            "UNION(a.id, values(expression 1))",
        ),
        (
            "SELECT id FROM a UNION VALUES (1), (2)",
            "UNION(a.id, values(expression 1, expression 2))",
        ),
        ("SELECT v.r FROM a, LATERAL (VALUES (a.id), (a.x)) AS v(r)", "values(a.id, a.x)"),
    ] {
        assert_eq!(outputs(sql), [expected], "{sql}");
    }
    let sql = "SELECT (SELECT v.r FROM (VALUES (a.id)) AS v(r)) <-> 1 FROM a";
    assert_eq!(outputs_of(sql, first_subquery), ["values(a.id)"]);
}

// PostgreSQL rejects a `VALUES` list whose rows differ in length, so none of
// its columns is enumerated.
#[test]
fn ragged_values_lists_enumerate_no_column() {
    assert_eq!(outputs("VALUES (1, 2), (3)"), ["unexpanded"]);
    assert_eq!(outputs("SELECT * FROM (VALUES (1, 2), (3)) AS v"), ["unexpanded"]);
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
    assert_eq!(names("SELECT '1'::nvarchar FROM b"), ["?"]);
}

// PostgreSQL names an unaliased select-list entry of a derived table, a CTE
// or a view the way it names a top-level output, `?column?` for one it names
// nothing, so a wildcard over that relation lists every column it holds.
#[test]
fn unaliased_derived_columns_take_the_names_postgresql_figures() {
    let sql = "SELECT * FROM (SELECT id + 2, lower('x'), id::text, CASE WHEN true THEN 1 END, 7 \
               FROM a) AS t";
    assert_eq!(names(sql), ["\"?column?\"", "lower", "id", "\"case\"", "\"?column?\""]);
    assert_eq!(
        outputs(sql),
        [
            "expression id + 2",
            "expression lower('x')",
            "expression id::TEXT",
            "expression CASE WHEN true THEN 1 END",
            "expression 7",
        ]
    );
    assert_eq!(names("WITH c AS (SELECT id + 2 FROM a) SELECT * FROM c"), ["\"?column?\""]);
    assert_eq!(
        outputs("SELECT t.\"?column?\" FROM (SELECT id + 2 FROM a) AS t"),
        ["expression id + 2"]
    );
    assert_eq!(
        outputs("SELECT t.lower FROM (SELECT lower('x') FROM a) AS t"),
        ["expression lower('x')"]
    );
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE a(id INT); CREATE VIEW v AS SELECT id + 2 FROM a;",
    )
    .expect("schema parses");
    assert_eq!(outputs_in(&database, "SELECT * FROM v", |query| query), ["expression id + 2"]);
}

#[test]
fn a_derived_column_whose_name_is_not_modeled_leaves_the_wildcard_unexpanded() {
    assert_eq!(names("SELECT * FROM (SELECT id + 2, '1'::nvarchar FROM b) AS t"), ["*"]);
}

#[test]
fn in_table_and_unnest_outputs_have_unnamed_expression_names() {
    use sqlparser::dialect::SQLiteDialect;

    let database =
        ParserDB::parse::<SQLiteDialect>("CREATE TABLE t(a INT, b INT); CREATE TABLE u(a INT);")
            .expect("schema parses");
    for (dialect, sql) in [
        (
            &SQLiteDialect {} as &dyn sqlparser::dialect::Dialect,
            "SELECT a IN u, f(a) IN json_each(b), a NOT IN u FROM t",
        ),
        (&PostgreSqlDialect {}, "SELECT a IN UNNEST(b) FROM t"),
    ] {
        let query = query_with(dialect, sql);
        let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
            .expect("scope builds");
        let outputs = scope.scope_for_query(&query).expect("query is recorded").outputs();
        for output in outputs {
            let QueryOutput::Column { name, definition, .. } = output else {
                panic!("expected an expression output")
            };
            assert_eq!(name, Some(("?column?", true)), "{sql}");
            assert!(matches!(definition, ColumnDefinition::Expression { .. }));
        }
    }
}

#[test]
fn unnest_columns_expand_an_argument_shown_to_be_an_array() {
    for (sql, expected) in [
        (
            "SELECT x FROM unnest(enum_range(NULL::role)) AS t(x)",
            "element of enum_range(NULL::role)",
        ),
        ("SELECT x FROM unnest(ARRAY[1, 2]) AS x", "element of ARRAY[1, 2]"),
        ("SELECT x FROM unnest('{1}'::INT[]) AS x", "element of '{1}'::INT[]"),
        (
            "SELECT x FROM unnest(ARRAY(SELECT id FROM b)) AS x",
            "element of ARRAY(SELECT id FROM b)",
        ),
        ("SELECT x FROM pg_catalog.unnest(ARRAY[1]) AS x", "element of ARRAY[1]"),
        (
            "SELECT tag FROM docs CROSS JOIN LATERAL unnest(docs.tags) AS tag",
            "element of docs.tags",
        ),
        (
            "SELECT x FROM (SELECT tags AS list FROM docs) AS d, unnest(d.list) AS x",
            "element of d.list",
        ),
        ("SELECT x FROM unnest((ARRAY[1])) AS x", "element of (ARRAY[1])"),
        (
            "WITH c AS (SELECT ARRAY[1] AS list) SELECT x FROM c, unnest(c.list) AS x",
            "element of c.list",
        ),
        (
            "WITH RECURSIVE r(list) AS (SELECT ARRAY[1] UNION ALL SELECT list FROM r WHERE false)
             SELECT x FROM r, unnest(r.list) AS x",
            "element of r.list",
        ),
    ] {
        assert_eq!(outputs(sql), [expected], "{sql}");
    }
    let sql = "SELECT x FROM unnest(enum_range(NULL::role)) AS t(x)
               UNION SELECT y FROM unnest(ARRAY['reader'::role]) AS u(y)";
    assert_eq!(
        outputs(sql),
        ["UNION(element of enum_range(NULL::role), element of ARRAY['reader'::role])"]
    );
}

#[test]
fn unnest_columns_take_the_names_postgresql_gives_them() {
    assert_eq!(names("SELECT * FROM unnest(ARRAY[1])"), ["\"unnest\""]);
    assert_eq!(outputs("SELECT unnest.unnest FROM unnest(ARRAY[1])"), ["element of ARRAY[1]"]);
    assert_eq!(names("SELECT * FROM unnest(ARRAY[1]) AS x"), ["x"]);
    assert_eq!(
        names("SELECT * FROM unnest(ARRAY[1], ARRAY['a']) AS t"),
        ["\"unnest\"", "\"unnest\""]
    );
    assert_eq!(
        outputs("SELECT * FROM unnest(ARRAY[1], ARRAY['a']) AS t(x, y)"),
        ["element of ARRAY[1]", "element of ARRAY['a']"]
    );
    assert_eq!(
        names("SELECT * FROM unnest(ARRAY[1]) WITH ORDINALITY"),
        ["\"unnest\"", "\"ordinality\""]
    );
    assert_eq!(
        names("SELECT * FROM unnest(ARRAY[1]) WITH ORDINALITY AS t"),
        ["t", "\"ordinality\""]
    );
    let sql = "SELECT * FROM unnest(ARRAY[1], ARRAY['a']) WITH ORDINALITY AS t(x, y, n)";
    assert_eq!(outputs(sql), ["element of ARRAY[1]", "element of ARRAY['a']", "ordinality"]);
    assert_eq!(names(sql), ["x", "y", "n"]);
}

#[test]
fn an_unnest_not_shown_to_read_arrays_stays_opaque() {
    // `unnest(tsvector)` outputs `lexeme`, `positions` and `weights`.
    for sql in [
        "SELECT lexeme FROM docs, unnest(docs.body)",
        "SELECT lexeme FROM docs, unnest(docs.tags, docs.body) AS t",
        "SELECT x FROM unnest((SELECT tags FROM docs LIMIT 1)) AS x",
        "SELECT x FROM unnest(ARRAY[1]) AS t(x, y)",
        "SELECT x FROM pg_catalog.unnest(ARRAY[1], ARRAY[2]) AS x",
        "SELECT x FROM pg_catalog.unnest() AS x",
        "SELECT x FROM pg_catalog.unnest(list => ARRAY[1]) AS x",
        "SELECT x FROM other.unnest(ARRAY[1]) AS x",
        "SELECT x FROM unnest(current_timestamp) AS x",
        "SELECT x FROM unnest(current_schema) AS x",
        "SELECT x FROM unnest(missing) AS x",
        "SELECT y FROM a, b, unnest(id) AS y",
        "SELECT y FROM unnest(ARRAY[1]) AS x, unnest(x) AS y",
        "SELECT y FROM unnest(ARRAY[1]) WITH ORDINALITY AS t(x, n), unnest(t.n) AS y",
        "SELECT y FROM generate_series(1, 2) AS g(n), unnest(g.n) AS y",
    ] {
        assert_eq!(outputs(sql), ["opaque"], "{sql}");
    }
}

/// `COALESCE`, `GREATEST`, `LEAST`, a set operation and a `VALUES` column
/// take the common type of their inputs, an array when any input is one,
/// and `NULLIF` the type of its first argument.
#[test]
fn an_unnest_of_a_common_type_expands_when_any_input_is_an_array() {
    for (sql, expected) in [
        (
            "SELECT tag FROM docs, unnest(coalesce(docs.tags, '{}')) AS tag",
            "element of coalesce(docs.tags, '{}')",
        ),
        (
            "SELECT x FROM unnest(greatest(NULL, ARRAY[1])) AS x",
            "element of greatest(NULL, ARRAY[1])",
        ),
        (
            "SELECT tag FROM docs, unnest(nullif(docs.tags, '{}')) AS tag",
            "element of nullif(docs.tags, '{}')",
        ),
        (
            "SELECT x FROM (SELECT NULL AS list UNION SELECT ARRAY[1]) AS d, unnest(d.list) AS x",
            "element of d.list",
        ),
        (
            "SELECT x FROM (VALUES (NULL), (ARRAY[1])) AS v(list), unnest(v.list) AS x",
            "element of v.list",
        ),
    ] {
        assert_eq!(outputs(sql), [expected], "{sql}");
    }
    for sql in [
        "SELECT x FROM unnest(coalesce(1, 2)) AS x",
        "SELECT x FROM docs, unnest(coalesce(docs.*)) AS x",
        "SELECT x FROM docs, unnest(nullif(docs.id, 0)) AS x",
        "SELECT x FROM (SELECT 1 AS list UNION SELECT 2) AS d, unnest(d.list) AS x",
    ] {
        assert_eq!(outputs(sql), ["opaque"], "{sql}");
    }
}

#[test]
fn an_unnest_a_function_of_the_database_may_answer_stays_opaque() {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE FUNCTION unnest(list INT[]) RETURNS SETOF INT LANGUAGE sql AS 'SELECT 1';",
    )
    .expect("schema parses");
    let outputs = |sql| outputs_in(&database, sql, |query| query);
    assert_eq!(outputs("SELECT x FROM unnest(ARRAY[1]) AS x"), ["opaque"]);
    assert_eq!(outputs("SELECT x FROM unnest(ARRAY[1], ARRAY[2]) AS t(x)"), ["opaque"]);
    assert_eq!(outputs("SELECT x FROM pg_catalog.unnest(ARRAY[1]) AS x"), ["element of ARRAY[1]"]);
}

#[test]
fn an_unnest_argument_resolves_in_the_scope_its_item_sees() {
    let database = database();
    let query = query("SELECT t.x FROM a CROSS JOIN unnest(ARRAY[a.id]) AS t(x)");
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let mut outputs = scope.scope_for_query(&query).expect("query is recorded").outputs();
    let Some(QueryOutput::Column {
        definition: ColumnDefinition::ArrayElement { array: Expr::Array(array), scope: defining },
        ..
    }) = outputs.next()
    else {
        panic!("expected an array element output")
    };
    let id = defining
        .resolve_column_definition(&array.elem[0])
        .expect("lookup succeeds")
        .expect("a.id resolves");
    assert_eq!(describe(&id), "a.id");
}

/// An `unnest` beside a table is a relation of its own to the projection's
/// source, so a name both may answer has no single source table.
#[test]
fn an_unnest_item_takes_part_in_the_projection_source() {
    let database = database();
    let source = |sql| {
        query(sql)
            .projection_source_table(&database)
            .expect("source resolves")
            .map(TableLike::table_name)
    };
    assert_eq!(source("SELECT a.x FROM a, unnest(ARRAY[1]) AS x"), Some("a"));
    assert_eq!(source("SELECT x FROM a, unnest(ARRAY[1]) AS x"), None);
}
