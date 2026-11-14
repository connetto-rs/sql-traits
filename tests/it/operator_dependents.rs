//! Tests that each stored expression holds the operators its applications
//! select, so that `DROP OPERATOR` refuses while one is held and `CASCADE`
//! takes the holders along, as PostgreSQL 18 does.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Comparison functions over `int4`, `int8` and `text`, and `trg` for the
/// triggers.
const FUNCTIONS: &str = "SET search_path = public, app;
     CREATE SCHEMA app;
     CREATE FUNCTION app.lt(int4, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
     CREATE FUNCTION app.lt8(int8, int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
     CREATE FUNCTION app.lt84(int8, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
     CREATE FUNCTION app.ltt(text, text) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
     CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
     CREATE TABLE t (a INT);";

/// `<<<` over two `int4`.
const INT4: &str = "CREATE OPERATOR app.<<< (LEFTARG = int4, RIGHTARG = int4, FUNCTION = app.lt);";

/// `<<<` over `int8` and `int4`.
const INT8_INT4: &str =
    "CREATE OPERATOR app.<<< (LEFTARG = int8, RIGHTARG = int4, FUNCTION = app.lt84);";

/// `<<<` over two `int8`.
const INT8: &str = "CREATE OPERATOR app.<<< (LEFTARG = int8, RIGHTARG = int8, FUNCTION = app.lt8);";

/// `<<<` over two `text`.
const TEXT: &str = "CREATE OPERATOR app.<<< (LEFTARG = text, RIGHTARG = text, FUNCTION = app.ltt);";

/// Each object applying `<<<` over two `int4`, with the kind and name the
/// refusal to drop the operator gives it, and a statement PostgreSQL 18
/// accepts only once `DROP OPERATOR ... CASCADE` took that object and kept
/// what it belongs to.
const DEPENDENTS: [(&str, ObjectKind, &str, &str); 16] = [
    (
        "CREATE TABLE d (a BOOL DEFAULT (1 <<< 2));",
        ObjectKind::ColumnDefault,
        "d.a",
        "ALTER TABLE d DROP COLUMN a;",
    ),
    (
        "CREATE TABLE d (x INT, a BOOL GENERATED ALWAYS AS (x <<< 1) STORED);",
        ObjectKind::Column,
        "d.a",
        "ALTER TABLE d ADD COLUMN a INT;",
    ),
    (
        "CREATE TABLE d (a INT CONSTRAINT ck CHECK (a <<< 5));",
        ObjectKind::CheckConstraint,
        "d.ck",
        "ALTER TABLE d ADD CONSTRAINT ck CHECK (a > 0);",
    ),
    (
        "ALTER TABLE t ALTER COLUMN a SET DEFAULT CASE WHEN 1 <<< 2 THEN 1 ELSE 0 END;",
        ObjectKind::ColumnDefault,
        "t.a",
        "ALTER TABLE t DROP COLUMN a;",
    ),
    ("CREATE INDEX i ON t ((a <<< 1));", ObjectKind::Index, "i", "CREATE INDEX i ON t (a);"),
    ("CREATE INDEX i ON t (a) WHERE a <<< 1;", ObjectKind::Index, "i", "CREATE INDEX i ON t (a);"),
    (
        "CREATE TABLE x (a INT, CONSTRAINT xc EXCLUDE USING btree ((a <<< 1) WITH =));",
        ObjectKind::Index,
        "xc",
        "ALTER TABLE x ADD CONSTRAINT xc EXCLUDE USING btree (a WITH =);",
    ),
    (
        "CREATE VIEW v AS SELECT 1 <<< 2 AS one;",
        ObjectKind::View,
        "v",
        "CREATE VIEW v AS SELECT 1 AS one;",
    ),
    (
        "CREATE MATERIALIZED VIEW v AS SELECT 1 <<< 2 AS one;",
        ObjectKind::MaterializedView,
        "v",
        "CREATE MATERIALIZED VIEW v AS SELECT 1 AS one;",
    ),
    (
        "CREATE POLICY pol ON t USING (a <<< 1);",
        ObjectKind::Policy,
        "pol",
        "CREATE POLICY pol ON t USING (true);",
    ),
    (
        "CREATE RULE r AS ON INSERT TO t WHERE new.a <<< 1 DO INSTEAD NOTHING;",
        ObjectKind::Rule,
        "r",
        "CREATE RULE r AS ON INSERT TO t DO INSTEAD NOTHING;",
    ),
    (
        "CREATE DOMAIN d AS INT CHECK (VALUE <<< 10);",
        ObjectKind::Domain,
        "d",
        "CREATE TABLE z (c d);",
    ),
    (
        "CREATE DOMAIN d AS BOOL DEFAULT (1 <<< 2);",
        ObjectKind::Domain,
        "d",
        "CREATE DOMAIN d AS INT;",
    ),
    (
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (new.a <<< 1)
         EXECUTE FUNCTION trg();",
        ObjectKind::Trigger,
        "tr",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();",
    ),
    (
        "CREATE TABLE p (a INT) PARTITION BY RANGE ((a <<< 1));",
        ObjectKind::Table,
        "p",
        "CREATE TABLE p (a INT);",
    ),
    (
        "CREATE FUNCTION caller() RETURNS BOOL LANGUAGE sql RETURN 1 <<< 2;",
        ObjectKind::Function,
        "caller",
        "CREATE FUNCTION caller() RETURNS INT LANGUAGE sql RETURN 1;",
    ),
];

/// Asserts that `sql` ends refused to drop `<<<` while `kind` `name` holds
/// it.
#[track_caller]
fn assert_held(sql: &str, kind: ObjectKind, name: &str) {
    let result = parse(sql);
    assert!(
        matches!(
            &result,
            Err(Error::OperatorReferenced { operator_name, dependent_kind, dependent_name })
                if &**operator_name == "<<<" && *dependent_kind == kind && dependent_name == name
        ),
        "{sql}\ngot {result:?}"
    );
}

/// Each object applying the operator keeps a drop without `CASCADE` from
/// removing it, named as the refusal names it.
#[test]
fn every_dependent_refuses_a_restricted_drop() {
    for (dependent, kind, name, _) in DEPENDENTS {
        assert_held(
            &format!("{FUNCTIONS} {INT4} {dependent} DROP OPERATOR app.<<< (int4, int4);"),
            kind,
            name,
        );
    }
}

/// Each object applying the operator goes with it under `CASCADE`, and what
/// it belongs to stays.
#[test]
fn every_dependent_goes_with_its_operator() {
    for (dependent, _, _, after) in DEPENDENTS {
        parse(&format!(
            "{FUNCTIONS} {INT4} {dependent} DROP OPERATOR app.<<< (int4, int4) CASCADE; {after} {INT4}"
        ))
        .expect(dependent);
    }
}

/// An expression holds the overload its operand types select, and no other
/// operator of the same name.
#[test]
fn only_the_selected_overload_is_held() {
    // A trigger runs a function, which holds no operator.
    let view = "CREATE VIEW v AS SELECT 'a'::text <<< 'b' AS x;
                CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();";
    parse(&format!("{FUNCTIONS} {INT4} {TEXT} {view} DROP OPERATOR app.<<< (int4, int4);"))
        .expect("the view applies `<<<(text, text)` alone");
    assert_held(
        &format!("{FUNCTIONS} {INT4} {TEXT} {view} DROP OPERATOR app.<<< (text, text);"),
        ObjectKind::View,
        "v",
    );
}

/// An expression keeps the operator it selected when it was created, though
/// a better match was created since.
#[test]
fn a_later_better_match_leaves_the_dependency() {
    let view = "CREATE VIEW v AS SELECT 1 <<< 2 AS x;";
    parse(&format!("{FUNCTIONS} {INT8} {view} {INT4} DROP OPERATOR app.<<< (int4, int4);"))
        .expect("the view still applies `<<<(int8, int8)`");
    assert_held(
        &format!("{FUNCTIONS} {INT8} {view} {INT4} DROP OPERATOR app.<<< (int8, int8);"),
        ObjectKind::View,
        "v",
    );
}

/// An operator moved to another schema keeps its dependents.
#[test]
fn a_moved_operator_keeps_its_dependents() {
    assert_held(
        &format!(
            "{FUNCTIONS} {INT4} CREATE SCHEMA other;
             CREATE VIEW v AS SELECT 1 <<< 2 AS x;
             ALTER OPERATOR app.<<< (int4, int4) SET SCHEMA other;
             DROP OPERATOR other.<<< (int4, int4);"
        ),
        ObjectKind::View,
        "v",
    );
}

/// Every cascade removing an operator takes its dependents: the cascade of
/// its function, of an operand type and of its schema.
#[test]
fn removing_an_operator_takes_its_dependents() {
    let view = "CREATE VIEW public.v AS SELECT 1 <<< 2 AS x;";
    let again = "CREATE VIEW public.v AS SELECT 1 AS x;";
    parse(&format!("{FUNCTIONS} {INT4} {view} DROP FUNCTION app.lt(int4, int4) CASCADE; {again}"))
        .expect("the view went with the operator of `lt`");
    parse(&format!("{FUNCTIONS} {INT4} {view} DROP SCHEMA app CASCADE; {again}"))
        .expect("the view went with the operator of `app`");
    parse(&format!(
        "{FUNCTIONS} CREATE TYPE mood AS ENUM ('a', 'b');
         CREATE FUNCTION app.ltm(mood, mood) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 < $2';
         CREATE OPERATOR app.<<< (LEFTARG = mood, RIGHTARG = mood, FUNCTION = app.ltm);
         CREATE VIEW v AS SELECT 'a'::mood <<< 'b'::mood AS x;
         DROP TYPE mood CASCADE; {again}"
    ))
    .expect("the view went with the operator over `mood`");
}

/// Replacing a dependent drops what it held, and a rollback restores what a
/// dependent held with the dependent.
#[test]
fn a_dependency_lives_and_dies_with_its_dependent() {
    parse(&format!(
        "{FUNCTIONS} {INT4} CREATE VIEW v AS SELECT 1 <<< 2 AS x;
         CREATE OR REPLACE VIEW v AS SELECT 1 < 2 AS x;
         DROP OPERATOR app.<<< (int4, int4);"
    ))
    .expect("the replaced view no longer applies `<<<`");
    parse(&format!(
        "{FUNCTIONS} {INT4} BEGIN; CREATE VIEW v AS SELECT 1 <<< 2 AS x; ROLLBACK;
         DROP OPERATOR app.<<< (int4, int4);"
    ))
    .expect("the view was rolled back");
    assert_held(
        &format!(
            "{FUNCTIONS} {INT4} BEGIN; CREATE VIEW v AS SELECT 1 <<< 2 AS x; SAVEPOINT s;
             DROP OPERATOR app.<<< (int4, int4) CASCADE; ROLLBACK TO SAVEPOINT s;
             DROP OPERATOR app.<<< (int4, int4);"
        ),
        ObjectKind::View,
        "v",
    );
}

/// A rebuild of a table leaves each dependency with the object holding it,
/// through renames and next to expressions of the same column.
#[test]
fn a_rebuild_keeps_each_dependency_with_its_owner() {
    assert_held(
        &format!(
            "{FUNCTIONS} {INT4} CREATE TABLE d (x BOOL DEFAULT (1 <<< 2) CHECK (x = true));
             ALTER TABLE d ADD COLUMN y INT;
             DROP OPERATOR app.<<< (int4, int4);"
        ),
        ObjectKind::ColumnDefault,
        "d.x",
    );
    assert_held(
        &format!(
            "{FUNCTIONS} {INT4} CREATE TABLE d (a INT CONSTRAINT ck CHECK (a::int4 <<< INT4 '5'));
             ALTER TABLE d RENAME COLUMN a TO b;
             DROP OPERATOR app.<<< (int4, int4);"
        ),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
    let moved = format!(
        "{FUNCTIONS} {INT8} CREATE FUNCTION moved(int4) RETURNS INT4 LANGUAGE sql IMMUTABLE
             AS 'SELECT $1';
         CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (moved(a) <<< 1));
         {INT4} CREATE SCHEMA other; ALTER FUNCTION moved(int4) SET SCHEMA other;"
    );
    let retyped = format!("{moved} ALTER TABLE d ALTER COLUMN a TYPE int2;");
    for moved in [&moved, &retyped] {
        parse(&format!("{moved} DROP OPERATOR app.<<< (int4, int4);"))
            .expect("the check still applies `<<<(int8, int8)`");
        assert_held(
            &format!("{moved} DROP OPERATOR app.<<< (int8, int8);"),
            ObjectKind::CheckConstraint,
            "d.ck",
        );
    }
}

/// Dropping one constraint leaves each other constraint with what it
/// selected, whether it reads another column or reads the same one written
/// alike after a better match was created.
#[test]
fn a_dropped_constraint_leaves_the_others_their_selections() {
    let columns = format!(
        "{FUNCTIONS} {INT4} {INT8_INT4}
         CREATE TABLE d (a INT4, b INT8, CONSTRAINT c1 CHECK (a <<< 1), CONSTRAINT c2 CHECK (b <<< 1));
         ALTER TABLE d DROP CONSTRAINT c1;"
    );
    parse(&format!("{columns} DROP OPERATOR app.<<< (int4, int4);"))
        .expect("only the dropped check applied `<<<(int4, int4)`");
    assert_held(
        &format!("{columns} DROP OPERATOR app.<<< (int8, int4);"),
        ObjectKind::CheckConstraint,
        "d.c2",
    );
    let alike = format!(
        "{FUNCTIONS} {INT8} CREATE TABLE d (a INT4, CONSTRAINT c1 CHECK (a <<< 1));
         {INT4} ALTER TABLE d ADD CONSTRAINT c2 CHECK (a <<< 1);
         ALTER TABLE d DROP CONSTRAINT c1;"
    );
    parse(&format!("{alike} DROP OPERATOR app.<<< (int8, int8);"))
        .expect("only the dropped check applied `<<<(int8, int8)`");
    assert_held(
        &format!("{alike} DROP OPERATOR app.<<< (int4, int4);"),
        ObjectKind::CheckConstraint,
        "d.c2",
    );
    let unnamed = format!(
        "{FUNCTIONS} {INT8} CREATE TABLE d (a INT4, CHECK (a <<< 1));
         {INT4} ALTER TABLE d ADD CHECK (a <<< 1);
         ALTER TABLE d DROP CONSTRAINT d_a_check;"
    );
    parse(&format!("{unnamed} DROP OPERATOR app.<<< (int8, int8);"))
        .expect("only `d_a_check` applied `<<<(int8, int8)`");
    assert_held(
        &format!("{unnamed} DROP OPERATOR app.<<< (int4, int4);"),
        ObjectKind::CheckConstraint,
        "d.d_a_check1",
    );
}

/// A default the statement writes again selects anew, though it is written
/// as before.
#[test]
fn a_rewritten_default_selects_anew() {
    let written = format!(
        "{FUNCTIONS} {INT8} CREATE TABLE d (x BOOL DEFAULT (1 <<< 2) CHECK (x = true));
         {INT4} ALTER TABLE d ALTER COLUMN x SET DEFAULT (1 <<< 2);"
    );
    parse(&format!("{written} DROP OPERATOR app.<<< (int8, int8);"))
        .expect("the rewritten default no longer applies `<<<(int8, int8)`");
    assert_held(
        &format!("{written} DROP OPERATOR app.<<< (int4, int4);"),
        ObjectKind::ColumnDefault,
        "d.x",
    );
}

/// A column type change selects anew where the operand matched exactly.
#[test]
fn a_type_change_selects_anew_for_an_exact_operand() {
    let check = "CREATE TABLE d (a INT CONSTRAINT ck CHECK (d.a <<< 5));
                 ALTER TABLE d ALTER COLUMN a TYPE int8, ADD COLUMN y INT;";
    parse(&format!("{FUNCTIONS} {INT4} {INT8_INT4} {check} DROP OPERATOR app.<<< (int4, int4);"))
        .expect("the check now applies `<<<(int8, int4)`");
    assert_held(
        &format!("{FUNCTIONS} {INT4} {INT8_INT4} {check} DROP OPERATOR app.<<< (int8, int4);"),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
}

/// A column type change keeps the operator an operand was converted to, as
/// PostgreSQL's deparsed cast pins it.
#[test]
fn a_type_change_keeps_a_converted_operand() {
    let check = format!(
        "CREATE TABLE d (a INT CONSTRAINT ck CHECK (a <<< 1)); {INT4}
         ALTER TABLE d ALTER COLUMN a TYPE int2;"
    );
    parse(&format!("{FUNCTIONS} {INT8} {check} DROP OPERATOR app.<<< (int4, int4);"))
        .expect("the check still applies `<<<(int8, int8)`");
    assert_held(
        &format!("{FUNCTIONS} {INT8} {check} DROP OPERATOR app.<<< (int8, int8);"),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
    let called = format!(
        "CREATE FUNCTION wrap(int8) RETURNS INT8 LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (wrap(a) <<< 1));
         CREATE FUNCTION wrap(int4) RETURNS TEXT LANGUAGE sql IMMUTABLE AS 'SELECT $1::text';
         {TEXT} ALTER TABLE d ALTER COLUMN a TYPE int2, ADD COLUMN y INT;"
    );
    parse(&format!("{FUNCTIONS} {INT8} {called} DROP OPERATOR app.<<< (text, text);"))
        .expect("the call still runs `wrap(int8)`, its argument cast pinned");
    assert_held(
        &format!("{FUNCTIONS} {INT8} {called} DROP OPERATOR app.<<< (int8, int8);"),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
}

/// A column type change keeps the function an argument was converted to,
/// as PostgreSQL's deparsed cast pins it, in a check applying no operator.
#[test]
fn a_type_change_keeps_a_converted_argument() {
    let check = "CREATE FUNCTION f8(int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (f8(a)));
         CREATE FUNCTION f8(text) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT true';
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text;";
    parse(&format!("{check} DROP FUNCTION f8(text);")).expect("the check still calls `f8(int8)`");
    let result = parse(&format!("{check} DROP FUNCTION f8(int8);"));
    assert!(
        matches!(
            &result,
            Err(Error::FunctionReferenced { function_name, dependent_kind, dependent_name })
                if function_name == "f8"
                    && *dependent_kind == ObjectKind::CheckConstraint
                    && dependent_name == "d.ck"
        ),
        "got {result:?}"
    );
}

/// Retyping both operands in one statement keeps the casts the first
/// selection's deparse wrote around them.
#[test]
fn a_statement_retyping_both_operands_keeps_their_casts() {
    let check = format!(
        "{FUNCTIONS} {INT8} CREATE TABLE d (a INT4, b INT4, CONSTRAINT ck CHECK (a <<< b)); {TEXT}
         ALTER TABLE d ALTER COLUMN a TYPE text USING a::text,
             ALTER COLUMN b TYPE text USING b::text;"
    );
    parse(&format!("{check} DROP OPERATOR app.<<< (text, text);"))
        .expect("the check still applies `<<<(int8, int8)`");
    assert_held(
        &format!("{check} DROP OPERATOR app.<<< (int8, int8);"),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
}

/// A column type change finds the operator in its own schema, off the path
/// the statement runs under, as PostgreSQL's deparsed qualification does.
#[test]
fn a_type_change_finds_the_operator_off_the_path() {
    assert_held(
        &format!(
            "{FUNCTIONS} {INT4} {INT8_INT4}
             CREATE TABLE d (a INT CONSTRAINT ck CHECK (a <<< 5));
             SET search_path = public;
             ALTER TABLE d ALTER COLUMN a TYPE int8;
             DROP OPERATOR app.<<< (int8, int4);"
        ),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
    assert_held(
        &format!(
            "{FUNCTIONS} {INT8}
             CREATE FUNCTION wrap(int4) RETURNS INT4 LANGUAGE sql IMMUTABLE AS 'SELECT $1';
             CREATE TABLE d (a INT4, CONSTRAINT ck CHECK (wrap(a) <<< 1));
             SET search_path = app;
             ALTER TABLE public.d ALTER COLUMN a TYPE int2;
             DROP OPERATOR app.<<< (int8, int8);"
        ),
        ObjectKind::CheckConstraint,
        "d.ck",
    );
}

/// A column type change keeps the element type of the array PostgreSQL built
/// from an `IN` list, which its deparse writes as `ARRAY[...]`.
#[test]
fn a_type_change_keeps_the_built_array() {
    let retyped = "SET search_path = app, pg_catalog, public;
         CREATE SCHEMA app;
         CREATE FUNCTION app.eq84(int8, int4) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 = $2';
         CREATE FUNCTION app.eq88(int8, int8) RETURNS BOOL LANGUAGE sql IMMUTABLE AS 'SELECT $1 = $2';
         CREATE OPERATOR app.= (LEFTARG = int8, RIGHTARG = int4, FUNCTION = app.eq84);
         CREATE OPERATOR app.= (LEFTARG = int8, RIGHTARG = int8, FUNCTION = app.eq88);
         CREATE TABLE public.d (a INT4, CONSTRAINT ck CHECK (a IN (1, 2)));
         ALTER TABLE public.d ALTER COLUMN a TYPE int8;";
    parse(&format!("{retyped} DROP OPERATOR app.= (int8, int8);"))
        .expect("the check compares with the `int4` elements");
    let result = parse(&format!("{retyped} DROP OPERATOR app.= (int8, int4);"));
    assert!(
        matches!(
            &result,
            Err(Error::OperatorReferenced { operator_name, dependent_kind, dependent_name })
                if &**operator_name == "=" && *dependent_kind == ObjectKind::CheckConstraint
                    && dependent_name == "public.d.ck"
        ),
        "got {result:?}"
    );
}
