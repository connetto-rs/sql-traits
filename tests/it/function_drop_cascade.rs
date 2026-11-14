//! Tests that a `CASCADE` removing a function takes everything holding the
//! function, transitively, as PostgreSQL 18 does.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

const CALLED: &str = "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
     CREATE TABLE t (a INT);";

/// Declares `f` again, which only succeeds once the cascade took it.
const REDECLARED: &str = "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';";

/// A statement writing a call to `f` into each object PostgreSQL binds the
/// call of, with statements PostgreSQL 18 accepts only once
/// `DROP FUNCTION f() CASCADE` took that object and kept what it belongs to.
const DEPENDENTS: [(&str, &str); 17] = [
    ("CREATE TABLE d (a INT DEFAULT f());", "ALTER TABLE d DROP COLUMN a;"),
    ("CREATE TABLE d (a INT GENERATED ALWAYS AS (f()) STORED);", "ALTER TABLE d ADD COLUMN a INT;"),
    (
        "CREATE TABLE d (a INT CONSTRAINT ck CHECK (a > f()));",
        "ALTER TABLE d ADD CONSTRAINT ck CHECK (a > 0);",
    ),
    ("ALTER TABLE t ALTER COLUMN a SET DEFAULT f();", "ALTER TABLE t DROP COLUMN a;"),
    (
        "CREATE TABLE d (a INT CHECK (a > f()));",
        "ALTER TABLE d ADD CONSTRAINT d_a_check CHECK (a > 0);",
    ),
    ("CREATE INDEX i ON t ((f() + a));", "CREATE INDEX i ON t (a);"),
    ("CREATE INDEX i ON t (a) WHERE f() > 0;", "CREATE INDEX i ON t (a);"),
    (
        "CREATE TABLE x (a INT, CONSTRAINT xc EXCLUDE USING btree ((a + f()) WITH =));",
        "ALTER TABLE x ADD CONSTRAINT xc EXCLUDE USING btree (a WITH =);",
    ),
    ("CREATE VIEW v AS SELECT f() AS one;", "CREATE VIEW v AS SELECT 1 AS one;"),
    (
        "CREATE MATERIALIZED VIEW v AS SELECT f() AS one;",
        "CREATE MATERIALIZED VIEW v AS SELECT 1 AS one;",
    ),
    ("CREATE POLICY pol ON t USING (f() > 0);", "CREATE POLICY pol ON t USING (true);"),
    (
        "CREATE RULE r AS ON INSERT TO t WHERE f() > 0 DO INSTEAD NOTHING;",
        "CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING;",
    ),
    ("CREATE DOMAIN d AS INT CHECK (VALUE > f());", "CREATE TABLE z (c d);"),
    ("CREATE DOMAIN d AS INT DEFAULT f();", "CREATE DOMAIN d AS INT;"),
    (
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (f() > 0) EXECUTE FUNCTION trg();",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();",
    ),
    ("CREATE TABLE p (a INT) PARTITION BY RANGE ((a + f()));", "CREATE TABLE p (a INT);"),
    (
        "CREATE FUNCTION caller() RETURNS INT LANGUAGE sql RETURN f();",
        "CREATE FUNCTION caller() RETURNS INT LANGUAGE sql RETURN 1;",
    ),
];

/// Each object a call is bound into goes with the function under `CASCADE`,
/// and what it belongs to stays.
#[test]
fn every_dependent_goes_with_its_function() {
    for (dependent, after) in DEPENDENTS {
        parse(&format!("{CALLED} {dependent} DROP FUNCTION f() CASCADE; {REDECLARED} {after}"))
            .expect(dependent);
    }
    parse(&format!(
        "{CALLED} CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();
         DROP FUNCTION trg() CASCADE;
         CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();"
    ))
    .expect("the trigger running trg went with it");
}

/// A default goes alone, its column staying, and a domain check goes alone,
/// its domain staying.
#[test]
fn a_default_or_a_domain_check_goes_alone() {
    let db = parse(&format!(
        "{CALLED} ALTER TABLE t ALTER COLUMN a SET DEFAULT f();
         CREATE DOMAIN d AS INT CHECK (VALUE > f());
         DROP FUNCTION f() CASCADE;"
    ))
    .expect("the cascade takes the default and the check");
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("t is looked up")
        .expect("t stays");
    let column = table
        .column("a", &db, IdentifierCase::AsWritten)
        .expect("a is looked up")
        .expect("a stays");
    assert!(!column.has_default(), "the default went with f");
    let domain = db.domains().next().expect("d stays");
    assert!(domain.constraints.is_empty(), "the check went with f");
}

/// What a cascade takes takes its own dependents in turn, through every
/// kind of object, as PostgreSQL 18 lists them.
#[test]
fn a_cascade_takes_the_dependents_of_what_it_takes() {
    for (setup, after) in [
        // type d, function g(d), the default of t.b, constraint t_a_check,
        // column y of table u
        (
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
             CREATE DOMAIN d AS INT DEFAULT f();
             CREATE FUNCTION g(x d) RETURNS INT LANGUAGE sql IMMUTABLE RETURN x;
             CREATE TABLE t (a INT CHECK (g(a::d) > 0), b INT DEFAULT g(1::d));
             CREATE TABLE u (y d, k INT);
             DROP FUNCTION f() CASCADE;",
            "CREATE DOMAIN d AS INT;
             CREATE FUNCTION g(x d) RETURNS INT LANGUAGE sql RETURN x;
             ALTER TABLE t ADD CONSTRAINT t_a_check CHECK (a > 0);
             ALTER TABLE u ADD COLUMN y INT;",
        ),
        // function caller(), function outer_caller(), view vc, the default
        // of t.a
        (
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
             CREATE FUNCTION caller() RETURNS INT LANGUAGE sql IMMUTABLE RETURN f();
             CREATE FUNCTION outer_caller() RETURNS INT LANGUAGE sql IMMUTABLE
                 RETURN caller() + 1;
             CREATE VIEW vc AS SELECT outer_caller() AS c;
             CREATE TABLE t (a INT DEFAULT outer_caller());
             DROP FUNCTION f() CASCADE;",
            "CREATE FUNCTION caller() RETURNS INT LANGUAGE sql RETURN 1;
             CREATE FUNCTION outer_caller() RETURNS INT LANGUAGE sql RETURN 1;
             CREATE VIEW vc AS SELECT 1 AS c;",
        ),
        // column g of table t, view v, function h(), index si
        (
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
             CREATE TABLE t (a INT, g INT GENERATED ALWAYS AS (f()) STORED);
             CREATE VIEW v AS SELECT g FROM t;
             CREATE FUNCTION h() RETURNS BIGINT LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT count(*) FROM v; END;
             CREATE TABLE s (a INT);
             CREATE INDEX si ON s ((a + h()));
             DROP FUNCTION f() CASCADE;",
            "ALTER TABLE t ADD COLUMN g INT;
             CREATE VIEW v AS SELECT 1 AS g;
             CREATE FUNCTION h() RETURNS INT LANGUAGE sql RETURN 1;
             CREATE INDEX si ON s (a);",
        ),
        // table p with its partition p1, view pv
        (
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
             CREATE TABLE p (a INT) PARTITION BY RANGE ((a + f()));
             CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
             CREATE VIEW pv AS SELECT a FROM p;
             DROP FUNCTION f() CASCADE;",
            "CREATE TABLE p (a INT); CREATE TABLE p1 (a INT); CREATE VIEW pv AS SELECT 1 AS a;",
        ),
        // type fr, column x of table ut
        (
            "CREATE FUNCTION sd(a FLOAT8, b FLOAT8) RETURNS FLOAT8 LANGUAGE sql IMMUTABLE
                 RETURN a - b;
             CREATE TYPE fr AS RANGE (subtype = FLOAT8, subtype_diff = sd);
             CREATE TABLE ut (x fr, k INT);
             DROP FUNCTION sd(FLOAT8, FLOAT8) CASCADE;",
            "CREATE TYPE fr AS ENUM ('a'); ALTER TABLE ut ADD COLUMN x INT;",
        ),
    ] {
        parse(&format!("{setup} {after}")).expect(setup);
    }
}

/// Every other `CASCADE` removing a function takes what holds that function
/// too, and keeps another overload of its name.
#[test]
fn every_cascade_removing_a_function_takes_its_dependents() {
    for (setup, after) in [
        // function k(e), constraint t_check
        (
            "CREATE TYPE e AS ENUM ('a');
             CREATE FUNCTION k(e) RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
             CREATE TABLE t (a INT CHECK (k('a'::e) > 0));
             DROP TYPE e CASCADE;",
            "CREATE FUNCTION k() RETURNS INT LANGUAGE sql RETURN 1;
             ALTER TABLE t ADD CONSTRAINT t_check CHECK (a > 0);",
        ),
        // function g(), constraint t_check
        (
            "CREATE TABLE s (a INT);
             CREATE VIEW v AS SELECT a FROM s;
             CREATE FUNCTION g() RETURNS BIGINT LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT count(*) FROM v; END;
             CREATE FUNCTION g(INT) RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
             CREATE TABLE t (a INT CHECK (g() >= 0), b INT DEFAULT g(2));
             DROP VIEW v CASCADE;",
            "ALTER TABLE t ADD CONSTRAINT t_check CHECK (a > 0);
             CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN 1;",
        ),
        // function g(), policy pol
        (
            "CREATE TABLE s (a INT, b INT);
             CREATE FUNCTION g() RETURNS BIGINT LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT sum(a) FROM s; END;
             CREATE TABLE t (a INT);
             CREATE POLICY pol ON t USING (g() > 0);
             ALTER TABLE s DROP COLUMN a CASCADE;",
            "CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN 1;
             CREATE POLICY pol ON t USING (true);",
        ),
        // function g(), the default of t.a, trigger tr
        (
            "CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
             CREATE TABLE s (a INT);
             CREATE FUNCTION g() RETURNS BIGINT LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT count(*) FROM s; END;
             CREATE TABLE t (a INT DEFAULT g(), b INT);
             CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (g() > 0)
                 EXECUTE FUNCTION trg();
             DROP TABLE s CASCADE;",
            "CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN 1;
             CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();",
        ),
        // function g(), index ti
        (
            "CREATE SEQUENCE q;
             CREATE FUNCTION g() RETURNS BIGINT LANGUAGE sql IMMUTABLE
                 BEGIN ATOMIC SELECT last_value FROM q; END;
             CREATE TABLE t (a INT);
             CREATE INDEX ti ON t ((a + g()));
             DROP SEQUENCE q CASCADE;",
            "CREATE FUNCTION g() RETURNS INT LANGUAGE sql RETURN 1;
             CREATE INDEX ti ON t (a);",
        ),
    ] {
        parse(&format!("{setup} {after}")).expect(setup);
    }

    // The overload no view reads stays, held by the default calling it.
    let result = parse(
        "CREATE TABLE s (a INT);
         CREATE VIEW v AS SELECT a FROM s;
         CREATE FUNCTION g() RETURNS BIGINT LANGUAGE sql IMMUTABLE
             BEGIN ATOMIC SELECT count(*) FROM v; END;
         CREATE FUNCTION g(INT) RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
         CREATE TABLE t (a INT CHECK (g() >= 0), b INT DEFAULT g(2));
         DROP VIEW v CASCADE;
         DROP FUNCTION g(INT);",
    );
    assert!(
        matches!(
            &result,
            Err(Error::FunctionReferenced {
                function_name,
                dependent_kind: ObjectKind::ColumnDefault,
                dependent_name,
            }) if function_name == "g" && dependent_name == "t.b"
        ),
        "got {result:?}"
    );
}
