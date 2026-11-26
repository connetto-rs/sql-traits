//! Tests that a view reports the relation PostgreSQL rewrites a write through
//! it onto, with the column each of its output columns writes, exactly when
//! PostgreSQL makes the view automatically updatable.
//!
//! Every PostgreSQL statement here was measured against PostgreSQL 18, through
//! `information_schema.views`, `information_schema.columns` and
//! `pg_relation_is_updatable`.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

const SCHEMA: &str = "
    CREATE TABLE t (id INT PRIMARY KEY, x INT, y INT);
    CREATE TABLE u (id INT);
    CREATE TABLE w (id INT, txt TEXT);
    CREATE SEQUENCE s;
    CREATE MATERIALIZED VIEW mv AS SELECT id, x FROM t;
";

fn postgres(views: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{SCHEMA} {views}")).expect("the schema parses")
}

fn base<'db>(db: &'db ParserDB, name: &str) -> Option<UpdatableViewBase<'db, ParserDB>> {
    db.view_by_target(TargetName::new(name, false), IdentifierCase::Folded)
        .expect("the name resolves")
        .expect("the view exists")
        .updatable_base(db)
        .expect("the definition resolves")
}

fn written<'db>(db: &'db ParserDB, name: &str) -> Option<WrittenRelation<'db, ParserDB>> {
    base(db, name).map(|base| base.relation)
}

/// The table a view writes and the name of the column each output writes.
fn table_columns<'db>(db: &'db ParserDB, name: &str) -> (&'db str, Vec<Option<&'db str>>) {
    match written(db, name) {
        Some(WrittenRelation::Table { table, columns, .. }) => {
            (
                table.table_name(),
                columns.iter().map(|column| column.map(ColumnLike::column_name)).collect(),
            )
        }
        other => panic!("{name} should write a table, got {other:?}"),
    }
}

#[test]
fn a_view_renaming_a_column_writes_the_column_it_renames() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE users (id INT PRIMARY KEY);
         CREATE TABLE invitations (id INT PRIMARY KEY, organization_id INT NOT NULL,
           email TEXT NOT NULL, accepted_by_id INT REFERENCES users (id));
         CREATE VIEW open_invitations AS
           SELECT id, organization_id, accepted_by_id AS acceptor FROM invitations
           WHERE accepted_by_id IS NULL;",
    )
    .expect("the schema parses");
    assert_eq!(
        table_columns(&db, "open_invitations"),
        ("invitations", vec![Some("id"), Some("organization_id"), Some("accepted_by_id")])
    );
}

#[test]
fn only_a_simple_reference_to_a_column_writes_it() {
    let db = postgres(
        "CREATE VIEW plain AS SELECT id, x AS renamed, y + 1 AS computed, ctid AS row_ctid,
           t AS whole, x AS again FROM t WHERE id IN (SELECT id FROM u) ORDER BY id;
         CREATE VIEW wrapped AS SELECT id::bigint AS x8, (id) AS paren, txt COLLATE \"C\" AS coll,
           w.txt AS qual, public.w.id AS full FROM w;",
    );
    assert_eq!(
        table_columns(&db, "plain"),
        ("t", vec![Some("id"), Some("x"), None, None, None, Some("x")])
    );
    assert_eq!(
        table_columns(&db, "wrapped"),
        ("w", vec![None, Some("id"), None, Some("txt"), Some("id")])
    );
}

#[test]
fn columns_map_by_position_under_declared_and_alias_names() {
    let db = postgres(
        "CREATE VIEW declared (a, b) AS SELECT y, x FROM t;
         CREATE VIEW aliased AS SELECT q.p, r FROM t AS q(p, r);",
    );
    assert_eq!(table_columns(&db, "declared"), ("t", vec![Some("y"), Some("x")]));
    assert_eq!(table_columns(&db, "aliased"), ("t", vec![Some("id"), Some("x")]));
}

/// PostgreSQL still deletes through a view none of whose columns it can
/// write, so the view answers with no column to write.
#[test]
fn a_view_with_no_writable_column_still_names_its_table() {
    let db = postgres("CREATE VIEW constant AS SELECT 1 AS one FROM t;");
    assert_eq!(table_columns(&db, "constant"), ("t", vec![None]));
}

#[test]
fn clauses_postgres_accepts_keep_a_view_updatable() {
    let db = postgres(
        "CREATE VIEW locked AS SELECT id FROM t FOR UPDATE;
         CREATE VIEW only_parent AS SELECT id FROM ONLY t;
         CREATE VIEW parenthesized AS (SELECT id FROM t);
         CREATE VIEW windowed AS SELECT id FROM t WINDOW win AS (ORDER BY id);
         CREATE VIEW inner_aggregate AS SELECT (SELECT max(u.id) FROM u) AS m, id FROM t;
         CREATE VIEW mixed_aggregate AS SELECT (SELECT max(u.id + t.id) FROM u) AS m, id FROM t;
         CREATE VIEW nested_aggregate AS
             SELECT (SELECT max((SELECT w.id FROM w WHERE w.id = u.id)) FROM u) AS m, id FROM t;
         CREATE VIEW inner_series AS SELECT (SELECT generate_series(1, 2) LIMIT 1) AS g, id FROM t;
         CREATE VIEW whole_table AS TABLE w;",
    );
    for name in ["locked", "only_parent", "parenthesized", "windowed"] {
        assert_eq!(table_columns(&db, name), ("t", vec![Some("id")]), "{name}");
    }
    for name in ["inner_aggregate", "mixed_aggregate", "nested_aggregate", "inner_series"] {
        assert_eq!(table_columns(&db, name), ("t", vec![None, Some("id")]), "{name}");
    }
    assert_eq!(table_columns(&db, "whole_table"), ("w", vec![Some("id"), Some("txt")]));
}

#[test]
fn clauses_postgres_refuses_make_a_view_read_only() {
    let refused = [
        ("distinct", "SELECT DISTINCT id FROM t"),
        ("grouped", "SELECT id FROM t GROUP BY id"),
        ("having", "SELECT 1 AS one FROM t HAVING count(*) > 0"),
        ("limited", "SELECT id FROM t LIMIT 1"),
        ("offset", "SELECT id FROM t OFFSET 1"),
        ("fetched", "SELECT id FROM t FETCH FIRST 1 ROW ONLY"),
        ("unioned", "SELECT id FROM t UNION ALL SELECT id FROM u"),
        ("with_cte", "WITH c AS (SELECT 1) SELECT id FROM t"),
        ("joined", "SELECT t.id FROM t JOIN u ON t.id = u.id"),
        ("two_tables", "SELECT t.id FROM t, u"),
        ("derived", "SELECT id FROM (SELECT id FROM t) AS d"),
        ("function", "SELECT g FROM generate_series(1, 2) AS g"),
        ("listed", "VALUES (1)"),
        ("aggregate", "SELECT count(*) AS n FROM t"),
        ("window", "SELECT id, row_number() OVER () AS n FROM t"),
        ("series", "SELECT id, generate_series(1, 2) AS g FROM t"),
        ("ordered_aggregate", "SELECT 1 AS one FROM t ORDER BY count(*)"),
        ("ordered_window", "SELECT id FROM t ORDER BY row_number() OVER ()"),
        ("ordered_series", "SELECT id FROM t ORDER BY generate_series(1, 2)"),
        ("outer_aggregate", "SELECT (SELECT max(t.x) FROM u) AS m FROM t"),
        (
            "nested_outer_aggregate",
            "SELECT (SELECT max((SELECT t.x FROM w LIMIT 1)) FROM u) AS m FROM t",
        ),
        ("sampled", "SELECT id FROM t TABLESAMPLE SYSTEM (50)"),
        ("snapshot", "SELECT id FROM mv"),
        ("sequence", "SELECT last_value FROM s"),
    ];
    let views = refused.map(|(name, query)| format!("CREATE VIEW {name} AS {query};")).join("\n");
    let db = postgres(&views);
    for (name, _) in refused {
        assert!(base(&db, name).is_none(), "{name} is not automatically updatable");
    }
}

/// PostgreSQL rewrites a write through a view over a view onto the inner view
/// whether or not that one is automatically updatable, since an `INSTEAD OF`
/// trigger on it may take the write.
#[test]
fn a_view_over_a_view_writes_the_inner_view_by_output_position() {
    let db = postgres(
        "CREATE VIEW plain AS SELECT id, x AS renamed, y + 1 AS computed FROM t;
         CREATE VIEW distinct_ids AS SELECT DISTINCT id FROM t;
         CREATE VIEW whole_table AS TABLE w;
         CREATE VIEW over_plain AS SELECT computed, id, renamed AS again FROM plain;
         CREATE VIEW over_distinct AS SELECT id AS b FROM distinct_ids;
         CREATE VIEW over_whole_table AS SELECT * FROM whole_table;",
    );
    let Some(WrittenRelation::View { view, columns, .. }) = written(&db, "over_plain") else {
        panic!("over_plain writes a view");
    };
    assert_eq!((view.view_name(), columns), ("plain", vec![Some(2), Some(0), Some(1)]));
    let Some(WrittenRelation::View { view, columns, .. }) = written(&db, "over_distinct") else {
        panic!("over_distinct writes a view");
    };
    assert_eq!((view.view_name(), columns), ("distinct_ids", vec![Some(0)]));
    let Some(WrittenRelation::View { view, columns, .. }) = written(&db, "over_whole_table") else {
        panic!("over_whole_table writes a view");
    };
    assert_eq!((view.view_name(), columns), ("whole_table", vec![Some(0), Some(1)]));
}

#[test]
fn a_view_over_a_foreign_table_writes_the_foreign_table() {
    let db = postgres(
        "CREATE FOREIGN DATA WRAPPER wrapper; CREATE SERVER server FOREIGN DATA WRAPPER wrapper;
         CREATE FOREIGN TABLE remote (a INT, b INT) SERVER server;
         CREATE VIEW over_remote AS SELECT b FROM remote;",
    );
    let Some(WrittenRelation::ForeignTable { table, columns, .. }) = written(&db, "over_remote")
    else {
        panic!("over_remote writes a foreign table");
    };
    assert_eq!(table.table_name(), "remote");
    assert_eq!(
        columns.iter().map(|column| column.map(ColumnLike::column_name)).collect::<Vec<_>>(),
        [Some("b")]
    );
}

#[test]
fn a_materialized_view_takes_no_write() {
    let db = postgres("");
    let snapshot = db
        .materialized_view_by_target(TargetName::new("mv", false), IdentifierCase::Folded)
        .expect("the name resolves")
        .expect("the view exists");
    assert!(snapshot.updatable_base(&db).expect("the definition resolves").is_none());
}

/// A view keeps writing the table its name reached at creation after the
/// search path brings another table of that name forward, whose columns sit
/// in another order.
#[test]
fn a_view_writes_the_table_bound_at_creation() {
    let db = postgres(
        "CREATE TABLE orders (id INT, user_id INT);
         CREATE SCHEMA alt;
         CREATE TABLE alt.orders (user_id INT, id INT);
         CREATE VIEW open_orders AS SELECT user_id FROM orders;
         SET search_path TO alt, public;",
    );
    let Some(WrittenRelation::Table { table, columns, .. }) = written(&db, "open_orders") else {
        panic!("open_orders writes a table");
    };
    assert_eq!(table.table_schema(), None);
    assert_eq!(
        columns.iter().map(|column| column.map(ColumnLike::column_name)).collect::<Vec<_>>(),
        [Some("user_id")]
    );
    let written = columns[0].expect("the view writes `user_id`");
    let position = table
        .columns(&db)
        .expect("the table is held")
        .position(|column| core::ptr::eq(column, written));
    assert_eq!(position, Some(1));
}

/// SQLite never writes through a view, which only an `INSTEAD OF` trigger
/// makes writable there.
#[test]
fn a_sqlite_view_takes_no_write() {
    let db = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE t (a INTEGER); CREATE VIEW v AS SELECT a FROM t;",
    )
    .expect("the schema parses");
    assert!(base(&db, "v").is_none());
}

/// The text of each column reference the condition of `name` reads and the
/// name of the table column it reads, found by the index sqlparser's
/// `visit_expressions` walk gives the reference.
#[expect(
    clippy::disallowed_methods,
    reason = "the indices are the ones sqlparser's own walk gives a consumer"
)]
fn condition_reads(db: &ParserDB, name: &str) -> Vec<(String, String)> {
    let base = base(db, name).expect("the view is automatically updatable");
    let mut walked = Vec::new();
    if let Some(condition) = base.condition {
        let _ = sqlparser::ast::visit_expressions(condition, |expression| {
            walked.push(expression.to_string());
            core::ops::ControlFlow::<()>::Continue(())
        });
    }
    let WrittenRelation::Table { condition_columns, .. } = base.relation else {
        panic!("{name} should write a table");
    };
    condition_columns
        .iter()
        .map(|(index, column)| (walked[*index].clone(), column.column_name().to_string()))
        .collect()
}

/// A condition reads the base columns through the alias and column aliases
/// its `FROM` item gives them, qualified or not.
#[test]
fn a_condition_reads_base_columns_through_the_from_item_names() {
    let db = postgres(
        "CREATE VIEW moves AS SELECT a.id, a.moved FROM t AS a (id, moved)
           WHERE a.moved > 0 AND y IS NOT NULL;
         CREATE VIEW everything AS SELECT id FROM t;",
    );
    let moves = base(&db, "moves").expect("moves is automatically updatable");
    assert_eq!(
        moves.condition.map(ToString::to_string).as_deref(),
        Some("a.moved > 0 AND y IS NOT NULL")
    );
    assert_eq!(
        condition_reads(&db, "moves"),
        [("a.moved".to_string(), "x".to_string()), ("y".to_string(), "y".to_string())]
    );
    let everything = base(&db, "everything").expect("everything is automatically updatable");
    assert!(everything.condition.is_none());
    assert_eq!(condition_reads(&db, "everything"), Vec::<(String, String)>::new());
}

/// A reference in a subquery of the condition binding to the subquery's own
/// relation is left out, while one reaching out to the base relation is
/// listed.
#[test]
fn a_condition_subquery_lists_only_references_to_the_base_relation() {
    let db = postgres(
        "CREATE VIEW linked AS SELECT id FROM t
           WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.x);",
    );
    assert_eq!(condition_reads(&db, "linked"), [("t.x".to_string(), "x".to_string())]);
}

/// A whole-row reference to the base relation reads no single column, so it
/// is left out.
#[test]
fn a_whole_row_reference_in_the_condition_is_left_out() {
    let db =
        postgres("CREATE VIEW described AS SELECT id FROM t WHERE row_to_json(t) IS NOT NULL;");
    assert_eq!(condition_reads(&db, "described"), Vec::<(String, String)>::new());
}

/// The field a composite column's access chain names is no column reference,
/// so only the column holding the composite is listed.
#[test]
fn a_field_name_in_the_condition_is_left_out() {
    let db = postgres(
        "CREATE TYPE money_t AS (amount INT);
         CREATE TABLE payments (id INT, payload money_t);
         CREATE VIEW positive AS SELECT id FROM payments AS p WHERE (p.payload).amount > 0;",
    );
    assert_eq!(
        condition_reads(&db, "positive"),
        [("p.payload".to_string(), "payload".to_string())]
    );
}

/// The condition of a view over a view reads the inner view's output columns
/// by position.
#[test]
fn a_condition_over_a_view_reads_output_positions() {
    let db = postgres(
        "CREATE VIEW plain AS SELECT id, x AS renamed FROM t;
         CREATE VIEW over_plain AS SELECT id FROM plain WHERE renamed > 0;",
    );
    let Some(WrittenRelation::View { condition_columns, .. }) = written(&db, "over_plain") else {
        panic!("over_plain writes a view");
    };
    // `renamed > 0` is the walk's first node, `renamed` its second.
    assert_eq!(condition_columns, [(1, 1)]);
}
