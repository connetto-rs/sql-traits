//! Tests that `CREATE RULE` is recorded on the relation it is defined on, and
//! that the rule follows that relation through renames and drops.
//!
//! A rule depends on its table or view and on the columns of it the condition
//! and action read through `NEW` and `OLD`, so those are the references kept
//! current. Relations the action names are kept as written.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::{
    ast::{CreateRuleAction, CreateRuleEvent},
    dialect::PostgreSqlDialect,
};

const ORDERS: &str = "CREATE TABLE orders (id INT PRIMARY KEY, total INT);
     CREATE TABLE audit (id INT);
";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn only_rule(database: &ParserDB) -> &<ParserDB as DatabaseLike>::Rule {
    let mut rules = database.rules();
    let rule = rules.next().expect("one rule is recorded");
    assert!(rules.next().is_none(), "exactly one rule is recorded");
    rule
}

fn rule_names(database: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> = database.rules().map(|rule| rule.name().to_string()).collect();
    names.sort();
    names
}

fn rule_targets(database: &ParserDB) -> Vec<String> {
    let mut targets: Vec<String> =
        database.rules().map(|rule| rule.target_relation_name().to_string()).collect();
    targets.sort();
    targets
}

fn action_text(rule: &<ParserDB as DatabaseLike>::Rule) -> String {
    rule.action().to_string()
}

fn rule_named<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Rule {
    database.rules().find(|rule| rule.name() == name).expect("the rule is recorded")
}

/// The kind and name of the relation a rule resolves to.
fn relation_of(
    database: &ParserDB,
    rule: &<ParserDB as DatabaseLike>::Rule,
) -> (&'static str, String) {
    match rule.relation(database).expect("the rule's relation resolves") {
        Relation::Table(table) => ("table", table.table_name().to_string()),
        Relation::View(view) => ("view", view.view_name().to_string()),
        // Ingestion refuses a rule on a materialized view or a sequence.
        Relation::MaterializedView(_) | Relation::Sequence(_) => {
            panic!("a rule never resolves to a materialized view or a sequence")
        }
    }
}

#[test]
fn a_rule_on_a_table_is_readable() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE log_order AS ON INSERT TO orders WHERE new.total > 0
             DO ALSO INSERT INTO audit VALUES (new.id);"
    ))
    .expect("schema builds");

    let rule = only_rule(&database);
    assert_eq!(rule.name(), "log_order");
    assert_eq!(rule.event(), CreateRuleEvent::Insert);
    assert!(!rule.is_instead());
    assert_eq!(rule.condition().map(ToString::to_string).as_deref(), Some("new.total > 0"));
    assert!(matches!(rule.action(), CreateRuleAction::Statement(_)));
    assert_eq!(action_text(rule), "INSERT INTO audit VALUES (new.id)");
    assert_eq!(rule.target_relation_name().to_string(), "orders");
    assert_eq!(relation_of(&database, rule), ("table", "orders".to_string()));

    let orders = database
        .table_by_target(TargetName::new("orders", false), IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("orders exists");
    let on_orders: Vec<&str> =
        orders.rules(&database).expect("orders is in the database").map(RuleLike::name).collect();
    assert_eq!(on_orders, ["log_order"]);
    let audit = database
        .table_by_target(TargetName::new("audit", false), IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("audit exists");
    assert_eq!(audit.rules(&database).expect("audit is in the database").count(), 0);
}

#[test]
fn a_bare_do_reads_as_also() {
    let database = parse(&format!("{ORDERS}CREATE RULE quiet AS ON DELETE TO orders DO NOTHING;"))
        .expect("schema builds");

    let rule = only_rule(&database);
    assert_eq!(rule.event(), CreateRuleEvent::Delete);
    assert!(!rule.is_instead());
    assert!(rule.condition().is_none());
    assert!(matches!(rule.action(), CreateRuleAction::Nothing));
}

#[test]
fn a_rule_on_a_view_is_readable() {
    let database = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id, total FROM orders;
         CREATE RULE recent_insert AS ON INSERT TO recent
             DO INSTEAD INSERT INTO orders VALUES (new.id, new.total);"
    ))
    .expect("schema builds");

    let rule = only_rule(&database);
    assert!(rule.is_instead());
    assert_eq!(relation_of(&database, rule), ("view", "recent".to_string()));
}

/// Everything a rule answers, read through whichever implementation `R` is.
fn answers<R: RuleLike<DB = ParserDB>>(
    rule: &R,
    database: &ParserDB,
) -> (String, String, CreateRuleEvent, bool, Option<String>, String, String) {
    let relation = rule.relation(database).expect("the rule's relation resolves");
    (
        rule.name().to_string(),
        rule.target_relation_name().to_string(),
        rule.event(),
        rule.is_instead(),
        rule.condition().map(ToString::to_string),
        rule.action().to_string(),
        format!("{relation:?}"),
    )
}

/// A caller iterating rules holds references, and reading through one answers
/// what the rule itself answers, quoting and qualification included.
#[test]
fn a_rule_read_through_a_reference_answers_the_same() {
    let database = parse(
        "CREATE SCHEMA \"App\";
         CREATE TABLE \"App\".\"Docs\" (id INT);
         CREATE RULE keep AS ON DELETE TO \"App\".\"Docs\" WHERE old.id > 0 DO INSTEAD NOTHING;",
    )
    .expect("schema builds");
    let rule = only_rule(&database);

    let direct = answers(rule, &database);
    assert_eq!(&direct.1, "\"App\".\"Docs\"");
    assert_eq!(direct.4.as_deref(), Some("old.id > 0"));
    assert!(direct.6.starts_with("Table("), "got {}", direct.6);
    assert_eq!(answers(&rule, &database), direct);
}

/// A rule asked about a database that never created its relation reports the
/// relation missing.
#[test]
fn a_rule_asked_about_another_database_finds_no_relation() {
    let with_view = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id FROM orders;
         CREATE RULE r AS ON DELETE TO recent DO INSTEAD NOTHING;"
    ))
    .expect("schema builds");
    let without_view = parse(ORDERS).expect("schema builds");

    assert_eq!(
        only_rule(&with_view).relation(&without_view).err(),
        Some(LookupError::RelationNotFound { object_name: "recent".to_string() })
    );
}

#[test]
fn a_bare_target_binds_to_the_schema_it_reached() {
    let database = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.orders (id INT);
         SET search_path TO app;
         CREATE RULE quiet AS ON DELETE TO orders DO INSTEAD NOTHING;
         SET search_path TO public;",
    )
    .expect("schema builds");

    let rule = only_rule(&database);
    assert_eq!(rule.target_relation_name().to_string(), "app.orders");
    assert_eq!(relation_of(&database, rule), ("table", "orders".to_string()));
}

/// PostgreSQL folds an unquoted target, so a spelling in another case reaches
/// the same relation, and the rule leaves with it.
#[test]
fn an_unquoted_target_in_another_case_reaches_the_relation() {
    let database = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id FROM orders;
         CREATE RULE on_orders AS ON DELETE TO Orders DO INSTEAD NOTHING;
         CREATE RULE on_recent AS ON DELETE TO RECENT DO INSTEAD NOTHING;"
    ))
    .expect("schema builds");

    let mut relations: Vec<(String, (&str, String))> = database
        .rules()
        .map(|rule| (rule.name().to_string(), relation_of(&database, rule)))
        .collect();
    relations.sort();
    assert_eq!(
        relations,
        [
            ("on_orders".to_string(), ("table", "orders".to_string())),
            ("on_recent".to_string(), ("view", "recent".to_string())),
        ]
    );

    let dropped = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id FROM orders;
         CREATE RULE on_recent AS ON DELETE TO RECENT DO INSTEAD NOTHING;
         DROP VIEW recent;"
    ))
    .expect("schema builds");
    assert_eq!(rule_names(&dropped), Vec::<String>::new());
}

#[test]
fn an_absent_target_is_refused() {
    let error = parse("CREATE RULE r AS ON INSERT TO nowhere DO NOTHING;")
        .expect_err("PostgreSQL refuses a rule on a relation that does not exist");

    assert!(
        matches!(
            &error,
            Error::RelationNotFoundForRule { relation_name, rule_name }
                if relation_name == "nowhere" && rule_name == "r"
        ),
        "got {error:?}"
    );
}

#[test]
fn a_materialized_view_target_is_refused() {
    let error = parse(&format!(
        "{ORDERS}CREATE MATERIALIZED VIEW totals AS SELECT id, total FROM orders;
         CREATE RULE r AS ON INSERT TO totals DO INSTEAD NOTHING;"
    ))
    .expect_err("PostgreSQL does not support rules on materialized views");

    assert!(
        matches!(
            &error,
            Error::RuleOnMaterializedView { rule_name, view_name }
                if rule_name == "r" && view_name == "totals"
        ),
        "got {error:?}"
    );
}

#[test]
fn a_select_rule_is_refused() {
    let error = parse(&format!(
        "{ORDERS}CREATE RULE \"_RETURN\" AS ON SELECT TO orders DO INSTEAD SELECT 1 AS id, 2 AS total;"
    ))
    .expect_err("an ON SELECT rule would turn the relation into a view");

    assert!(
        matches!(
            &error,
            Error::SelectRuleUnsupported { rule_name, relation_name }
                if rule_name == "_RETURN" && relation_name == "orders"
        ),
        "got {error:?}"
    );
}

#[test]
fn a_second_rule_of_the_same_name_on_the_relation_is_refused() {
    let error = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON INSERT TO orders DO NOTHING;
         CREATE RULE R AS ON UPDATE TO orders DO NOTHING;"
    ))
    .expect_err("a rule name is unique per relation");

    assert!(
        matches!(
            &error,
            Error::RuleAlreadyExists { rule_name, relation_name }
                if rule_name == "R" && relation_name == "orders"
        ),
        "got {error:?}"
    );
}

#[test]
fn the_same_rule_name_on_another_relation_is_kept() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON INSERT TO orders DO NOTHING;
         CREATE RULE r AS ON INSERT TO audit DO NOTHING;"
    ))
    .expect("a rule name is unique per relation only");

    assert_eq!(rule_targets(&database), ["audit", "orders"]);
}

#[test]
fn or_replace_replaces_the_rule_in_place() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON INSERT TO orders DO NOTHING;
         CREATE OR REPLACE RULE r AS ON UPDATE TO orders DO INSTEAD NOTHING;"
    ))
    .expect("OR REPLACE replaces the rule");

    let rule = only_rule(&database);
    assert_eq!(rule.event(), CreateRuleEvent::Update);
    assert!(rule.is_instead());
}

#[test]
fn dropping_the_table_drops_its_rules_only() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE on_orders AS ON INSERT TO orders DO NOTHING;
         CREATE RULE on_audit AS ON INSERT TO audit DO NOTHING;
         DROP TABLE orders;"
    ))
    .expect("schema builds");

    assert_eq!(rule_names(&database), ["on_audit"]);
}

#[test]
fn dropping_the_view_drops_its_rules() {
    let database = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id FROM orders;
         CREATE RULE on_recent AS ON DELETE TO recent DO INSTEAD NOTHING;
         CREATE RULE on_orders AS ON DELETE TO orders DO NOTHING;
         DROP VIEW recent;"
    ))
    .expect("schema builds");

    assert_eq!(rule_names(&database), ["on_orders"]);
}

#[test]
fn a_cascade_through_a_view_drops_its_rules() {
    let database = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id FROM orders;
         CREATE VIEW newest AS SELECT id FROM recent;
         CREATE RULE on_newest AS ON DELETE TO newest DO INSTEAD NOTHING;
         CREATE RULE on_audit AS ON DELETE TO audit DO NOTHING;
         DROP TABLE orders CASCADE;"
    ))
    .expect("schema builds");

    assert_eq!(rule_names(&database), ["on_audit"]);
}

#[test]
fn renaming_the_table_carries_its_rules() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE on_orders AS ON INSERT TO orders DO NOTHING;
         CREATE RULE on_audit AS ON INSERT TO audit DO NOTHING;
         ALTER TABLE orders RENAME TO purchases;"
    ))
    .expect("schema builds");

    let rule = rule_named(&database, "on_orders");
    assert_eq!(rule.target_relation_name().to_string(), "purchases");
    assert_eq!(relation_of(&database, rule), ("table", "purchases".to_string()));
    assert_eq!(rule_named(&database, "on_audit").target_relation_name().to_string(), "audit");
}

#[test]
fn renaming_the_view_carries_its_rules() {
    let database = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id FROM orders;
         CREATE RULE on_recent AS ON DELETE TO recent DO INSTEAD NOTHING;
         CREATE RULE on_orders AS ON DELETE TO orders DO NOTHING;
         ALTER TABLE recent RENAME TO latest;"
    ))
    .expect("schema builds");

    let rule = rule_named(&database, "on_recent");
    assert_eq!(rule.target_relation_name().to_string(), "latest");
    assert_eq!(relation_of(&database, rule), ("view", "latest".to_string()));
    assert_eq!(rule_named(&database, "on_orders").target_relation_name().to_string(), "orders");
}

#[test]
fn renaming_a_column_rewrites_new_and_old_references() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON UPDATE TO orders WHERE OLD.total <> new.total
             DO ALSO INSERT INTO audit (id) VALUES (new.id);
         ALTER TABLE orders RENAME COLUMN total TO amount;
         ALTER TABLE orders RENAME COLUMN id TO order_id;"
    ))
    .expect("schema builds");

    let rule = only_rule(&database);
    assert_eq!(
        rule.condition().map(ToString::to_string).as_deref(),
        Some("OLD.amount <> new.amount")
    );
    // `audit (id)` names a column of audit, which the rename does not touch.
    assert_eq!(action_text(rule), "INSERT INTO audit (id) VALUES (new.order_id)");
}

#[test]
fn renaming_a_column_of_another_table_leaves_the_rule_alone() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON INSERT TO orders DO ALSO INSERT INTO audit VALUES (new.id);
         ALTER TABLE audit RENAME COLUMN id TO order_id;"
    ))
    .expect("schema builds");

    assert_eq!(action_text(only_rule(&database)), "INSERT INTO audit VALUES (new.id)");
}

/// A view column rename reaches the rules on that view only, so a rule on the
/// table under it keeps reading the table's column.
#[test]
fn renaming_a_view_column_rewrites_new_references() {
    let database = parse(&format!(
        "{ORDERS}CREATE VIEW recent AS SELECT id, total FROM orders;
         CREATE RULE on_recent AS ON INSERT TO recent
             DO INSTEAD INSERT INTO orders VALUES (new.id, new.total);
         CREATE RULE on_orders AS ON INSERT TO orders DO ALSO INSERT INTO audit VALUES (new.total);
         ALTER TABLE recent RENAME COLUMN total TO amount;"
    ))
    .expect("schema builds");

    assert_eq!(
        action_text(rule_named(&database, "on_recent")),
        "INSERT INTO orders VALUES (new.id, new.amount)"
    );
    assert_eq!(
        action_text(rule_named(&database, "on_orders")),
        "INSERT INTO audit VALUES (new.total)"
    );
}

/// Only a two-part `NEW.column` or `OLD.column` names the rule's row, so a
/// fully qualified column of another table is left alone.
#[test]
fn a_qualified_column_of_another_table_is_not_a_row_reference() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON INSERT TO orders
             DO ALSO INSERT INTO audit (id) SELECT public.audit.id FROM public.audit
                 WHERE public.audit.id = new.id;
         ALTER TABLE orders RENAME COLUMN id TO order_id;"
    ))
    .expect("schema builds");

    assert_eq!(
        action_text(only_rule(&database)),
        "INSERT INTO audit (id) SELECT public.audit.id FROM public.audit WHERE public.audit.id = new.order_id"
    );
}

#[test]
fn dropping_a_column_a_rule_reads_needs_cascade() {
    let error = parse(&format!(
        "{ORDERS}CREATE RULE r AS ON INSERT TO orders DO ALSO INSERT INTO audit VALUES (new.total);
         ALTER TABLE orders DROP COLUMN total;"
    ))
    .expect_err("the rule reads the column through NEW");

    assert!(
        matches!(
            &error,
            Error::ColumnReferenced { table_name, column_name }
                if table_name == "orders" && column_name == "total"
        ),
        "got {error:?}"
    );
}

#[test]
fn dropping_a_column_a_rule_reads_with_cascade_drops_the_rule() {
    let database = parse(&format!(
        "{ORDERS}CREATE RULE reads_total AS ON INSERT TO orders
             DO ALSO INSERT INTO audit VALUES (new.total);
         CREATE RULE reads_id AS ON INSERT TO orders DO ALSO INSERT INTO audit VALUES (new.id);
         ALTER TABLE orders DROP COLUMN total CASCADE;"
    ))
    .expect("CASCADE takes the rule along");

    assert_eq!(rule_names(&database), ["reads_id"]);
}
