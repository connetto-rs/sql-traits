//! Integration tests for the select-list item and the binding behind each
//! output of a recorded query.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking on unexpected outputs"
)]

use sql_traits::prelude::*;
use sqlparser::{
    ast::{Expr, Ident, Query, SetExpr, TableFactor},
    dialect::PostgreSqlDialect,
};

use crate::support::query_parsing::query_with;

const SCHEMA: &str = "
    CREATE TABLE t(id INT, c INT);
    CREATE TABLE u(id INT, w INT);
    CREATE VIEW v AS SELECT id, c AS renamed FROM t;
";

fn database() -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses")
}

/// Renders each output as its select-list item and, for a column a wildcard
/// expanded into, the reference reaching it, `relation.column` or the bare
/// name of a merged column.
fn origins(sql: &str) -> Vec<String> {
    let database = database();
    let query = query_with(&PostgreSqlDialect {}, sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    scope
        .scope_for_query(&query)
        .expect("query is recorded")
        .outputs()
        .map(|output| {
            match output {
                QueryOutput::Column { item, wildcard: None, .. } => format!("{item}"),
                QueryOutput::Column { item, wildcard: Some(binding), .. } => {
                    let (column, _) = binding.exposed().expect("an expanded column is named");
                    let merged = if binding.merge().is_some() { " merged" } else { "" };
                    match binding.relation() {
                        Some((relation, _)) => format!("{item} {relation}.{column}{merged}"),
                        None => format!("{item} {column}{merged}"),
                    }
                }
                QueryOutput::Unexpanded { item: Some(item) } => format!("{item} *"),
                QueryOutput::Unexpanded { item: None } => "*".to_owned(),
            }
        })
        .collect()
}

/// The reference `binding` names, `relation.column` or the bare name of a
/// merged column.
fn reference(binding: &ColumnBinding<'_>) -> Expr {
    let (column, quoted) = binding.exposed().expect("an expanded column is named");
    let column = ident(column, quoted);
    match binding.relation() {
        Some((relation, quoted)) => Expr::CompoundIdentifier(vec![ident(relation, quoted), column]),
        None => Expr::Identifier(column),
    }
}

fn ident(value: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', value) } else { Ident::new(value) }
}

/// Asserts that a reference written from each expanded column's binding
/// resolves to that same column.
fn assert_bindings_resolve(sql: &str) {
    let database = database();
    let query = query_with(&PostgreSqlDialect {}, sql);
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let mut expanded = 0;
    for output in scope.scope_for_query(&query).expect("query is recorded").outputs() {
        let QueryOutput::Column { wildcard: Some(binding), .. } = output else { continue };
        let written = reference(&binding);
        let (_, resolved) = scope
            .resolve_column_binding(&written)
            .expect("the reference resolves")
            .unwrap_or_else(|| panic!("`{written}` binds in `{sql}`"));
        assert_eq!(resolved.entry(), binding.entry(), "`{written}` reaches its entry in `{sql}`");
        assert_eq!(resolved.merge(), binding.merge(), "`{written}` reaches its merge in `{sql}`");
        assert_eq!(resolved.relation(), binding.relation(), "`{written}` in `{sql}`");
        assert_eq!(resolved.exposed(), binding.exposed(), "`{written}` in `{sql}`");
        expanded += 1;
    }
    assert!(expanded > 0, "`{sql}` expands a wildcard");
}

/// The `FROM` item a query's leading join tree holds at `index`, its first
/// relation at zero and the relation of join `index - 1` after it.
fn from_factor(query: &Query, index: usize) -> &TableFactor {
    let SetExpr::Select(select) = query.body.as_ref() else { panic!("expected a SELECT body") };
    let tree = &select.from[0];
    match index.checked_sub(1) {
        None => &tree.relation,
        Some(join) => &tree.joins[join].relation,
    }
}

#[test]
fn each_output_names_its_select_list_item_and_the_relation_a_wildcard_reads() {
    assert_eq!(
        origins("SELECT a.*, 1 AS k, b.* FROM t AS a JOIN t AS b ON a.id = b.id"),
        ["0 a.id", "0 a.c", "1", "2 b.id", "2 b.c"]
    );
    assert_eq!(origins("SELECT *, t.* FROM t"), ["0 t.id", "0 t.c", "1 t.id", "1 t.c"]);
}

#[test]
fn a_self_join_keeps_the_two_copies_of_a_column_apart() {
    let database = database();
    let query = query_with(&PostgreSqlDialect {}, "SELECT * FROM t AS a JOIN t AS b ON true");
    let scope = ColumnScope::from_query(&query, &database, IdentifierCase::AsWritten)
        .expect("scope builds");
    let entries = scope
        .scope_for_query(&query)
        .expect("query is recorded")
        .outputs()
        .map(|output| {
            let QueryOutput::Column { wildcard: Some(binding), .. } = output else {
                panic!("expected an expanded column")
            };
            let entry = binding.entry().expect("a column of one FROM item");
            (core::ptr::from_ref(entry.factor()), entry.ordinal())
        })
        .collect::<Vec<_>>();
    let a = core::ptr::from_ref(from_factor(&query, 0));
    let b = core::ptr::from_ref(from_factor(&query, 1));
    assert_eq!(entries, [(a, 0), (a, 1), (b, 0), (b, 1)]);
}

#[test]
fn merged_columns_lead_a_wildcard_over_a_using_or_natural_join() {
    assert_eq!(origins("SELECT * FROM t JOIN u USING (id)"), ["0 id merged", "0 t.c", "0 u.w"]);
    assert_eq!(origins("SELECT * FROM t NATURAL JOIN u"), ["0 id merged", "0 t.c", "0 u.w"]);
    assert_eq!(
        origins("SELECT u.*, t.* FROM t JOIN u USING (id)"),
        ["0 u.id", "0 u.w", "1 t.id", "1 t.c"]
    );
    assert_eq!(origins("SELECT j.* FROM t JOIN u USING (id) AS j"), ["0 j.id merged"]);
}

#[test]
fn derived_relations_views_and_ctes_are_read_through_their_exposed_names() {
    assert_eq!(
        origins("SELECT * FROM (SELECT id FROM t) AS s, LATERAL (SELECT s.id AS l_id) AS l"),
        ["0 s.id", "0 l.l_id"]
    );
    assert_eq!(origins("WITH q(x) AS (SELECT c FROM t) SELECT * FROM q"), ["0 q.x"]);
    assert_eq!(origins("SELECT * FROM v"), ["0 v.id", "0 v.renamed"]);
    assert_eq!(origins("SELECT * FROM t AS r(p)"), ["0 r.p", "0 r.c"]);
    assert_eq!(origins("SELECT * FROM (SELECT id FROM t)"), ["0 id"]);
}

#[test]
fn an_expanded_column_binds_where_a_reference_from_its_binding_binds() {
    for sql in [
        "SELECT a.*, 1 AS k, b.* FROM t AS a JOIN t AS b ON a.id = b.id",
        "SELECT * FROM t JOIN u USING (id)",
        "SELECT * FROM t NATURAL JOIN u",
        "SELECT u.*, t.* FROM t JOIN u USING (id)",
        "SELECT j.* FROM t JOIN u USING (id) AS j",
        "SELECT * FROM (SELECT id FROM t) AS s, LATERAL (SELECT s.id AS l_id) AS l",
        "WITH q(x) AS (SELECT c FROM t) SELECT * FROM q",
        "SELECT * FROM v",
        "SELECT * FROM t AS r(p)",
        "SELECT * FROM \"t\" AS \"R\"",
        "SELECT * FROM (SELECT id FROM t)",
    ] {
        assert_bindings_resolve(sql);
    }
}

#[test]
fn an_unexpanded_wildcard_names_its_item() {
    assert_eq!(origins("SELECT t.id, *, 1 FROM t, generate_series(1, 2) AS g"), ["0", "1 *", "2"]);
}

#[test]
fn set_operations_keep_the_items_of_their_leading_select() {
    assert_eq!(origins("SELECT t.*, 1 FROM t UNION SELECT 1, 2, 3"), ["0 t.id", "0 t.c", "1"]);
    assert_eq!(origins("VALUES (1, 2)"), ["0", "1"]);
    assert_eq!(origins("VALUES (1, 2), (3)"), ["*"]);
}
