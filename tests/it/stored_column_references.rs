//! Tests that a column reference no relation in reach holds is refused where
//! PostgreSQL 18 refuses it with `42703` when the statement storing it runs,
//! and that every reference PostgreSQL resolves stays accepted.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

/// The table every case reads.
const BASE: &str = "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT);";

/// A trigger function every trigger case executes.
const FUNCTION: &str =
    "CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;";

fn parse(statements: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{BASE}\n{FUNCTION}\n{statements}"))
}

/// Each statement naming a column no relation in reach holds is refused.
#[test]
fn an_unknown_column_is_refused() {
    for statement in [
        r#"CREATE VIEW v1 AS SELECT "nosuch" FROM t;"#,
        r#"CREATE VIEW v3 AS SELECT id FROM t WHERE "nosuch" = 'nosuch';"#,
        "CREATE VIEW v2 AS SELECT nosuch FROM t;",
        r#"CREATE MATERIALIZED VIEW m AS SELECT "nosuch" FROM t;"#,
        r#"CREATE TABLE c2 AS SELECT "nosuch" FROM t;"#,
        r#"SELECT "nosuch" INTO c3 FROM t;"#,
        r#"CREATE TABLE c4 (id INT, g TEXT GENERATED ALWAYS AS ("nosuch") STORED);"#,
        r#"ALTER TABLE t ADD COLUMN g TEXT GENERATED ALWAYS AS ("nosuch") STORED;"#,
        r#"CREATE UNIQUE INDEX i2 ON t (b) WHERE "nosuch" > 0;"#,
        r#"CREATE INDEX i3 ON t (("nosuch"));"#,
        r#"CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW."nosuch" IS NULL) EXECUTE FUNCTION f();"#,
        r#"CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN ("nosuch" IS NULL) EXECUTE FUNCTION f();"#,
        r#"CREATE POLICY p1 ON t USING ("nosuch" = 'nosuch');"#,
        r#"CREATE POLICY p2 ON t FOR INSERT WITH CHECK ("nosuch" = 'nosuch');"#,
        r#"CREATE POLICY p3 ON t USING (true); ALTER POLICY p3 ON t USING ("nosuch" = 'nosuch');"#,
        r#"CREATE VIEW v4 AS SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t AS u WHERE u."nosuch" = t.a);"#,
        r#"CREATE VIEW v5 AS WITH w AS (SELECT id FROM t) SELECT "nosuch" FROM w;"#,
        r#"CREATE VIEW v6 AS SELECT t."nosuch" FROM t;"#,
        r#"CREATE MATERIALIZED VIEW m AS SELECT a FROM t; CREATE INDEX mi ON m (("nosuch"));"#,
        r#"CREATE VIEW v7 AS SELECT public.t."nosuch" FROM t;"#,
        r#"CREATE VIEW v8 AS SELECT unknown_catalog.public.t."nosuch" FROM t;"#,
        r#"CREATE VIEW v9 AS SELECT id FROM t ORDER BY "nosuch";"#,
        r#"CREATE VIEW v10 AS SELECT id FROM t UNION SELECT id FROM t ORDER BY "nosuch";"#,
    ] {
        let parsed = parse(statement);
        assert!(
            matches!(
                &parsed,
                Err(Error::UnknownColumn { reference }) if reference.contains("nosuch")
            ),
            "{statement}\ngot {parsed:?}"
        );
    }
}

/// Each reference PostgreSQL resolves stays accepted, whole-row references,
/// system columns, fields of a composite column, a function applied to a row,
/// niladic functions, output names and outer references included.
#[test]
fn a_resolved_column_stays_accepted() {
    for statement in [
        "CREATE VIEW v AS SELECT t FROM t;",
        "CREATE VIEW v AS SELECT row_to_json(t) AS j FROM t;",
        "CREATE VIEW v AS SELECT current_user AS u, session_user AS s, current_date AS d FROM t;",
        "CREATE VIEW v AS SELECT a AS x FROM t ORDER BY x;",
        "CREATE VIEW v AS SELECT a AS x, count(*) AS n FROM t GROUP BY x;",
        "CREATE VIEW v AS SELECT id FROM t JOIN t AS u USING (id);",
        "CREATE VIEW v AS WITH w AS (SELECT id AS k FROM t) SELECT k FROM w;",
        "CREATE VIEW v AS SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t AS u WHERE u.a = t.a);",
        "CREATE VIEW v AS SELECT id, l.n FROM t, LATERAL (SELECT a + 1 AS n) AS l;",
        "CREATE VIEW v AS SELECT x FROM generate_series(1, 3) AS x;",
        "CREATE VIEW v (c) AS SELECT a FROM t;",
        "CREATE TABLE c AS SELECT a, b FROM t;",
        "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id * 2) STORED);",
        "CREATE TABLE c (g INT GENERATED ALWAYS AS (id * 2) STORED, id INT);",
        "CREATE INDEX i ON t ((a + 1)) WHERE b IS NOT NULL;",
        "CREATE INDEX i ON t (lower(b)) WHERE t.a > 0;",
        "CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW IS DISTINCT FROM OLD) EXECUTE FUNCTION f();",
        "CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.a <> OLD.a) EXECUTE FUNCTION f();",
        "CREATE POLICY p ON t USING (b = current_user);",
        "CREATE POLICY p ON t USING (a IN (SELECT u.a FROM t AS u WHERE u.id = t.id));",
        "CREATE POLICY p ON t FOR INSERT WITH CHECK (t.b IS NOT NULL);",
        "CREATE VIEW v AS SELECT ctid, tableoid, t.xmin FROM t;",
        "CREATE VIEW v AS SELECT id FROM t ORDER BY ctid;",
        "CREATE VIEW v AS SELECT column1 FROM (VALUES (1)) AS x;",
        "CREATE VIEW v AS SELECT t.row_to_json, t.text FROM t;",
        "CREATE VIEW v AS SELECT make_interval(days => a) AS i FROM t;",
        "CREATE TYPE pair AS (x INT, y INT);
         CREATE TABLE p (id INT, q pair);
         CREATE VIEW v AS SELECT (q).x, p.q.y, (p.q).x AS z FROM p;",
        "CREATE TABLE c (a INT, g INT GENERATED ALWAYS AS (a * 2) STORED);
         ALTER TABLE c RENAME COLUMN a TO b;
         ALTER TABLE c ADD COLUMN z INT;",
        "CREATE MATERIALIZED VIEW m AS SELECT a FROM t; CREATE INDEX mi ON m ((a + 1));",
        "CREATE VIEW v AS SELECT public.t.a, unknown_catalog.public.t.b FROM t;",
    ] {
        let parsed = parse(statement);
        assert!(parsed.is_ok(), "{statement}\ngot {parsed:?}");
    }
}

/// A double-quoted name SQLite reads as a string when no column holds it is
/// left to SQLite's own reading.
#[test]
fn another_dialect_keeps_its_reading() {
    let parsed = ParserDB::parse::<SQLiteDialect>(
        r#"CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT);
           CREATE VIEW v AS SELECT "nosuch" FROM t;
           CREATE INDEX i ON t (b) WHERE "nosuch" > 0;"#,
    );
    assert!(parsed.is_ok(), "got {parsed:?}");
}
