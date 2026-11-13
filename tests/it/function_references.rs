//! Tests that a function counts as in use only where a call can reach it.
//!
//! PostgreSQL resolves a call by schema and by arguments, so a function of the
//! same name in another schema or taking other arguments is free to drop.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn assert_in_use(result: &Result<ParserDB, Error>) {
    assert!(
        matches!(result, Err(Error::FunctionReferenced { function_name, .. }) if function_name == "f"),
        "got {result:?}"
    );
}

#[test]
fn a_function_in_a_schema_no_call_reaches_can_be_dropped() {
    let calls_public = "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION app.f() RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT, CHECK (f()));
         CREATE POLICY p ON t USING (f()) WITH CHECK (f());";
    let dropped = parse(&format!("{calls_public} DROP FUNCTION app.f();"))
        .expect("the unqualified calls reach public.f");
    assert_eq!(dropped.functions().filter(|function| function.name() == "f").count(), 1);
    assert_in_use(&parse(&format!("{calls_public} DROP FUNCTION f();")));

    let calls_app = "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION app.f() RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (app.f());";
    parse(&format!("{calls_app} DROP FUNCTION f();")).expect("the call names app.f");
    assert_in_use(&parse(&format!("{calls_app} DROP FUNCTION app.f();")));
}

/// An unqualified call looks through the search path in force when it was
/// written.
#[test]
fn an_unqualified_call_reaches_the_schemas_on_the_search_path() {
    let result = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS BOOL AS 'SELECT true';
         SET search_path TO app, public;
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f());
         DROP FUNCTION app.f();",
    );
    assert_in_use(&result);
}

/// The number of arguments narrows a call to the overloads that accept it,
/// and among those PostgreSQL binds the one the argument types select, so
/// another overload taking as many arguments is free to drop.
#[test]
fn a_call_reaches_the_overloads_that_accept_its_arguments() {
    let overloads = "CREATE FUNCTION f(x INT) RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION f(x TEXT) RETURNS BOOL AS 'SELECT true';
         CREATE FUNCTION f(x INT, y INT) RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f(id));";
    parse(&format!("{overloads} DROP FUNCTION f(INT, INT);"))
        .expect("no call passes two arguments");
    parse(&format!("{overloads} DROP FUNCTION f(TEXT);")).expect("the call selects f(INT)");
    assert_in_use(&parse(&format!("{overloads} DROP FUNCTION f(INT);")));

    let defaulted = parse(
        "CREATE FUNCTION f(x INT, y INT DEFAULT 0) RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f(id));
         DROP FUNCTION f(INT, INT);",
    );
    assert_in_use(&defaulted);

    let variadic = parse(
        "CREATE FUNCTION f(VARIADIC xs INT[]) RETURNS BOOL AS 'SELECT true';
         CREATE TABLE t (id INT);
         CREATE POLICY p ON t USING (f(id, id, id));
         DROP FUNCTION f(INT[]);",
    );
    assert_in_use(&variadic);
}

/// A trigger runs a function taking no arguments, found the way a call is.
#[test]
fn a_trigger_reaches_only_the_function_it_runs() {
    let triggered = "CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION app.f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1';
         CREATE TABLE t (id INT);
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();";
    parse(&format!("{triggered} DROP FUNCTION app.f();")).expect("the trigger runs public.f");
    parse(&format!("{triggered} DROP FUNCTION f(INT);")).expect("a trigger function takes nothing");
    assert_in_use(&parse(&format!("{triggered} DROP FUNCTION f();")));
}

/// The function a trigger reports is the one it was bound to, however many
/// functions of that name other schemas hold.
#[test]
fn a_trigger_reports_the_function_it_runs() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE t (id INT);
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();",
    )
    .expect("schema parses");
    let trigger = db.triggers().next().expect("the trigger exists");
    let function = trigger.function(&db).expect("the trigger runs a function");

    assert_eq!(function.target_name().schema(), None);
    assert_eq!(function.argument_type_names(&db).count(), 0);

    let spelled_public = parse(
        "CREATE FUNCTION public.f() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE t (id INT);
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();",
    )
    .expect("schema parses");
    let trigger = spelled_public.triggers().next().expect("the trigger exists");
    assert!(trigger.function(&spelled_public).is_some(), "public.f is the f a bare name reaches");
}

/// A trigger name is unique per table, so one name on two tables names two
/// triggers, each with the calls of its own `WHEN` condition.
#[test]
fn a_trigger_name_on_two_tables_names_each_tables_own_trigger() {
    let on_a =
        "CREATE TRIGGER t BEFORE INSERT ON a FOR EACH ROW WHEN (fa() > 0) EXECUTE FUNCTION trg();";
    let on_b =
        "CREATE TRIGGER t BEFORE INSERT ON b FOR EACH ROW WHEN (fb() > 0) EXECUTE FUNCTION trg();";
    for triggers in [format!("{on_a} {on_b}"), format!("{on_b} {on_a}")] {
        let db = parse(&format!(
            "CREATE FUNCTION fa() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
             CREATE FUNCTION fb() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';
             CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
             CREATE TABLE a (id INT);
             CREATE TABLE b (id INT);
             {triggers}"
        ))
        .expect("schema parses");
        for (table, called) in [("a", "fa"), ("b", "fb")] {
            let table = db
                .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
                .expect("the lookup runs")
                .expect("the table exists");
            let trigger = db.trigger("t", table).expect("the table carries t");
            assert_eq!(trigger.table(&db).expect("the trigger's table"), table);
            let calls: Vec<&str> = db
                .trigger_metadata(trigger)
                .expect("the trigger is held")
                .functions()
                .map(FunctionLike::name)
                .collect();
            assert_eq!(calls, [called], "{triggers}");
        }
    }
}

/// The functions `f` and `trg` and the table `t` every dependent below is
/// written against.
const CALLED: &str = "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
     CREATE TABLE t (a INT);";

/// A statement writing a call to `f` into each object PostgreSQL binds the
/// call of at creation, with the object a refused drop names, as PostgreSQL
/// 18 lists it.
const DEPENDENTS: [(&str, ObjectKind, &str); 20] = [
    ("CREATE TABLE d (a INT DEFAULT f());", ObjectKind::ColumnDefault, "d.a"),
    ("CREATE TABLE d (a INT GENERATED ALWAYS AS (f()) STORED);", ObjectKind::Column, "d.a"),
    ("CREATE TABLE d (a INT CONSTRAINT ck CHECK (a > f()));", ObjectKind::CheckConstraint, "d.ck"),
    ("ALTER TABLE t ALTER COLUMN a SET DEFAULT f();", ObjectKind::ColumnDefault, "t.a"),
    ("ALTER TABLE t ADD COLUMN b INT DEFAULT f();", ObjectKind::ColumnDefault, "t.b"),
    ("CREATE TABLE d (a INT CHECK (a > f()));", ObjectKind::CheckConstraint, "d.d_a_check"),
    ("ALTER TABLE t ADD CONSTRAINT ck CHECK (a > f());", ObjectKind::CheckConstraint, "t.ck"),
    ("CREATE INDEX i ON t ((f() + a));", ObjectKind::Index, "i"),
    ("CREATE INDEX i ON t (a) WHERE f() > 0;", ObjectKind::Index, "i"),
    (
        "CREATE TABLE x (a INT, CONSTRAINT xc EXCLUDE USING btree ((a + f()) WITH =));",
        ObjectKind::Index,
        "xc",
    ),
    ("CREATE VIEW v AS SELECT f() AS one;", ObjectKind::View, "v"),
    ("CREATE MATERIALIZED VIEW v AS SELECT f() AS one;", ObjectKind::MaterializedView, "v"),
    ("CREATE POLICY pol ON t USING (f() > 0);", ObjectKind::Policy, "pol"),
    ("CREATE RULE r AS ON INSERT TO t WHERE f() > 0 DO INSTEAD NOTHING;", ObjectKind::Rule, "r"),
    ("CREATE RULE r AS ON INSERT TO t DO INSTEAD SELECT f();", ObjectKind::Rule, "r"),
    ("CREATE DOMAIN d AS INT CHECK (VALUE > f());", ObjectKind::Domain, "d"),
    ("CREATE DOMAIN d AS INT DEFAULT f();", ObjectKind::Domain, "d"),
    (
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (f() > 0) EXECUTE FUNCTION trg();",
        ObjectKind::Trigger,
        "tr",
    ),
    ("CREATE TABLE p (a INT) PARTITION BY RANGE ((a + f()));", ObjectKind::Table, "p"),
    (
        "CREATE FUNCTION caller() RETURNS INT LANGUAGE sql RETURN f();",
        ObjectKind::Function,
        "caller",
    ),
];

/// Each object a call is bound into holds the function, so dropping it is
/// refused while the object remains, the refusal naming the object.
#[test]
fn every_bound_call_holds_its_function() {
    for (dependent, kind, name) in DEPENDENTS {
        parse(&format!("{CALLED} {dependent}")).expect(dependent);
        let result = parse(&format!("{CALLED} {dependent} DROP FUNCTION f();"));
        assert!(
            matches!(
                &result,
                Err(Error::FunctionReferenced { function_name, dependent_kind, dependent_name })
                    if function_name == "f" && *dependent_kind == kind && dependent_name == name
            ),
            "{dependent} got {result:?}"
        );
    }
}

/// A trigger holds the function it runs, and several functions dropped in
/// one statement do not hold each other, the refusal naming the function
/// something else still holds, as PostgreSQL 18 details it.
#[test]
fn a_refusal_names_the_function_held_and_what_holds_it() {
    let triggered = format!(
        "{CALLED} CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg();"
    );
    let result = parse(&format!("{triggered} DROP FUNCTION trg();"));
    assert!(
        matches!(
            &result,
            Err(Error::FunctionReferenced {
                function_name,
                dependent_kind: ObjectKind::Trigger,
                dependent_name,
            }) if function_name == "trg" && dependent_name == "tr"
        ),
        "got {result:?}"
    );

    let together = "CREATE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE RETURN 1;
         CREATE FUNCTION g() RETURNS INT LANGUAGE sql IMMUTABLE RETURN f();
         CREATE TABLE t (a INT CHECK (a > g()));";
    parse(&format!("{together} ALTER TABLE t DROP CONSTRAINT t_a_check; DROP FUNCTION f(), g();"))
        .expect("g calling f goes with it");
    let result = parse(&format!("{together} DROP FUNCTION f(), g();"));
    assert!(
        matches!(
            &result,
            Err(Error::FunctionReferenced {
                function_name,
                dependent_kind: ObjectKind::CheckConstraint,
                dependent_name,
            }) if function_name == "g" && dependent_name == "t.t_a_check"
        ),
        "got {result:?}"
    );

    let defaulted = format!("{CALLED} CREATE TABLE d (a INT DEFAULT f()); DROP FUNCTION f();");
    assert_eq!(
        parse(&defaulted).expect_err("the default calls f").to_string(),
        "Function `f` cannot be dropped because Column default `d.a` uses it."
    );
}

/// An unnamed exclusion constraint is named as PostgreSQL 18 names it.
#[test]
fn an_unnamed_exclusion_is_named_as_postgresql_names_it() {
    for (elements, name) in [
        ("(a + f(a)) WITH =", "d_expr_excl"),
        ("(f(a)) WITH =", "d_f_excl"),
        ("(f(a)::TEXT) WITH =", "d_f_excl"),
        ("a WITH =, (f(b)) WITH =", "d_a_f_excl"),
    ] {
        let result = parse(&format!(
            "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
             CREATE TABLE d (a INT, b INT, EXCLUDE USING btree ({elements}));
             DROP FUNCTION f(INT);"
        ));
        assert!(
            matches!(
                &result,
                Err(Error::FunctionReferenced { dependent_kind: ObjectKind::Index, dependent_name, .. })
                    if dependent_name == name
            ),
            "{elements} got {result:?}"
        );
    }
}

/// A dialect giving constraints written without a name none leaves the
/// exclusion known by how it is written.
#[test]
fn an_exclusion_no_dialect_rule_names_is_named_as_written() {
    let result = ParserDB::parse::<sqlparser::dialect::GenericDialect>(
        "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE TABLE d (a INT, EXCLUDE USING btree ((f(a)) WITH =));
         DROP FUNCTION f(INT);",
    );
    assert!(
        matches!(
            &result,
            Err(Error::FunctionReferenced { dependent_kind: ObjectKind::Index, dependent_name, .. })
                if dependent_name == "EXCLUDE USING btree ((f(a)) WITH =)"
        ),
        "got {result:?}"
    );
}

/// The overloads `f(INT)` and `f(TEXT)`, the trigger function `trg` and the
/// table `t` every dependent below is written against, beside an object of
/// every kind already holding the `g(INT)` its own call selected.
const OVERLOADED: &str =
    "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE FUNCTION f(x TEXT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';
     CREATE FUNCTION g(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
     CREATE FUNCTION g(x TEXT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';
     CREATE FUNCTION trg() RETURNS TRIGGER LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
     CREATE TABLE t (a INT);
     CREATE TABLE d0 (a INT DEFAULT g(1), b INT GENERATED ALWAYS AS (g(a)) STORED, CHECK (g(a) > 0));
     CREATE INDEX i0 ON d0 ((g(a)));
     CREATE VIEW v0 AS SELECT g(1) AS one;
     CREATE MATERIALIZED VIEW m0 AS SELECT g(1) AS one;
     CREATE POLICY p0 ON d0 USING (g(a) > 0);
     CREATE RULE r0 AS ON UPDATE TO d0 DO ALSO SELECT g(new.a);
     CREATE TRIGGER tr0 BEFORE INSERT ON d0 FOR EACH ROW WHEN (g(NEW.a) > 0) EXECUTE FUNCTION trg();
     CREATE DOMAIN dd0 AS INT CHECK (VALUE > g(VALUE));
     CREATE TABLE e0 (a INT, EXCLUDE USING btree ((a + g(a)) WITH =));
     CREATE TABLE k0 (a INT) PARTITION BY RANGE ((a + g(a)));
     CREATE FUNCTION caller0(x INT) RETURNS INT LANGUAGE sql RETURN g(x);";

/// A statement writing a call `f(INT)` answers into each object PostgreSQL
/// binds the call of at creation.
const SELECTING: [&str; 23] = [
    "CREATE TABLE d (a INT DEFAULT f(1));",
    "CREATE TABLE d (a INT, b INT GENERATED ALWAYS AS (f(a)) STORED);",
    "CREATE TABLE d (a INT CHECK (f(a) > 0));",
    "CREATE TABLE d (a INT, CHECK (f(a) > 0));",
    "ALTER TABLE t ADD CONSTRAINT c CHECK (f(a) > 0);",
    "ALTER TABLE t ALTER COLUMN a SET DEFAULT f(1);",
    "ALTER TABLE t ADD COLUMN b INT DEFAULT f(1);",
    "CREATE INDEX i ON t ((f(a) + a));",
    "CREATE INDEX i ON t (a) WHERE f(a) > 0;",
    "CREATE VIEW v AS SELECT f(a) AS one FROM t;",
    "CREATE MATERIALIZED VIEW v AS SELECT f(1) AS one;",
    "CREATE POLICY p ON t USING (f(a) > 0);",
    "CREATE POLICY p ON t WITH CHECK (f(a) > 0);",
    "CREATE RULE r AS ON INSERT TO t WHERE f(new.a) > 0 DO INSTEAD NOTHING;",
    "CREATE RULE r AS ON INSERT TO t DO INSTEAD SELECT f(new.a);",
    "CREATE DOMAIN d AS INT CHECK (VALUE > f(VALUE));",
    "CREATE DOMAIN d AS INT DEFAULT f(1);",
    "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (f(NEW.a) > 0) EXECUTE FUNCTION trg();",
    "CREATE FUNCTION caller(x INT) RETURNS INT LANGUAGE sql RETURN f(x);",
    "CREATE FUNCTION caller(x INT) RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT f(x); END;",
    "CREATE FUNCTION caller() RETURNS INT LANGUAGE sql BEGIN ATOMIC
         WITH u AS (UPDATE t SET a = a RETURNING f(a) AS b) SELECT b FROM u;
     END;",
    "CREATE TABLE d (a INT) PARTITION BY RANGE ((a + f(a)));",
    "CREATE TABLE d (a INT, EXCLUDE USING btree ((a + f(a)) WITH =));",
];

/// Each object holds the overload its call selects and leaves the other
/// overload of the same arity free to drop, as PostgreSQL 18 records it.
#[test]
fn every_bound_call_holds_only_the_overload_it_selects() {
    for dependent in SELECTING {
        parse(&format!("{OVERLOADED} {dependent} DROP FUNCTION f(TEXT);")).expect(dependent);
        let result = parse(&format!("{OVERLOADED} {dependent} DROP FUNCTION f(INT);"));
        assert_in_use_by(&result, dependent);
    }
}

/// A call whose argument types the model cannot settle on one overload holds
/// every overload of its arity, so dropping any of them is refused. A scalar
/// subquery argument is not typed yet, and a rule on a view sees rows no
/// typing scope describes, where PostgreSQL 18 selects `f(INT)` for both.
#[test]
fn a_call_its_argument_types_do_not_settle_holds_every_overload() {
    for unsettled in [
        "CREATE VIEW v AS SELECT f((SELECT 1)) AS one;",
        "CREATE VIEW tv AS SELECT a FROM t;
         CREATE RULE rv AS ON INSERT TO tv DO INSTEAD SELECT f(new.a);",
    ] {
        for dropped in ["f(TEXT)", "f(INT)"] {
            let result = parse(&format!("{OVERLOADED} {unsettled} DROP FUNCTION {dropped};"));
            assert_in_use_by(&result, unsettled);
        }
    }
}

/// A column type change has PostgreSQL 18 parse the table's checks and
/// indexes again, their calls selecting the overload the new type selects
/// among the functions declared by then.
#[test]
fn a_retyped_column_has_its_calls_select_again() {
    let retyped = format!(
        "{OVERLOADED} CREATE TABLE d (a INT CHECK (f(a) > 0));
         CREATE INDEX i ON d ((f(a)));
         ALTER TABLE d ALTER COLUMN a TYPE TEXT;"
    );
    parse(&format!("{retyped} DROP FUNCTION f(INT);")).expect("both calls select f(TEXT)");
    assert_in_use_by(&parse(&format!("{retyped} DROP FUNCTION f(TEXT);")), "f(TEXT)");

    let later = "CREATE FUNCTION h(x INT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 1';
         CREATE TABLE d (a INT CHECK (h(a) > 0));
         CREATE FUNCTION h(x TEXT) RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';
         ALTER TABLE d ALTER COLUMN a TYPE TEXT;";
    parse(&format!("{later} DROP FUNCTION h(INT);")).expect("the check selects h(TEXT)");
    let result = parse(&format!("{later} DROP FUNCTION h(TEXT);"));
    assert!(
        matches!(&result, Err(Error::FunctionReferenced { function_name, .. }) if function_name == "h"),
        "got {result:?}"
    );
}

fn assert_in_use_by(result: &Result<ParserDB, Error>, dependent: &str) {
    assert!(
        matches!(result, Err(Error::FunctionReferenced { function_name, .. }) if function_name == "f"),
        "{dependent} got {result:?}"
    );
}

/// Replacing or altering the function keeps it the one each object calls.
#[test]
fn a_replaced_function_stays_held() {
    for (dependent, _, _) in DEPENDENTS {
        for change in [
            "CREATE OR REPLACE FUNCTION f() RETURNS INT LANGUAGE sql IMMUTABLE AS 'SELECT 2';",
            "ALTER FUNCTION f() STABLE;",
        ] {
            let result = parse(&format!("{CALLED} {dependent} {change} DROP FUNCTION f();"));
            assert!(
                matches!(&result, Err(Error::FunctionReferenced { .. })),
                "{dependent} {change} got {result:?}"
            );
        }
    }
}

/// A partition key and an exclusion constraint keep the function their call
/// reached under the path of the statement creating the table, through a
/// rebuild of the table and a rename of the function, as PostgreSQL 18
/// records it.
#[test]
fn a_table_holds_the_function_its_key_or_exclusion_reached() {
    for (holder, kind, name) in [
        (
            "CREATE TABLE public.p (a INT) PARTITION BY RANGE ((f(a)));",
            ObjectKind::Table,
            "public.p",
        ),
        (
            "CREATE TABLE public.p (a INT, CONSTRAINT pc EXCLUDE USING btree ((f(a)) WITH =));",
            ObjectKind::Index,
            "pc",
        ),
    ] {
        let keyed = format!(
            "CREATE SCHEMA app;
             CREATE FUNCTION app.f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE RETURN x;
             SET search_path TO app, public;
             {holder}
             RESET search_path;
             CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql IMMUTABLE RETURN x;
             ALTER TABLE p ADD COLUMN b INT;"
        );
        parse(&format!("{keyed} DROP FUNCTION public.f(INT);")).expect(holder);
        for dropped in [
            "DROP FUNCTION app.f(INT);",
            "ALTER FUNCTION app.f(INT) RENAME TO g; DROP FUNCTION app.g(INT);",
        ] {
            let result = parse(&format!("{keyed} {dropped}"));
            assert!(
                matches!(
                    &result,
                    Err(Error::FunctionReferenced { dependent_kind, dependent_name, .. })
                        if *dependent_kind == kind && dependent_name == name
                ),
                "{holder} {dropped} got {result:?}"
            );
        }
    }
}

/// A call binds under the search path of the statement writing it, and keeps
/// that function when the path changes and another function of the name
/// appears.
#[test]
fn a_bound_call_keeps_the_function_its_path_reached() {
    let bound = "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         SET search_path TO app, public;
         CREATE TABLE d (a INT DEFAULT f());
         CREATE VIEW v AS SELECT f() AS one;
         CREATE DOMAIN dd AS INT DEFAULT f();
         RESET search_path;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 2';";
    parse(&format!("{bound} DROP FUNCTION public.f();")).expect("every call reached app.f");
    assert_in_use(&parse(&format!("{bound} DROP FUNCTION app.f();")));
}

/// Removing the call, or the object holding it, frees the function.
#[test]
fn removing_the_call_frees_the_function() {
    for (dependent, release) in [
        ("CREATE TABLE d (a INT DEFAULT f());", "ALTER TABLE d ALTER COLUMN a DROP DEFAULT;"),
        ("CREATE TABLE d (a INT DEFAULT f());", "DROP TABLE d;"),
        ("CREATE INDEX i ON t ((f() + a));", "DROP INDEX i;"),
        ("CREATE VIEW v AS SELECT f() AS one;", "CREATE OR REPLACE VIEW v AS SELECT 1 AS one;"),
        ("CREATE MATERIALIZED VIEW v AS SELECT f() AS one;", "DROP MATERIALIZED VIEW v;"),
        ("CREATE DOMAIN d AS INT DEFAULT f();", "DROP DOMAIN d;"),
        (
            "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (f() > 0) EXECUTE FUNCTION trg();",
            "DROP TRIGGER tr ON t;",
        ),
    ] {
        let result = parse(&format!("{CALLED} {dependent} {release} DROP FUNCTION f();"));
        assert!(result.is_ok(), "{dependent} {release} got {result:?}");
    }
}

/// A table rebuilt under another search path keeps the functions its checks,
/// defaults and indexes were bound to, and a call it writes anew binds under
/// the path in force.
#[test]
fn a_rebuilt_table_keeps_its_bound_calls() {
    let bound = "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 1';
         SET search_path TO app, public;
         CREATE TABLE d (a INT DEFAULT f(), b INT CHECK (b > f()), c INT, CHECK (c > f()));
         CREATE INDEX i ON d ((f() + a));
         RESET search_path;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 2';";
    for rebuild in [
        "ALTER TABLE app.d ADD COLUMN e INT;",
        "ALTER TABLE app.d RENAME COLUMN a TO z;",
        "ALTER TABLE app.d ALTER COLUMN c SET DEFAULT f();",
    ] {
        let rebuilt = format!("{bound} {rebuild}");
        assert_in_use(&parse(&format!("{rebuilt} DROP FUNCTION app.f();")));
        let public = parse(&format!("{rebuilt} DROP FUNCTION public.f();"));
        assert_eq!(public.is_ok(), !rebuild.contains("SET DEFAULT"), "{rebuild} got {public:?}");
    }
}
