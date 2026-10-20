//! A view keeps reading the relations and columns it was created over, under
//! whatever names they carry later, and keeps the column names it exposes,
//! while the model keeps the definition as written where PostgreSQL deparses
//! it.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::{ast::Statement, dialect::PostgreSqlDialect, parser::Parser};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the schema parses")
}

/// A query as the parser renders it, so an expectation does not depend on
/// how the renderer spaces or capitalizes.
fn rendered(query: &str) -> String {
    let Some(Statement::Query(query)) =
        Parser::parse_sql(&PostgreSqlDialect {}, query).expect("the query parses").pop()
    else {
        panic!("expected a query")
    };
    query.to_string()
}

fn view_definition(db: &ParserDB, name: &str) -> String {
    let target = TargetName::new(name, false);
    if let Some(view) = db.view_by_target(target.clone(), IdentifierCase::AsWritten).unwrap() {
        return view.definition().to_string();
    }
    db.materialized_view_by_target(target, IdentifierCase::AsWritten)
        .unwrap()
        .expect("the view exists")
        .definition()
        .to_string()
}

fn assert_definition(schema: &str, view: &str, expected: &str) {
    let db = parse(schema);
    assert_eq!(view_definition(&db, view), rendered(expected), "{schema}");
}

#[test]
fn a_view_follows_a_renamed_table_and_column() {
    assert_definition(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t;
         ALTER TABLE t RENAME TO u; ALTER TABLE u RENAME COLUMN a TO b;",
        "v",
        "SELECT b AS a FROM u",
    );
}

#[test]
fn a_qualifier_naming_the_renamed_table_follows_it() {
    assert_definition(
        "CREATE TABLE t (a INT); CREATE TABLE s (c INT);
         CREATE VIEW v AS SELECT t.a, x.c FROM t JOIN s AS x ON x.c = t.a;
         ALTER TABLE t RENAME TO u;",
        "v",
        "SELECT u.a, x.c FROM u JOIN s AS x ON x.c = u.a",
    );
    assert_definition(
        "CREATE SCHEMA s; CREATE TABLE s.t (a INT);
         CREATE VIEW v AS SELECT s.t.a FROM s.t;
         ALTER TABLE s.t RENAME TO u;",
        "v",
        "SELECT s.u.a FROM s.u",
    );
}

#[test]
fn an_alias_shields_its_references_from_a_table_rename() {
    assert_definition(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT x.a FROM t AS x WHERE x.a > 0;
         ALTER TABLE t RENAME TO u; ALTER TABLE u RENAME COLUMN a TO b;",
        "v",
        "SELECT x.b AS a FROM u AS x WHERE x.b > 0",
    );
}

#[test]
fn a_renamed_column_keeps_every_exposed_name_in_nested_queries() {
    assert_definition(
        "CREATE TABLE s1 (a INT);
         CREATE VIEW v AS
             SELECT s.a FROM (SELECT a FROM s1) AS s
             WHERE a IN (SELECT a FROM s1 AS t2) ORDER BY a;
         ALTER TABLE s1 RENAME COLUMN a TO b;",
        "v",
        "SELECT s.a FROM (SELECT b AS a FROM s1) AS s
         WHERE a IN (SELECT b AS a FROM s1 AS t2) ORDER BY a",
    );
    assert_definition(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS WITH c AS (SELECT a FROM t) SELECT a FROM c;
         ALTER TABLE t RENAME COLUMN a TO b;",
        "v",
        "WITH c AS (SELECT b AS a FROM t) SELECT a FROM c",
    );
}

#[test]
fn a_renamed_column_follows_into_expressions_and_clauses() {
    assert_definition(
        "CREATE TABLE t (a INT, k INT);
         CREATE VIEW v AS
             SELECT a + 1 AS n, a, count(*) AS c FROM t WHERE a > 0 GROUP BY a ORDER BY a;
         ALTER TABLE t RENAME COLUMN a TO b;",
        "v",
        "SELECT b + 1 AS n, b AS a, count(*) AS c FROM t WHERE b > 0 GROUP BY b ORDER BY a",
    );
}

#[test]
fn a_rename_another_relation_would_capture_is_qualified() {
    assert_definition(
        "CREATE TABLE t (a INT); CREATE TABLE x (b INT);
         CREATE VIEW v AS SELECT a FROM t WHERE EXISTS (SELECT 1 FROM x WHERE x.b = a);
         ALTER TABLE t RENAME COLUMN a TO b;",
        "v",
        "SELECT b AS a FROM t WHERE EXISTS (SELECT 1 FROM x WHERE x.b = t.b)",
    );
    assert_definition(
        "CREATE TABLE t (a INT); CREATE TABLE x (b INT);
         CREATE VIEW v AS SELECT a FROM t, x;
         ALTER TABLE t RENAME COLUMN a TO b;",
        "v",
        "SELECT t.b AS a FROM t, x",
    );
}

#[test]
fn a_join_on_a_renamed_column_keeps_its_using_list() {
    assert_definition(
        "CREATE TABLE j1 (k INT, x INT); CREATE TABLE j2 (k INT, y INT);
         CREATE VIEW v AS SELECT k, x, y FROM j1 JOIN j2 USING (k);
         ALTER TABLE j2 RENAME COLUMN k TO kk;",
        "v",
        "SELECT k, x, y FROM j1 JOIN j2 AS j2 (k, y) USING (k)",
    );
}

#[test]
fn a_wildcard_is_expanded_when_the_view_is_created() {
    assert_definition(
        "CREATE TABLE q (a INT); CREATE VIEW v AS SELECT * FROM q;",
        "v",
        "SELECT a FROM q",
    );
    assert_definition(
        "CREATE TABLE q (a INT); CREATE VIEW v AS SELECT * FROM q;
         ALTER TABLE q ADD COLUMN b INT; ALTER TABLE q RENAME COLUMN a TO aa;",
        "v",
        "SELECT aa AS a FROM q",
    );
    assert_definition(
        "CREATE TABLE q (a INT, \"B\" INT); CREATE VIEW v AS SELECT q.*, 1 AS one FROM q;",
        "v",
        "SELECT q.a, q.\"B\", 1 AS one FROM q",
    );
    let joined = |join: &str, expected: &str| {
        assert_definition(
            &format!(
                "CREATE TABLE j1 (k INT, x INT); CREATE TABLE j2 (k INT, y INT);
                 CREATE VIEW v AS SELECT * FROM j1 {join} j2 USING (k);"
            ),
            "v",
            expected,
        );
    };
    joined("JOIN", "SELECT j1.k, j1.x, j2.y FROM j1 JOIN j2 USING (k)");
    joined("LEFT JOIN", "SELECT j1.k, j1.x, j2.y FROM j1 LEFT JOIN j2 USING (k)");
    joined("RIGHT JOIN", "SELECT j2.k, j1.x, j2.y FROM j1 RIGHT JOIN j2 USING (k)");
    joined("FULL JOIN", "SELECT k, j1.x, j2.y FROM j1 FULL JOIN j2 USING (k)");
}

#[test]
fn a_natural_join_is_spelled_out_when_the_view_is_created() {
    assert_definition(
        "CREATE TABLE j1 (k INT, x INT); CREATE TABLE j2 (k INT, y INT);
         CREATE VIEW v AS SELECT * FROM j1 NATURAL JOIN j2;
         ALTER TABLE j2 RENAME COLUMN k TO kk;",
        "v",
        "SELECT j1.k, j1.x, j2.y FROM j1 JOIN j2 AS j2 (k, y) USING (k)",
    );
}

#[test]
fn a_view_follows_a_renamed_view_and_view_column() {
    let schema = "CREATE TABLE p (a INT, c INT);
                  CREATE VIEW pv AS SELECT a, c FROM p;
                  CREATE VIEW pw AS SELECT a FROM pv;
                  ALTER TABLE pv RENAME COLUMN a TO z;";
    assert_definition(schema, "pw", "SELECT z AS a FROM pv");
    assert_definition(schema, "pv", "SELECT a AS z, c FROM p");
    assert_definition(
        &format!("{schema} ALTER TABLE pv RENAME TO pv2;"),
        "pw",
        "SELECT z AS a FROM pv2",
    );
    assert_definition(
        "CREATE TABLE p (a INT, c INT);
         CREATE VIEW pv (x, y) AS SELECT a, c FROM p;
         CREATE VIEW pw AS SELECT x FROM pv;
         ALTER TABLE pv RENAME COLUMN x TO z;",
        "pw",
        "SELECT z AS x FROM pv",
    );
    assert_definition(
        "CREATE TABLE p (a INT, c INT);
         CREATE MATERIALIZED VIEW pm AS SELECT a, c FROM p;
         CREATE VIEW px AS SELECT a FROM pm;
         ALTER TABLE pm RENAME COLUMN a TO y;",
        "px",
        "SELECT y AS a FROM pm",
    );
    // An unaliased computed column leaves `pv` opaque to the resolver.
    let computed = "CREATE TABLE p (a INT, c INT);
                    CREATE VIEW pv AS SELECT a + 1, c FROM p;
                    CREATE VIEW pw AS SELECT c FROM pv;
                    ALTER TABLE pv RENAME COLUMN c TO z;";
    assert_definition(computed, "pv", "SELECT a + 1, c AS z FROM p");
    assert_definition(computed, "pw", "SELECT c FROM pv AS pv (\"?column?\", c)");
    assert_definition(
        &format!("{computed} ALTER TABLE pv RENAME COLUMN \"?column?\" TO n;"),
        "pv",
        "SELECT a + 1 AS n, c AS z FROM p",
    );
}

#[test]
fn a_view_over_a_child_follows_a_parent_column_rename() {
    let schema = "CREATE TABLE parent (a INT); CREATE TABLE child () INHERITS (parent);
                  CREATE VIEW pv AS SELECT a FROM parent;
                  CREATE VIEW cv AS SELECT a FROM child;
                  ALTER TABLE parent RENAME COLUMN a TO z;";
    assert_definition(schema, "pv", "SELECT z AS a FROM parent");
    assert_definition(schema, "cv", "SELECT z AS a FROM child");
}

#[test]
fn a_renamed_column_a_view_does_not_read_leaves_it_alone() {
    assert_definition(
        "CREATE TABLE t (a INT, c INT); CREATE TABLE s (a INT);
         CREATE VIEW v AS SELECT t.a FROM t;
         ALTER TABLE t RENAME COLUMN c TO d; ALTER TABLE s RENAME COLUMN a TO b;
         ALTER TABLE s RENAME TO r;",
        "v",
        "SELECT t.a FROM t",
    );
}

#[test]
fn a_renamed_table_still_refuses_a_drop_while_a_view_reads_it() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t;
         ALTER TABLE t RENAME TO u; DROP TABLE u;",
    )
    .expect_err("PostgreSQL refuses: view v depends on table u");
    assert!(
        matches!(
            &error,
            Error::RelationHasDependents { object_kind: ObjectKind::Table, dependent_name, .. }
                if dependent_name == "v"
        ),
        "{error}"
    );
}

#[test]
fn a_definition_whose_bindings_are_uncertain_keeps_its_old_names() {
    // The resolver reads a parenthesized join as a relation it cannot see into.
    assert_definition(
        "CREATE TABLE t (a INT, c INT); CREATE TABLE s (d INT);
         CREATE VIEW v AS SELECT a, d FROM (t JOIN s ON t.c = s.d);
         ALTER TABLE t RENAME COLUMN a TO b;",
        "v",
        "SELECT a, d FROM (t AS t (a, c) JOIN s ON t.c = s.d)",
    );
    assert_definition(
        "CREATE TABLE t (a INT, c INT); CREATE TABLE s (d INT);
         CREATE VIEW v AS SELECT t.a, d FROM (t JOIN s ON t.c = s.d);
         ALTER TABLE t RENAME TO u;",
        "v",
        "SELECT t.a, d FROM (u AS t JOIN s ON t.c = s.d)",
    );
}

#[test]
fn a_new_table_name_another_relation_already_exposes_keeps_the_old_one() {
    assert_definition(
        "CREATE TABLE t (a INT); CREATE TABLE y (z INT);
         CREATE VIEW v AS
             SELECT u.z FROM y AS u WHERE EXISTS (SELECT 1 FROM t WHERE t.a = u.z);
         ALTER TABLE t RENAME TO u;",
        "v",
        "SELECT u.z FROM y AS u WHERE EXISTS (SELECT 1 FROM u AS t WHERE t.a = u.z)",
    );
}

#[test]
fn a_view_column_rename_is_refused_where_postgresql_refuses_it() {
    let schema = "CREATE TABLE p (a INT, c INT); CREATE VIEW pv AS SELECT a, c FROM p;";
    let missing = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{schema} ALTER TABLE pv RENAME COLUMN nope TO x;"
    ))
    .expect_err("PostgreSQL refuses: column \"nope\" does not exist");
    assert!(
        matches!(
            &missing,
            Error::IdentifierLookupError(LookupError::ColumnNotFound { column_name, .. })
                if column_name == "nope"
        ),
        "{missing}"
    );
    let taken = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{schema} ALTER TABLE pv RENAME COLUMN a TO c;"
    ))
    .expect_err("PostgreSQL refuses: column \"c\" of relation \"pv\" already exists");
    assert!(
        matches!(&taken, Error::ColumnAlreadyExists { column_name, .. } if column_name == "c"),
        "{taken}"
    );
}

#[test]
fn a_rolled_back_rename_restores_the_view_definitions_it_rewrote() {
    let schema = "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t;
                  CREATE VIEW w AS SELECT a FROM v;";
    let renames = "ALTER TABLE t RENAME TO u; ALTER TABLE u RENAME COLUMN a TO b;
                   ALTER TABLE v RENAME COLUMN a TO c;";
    for (open, close) in
        [("BEGIN;", "ROLLBACK;"), ("BEGIN; SAVEPOINT s;", "ROLLBACK TO SAVEPOINT s; COMMIT;")]
    {
        let sql = format!("{schema} {open} {renames} {close}");
        assert_definition(&sql, "v", "SELECT a FROM t");
        assert_definition(&sql, "w", "SELECT a FROM v");
        let error = ParserDB::parse::<PostgreSqlDialect>(&format!("{sql} DROP TABLE t;"))
            .expect_err("the restored view still reads t");
        assert!(matches!(&error, Error::RelationHasDependents { .. }), "{error}");
    }
}
