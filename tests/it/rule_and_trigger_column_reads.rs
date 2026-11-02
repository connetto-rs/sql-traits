//! Tests that a rule's action and condition bind the relations they name at
//! creation, and that rules and trigger `WHEN` conditions keep reading what
//! they read through renames and drops.
//!
//! A rule depends on every relation its action and condition name as well as
//! on the columns of that relation they read, so a rename rewrites those
//! references and a drop of them refuses without `CASCADE`. The `NEW` and
//! `OLD` of a rule or a trigger's `WHEN` condition name the row of the
//! relation the statement is defined on, which a statement on another
//! relation never reads. The expected outcomes are the ones PostgreSQL 18
//! gives for the same statements.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const TRIGGER: &str =
    "CREATE FUNCTION touch() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn only_rule(database: &ParserDB) -> &<ParserDB as DatabaseLike>::Rule {
    let mut rules = database.rules();
    let rule = rules.next().expect("one rule is recorded");
    assert!(rules.next().is_none(), "exactly one rule is recorded");
    rule
}

fn rule_action(rule: &<ParserDB as DatabaseLike>::Rule) -> String {
    rule.action().to_string()
}

fn rule_condition(rule: &<ParserDB as DatabaseLike>::Rule) -> String {
    rule.condition().map(ToString::to_string).expect("the rule has a condition")
}

fn only_trigger(database: &ParserDB) -> &sqlparser::ast::CreateTrigger {
    let mut triggers = database.triggers();
    let trigger = triggers.next().expect("one trigger is recorded");
    assert!(triggers.next().is_none(), "exactly one trigger is recorded");
    trigger
}

/// A rule writing to a renamed column of its target follows the rename.
#[test]
fn a_rule_action_follows_a_renamed_column_of_its_target() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         ALTER TABLE t RENAME COLUMN a TO aa;",
    )
    .expect("schema builds");

    assert_eq!(rule_action(only_rule(&database)), "INSERT INTO t (aa) VALUES (new.x)");
}

/// A rule action reading the renamed column of another relation follows it,
/// keeping the output name the query exposed.
#[test]
fn a_rule_action_reading_a_renamed_column_follows_it() {
    let database = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE log (a INT);
         CREATE RULE r AS ON DELETE TO t DO ALSO INSERT INTO log (a) SELECT a FROM t;
         ALTER TABLE t RENAME COLUMN a TO aa;",
    )
    .expect("schema builds");

    assert_eq!(rule_action(only_rule(&database)), "INSERT INTO log (a) SELECT aa AS a FROM t");
}

/// A rule condition reading the renamed column of another relation follows it.
#[test]
fn a_rule_condition_follows_a_renamed_column() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s WHERE (SELECT max(a) FROM t) > 0 DO ALSO NOTHING;
         ALTER TABLE t RENAME COLUMN a TO aa;",
    )
    .expect("schema builds");

    assert_eq!(rule_condition(only_rule(&database)), "(SELECT max(aa) FROM t) > 0");
}

/// A rule's own `NEW` and `OLD` references follow the rename of their
/// relation's column, in the condition and the action alike.
#[test]
fn a_rule_condition_and_action_follow_a_renamed_column_through_new_and_old() {
    let database = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE log (a INT);
         CREATE RULE r AS ON UPDATE TO t WHERE new.a > old.a
             DO ALSO INSERT INTO log (a) VALUES (new.a);
         ALTER TABLE t RENAME COLUMN a TO aa;",
    )
    .expect("schema builds");

    let rule = only_rule(&database);
    assert_eq!(rule_condition(rule), "new.aa > old.aa");
    assert_eq!(rule_action(rule), "INSERT INTO log (a) VALUES (new.aa)");
}

/// A rule's `NEW` and `OLD` name the rule's relation row, so a rename of the
/// write target's column rewrites the column list and leaves the row
/// reference as written.
#[test]
fn a_rule_action_new_reference_keeps_a_write_target_rename_alone() {
    let database = parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE log (a INT);
         CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO log (a) VALUES (new.a);
         ALTER TABLE log RENAME COLUMN a TO z;",
    )
    .expect("schema builds");

    assert_eq!(rule_action(only_rule(&database)), "INSERT INTO log (z) VALUES (new.a)");
}

/// A rule action naming a renamed relation follows its new name.
#[test]
fn a_rule_action_follows_a_renamed_relation() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         ALTER TABLE t RENAME TO tt;",
    )
    .expect("schema builds");

    assert_eq!(rule_action(only_rule(&database)), "INSERT INTO tt (a) VALUES (new.x)");
}

/// A rule condition naming a renamed relation follows its new name.
#[test]
fn a_rule_condition_follows_a_renamed_relation() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s WHERE (SELECT max(a) FROM t) > 0 DO ALSO NOTHING;
         ALTER TABLE t RENAME TO tt;",
    )
    .expect("schema builds");

    assert_eq!(rule_condition(only_rule(&database)), "(SELECT max(a) FROM tt) > 0");
}

/// A `CREATE RULE` whose action names a relation the database does not hold
/// is refused before it records anything.
#[test]
fn a_rule_naming_an_absent_relation_in_its_action_is_refused() {
    let error = parse(
        "CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO nosuch (a) VALUES (new.a);",
    )
    .expect_err("the action names an absent relation");

    assert!(
        matches!(
            &error,
            Error::RelationNotFoundForRuleAction { relation_name, rule_name }
                if relation_name == "nosuch" && rule_name == "r"
        ),
        "got {error:?}"
    );
}

/// A `CREATE RULE` whose condition names a relation the database does not
/// hold is refused before it records anything.
#[test]
fn a_rule_naming_an_absent_relation_in_its_condition_is_refused() {
    let error = parse(
        "CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO t WHERE (SELECT max(a) FROM nosuch) > 0 DO ALSO NOTHING;",
    )
    .expect_err("the condition names an absent relation");

    assert!(
        matches!(
            &error,
            Error::RelationNotFoundForRuleAction { relation_name, rule_name }
                if relation_name == "nosuch" && rule_name == "r"
        ),
        "got {error:?}"
    );
}

/// The action's names bind under the `search_path` in effect at creation, so
/// a later change of it leaves the rule reading what it reached.
#[test]
fn a_rule_action_binds_under_the_search_path_in_effect_at_creation() {
    let database = parse(
        "CREATE SCHEMA one;
         CREATE SCHEMA two;
         CREATE TABLE one.t (a INT);
         CREATE TABLE two.t (a INT);
         SET search_path = one;
         CREATE TABLE s (x INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         SET search_path = two;
         ALTER TABLE two.t RENAME COLUMN a TO unchanged;
         ALTER TABLE one.t RENAME COLUMN a TO aa;",
    )
    .expect("schema builds");

    assert_eq!(rule_action(only_rule(&database)), "INSERT INTO one.t (aa) VALUES (new.x)");
}

/// `NEW` and `OLD` in a rule are the rule's relation row, so a relation
/// spelled `new` is neither reached by them nor refused at creation.
#[test]
fn a_rule_condition_new_reference_is_not_a_relation() {
    let database = parse(
        "CREATE TABLE new (a INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO t WHERE new.a > 0 DO ALSO NOTHING;
         ALTER TABLE new RENAME TO renamed;",
    )
    .expect("schema builds");

    assert_eq!(rule_condition(only_rule(&database)), "new.a > 0");
}

/// A condition reading the rule's row and another table's column depends on
/// that column only, so another column of the table drops freely.
#[test]
fn a_rule_condition_mixing_new_and_a_subquery_depends_on_the_named_column_only() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT, b INT);
         CREATE RULE r AS ON INSERT TO s WHERE new.x > 0 AND (SELECT max(a) FROM t) > 0
             DO ALSO NOTHING;
         ALTER TABLE t DROP COLUMN b;",
    )
    .expect("the condition names t.a only");
    assert_eq!(rule_condition(only_rule(&database)), "new.x > 0 AND (SELECT max(a) FROM t) > 0");

    let error = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT, b INT);
         CREATE RULE r AS ON INSERT TO s WHERE new.x > 0 AND (SELECT max(a) FROM t) > 0
             DO ALSO NOTHING;
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect_err("the condition names t.a");
    assert!(matches!(error, Error::ColumnReferenced { .. }), "{error:?}");
}

/// A trigger `WHEN` condition naming the renamed column follows the rename.
#[test]
fn a_trigger_when_follows_a_renamed_column() {
    let database = parse(&format!(
        "{TRIGGER}
         CREATE TABLE t (a INT, b INT);
         CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.a > OLD.a) EXECUTE FUNCTION touch();
         ALTER TABLE t RENAME COLUMN a TO aa;"
    ))
    .expect("schema builds");

    assert_eq!(
        only_trigger(&database).condition.as_ref().map(ToString::to_string).as_deref(),
        Some("(NEW.aa > OLD.aa)")
    );
}

/// A trigger `WHEN` condition naming another column holds no dependency on
/// the dropped one.
#[test]
fn a_trigger_when_on_another_column_does_not_block_the_drop() {
    let database = parse(&format!(
        "{TRIGGER}
         CREATE TABLE t (a INT, b INT);
         CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.b > 0) EXECUTE FUNCTION touch();
         ALTER TABLE t DROP COLUMN a;"
    ))
    .expect("the WHEN names no dependency on the dropped column");

    assert_eq!(database.triggers().count(), 1);
}

/// A trigger `WHEN` condition reading the row as a whole holds no column
/// dependency.
#[test]
fn a_trigger_when_on_the_whole_row_does_not_block_the_drop() {
    let database = parse(&format!(
        "{TRIGGER}
         CREATE TABLE t (a INT, b INT);
         CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW IS DISTINCT FROM OLD) EXECUTE FUNCTION touch();
         ALTER TABLE t DROP COLUMN a;"
    ))
    .expect("the WHEN reads the row as a whole, which names no column");

    assert_eq!(database.triggers().count(), 1);
}

/// Dropping a column a trigger's `WHEN` condition names through `NEW` or
/// `OLD` refuses without `CASCADE`.
#[test]
fn a_trigger_when_naming_the_column_refuses_the_drop() {
    let error = parse(&format!(
        "{TRIGGER}
         CREATE TABLE t (a INT, b INT);
         CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.a > OLD.a) EXECUTE FUNCTION touch();
         ALTER TABLE t DROP COLUMN a;"
    ))
    .expect_err("the WHEN names the dropped column");

    assert!(
        matches!(
            &error,
            Error::ColumnReferenced { table_name, column_name }
                if table_name == "t" && column_name == "a"
        ),
        "got {error:?}"
    );
}

/// `CASCADE` takes a trigger whose `WHEN` condition names the dropped column
/// along with it.
#[test]
fn a_trigger_when_naming_the_column_drops_with_cascade() {
    let database = parse(&format!(
        "{TRIGGER}
         CREATE TABLE t (a INT, b INT);
         CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.a > OLD.a) EXECUTE FUNCTION touch();
         CREATE TRIGGER h BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.b > 0) EXECUTE FUNCTION touch();
         ALTER TABLE t DROP COLUMN a CASCADE;"
    ))
    .expect("CASCADE takes the reading trigger along");

    let names: Vec<String> = database
        .triggers()
        .map(|trigger| trigger.name.0.last().map_or_else(String::new, ToString::to_string))
        .collect();
    assert_eq!(names, ["h"]);
}

/// Dropping a column a rule on another relation names in its action refuses
/// without `CASCADE`.
#[test]
fn a_rule_on_another_relation_refuses_the_drop_column() {
    let error = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect_err("the rule names the dropped column in its action");

    assert!(
        matches!(
            &error,
            Error::ColumnReferenced { table_name, column_name }
                if table_name == "t" && column_name == "a"
        ),
        "got {error:?}"
    );
}

/// `CASCADE` takes a rule on another relation that names the dropped column
/// in its action or condition along with it, and leaves the other rules.
#[test]
fn a_rule_naming_the_column_drops_with_cascade() {
    let database = parse(
        "CREATE TABLE s (x INT, y INT);
         CREATE TABLE t (a INT, b INT);
         CREATE RULE reads_a AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         CREATE RULE reads_b AS ON INSERT TO s WHERE (SELECT max(b) FROM t) > 0 DO ALSO NOTHING;
         ALTER TABLE t DROP COLUMN a CASCADE;",
    )
    .expect("CASCADE takes the reading rule along");

    let mut names: Vec<String> = database.rules().map(|rule| rule.name().to_string()).collect();
    names.sort();
    assert_eq!(names, ["reads_b"]);
}

/// A rule whose action spells no column list writes the target's first
/// columns, which the drop of one of them reaches.
#[test]
fn a_rule_action_without_a_column_list_spells_the_target_columns() {
    let error = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT, b INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t VALUES (new.x);
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect_err("the action writes the target's first column");

    assert!(
        matches!(
            &error,
            Error::ColumnReferenced { table_name, column_name }
                if table_name == "t" && column_name == "a"
        ),
        "got {error:?}"
    );
}

/// Dropping a table a rule on another relation names in its action refuses
/// without `CASCADE`, naming the rule as the dependent.
#[test]
fn a_rule_on_another_relation_refuses_the_drop_table() {
    let error = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         DROP TABLE t;",
    )
    .expect_err("the rule names the dropped table in its action");

    assert!(
        matches!(
            &error,
            Error::RelationHasDependents { object_kind, dependent_kind, dependent_name, .. }
                if *object_kind == sql_traits::errors::ObjectKind::Table
                    && *dependent_kind == sql_traits::errors::ObjectKind::Rule
                    && dependent_name == "r"
        ),
        "got {error:?}"
    );
}

/// `CASCADE` takes a rule on another relation that names the dropped table
/// along with it.
#[test]
fn a_rule_naming_the_dropped_table_drops_with_cascade() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE reads_t AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         CREATE RULE reads_s AS ON INSERT TO t DO ALSO INSERT INTO s (x) VALUES (new.a);
         DROP TABLE t CASCADE;",
    )
    .expect("CASCADE takes the reading rule along");

    // `reads_s` goes with the table it is on, and `reads_t` goes as a
    // dependent naming the table from another relation.
    assert_eq!(database.rules().count(), 0);
}

/// A rule sharing the dropped reader's name on another relation is a
/// different rule and stays.
#[test]
fn a_cascade_keeps_a_rule_of_the_same_name_on_another_relation() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE u (x INT);
         CREATE TABLE t (a INT);
         CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         CREATE RULE r AS ON INSERT TO u DO ALSO NOTHING;
         DROP TABLE t CASCADE;",
    )
    .expect("CASCADE takes the reading rule along");

    let hosts: Vec<String> =
        database.rules().map(|rule| rule.target_relation_name().to_string()).collect();
    assert_eq!(hosts, ["u"]);
}

/// A rule named like a table is not that table, so a view reading the table
/// survives the rule's cascade.
#[test]
fn a_cascade_through_a_rule_stops_at_the_rule() {
    let database = parse(
        "CREATE TABLE s (x INT);
         CREATE TABLE t (a INT);
         CREATE TABLE x (y INT);
         CREATE VIEW v AS SELECT y FROM x;
         CREATE RULE x AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);
         DROP TABLE t CASCADE;",
    )
    .expect("CASCADE takes the reading rule along");

    assert_eq!(database.rules().count(), 0);
    assert_eq!(database.views().count(), 1);
}

/// A rule on the dropped table itself goes with it and does not refuse the
/// drop.
#[test]
fn a_rule_on_the_dropped_table_does_not_refuse_the_drop() {
    let database = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO t (b) VALUES (new.a);
         DROP TABLE t;",
    )
    .expect("the rule on the table goes with it");

    assert_eq!(database.rules().count(), 0);
}

/// A rule action using the row as a whole holds no column dependency.
#[test]
fn a_rule_action_using_the_whole_row_does_not_block_the_drop() {
    let database = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE TABLE copy (x INT);
         CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO copy (x) VALUES (new);
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("the action reads the row as a whole, which names no column");

    assert_eq!(database.rules().count(), 1);
}
