//! Tests that a trigger on a plain view is recorded on the view, under the
//! timings each engine allows there, and goes with the view, follows its
//! renames and survives its replacement as the engines do.
//!
//! Every PostgreSQL statement here was measured against PostgreSQL 18.6, and
//! every SQLite statement against SQLite 3.51.1.
#![expect(clippy::expect_used, clippy::panic, reason = "a test fails loudly on a broken premise")]

use sql_traits::{
    errors::{Error, ObjectKind, TriggerFiringRefusal},
    prelude::*,
};
use sqlparser::{
    ast::{TriggerEvent, TriggerPeriod},
    dialect::{PostgreSqlDialect, SQLiteDialect},
};

const SQLITE_SCHEMA: &str = "CREATE TABLE t (a INTEGER); CREATE VIEW v AS SELECT a FROM t;";

const POSTGRES_SCHEMA: &str = "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t;
     CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;";

fn sqlite(statements: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<SQLiteDialect>(&format!("{SQLITE_SCHEMA} {statements}"))
}

fn postgres(statements: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{POSTGRES_SCHEMA} {statements}"))
}

fn accepted(parsed: Result<ParserDB, Error>, statements: &str) -> ParserDB {
    parsed.unwrap_or_else(|error| panic!("{statements} was refused: {error:?}"))
}

fn view<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::View {
    database
        .view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("view {name} exists"))
}

/// The names of the triggers `database` holds on the view `name`.
fn view_trigger_names(database: &ParserDB, name: &str) -> Vec<String> {
    view(database, name).triggers(database).map(|trigger| trigger.name().to_string()).collect()
}

fn assert_refused(
    parsed: Result<ParserDB, Error>,
    statements: &str,
    expected: TriggerFiringRefusal,
) {
    match parsed {
        Err(Error::TriggerFiringRefused { reason, .. }) if reason == expected => {}
        other => panic!("{statements} should be refused with {expected:?}, got {other:?}"),
    }
}

#[test]
fn sqlite_instead_of_triggers_land_on_the_view() {
    let statements = "CREATE TRIGGER ins INSTEAD OF INSERT ON v BEGIN INSERT INTO t (a) VALUES (NEW.a); END;
         CREATE TRIGGER upd INSTEAD OF UPDATE OF a ON v WHEN OLD.a > 0 BEGIN UPDATE t SET a = NEW.a WHERE a = OLD.a; END;
         CREATE TRIGGER del INSTEAD OF DELETE ON v FOR EACH ROW BEGIN DELETE FROM t WHERE a = OLD.a; END;";
    let database = accepted(sqlite(statements), statements);

    let mut names = view_trigger_names(&database, "v");
    names.sort();
    assert_eq!(names, ["del", "ins", "upd"]);

    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(table.triggers(&database).expect("table in database").count(), 0);

    let update = database.triggers().find(|trigger| trigger.name() == "upd").expect("upd exists");
    assert!(matches!(
        update.relation(&database),
        Ok(Relation::View(on)) if on == view(&database, "v")
    ));
    assert_eq!(update.target_relation_name().name(), "v");
    assert_eq!(update.timing(), Some(TriggerPeriod::InsteadOf));
    assert!(matches!(update.events(), [TriggerEvent::Update(columns)] if columns.len() == 1));
    assert_eq!(update.condition().map(ToString::to_string).as_deref(), Some("OLD.a > 0"));
    assert!(update.statements.is_some());
}

#[test]
fn sqlite_refuses_instead_of_on_a_table_and_before_or_after_on_a_view() {
    for (statements, expected) in [
        (
            "CREATE TRIGGER x INSTEAD OF INSERT ON t BEGIN SELECT 1; END;",
            TriggerFiringRefusal::InsteadOfOnTable,
        ),
        (
            "CREATE TRIGGER x BEFORE INSERT ON v BEGIN SELECT 1; END;",
            TriggerFiringRefusal::BeforeOrAfterOnView,
        ),
        (
            "CREATE TRIGGER x AFTER UPDATE ON v BEGIN SELECT 1; END;",
            TriggerFiringRefusal::BeforeOrAfterOnView,
        ),
    ] {
        assert_refused(sqlite(statements), statements, expected);
    }
}

#[test]
fn postgres_instead_of_row_trigger_lands_on_the_view() {
    let statements = "CREATE TRIGGER x INSTEAD OF INSERT OR UPDATE OR DELETE ON v
         FOR EACH ROW EXECUTE FUNCTION f();";
    let database = accepted(postgres(statements), statements);
    assert_eq!(view_trigger_names(&database, "v"), ["x"]);
    let trigger = database.triggers().next().expect("x exists");
    assert!(matches!(trigger.relation(&database), Ok(Relation::View(_))));
    assert_eq!(trigger.function(&database).map(FunctionLike::name), Some("f"));
}

#[test]
fn postgres_accepts_statement_level_before_and_after_triggers_on_a_view() {
    let statements = "CREATE TRIGGER b BEFORE UPDATE OF a ON v EXECUTE FUNCTION f();
         CREATE TRIGGER s AFTER INSERT ON v FOR EACH STATEMENT EXECUTE FUNCTION f();";
    let database = accepted(postgres(statements), statements);
    let mut names = view_trigger_names(&database, "v");
    names.sort();
    assert_eq!(names, ["b", "s"]);
}

#[test]
fn postgres_refuses_what_it_refuses_on_tables_and_views() {
    use TriggerFiringRefusal::{
        InsteadOfColumnList, InsteadOfCondition, InsteadOfOnTable, InsteadOfPerStatement,
        RowLevelBeforeOrAfterOnView, TransitionTablesOnView, TruncateOnView,
    };
    for (statements, expected) in [
        (
            "CREATE TRIGGER x INSTEAD OF INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();",
            InsteadOfOnTable,
        ),
        ("CREATE TRIGGER x INSTEAD OF INSERT ON t EXECUTE FUNCTION f();", InsteadOfOnTable),
        (
            "CREATE TRIGGER x BEFORE INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();",
            RowLevelBeforeOrAfterOnView,
        ),
        (
            "CREATE TRIGGER x AFTER INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();",
            RowLevelBeforeOrAfterOnView,
        ),
        (
            "CREATE CONSTRAINT TRIGGER x AFTER INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();",
            RowLevelBeforeOrAfterOnView,
        ),
        ("CREATE TRIGGER x AFTER TRUNCATE ON v EXECUTE FUNCTION f();", TruncateOnView),
        (
            "CREATE TRIGGER x INSTEAD OF TRUNCATE ON v FOR EACH ROW EXECUTE FUNCTION f();",
            TruncateOnView,
        ),
        ("CREATE TRIGGER x INSTEAD OF INSERT ON v EXECUTE FUNCTION f();", InsteadOfPerStatement),
        (
            "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH STATEMENT EXECUTE FUNCTION f();",
            InsteadOfPerStatement,
        ),
        (
            "CREATE TRIGGER x INSTEAD OF UPDATE OF a ON v FOR EACH ROW EXECUTE FUNCTION f();",
            InsteadOfColumnList,
        ),
        (
            "CREATE TRIGGER x INSTEAD OF UPDATE ON v FOR EACH ROW WHEN (OLD.a > 0)
                 EXECUTE FUNCTION f();",
            InsteadOfCondition,
        ),
        (
            "CREATE TRIGGER x AFTER INSERT ON v REFERENCING NEW TABLE AS n
                 FOR EACH STATEMENT EXECUTE FUNCTION f();",
            TransitionTablesOnView,
        ),
        (
            "CREATE TRIGGER x INSTEAD OF INSERT ON v REFERENCING NEW TABLE AS n
                 FOR EACH ROW EXECUTE FUNCTION f();",
            TransitionTablesOnView,
        ),
    ] {
        assert_refused(postgres(statements), statements, expected);
    }
}

#[test]
fn a_refusal_names_the_trigger_the_relation_and_what_the_engine_refuses() {
    let row = "FOR EACH ROW EXECUTE FUNCTION f();";
    for (parsed, message) in [
        (
            postgres(&format!("CREATE TRIGGER x INSTEAD OF INSERT ON t {row}")),
            "Trigger `x` on `t` is refused because a table cannot have INSTEAD OF triggers.",
        ),
        (
            sqlite("CREATE TRIGGER x BEFORE INSERT ON v BEGIN SELECT 1; END;"),
            "Trigger `x` on `v` is refused because a view cannot have BEFORE or AFTER triggers.",
        ),
        (
            postgres(&format!("CREATE TRIGGER x AFTER INSERT ON v {row}")),
            "Trigger `x` on `v` is refused because a view cannot have row-level BEFORE or AFTER \
             triggers.",
        ),
        (
            postgres("CREATE TRIGGER x AFTER TRUNCATE ON v EXECUTE FUNCTION f();"),
            "Trigger `x` on `v` is refused because a view cannot have TRUNCATE triggers.",
        ),
        (
            postgres("CREATE TRIGGER x INSTEAD OF INSERT ON v EXECUTE FUNCTION f();"),
            "Trigger `x` on `v` is refused because INSTEAD OF triggers must be FOR EACH ROW.",
        ),
        (
            postgres(&format!("CREATE TRIGGER x INSTEAD OF UPDATE OF a ON v {row}")),
            "Trigger `x` on `v` is refused because INSTEAD OF triggers cannot have column lists.",
        ),
        (
            postgres(
                "CREATE TRIGGER x INSTEAD OF UPDATE ON v FOR EACH ROW WHEN (OLD.a > 0)
                     EXECUTE FUNCTION f();",
            ),
            "Trigger `x` on `v` is refused because INSTEAD OF triggers cannot have WHEN \
             conditions.",
        ),
        (
            postgres(
                "CREATE TRIGGER x AFTER INSERT ON v REFERENCING NEW TABLE AS n
                     FOR EACH STATEMENT EXECUTE FUNCTION f();",
            ),
            "Trigger `x` on `v` is refused because triggers on views cannot have transition \
             tables.",
        ),
    ] {
        assert_eq!(parsed.expect_err(message).to_string(), message);
    }
}

#[test]
fn a_trigger_on_a_materialized_view_is_refused() {
    let statements = "CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE TRIGGER x AFTER INSERT ON m FOR EACH STATEMENT EXECUTE FUNCTION f();";
    assert!(
        matches!(
            postgres(statements),
            Err(Error::RelationKindMismatch { actual_kind: ObjectKind::MaterializedView, .. })
        ),
        "{statements} should be refused"
    );
}

#[test]
fn a_trigger_name_is_unique_per_view_unless_replaced() {
    let first = "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();";
    let again = format!("{first} {first}");
    assert!(
        matches!(postgres(&again), Err(Error::TriggerAlreadyExists { .. })),
        "{again} should be refused"
    );
    let replaced = format!(
        "{first} CREATE OR REPLACE TRIGGER x INSTEAD OF DELETE ON v FOR EACH ROW EXECUTE FUNCTION f();"
    );
    let database = accepted(postgres(&replaced), &replaced);
    let triggers: Vec<_> = database.triggers().collect();
    assert_eq!(triggers.len(), 1);
    assert!(matches!(triggers[0].events(), [TriggerEvent::Delete]));
}

#[test]
fn drop_trigger_on_a_view_drops_it() {
    for statements in [
        "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();
         DROP TRIGGER x ON v;",
        "CREATE TRIGGER x INSTEAD OF DELETE ON v FOR EACH ROW EXECUTE FUNCTION f();
         ALTER VIEW v RENAME TO w; DROP TRIGGER x ON w;",
    ] {
        let database = accepted(postgres(statements), statements);
        assert_eq!(database.triggers().count(), 0, "{statements}");
    }
    let statements = "CREATE TRIGGER x INSTEAD OF INSERT ON v BEGIN SELECT 1; END; DROP TRIGGER x;";
    assert_eq!(accepted(sqlite(statements), statements).triggers().count(), 0);
}

#[test]
fn a_trigger_on_a_view_leaves_its_namesake_on_a_table_alone() {
    let both = "CREATE TRIGGER x AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();";
    for statements in [format!("{both} DROP TRIGGER x ON v;"), format!("{both} DROP VIEW v;")] {
        let database = accepted(postgres(&statements), &statements);
        let left: Vec<_> = database
            .triggers()
            .map(|trigger| (trigger.name(), trigger.target_relation_name().name().to_owned()))
            .collect();
        assert_eq!(left, [("x", "t".to_owned())], "{statements}");
    }
}

#[test]
fn the_triggers_go_with_the_view() {
    let cases = [
        (
            sqlite("CREATE TRIGGER x INSTEAD OF INSERT ON v BEGIN SELECT 1; END; DROP VIEW v;"),
            "SQLite DROP VIEW",
        ),
        (
            postgres(
                "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();
                 DROP VIEW v;",
            ),
            "DROP VIEW",
        ),
        (
            postgres(
                "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();
                 DROP TABLE t CASCADE;",
            ),
            "DROP TABLE CASCADE",
        ),
        (
            postgres(
                "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();
                 CREATE OR REPLACE VIEW public.v AS SELECT a FROM t;
                 DROP VIEW v;",
            ),
            "DROP VIEW after a replacement spelling the schema",
        ),
        (
            postgres(
                "CREATE SCHEMA s; CREATE VIEW s.w AS SELECT a FROM t;
                 CREATE TRIGGER x INSTEAD OF INSERT ON s.w FOR EACH ROW EXECUTE FUNCTION f();
                 DROP SCHEMA s CASCADE;",
            ),
            "DROP SCHEMA CASCADE",
        ),
    ];
    for (parsed, case) in cases {
        let database = accepted(parsed, case);
        assert_eq!(database.triggers().count(), 0, "{case}");
    }
}

#[test]
fn the_triggers_follow_a_renamed_view_and_survive_its_replacement() {
    let statements = "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();
         ALTER VIEW v RENAME TO w;
         CREATE OR REPLACE VIEW w AS SELECT a, 1 AS b FROM t;";
    let database = accepted(postgres(statements), statements);
    let names: Vec<&str> =
        view(&database, "w").triggers(&database).map(TriggerLike::name).collect();
    assert_eq!(names, ["x"]);
    assert_eq!(database.triggers().next().expect("x exists").target_relation_name().name(), "w");
}

#[test]
fn the_triggers_follow_a_renamed_schema() {
    let statements = "CREATE SCHEMA s; CREATE VIEW s.w AS SELECT a FROM t;
         CREATE TRIGGER x INSTEAD OF INSERT ON s.w FOR EACH ROW EXECUTE FUNCTION f();
         ALTER SCHEMA s RENAME TO s2;";
    let database = accepted(postgres(statements), statements);
    let moved = database
        .view_by_target(
            TargetName::new("w", false).with_schema("s2", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("s2.w exists");
    let names: Vec<&str> = moved.triggers(&database).map(TriggerLike::name).collect();
    assert_eq!(names, ["x"]);
}

#[test]
fn a_view_column_rename_and_rename_reach_only_the_view_triggers() {
    let statements = "CREATE TRIGGER tt BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION f();
         CREATE TRIGGER vt BEFORE INSERT OR UPDATE OF a ON v EXECUTE FUNCTION f();
         ALTER TABLE v RENAME COLUMN a TO b;
         ALTER VIEW v RENAME TO w;";
    let database = accepted(postgres(statements), statements);
    let trigger = |name| {
        database.triggers().find(|trigger| trigger.name() == name).expect("the trigger exists")
    };

    let on_view = trigger("vt");
    assert_eq!(on_view.target_relation_name().name(), "w");
    assert!(matches!(
        on_view.events(),
        [TriggerEvent::Insert, TriggerEvent::Update(columns)]
            if columns.len() == 1 && columns[0].value == "b"
    ));

    let on_table = trigger("tt");
    assert_eq!(on_table.target_relation_name().name(), "t");
    assert!(matches!(
        on_table.events(),
        [TriggerEvent::Update(columns)] if columns.len() == 1 && columns[0].value == "a"
    ));
}

#[test]
fn enabling_or_disabling_a_view_trigger_is_refused() {
    let statements = "CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();
         ALTER TABLE v DISABLE TRIGGER x;";
    assert!(
        matches!(
            postgres(statements),
            Err(Error::AlterActionUnsupportedOnRelation { object_kind: ObjectKind::View, .. })
        ),
        "{statements} should be refused"
    );
}

#[test]
fn a_view_trigger_is_no_maintenance_trigger() {
    let statements = "CREATE FUNCTION g() RETURNS TRIGGER LANGUAGE plpgsql AS
         $$ BEGIN NEW.a := 1; RETURN NEW; END $$;
         CREATE TRIGGER x INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION g();";
    let database = accepted(postgres(statements), statements);
    let trigger = database.triggers().next().expect("x exists");
    assert!(!trigger.is_maintenance_trigger(&database).expect("x resolves"));
    assert_eq!(
        trigger.maintenance_refusal(&database).expect("x resolves"),
        Some(MaintenanceRefusal::OnView)
    );
    assert_eq!(trigger.maintenance_assignments(&database).expect("x resolves").count(), 0);
}

#[test]
fn a_view_trigger_leaves_the_type_of_a_row_field_unknown() {
    let statements = "CREATE FUNCTION g() RETURNS TRIGGER LANGUAGE plpgsql AS
         $$ BEGIN EXECUTE 'SELECT ' || NEW.a; RETURN NEW; END $$;
         CREATE TRIGGER on_t BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION g();
         CREATE TRIGGER on_v INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION g();";
    let database = accepted(postgres(statements), statements);
    let function = database
        .function_by_target(TargetName::new("g", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("g exists");
    let body = function.dynamic_execution_sites(&database).expect("body parses");
    let sites = body.sites().expect("sites resolve");
    let scope = sites[0].scope();
    let context = ExpressionContext::for_expression(
        sites[0].command(),
        &scope,
        &database,
        IdentifierCase::Folded,
    )
    .expect("the command is in its scope");
    let sqlparser::ast::Expr::BinaryOp { right: field, .. } = sites[0].command() else {
        panic!("a concatenation")
    };
    assert_eq!(context.type_of(field).expect("typed"), OperandType::Unknown);
    let relations: Vec<_> = context
        .row_field_types(field)
        .expect("typed")
        .into_iter()
        .map(|(relation, field_type)| {
            let name = match relation {
                Relation::Table(table) => table.table_name().to_string(),
                Relation::View(view) => view.view_name().to_string(),
                other => panic!("unexpected relation {other:?}"),
            };
            (name, field_type == OperandType::Unknown)
        })
        .collect();
    assert_eq!(relations, [("t".to_string(), false), ("v".to_string(), true)]);
}
