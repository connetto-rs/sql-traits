//! Integration tests for the matched names of clause keys and relation
//! qualifiers.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking on unexpected lookups"
)]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::{
    ast::{
        Distinct, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, OrderByKind,
        Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr,
    },
    dialect::{Dialect, PostgreSqlDialect, SQLiteDialect},
};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/query_parsing.rs"));

const SCHEMA: &str = "CREATE TABLE docs(id INT, x INT);";

type Name = Option<(String, bool)>;

fn database() -> ParserDB {
    ParserDB::parse::<SQLiteDialect>(SCHEMA).expect("schema parses")
}

fn query(dialect: &dyn Dialect, sql: &str) -> Query {
    query_with(dialect, sql)
}

fn body_select(query: &Query) -> &Select {
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a SELECT body") };
    select
}

fn projected_subquery(query: &Query) -> &Query {
    let SelectItem::UnnamedExpr(Expr::Subquery(subquery)) = &body_select(query).projection[0]
    else {
        panic!("expected a scalar subquery projection")
    };
    subquery
}

fn owned(name: Option<(&str, bool)>) -> Name {
    name.map(|(name, quoted)| (name.to_owned(), quoted))
}

fn describe(definition: &ColumnDefinition<'_, '_, '_, ParserDB>) -> String {
    match definition {
        ColumnDefinition::Base { table, column, .. } => {
            format!("{}.{}", table.table_name(), column.column_name())
        }
        ColumnDefinition::Expression { expression, .. } => format!("expression {expression}"),
        ColumnDefinition::SetOperation { operator, .. } => format!("{operator}"),
        ColumnDefinition::RecursiveUnion { .. } => "recursive".to_owned(),
        ColumnDefinition::Opaque => "opaque".to_owned(),
    }
}

/// The definition, relation and exposed name a clause key resolved to.
type Resolution = (String, Name, Name);
type Resolved = Option<Resolution>;

fn render(
    resolved: Option<(ColumnDefinition<'_, '_, '_, ParserDB>, ColumnBinding<'_>)>,
) -> Resolved {
    resolved.map(|(definition, binding)| {
        (describe(&definition), owned(binding.relation()), owned(binding.exposed()))
    })
}

fn label(definition: &str, name: &str, quoted: bool) -> Resolution {
    (definition.to_owned(), None, Some((name.to_owned(), quoted)))
}

fn column(definition: &str, relation: (&str, bool), exposed: (&str, bool)) -> Resolution {
    (
        definition.to_owned(),
        Some((relation.0.to_owned(), relation.1)),
        Some((exposed.0.to_owned(), exposed.1)),
    )
}

fn order_by_in(
    dialect: &dyn Dialect,
    case: IdentifierCase,
    sql: &str,
    pick: fn(&Query) -> &Query,
) -> Result<Resolved, LookupError> {
    let database = database();
    let query = query(dialect, sql);
    let scope = ColumnScope::from_query(&query, &database, case).expect("scope builds");
    let target = pick(&query);
    let order_by = target.order_by.as_ref().expect("an ORDER BY clause");
    let OrderByKind::Expressions(keys) = &order_by.kind else { panic!("expected sort keys") };
    let outputs = scope.scope_for_query(target).expect("query is recorded");
    outputs.resolve_order_by_binding(&keys[0].expr).map(render)
}

fn sqlite_order_by(sql: &str) -> Result<Resolved, LookupError> {
    order_by_in(&SQLiteDialect {}, IdentifierCase::Folded, sql, |query| query)
}

fn select_key(
    dialect: &dyn Dialect,
    case: IdentifierCase,
    sql: &str,
    pick: fn(&Query) -> &Query,
    resolve: for<'scope> fn(
        &ColumnDefinitionScope<'scope, '_, '_, ParserDB>,
        &Select,
    ) -> Result<Resolved, LookupError>,
) -> Result<Resolved, LookupError> {
    let database = database();
    let query = query(dialect, sql);
    let scope = ColumnScope::from_query(&query, &database, case).expect("scope builds");
    let select = body_select(pick(&query));
    resolve(&scope.scope_for_select(select).expect("select is recorded"), select)
}

fn group_by_in(sql: &str, pick: fn(&Query) -> &Query) -> Result<Resolved, LookupError> {
    select_key(&SQLiteDialect {}, IdentifierCase::Folded, sql, pick, |scope, select| {
        let GroupByExpr::Expressions(keys, _) = &select.group_by else { panic!("expected keys") };
        scope.resolve_group_by_binding(&keys[0]).map(render)
    })
}

fn distinct_on(case: IdentifierCase, sql: &str) -> Result<Resolved, LookupError> {
    select_key(
        &PostgreSqlDialect {},
        case,
        sql,
        |query| query,
        |scope, select| {
            let Some(Distinct::On(keys)) = &select.distinct else { panic!("expected DISTINCT ON") };
            scope.resolve_distinct_on_binding(&keys[0]).map(render)
        },
    )
}

#[test]
fn an_order_by_key_reports_the_output_label_it_matched() {
    let sql = "SELECT id AS [VaLuE] FROM docs ORDER BY [VALUE]";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("docs.id", "VaLuE", true)));
    let sql = "SELECT id AS VaLuE FROM docs ORDER BY [value]";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("docs.id", "VaLuE", false)));
}

#[test]
fn an_order_by_key_that_names_no_output_reports_its_input_binding() {
    let sql = "SELECT id AS [VaLuE] FROM docs AS [D] ORDER BY [X]";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(column("docs.x", ("D", true), ("x", false))));
    let sql = "SELECT id AS [VaLuE] FROM docs AS [D] ORDER BY [d].[VALUE]";
    assert_eq!(sqlite_order_by(sql).unwrap(), None);
}

#[test]
fn an_order_by_label_matches_only_as_the_whole_key() {
    let sql = "SELECT x AS [Id] FROM docs ORDER BY [ID] + 1";
    assert_eq!(sqlite_order_by(sql).unwrap(), None);
    let sql = "SELECT x AS [Id] FROM docs ORDER BY ([ID])";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("docs.x", "Id", true)));
    let sql = "SELECT x AS [Id] FROM docs ORDER BY docs.[ID]";
    let expected = column("docs.id", ("docs", false), ("id", false));
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(expected));
}

#[test]
fn an_order_by_label_wins_over_inputs_that_ordinary_binding_finds_ambiguous() {
    let sql = "WITH a(result) AS (SELECT id FROM docs), b(result) AS (SELECT x FROM docs) \
               SELECT a.result AS result FROM a, b ORDER BY result";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("docs.id", "result", false)));
    let database = database();
    let query = query(&SQLiteDialect {}, sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::Folded).unwrap();
    let Some(OrderByKind::Expressions(keys)) = query.order_by.as_ref().map(|order| &order.kind)
    else {
        panic!("expected sort keys")
    };
    assert!(matches!(
        scope.resolve_column_binding(&keys[0].expr),
        Err(LookupError::AmbiguousTableLookup { .. })
    ));
}

#[test]
fn an_order_by_key_reports_the_first_of_several_labels_sharing_a_value() {
    let sql = "SELECT id AS [a], id AS [A] FROM docs ORDER BY a";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("docs.id", "a", true)));
}

#[test]
fn an_order_by_key_naming_labels_with_different_values_is_ambiguous() {
    let sql = "SELECT id AS [a], x AS [A] FROM docs ORDER BY a";
    assert!(matches!(sqlite_order_by(sql), Err(LookupError::AmbiguousTableLookup { .. })));
}

#[test]
fn an_order_by_key_after_a_set_operation_reports_the_leading_operands_label() {
    let sql = "SELECT id AS [VaLuE] FROM docs UNION SELECT x FROM docs ORDER BY [value]";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("UNION", "VaLuE", true)));
    let sql = "SELECT id AS [VaLuE] FROM docs UNION SELECT x FROM docs ORDER BY docs.id";
    assert_eq!(sqlite_order_by(sql).unwrap(), None);
}

#[test]
fn an_order_by_key_beside_an_unexpanded_output_stays_opaque() {
    let sql = "SELECT g.* FROM generate_series(1, 2) AS g ORDER BY missing";
    let resolved =
        order_by_in(&PostgreSqlDialect {}, IdentifierCase::AsWritten, sql, |query| query).unwrap();
    assert_eq!(resolved, Some(("opaque".to_owned(), None, None)));
}

#[test]
fn an_order_by_key_of_a_nested_query_reports_its_own_label() {
    let sql = "SELECT (SELECT [O].id AS [K] FROM docs AS i ORDER BY [k] LIMIT 1) FROM docs AS [o]";
    let resolved =
        order_by_in(&SQLiteDialect {}, IdentifierCase::Folded, sql, projected_subquery).unwrap();
    assert_eq!(resolved, Some(label("docs.id", "K", true)));
}

#[test]
fn an_order_by_key_over_a_recursive_cte_reports_the_label() {
    let sql = "WITH RECURSIVE r([N]) AS (SELECT 1 UNION ALL SELECT [n] + 1 FROM r WHERE [n] < 3) \
               SELECT [N] AS [Out] FROM r ORDER BY [OUT]";
    assert_eq!(sqlite_order_by(sql).unwrap(), Some(label("recursive", "Out", true)));
}

#[test]
fn a_group_by_key_prefers_an_input_column_over_a_label() {
    let sql = "SELECT id AS [X] FROM docs AS [D] GROUP BY x";
    assert_eq!(
        group_by_in(sql, |query| query).unwrap(),
        Some(column("docs.x", ("D", true), ("x", false)))
    );
}

#[test]
fn a_group_by_key_naming_no_input_column_reports_the_label() {
    let sql = "SELECT id AS [VaLuE] FROM docs GROUP BY [VALUE]";
    assert_eq!(group_by_in(sql, |query| query).unwrap(), Some(label("docs.id", "VaLuE", true)));
    let sql = "SELECT id AS [VaLuE] FROM docs GROUP BY [VALUE] + 1";
    assert_eq!(group_by_in(sql, |query| query).unwrap(), None);
}

#[test]
fn a_correlated_group_by_key_reports_the_outer_relation() {
    let sql = "SELECT (SELECT count(*) FROM docs AS i GROUP BY [O].[X]) FROM docs AS [o]";
    assert_eq!(
        group_by_in(sql, projected_subquery).unwrap(),
        Some(column("docs.x", ("o", true), ("x", false)))
    );
}

#[test]
fn a_distinct_on_key_prefers_a_label_over_an_input_column() {
    let sql = "SELECT DISTINCT ON (X) id AS \"X\" FROM docs";
    let expected = label("docs.id", "X", true);
    assert_eq!(distinct_on(IdentifierCase::Folded, sql).unwrap(), Some(expected));
    assert_eq!(
        distinct_on(IdentifierCase::AsWritten, sql).unwrap(),
        Some(column("docs.x", ("docs", false), ("x", false)))
    );
    let sql = "SELECT DISTINCT ON (x + 1) id AS x FROM docs";
    assert_eq!(distinct_on(IdentifierCase::Folded, sql).unwrap(), None);
}

fn relation_in(dialect: &dyn Dialect, sql: &str, pick: fn(&Query) -> &Query) -> Option<Name> {
    let database = database();
    let query = query(dialect, sql);
    let scope =
        ColumnScope::from_query(&query, &database, IdentifierCase::Folded).expect("scope builds");
    let select = body_select(pick(&query));
    let qualifier = match &select.projection[0] {
        SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(name), _) => name,
        SelectItem::UnnamedExpr(Expr::Function(function)) => {
            let FunctionArguments::List(list) = &function.args else {
                panic!("expected an argument list")
            };
            let FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(name)) = &list.args[0]
            else {
                panic!("expected a qualified wildcard argument")
            };
            name
        }
        other => panic!("expected a qualified wildcard, got {other}"),
    };
    let scope = scope.scope_for_select(select).expect("select is recorded");
    scope.resolve_relation_binding(qualifier).map(|binding| owned(binding.relation()))
}

fn sqlite_relation(sql: &str) -> Option<Name> {
    relation_in(&SQLiteDialect {}, sql, |query| query)
}

fn named(name: &str, quoted: bool) -> (String, bool) {
    (name.to_owned(), quoted)
}

#[test]
fn a_qualified_wildcard_reports_the_alias_it_matched() {
    let sql = "SELECT [MIXED].* FROM docs AS [MiXeD]";
    assert_eq!(sqlite_relation(sql), Some(Some(named("MiXeD", true))));
}

#[test]
fn a_qualified_wildcard_over_an_unaliased_table_reports_its_declared_name() {
    assert_eq!(sqlite_relation("SELECT [DOCS].* FROM docs"), Some(Some(named("docs", false))));
}

#[test]
fn a_qualified_wildcard_over_a_cte_reports_its_declared_name_or_alias() {
    let sql = "WITH [MiXeD] AS (SELECT id FROM docs) SELECT [mixed].* FROM [MIXED]";
    assert_eq!(sqlite_relation(sql), Some(Some(named("MiXeD", true))));
    let sql = "WITH [MiXeD] AS (SELECT id FROM docs) SELECT [C].* FROM [MIXED] AS [c]";
    assert_eq!(sqlite_relation(sql), Some(Some(named("c", true))));
}

#[test]
fn a_qualified_wildcard_over_a_derived_table_reports_its_alias() {
    let sql = "SELECT [D].* FROM (SELECT id FROM docs) AS [d]";
    assert_eq!(sqlite_relation(sql), Some(Some(named("d", true))));
}

#[test]
fn a_schema_qualified_wildcard_reports_the_table_it_matched() {
    let sql = "SELECT PUBLIC.\"DOCS\".* FROM public.docs";
    let resolved = relation_in(&PostgreSqlDialect {}, sql, |query| query);
    assert_eq!(resolved, Some(Some(named("docs", false))));
    let sql = "SELECT other.docs.* FROM public.docs";
    assert_eq!(relation_in(&PostgreSqlDialect {}, sql, |query| query), None);
}

#[test]
fn a_qualified_wildcard_over_an_opaque_relation_has_no_name() {
    let sql = "SELECT [G].* FROM generate_series(1, 2) AS [g]";
    assert_eq!(sqlite_relation(sql), Some(None));
}

#[test]
fn a_qualified_wildcard_naming_no_relation_is_unresolved() {
    assert_eq!(sqlite_relation("SELECT [missing].* FROM docs"), None);
}

#[test]
fn a_correlated_qualifier_reports_the_outer_relation() {
    let sql = "SELECT (SELECT count([O].*) FROM docs AS i) FROM docs AS [o]";
    let resolved = relation_in(&SQLiteDialect {}, sql, projected_subquery);
    assert_eq!(resolved, Some(Some(named("o", true))));
}

#[test]
fn a_local_relation_shadows_an_outer_one_of_the_same_name() {
    let sql = "SELECT (SELECT count([o].*) FROM docs AS [O]) FROM docs AS o";
    let resolved = relation_in(&SQLiteDialect {}, sql, projected_subquery);
    assert_eq!(resolved, Some(Some(named("O", true))));
}

#[test]
fn a_recursive_term_qualifies_its_own_cte_by_the_declared_name() {
    let database = database();
    let sql = "WITH RECURSIVE [MiXeD]([N]) AS \
               (SELECT 1 UNION ALL SELECT [mixed].* FROM [MIXED] WHERE [n] < 3) \
               SELECT [n] FROM [MiXeD]";
    let query = query(&SQLiteDialect {}, sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::Folded).unwrap();
    let cte = &query.with.as_ref().expect("a WITH clause").cte_tables[0].query;
    let SetExpr::SetOperation { right, .. } = cte.body.as_ref() else {
        panic!("expected a recursive union")
    };
    let SetExpr::Select(recursive) = right.as_ref() else { panic!("expected a recursive SELECT") };
    let SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(qualifier), _) =
        &recursive.projection[0]
    else {
        panic!("expected a qualified wildcard")
    };
    let recursive_scope = scope.scope_for_select(recursive).expect("the term is recorded");
    let binding = recursive_scope.resolve_relation_binding(qualifier).expect("relation resolves");
    assert_eq!(binding.relation(), Some(("MiXeD", true)));
}
