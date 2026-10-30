//! Tests that a view is modelled as the relation PostgreSQL treats it as.
//!
//! Every rule asserted here was measured against PostgreSQL 18.4 in Docker
//! rather than read from documentation. Three of the measurements overturned
//! what the shape of the code would otherwise have assumed, and each has a test
//! of its own below: a view cycle is accepted at creation and only refuses on
//! read, so the resolver has to terminate on one; a materialized view has no
//! replace form at all while a plain view has no `IF NOT EXISTS` form, both of
//! which the parser accepts; and a materialized view's rows are a stored
//! snapshot, so it answers what a column's table is and refuses to say that its
//! rows are that table's rows.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

extern crate alloc;

use alloc::sync::Arc;

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
    structs::{MaterializedView, ParserDBBuilder, View, ViewMetadata},
    traits::Metadata,
};
use sqlparser::{
    ast::{CreateView, Query, SelectItem, SetExpr, Statement},
    dialect::{GenericDialect, PostgreSqlDialect},
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The table a reference resolves to inside a query, or `None` for no answer.
fn resolve(schema: &str, query: &str, reference: &str) -> Option<String> {
    let db = ParserDB::parse::<GenericDialect>(schema).expect("the schema parses");
    resolve_in_db(&db, query, reference)
}

fn resolve_in_db(db: &ParserDB, query: &str, reference: &str) -> Option<String> {
    let mut statements = Parser::parse_sql(&GenericDialect {}, query).expect("the query parses");
    let Statement::Query(query) = statements.pop().expect("one statement") else {
        panic!("expected a query");
    };
    let scope =
        ColumnScope::from_query(&query, db, IdentifierCase::AsWritten).expect("the scope builds");
    let mut wrapped = Parser::parse_sql(&GenericDialect {}, &format!("SELECT {reference}"))
        .expect("the reference parses");
    let Statement::Query(wrapped) = wrapped.pop().expect("one statement") else {
        panic!("expected a query");
    };
    let SetExpr::Select(select) = wrapped.body.as_ref() else {
        panic!("expected a SELECT");
    };
    let SelectItem::UnnamedExpr(expr) = &select.projection[0] else {
        panic!("expected an expression projection");
    };
    scope
        .resolve_column(expr)
        .expect("resolution answers")
        .map(|table| table.table_name().to_string())
}

/// The single table a query's output rows are rows of, or `None`.
fn row_source(schema: &str, query: &str) -> Option<String> {
    let db = ParserDB::parse::<GenericDialect>(schema).expect("the schema parses");
    let mut statements = Parser::parse_sql(&GenericDialect {}, query).expect("the query parses");
    let Statement::Query(query) = statements.pop().expect("one statement") else {
        panic!("expected a query");
    };
    query
        .projection_source_table(&db)
        .expect("the row-identity question answers")
        .map(|table| table.table_name().to_string())
}

fn view_node(sql: &str) -> CreateView {
    let mut statements =
        Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("the declaration parses");
    let Statement::CreateView(view) = statements.pop().expect("one statement") else {
        panic!("expected a view declaration");
    };
    view
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DefaultView {
    name: String,
    schema: Option<String>,
    definition: Query,
}

impl Metadata for DefaultView {
    type Meta = ();
}

impl ViewLike for DefaultView {
    type DB = ParserDB;

    fn view_name(&self) -> &str {
        &self.name
    }

    fn view_schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    fn is_materialized(&self) -> bool {
        false
    }

    fn definition(&self) -> &Query {
        &self.definition
    }

    fn declared_column_names(&self) -> &[(String, bool)] {
        &[]
    }
}

#[test]
fn default_view_name_helpers_fold_unquoted_parts() {
    let source =
        parse("CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t;").expect("schema");
    let definition = source
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("view")
        .definition()
        .clone();
    let qualified = DefaultView {
        name: "MyView".to_string(),
        schema: Some("MySchema".to_string()),
        definition,
    };

    assert!(!qualified.view_name_is_quoted());
    assert!(!qualified.view_schema_is_quoted());
    assert_eq!(qualified.stored_view_name().as_ref(), "myview");
    assert_eq!(qualified.stored_view_schema().as_deref(), Some("myschema"));
    let target = qualified.target_name();
    assert_eq!(target.name(), "MyView");
    assert_eq!(target.schema(), Some("MySchema"));
    assert!(!target.name_is_quoted());
    assert!(!target.schema_is_quoted());

    let bare = DefaultView { schema: None, ..qualified };
    assert!(bare.stored_view_schema().is_none());
    assert!(bare.target_name().schema().is_none());
}

#[test]
fn a_view_is_recorded_and_listed_apart_from_tables() {
    let db = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v AS SELECT a, b FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;",
    )
    .expect("all three are recorded");

    assert_eq!(db.tables().count(), 1);
    assert_eq!(db.views().count(), 1);
    assert_eq!(db.materialized_views().count(), 1);

    let view = db
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the view is found");
    assert_eq!(view.view_name(), "v");
    assert!(!view.is_materialized());
    assert_eq!(view.definition().to_string(), "SELECT a, b FROM t");

    assert!(
        db.materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("found")
            .is_materialized()
    );
    // Each lookup answers only its own kind.
    assert!(
        db.view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    assert!(
        db.materialized_view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    assert!(
        db.table_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
}

#[test]
fn bulk_builder_orders_both_view_kinds_and_terminates_a_snapshot_cycle() {
    let views = ["CREATE VIEW z AS SELECT 1 AS x", "CREATE VIEW a AS SELECT 1 AS x"].map(|sql| {
        (Arc::new(View::from_node(&view_node(sql)).expect("named view")), ViewMetadata::default())
    });
    let materialized_views = [
        "CREATE MATERIALIZED VIEW z_snapshot AS SELECT x FROM z_snapshot",
        "CREATE MATERIALIZED VIEW a_snapshot AS SELECT 1 AS x",
    ]
    .map(|sql| {
        (
            Arc::new(MaterializedView::from_node(&view_node(sql)).expect("named view")),
            ViewMetadata::default(),
        )
    });
    let empty = parse("").expect("empty schema");
    let db: ParserDB = ParserDBBuilder::new("test".to_string(), *empty.dialect())
        .add_views(views)
        .add_materialized_views(materialized_views)
        .into();

    assert_eq!(db.views().map(ViewLike::view_name).collect::<Vec<_>>(), ["a", "z"]);
    assert_eq!(
        db.materialized_views().map(ViewLike::view_name).collect::<Vec<_>>(),
        ["a_snapshot", "z_snapshot"]
    );
    assert_eq!(resolve_in_db(&db, "SELECT 1 FROM z_snapshot", "z_snapshot.x"), None);
}

#[test]
fn a_view_name_folds_and_quotes_like_a_table_name() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW My_View AS SELECT a FROM t;
         CREATE VIEW \"Other\" AS SELECT a FROM t;",
    )
    .expect("both are recorded");

    let folded = db
        .view_by_target(TargetName::new("my_view", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("an unquoted name folds down");
    assert_eq!(folded.view_name(), "My_View", "the raw spelling is preserved");
    assert_eq!(folded.stored_view_name(), "my_view", "the stored name is folded");
    // An unquoted lookup folds too, so either spelling of it finds the view,
    // while a quoted lookup has to match the stored name exactly.
    assert!(
        db.view_by_target(TargetName::new("My_View", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
    assert!(
        db.view_by_target(TargetName::new("my_view", true), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
    assert!(
        db.view_by_target(TargetName::new("My_View", true), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    assert!(
        db.view_by_target(TargetName::new("Other", true), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some(),
        "a quoted name keeps its case"
    );
    assert!(
        db.view_by_target(TargetName::new("other", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "and an unquoted lookup cannot reach it"
    );
}

#[test]
fn tables_and_both_view_kinds_share_one_pool_of_names() {
    // Measured: `CREATE VIEW t` fails with `relation "t" already exists`.
    let refused = parse("CREATE TABLE t (a INT); CREATE VIEW t AS SELECT 1;")
        .expect_err("the table already holds the name");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. }
            if *object_kind == ObjectKind::View && *conflicting_kind == ObjectKind::Table),
        "got {refused:?}"
    );

    let refused =
        parse("CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t; CREATE TABLE v (x INT);")
            .expect_err("the view already holds the name");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. }
            if *object_kind == ObjectKind::Table && *conflicting_kind == ObjectKind::View),
        "got {refused:?}"
    );

    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         CREATE VIEW m AS SELECT a FROM t;",
    )
    .expect_err("the materialized view already holds the name");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { conflicting_kind, .. }
            if *conflicting_kind == ObjectKind::MaterializedView),
        "got {refused:?}"
    );

    // The same name in another schema is fine.
    parse("CREATE SCHEMA s; CREATE TABLE t (a INT); CREATE VIEW s.t AS SELECT a FROM public.t;")
        .expect("another schema is another pool");
}

#[test]
fn the_search_path_decides_which_schema_a_view_lands_in() {
    let db = parse(
        "CREATE SCHEMA s;
         CREATE TABLE t (a INT);
         SET search_path TO s;
         CREATE VIEW v AS SELECT a FROM public.t;",
    )
    .expect("the path places the view");
    assert!(
        db.view_by_target(
            TargetName::new("v", false).with_schema("s", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );
    assert!(
        db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "it is not in the default schema"
    );

    let refused = parse("SET search_path TO nope; CREATE VIEW v AS SELECT 1;")
        .expect_err("the path names no creatable schema");
    assert!(
        matches!(&refused, Error::SchemaNotFoundForRelation { object_kind, relation_name, .. }
            if *object_kind == ObjectKind::View && relation_name == "v"),
        "got {refused:?}"
    );
}

#[test]
fn a_column_list_renames_what_the_definition_produces() {
    // Measured: the list applies positionally, a partial list keeps the tail's
    // own names, and more names than columns is refused at creation.
    let db = parse("CREATE TABLE t (a INT, b INT); CREATE VIEW v (x) AS SELECT a, b FROM t;")
        .expect("a partial list is accepted");
    let view = db
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("found");
    assert_eq!(view.declared_column_names(), &[("x".to_string(), false)]);

    let schema = "CREATE TABLE t (a INT, b INT); CREATE VIEW v (x, y) AS SELECT a, b FROM t;";
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.x").as_deref(), Some("t"));
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.y").as_deref(), Some("t"));
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.a"), None, "the original name is hidden");
}

/// Whether the view or materialized view `name` of `db` filled its output from
/// a wildcard.
fn selects_wildcard(db: &ParserDB, name: &str) -> bool {
    let target = || TargetName::new(name, false);
    match db.view_by_target(target(), IdentifierCase::AsWritten).expect("unambiguous lookup") {
        Some(view) => view.selects_wildcard(),
        None => {
            db.materialized_view_by_target(target(), IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
                .expect("found")
                .selects_wildcard()
        }
    }
}

#[test]
fn a_view_remembers_a_wildcard_its_creation_spelled_out() {
    // Measured on PostgreSQL 18.4: after `ADD COLUMN`, a view created over
    // `SELECT *`, `t.*` or `TABLE t` keeps only the columns that existed then.
    let db = parse(
        "CREATE TABLE people (id INT, name TEXT);
         CREATE VIEW star AS SELECT * FROM people;
         CREATE VIEW qualified AS SELECT p.* FROM people AS p;
         CREATE VIEW written AS SELECT id, name FROM people;
         CREATE VIEW arm AS SELECT id, name FROM people UNION SELECT * FROM people;
         CREATE VIEW nested_arm AS (SELECT id, name FROM people) UNION ((SELECT id, name FROM people) EXCEPT (SELECT p.* FROM people AS p));
         CREATE VIEW table_query AS TABLE people;
         CREATE VIEW over_cte AS WITH c AS (SELECT id FROM people) SELECT * FROM c;
         CREATE VIEW inside_cte AS WITH c AS (SELECT * FROM people) SELECT id FROM c;
         CREATE VIEW inside_from AS SELECT s.id FROM (SELECT * FROM people) AS s;
         CREATE VIEW inside_exists AS SELECT id FROM people WHERE EXISTS (SELECT * FROM people);
         CREATE VIEW count_star AS SELECT count(*) AS n FROM people;
         CREATE MATERIALIZED VIEW snapshot AS SELECT * FROM people;",
    )
    .expect("every view is recorded");

    let star = db
        .view_by_target(TargetName::new("star", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("found");
    assert_eq!(star.definition().to_string(), "SELECT id, name FROM people");

    for (name, expected) in [
        ("star", true),
        ("qualified", true),
        ("written", false),
        ("arm", true),
        ("nested_arm", true),
        ("table_query", true),
        ("over_cte", true),
        ("inside_cte", false),
        ("inside_from", false),
        ("inside_exists", false),
        ("count_star", false),
        ("snapshot", true),
    ] {
        assert_eq!(selects_wildcard(&db, name), expected, "{name}");
    }
}

#[test]
fn a_remembered_wildcard_follows_the_view_through_later_statements() {
    let db = parse(
        "CREATE TABLE people (id INT, name TEXT);
         CREATE VIEW kept AS SELECT * FROM people;
         CREATE VIEW replaced AS SELECT * FROM people;
         CREATE OR REPLACE VIEW replaced AS SELECT id, name FROM people;
         CREATE VIEW starred AS SELECT id, name FROM people;
         CREATE OR REPLACE VIEW starred AS SELECT * FROM people;
         ALTER TABLE people RENAME COLUMN name TO full_name;
         ALTER TABLE people RENAME TO persons;
         ALTER TABLE kept RENAME TO still_kept;
         ALTER TABLE still_kept RENAME COLUMN id TO person_id;",
    )
    .expect("every statement applies");

    assert!(selects_wildcard(&db, "still_kept"));
    assert!(!selects_wildcard(&db, "replaced"));
    assert!(selects_wildcard(&db, "starred"));
}

#[test]
fn the_default_reads_the_wildcard_off_the_definition() {
    let query = |sql: &str| {
        let Statement::Query(query) =
            Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("parses").pop().expect("one")
        else {
            panic!("expected a query");
        };
        *query
    };
    let view =
        |sql: &str| DefaultView { name: "v".to_string(), schema: None, definition: query(sql) };

    assert!(view("SELECT * FROM t").selects_wildcard());
    assert!(view("SELECT a FROM t UNION SELECT t.* FROM t").selects_wildcard());
    assert!(!view("SELECT a FROM t WHERE EXISTS (SELECT * FROM t)").selects_wildcard());
}

#[test]
fn replacing_a_view_may_only_add_columns_on_the_end() {
    // Measured: `cannot change name of view column`, `cannot drop columns from
    // view`, and appending succeeds.
    let refused = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v (x, y) AS SELECT a, b FROM t;
         CREATE OR REPLACE VIEW v (z, y) AS SELECT a, b FROM t;",
    )
    .expect_err("a rename is refused");
    assert!(
        matches!(&refused, Error::ViewColumnRenamedByReplace { existing_column, new_column, .. }
            if existing_column == "x" && new_column == "z"),
        "got {refused:?}"
    );

    let refused = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v (x, y) AS SELECT a, b FROM t;
         CREATE OR REPLACE VIEW v (x) AS SELECT a FROM t;",
    )
    .expect_err("a drop is refused");
    assert!(matches!(&refused, Error::ViewColumnsDroppedByReplace { .. }), "got {refused:?}");

    let db = parse(
        "CREATE TABLE t (a INT, b INT);
         CREATE VIEW v (x) AS SELECT a FROM t;
         CREATE OR REPLACE VIEW v (x, y) AS SELECT a, b FROM t;",
    )
    .expect("appending is accepted");
    assert_eq!(
        db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("found")
            .declared_column_names()
            .len(),
        2
    );
    assert_eq!(db.views().count(), 1, "the replacement takes the recorded view's place");
}

#[test]
fn a_replacement_keeps_the_recorded_owner() {
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT, b INT);
         CREATE VIEW v (x) AS SELECT a FROM t;
         ALTER TABLE v OWNER TO r;
         CREATE OR REPLACE VIEW v (x, y) AS SELECT a, b FROM t;",
    )
    .expect("the replacement is accepted");
    let view = db
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("found");
    assert_eq!(db.view_metadata(view).expect("metadata").owner(), Some("r"));
}

#[test]
fn the_two_spellings_postgres_refuses_are_refused() {
    // Measured: both are syntax errors on the server, and the parser accepts
    // both, so this crate has to refuse them itself.
    let refused =
        parse("CREATE TABLE t (a INT); CREATE OR REPLACE MATERIALIZED VIEW m AS SELECT a FROM t;")
            .expect_err("a materialized view has no replace form");
    assert!(matches!(&refused, Error::MaterializedViewCannotBeReplaced { .. }), "got {refused:?}");

    let refused = parse("CREATE VIEW IF NOT EXISTS v AS SELECT 1;")
        .expect_err("a plain view has no IF NOT EXISTS form");
    assert!(matches!(&refused, Error::ViewIfNotExistsUnsupported { .. }), "got {refused:?}");

    // The materialized form does take it, and skips when the name is held.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW IF NOT EXISTS m AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW IF NOT EXISTS m AS SELECT a FROM t;",
    )
    .expect("the second is a no-op");
    assert_eq!(db.materialized_views().count(), 1);
}

#[test]
fn the_drop_spellings_check_the_kind_they_name() {
    // Measured: each of these four is refused, each with its own hint.
    let cases = [
        ("DROP TABLE v;", ObjectKind::Table, ObjectKind::View),
        ("DROP MATERIALIZED VIEW v;", ObjectKind::MaterializedView, ObjectKind::View),
        ("DROP VIEW t;", ObjectKind::View, ObjectKind::Table),
        ("DROP VIEW m;", ObjectKind::View, ObjectKind::MaterializedView),
    ];
    let schema = "CREATE TABLE t (a INT);
                  CREATE VIEW v AS SELECT a FROM t;
                  CREATE MATERIALIZED VIEW m AS SELECT a FROM t;";
    for (statement, expected, actual) in cases {
        let refused = parse(&format!("{schema} {statement}"))
            .expect_err("the statement names the wrong kind");
        assert!(
            matches!(&refused, Error::RelationKindMismatch { expected_kind, actual_kind, .. }
                if *expected_kind == expected && *actual_kind == actual),
            "{statement} got {refused:?}"
        );
    }
}

#[test]
fn dropping_a_view_frees_its_name() {
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         DROP VIEW v;
         CREATE TABLE v (x INT);",
    )
    .expect("the name is free again");
    assert!(
        db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    assert!(
        db.table_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         DROP MATERIALIZED VIEW m;
         CREATE TABLE m (x INT);",
    )
    .expect("the materialized view name is free again");
    assert!(
        db.materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    assert!(
        db.table_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );

    let refused = parse("DROP VIEW nope;").expect_err("nothing holds the name");
    assert!(
        matches!(&refused, Error::RelationNotFound { object_kind, .. }
            if *object_kind == ObjectKind::View),
        "got {refused:?}"
    );
    parse("DROP VIEW IF EXISTS nope;").expect("IF EXISTS skips");
}

#[test]
fn a_view_can_be_renamed_and_handed_to_a_role() {
    // `ALTER VIEW` does not parse at all in the pinned parser, and
    // `ALTER TABLE <view>` is what the server accepts too.
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         ALTER TABLE v RENAME TO w;
         ALTER TABLE w OWNER TO r;",
    )
    .expect("both actions are accepted");
    assert!(
        db.view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    let view = db
        .view_by_target(TargetName::new("w", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("renamed");
    assert_eq!(db.view_metadata(view).expect("metadata").owner(), Some("r"));

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         ALTER TABLE m RENAME TO n;
         ALTER TABLE n OWNER TO CURRENT_USER;",
    )
    .expect("a materialized view can be renamed and handed to the current user");
    assert!(
        db.materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none()
    );
    let view = db
        .materialized_view_by_target(TargetName::new("n", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("renamed");
    assert_eq!(db.materialized_view_metadata(view).expect("metadata").owner(), None);

    let refused = parse(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE v RENAME TO t;",
    )
    .expect_err("the table holds the new name");
    assert!(matches!(&refused, Error::RelationNameAlreadyTaken { .. }), "got {refused:?}");

    let refused = parse(
        "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t; ALTER TABLE v ADD COLUMN b INT;",
    )
    .expect_err("a view takes no columns");
    assert!(
        matches!(&refused, Error::AlterActionUnsupportedOnRelation { object_kind, .. }
            if *object_kind == ObjectKind::View),
        "got {refused:?}"
    );
}

#[test]
fn a_role_owning_a_view_cannot_be_dropped() {
    // Measured: `role "r" cannot be dropped because some objects depend on it`,
    // detailing the view it owns.
    for kind in ["VIEW", "MATERIALIZED VIEW"] {
        let refused = parse(&format!(
            "CREATE ROLE r;
             CREATE TABLE t (a INT);
             CREATE {kind} x AS SELECT a FROM t;
             ALTER TABLE x OWNER TO r;
             DROP ROLE r;"
        ))
        .expect_err("an owned view blocks the drop");
        assert!(matches!(&refused, Error::RoleReferenced { .. }), "{kind} got {refused:?}");
    }
}

#[test]
fn a_column_reference_through_a_view_answers_the_table_underneath() {
    // The reproduction the report carried, verbatim.
    let schema = "CREATE TABLE ownable_owners(ownable_id INT, owner_id INT); \
                  CREATE VIEW ownable_owners_unfiltered AS \
                  SELECT ownable_id, owner_id FROM ownable_owners;";
    assert_eq!(
        resolve(
            schema,
            "SELECT 1 FROM ownable_owners_unfiltered oo WHERE oo.ownable_id = 1",
            "oo.ownable_id",
        )
        .as_deref(),
        Some("ownable_owners"),
    );
    // Bare, and through the view's own name rather than an alias.
    assert_eq!(
        resolve(schema, "SELECT 1 FROM ownable_owners_unfiltered", "owner_id").as_deref(),
        Some("ownable_owners"),
    );
    assert_eq!(
        resolve(
            schema,
            "SELECT 1 FROM ownable_owners_unfiltered",
            "ownable_owners_unfiltered.owner_id",
        )
        .as_deref(),
        Some("ownable_owners"),
    );
}

#[test]
fn a_computed_view_column_answers_nothing() {
    let schema = "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT count(*) AS n FROM t;";
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.n"), None);
}

#[test]
fn too_many_declared_names_leave_a_view_opaque() {
    let schema = "CREATE TABLE t (a INT); CREATE VIEW v (x, y) AS SELECT a FROM t;";
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.x"), None);
}

#[test]
fn a_reference_resolves_through_a_chain_of_views() {
    let schema = "CREATE TABLE t (a INT, b INT);
                  CREATE VIEW v1 AS SELECT a, b FROM t;
                  CREATE VIEW v2 AS SELECT a FROM v1;
                  CREATE VIEW v3 AS SELECT a FROM v2;";
    assert_eq!(resolve(schema, "SELECT 1 FROM v3", "v3.a").as_deref(), Some("t"));
    assert_eq!(resolve(schema, "SELECT 1 FROM v3 x", "x.a").as_deref(), Some("t"));

    // A schema-qualified chain resolves the same way.
    let qualified = "CREATE SCHEMA app;
                     CREATE TABLE t (a INT);
                     CREATE VIEW app.v1 AS SELECT a FROM t;
                     CREATE VIEW app.v2 AS SELECT a FROM app.v1;";
    assert_eq!(resolve(qualified, "SELECT 1 FROM app.v2", "v2.a").as_deref(), Some("t"));
}

#[test]
fn a_view_cycle_terminates_rather_than_recursing() {
    // The report claimed PostgreSQL rejects a cycle at creation. Measured on
    // 18.4, it does not: the statement succeeds and only reading the view
    // fails, with `infinite recursion detected in rules for relation "v"`. So a
    // cycle is representable here and the resolver has to stop on it.
    let self_referencing = "CREATE TABLE t (a INT);
                            CREATE VIEW v AS SELECT a FROM t;
                            CREATE OR REPLACE VIEW v AS SELECT a FROM v;";
    parse(self_referencing).expect("the server accepts this, so this crate records it");
    assert_eq!(resolve(self_referencing, "SELECT 1 FROM v", "v.a"), None);

    let mutual = "CREATE TABLE t (a INT);
                  CREATE VIEW v1 AS SELECT a FROM t;
                  CREATE VIEW v2 AS SELECT a FROM v1;
                  CREATE OR REPLACE VIEW v1 AS SELECT a FROM v2;";
    parse(mutual).expect("a two-view cycle is accepted too");
    assert_eq!(resolve(mutual, "SELECT 1 FROM v2", "v2.a"), None);
}

#[test]
fn a_from_alias_column_list_renames_a_view_on_top_of_its_own() {
    let schema = "CREATE TABLE t (a INT, b INT); CREATE VIEW v (x, y) AS SELECT a, b FROM t;";
    assert_eq!(resolve(schema, "SELECT 1 FROM v w(p, q)", "w.p").as_deref(), Some("t"));
    assert_eq!(resolve(schema, "SELECT 1 FROM v w(p, q)", "w.x"), None);
}

#[test]
fn a_view_joined_with_a_table_resolves_both_sides() {
    let schema = "CREATE TABLE t (a INT);
                  CREATE TABLE u (b INT);
                  CREATE VIEW v AS SELECT a FROM t;";
    let query = "SELECT 1 FROM v JOIN u ON v.a = u.b";
    assert_eq!(resolve(schema, query, "v.a").as_deref(), Some("t"));
    assert_eq!(resolve(schema, query, "u.b").as_deref(), Some("u"));
}

#[test]
fn row_identity_carries_through_a_view_and_stops_at_a_snapshot() {
    // Measured: a pass-through view accepts an insert, so its rows are the
    // table's rows. A materialized view accepts no write at all, and its rows
    // were produced when it was last populated, so they are nobody's current
    // rows. The declared type of a column is inherited either way.
    let schema = "CREATE TABLE t (a INT);
                  CREATE VIEW v AS SELECT a FROM t;
                  CREATE MATERIALIZED VIEW m AS SELECT a FROM t;";

    assert_eq!(row_source(schema, "SELECT v.a FROM v").as_deref(), Some("t"));
    assert_eq!(row_source(schema, "SELECT m.a FROM m"), None);

    // The type question answers through both.
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.a").as_deref(), Some("t"));
    assert_eq!(resolve(schema, "SELECT 1 FROM m", "m.a").as_deref(), Some("t"));

    // A view whose body collapses rows carries no row identity either.
    let grouped = "CREATE TABLE t (a INT);
                   CREATE VIEW g AS SELECT DISTINCT a FROM t;";
    assert_eq!(row_source(grouped, "SELECT g.a FROM g"), None);
    assert_eq!(resolve(grouped, "SELECT 1 FROM g", "g.a").as_deref(), Some("t"));
}

#[test]
fn a_grant_reaches_views_and_says_which_kind_each_target_is() {
    // Measured: granting on a view is ordinary, column grants on one work, and
    // `ALL TABLES IN SCHEMA` covers views as well as tables.
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         GRANT SELECT ON t, v, m TO r;",
    )
    .expect("a mixed target list is accepted");
    let grant = db.table_grants().next().expect("one grant");
    let kinds: Vec<&str> = grant
        .relations(&db)
        .map(|relation| {
            match relation {
                Relation::Table(_) => "table",
                Relation::View(_) => "view",
                Relation::MaterializedView(_) => "materialized view",
                Relation::Sequence(_) => "sequence",
            }
        })
        .collect();
    assert_eq!(kinds, vec!["table", "view", "materialized view"]);
    // The narrower question keeps answering exactly what it always did.
    assert_eq!(grant.tables(&db).count(), 1);

    parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         GRANT SELECT (a) ON v TO r;
         REVOKE SELECT (a) ON v FROM r;",
    )
    .expect("a column grant on a view is accepted, and revocable");

    let refused = parse("CREATE ROLE r; GRANT SELECT ON nope TO r;")
        .expect_err("a name no relation holds is still refused");
    assert!(matches!(&refused, Error::TableNotFoundForGrant { .. }), "got {refused:?}");
}

#[test]
fn a_schema_wide_grant_covers_every_relation_kind() {
    let db = parse(
        "CREATE ROLE r;
         CREATE SCHEMA s;
         CREATE TABLE s.t (a INT);
         CREATE VIEW s.v AS SELECT a FROM s.t;
         CREATE MATERIALIZED VIEW s.m AS SELECT a FROM s.t;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO r;",
    )
    .expect("the blanket grant is accepted");
    let grant = db.table_grants().next().expect("one grant");
    let mut kinds: Vec<&str> = grant
        .relations(&db)
        .map(|relation| {
            match relation {
                Relation::Table(_) => "table",
                Relation::View(_) => "view",
                Relation::MaterializedView(_) => "materialized view",
                Relation::Sequence(_) => "sequence",
            }
        })
        .collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec!["materialized view", "table", "view"]);
}

/// A relation created without a qualifier lives in `public`, so a blanket
/// grant on `public` reaches it as one on any other schema reaches its own.
/// A temporary relation lives in `pg_temp` and stays out.
#[test]
fn a_schema_wide_grant_on_public_covers_unqualified_relations() {
    for schema in ["public", "PUBLIC"] {
        let db = parse(&format!(
            "CREATE ROLE r;
             CREATE TABLE t (a INT);
             CREATE TABLE public.u (a INT);
             CREATE TEMP TABLE scratch (a INT);
             CREATE VIEW v AS SELECT a FROM t;
             CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
             GRANT SELECT ON ALL TABLES IN SCHEMA {schema} TO r;"
        ))
        .expect("the blanket grant is accepted");
        let grant = db.table_grants().next().expect("one grant");
        let mut tables: Vec<&str> = grant.tables(&db).map(TableLike::table_name).collect();
        tables.sort_unstable();
        assert_eq!(tables, ["t", "u"], "{schema}");
        // The plain and the materialized view are reached as well.
        assert_eq!(grant.relations(&db).count(), 4, "{schema}");
        let bare = db
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("the name is well formed")
            .expect("the table is recorded");
        assert!(grant.applies_to_table(bare, &db), "{schema}");
    }

    // A quoted spelling that keeps its case names another schema.
    let db = parse(
        "CREATE ROLE r;
         CREATE SCHEMA \"PUBLIC\";
         CREATE TABLE t (a INT);
         GRANT SELECT ON ALL TABLES IN SCHEMA \"PUBLIC\" TO r;",
    )
    .expect("the blanket grant is accepted");
    let grant = db.table_grants().next().expect("one grant");
    assert_eq!(grant.relations(&db).count(), 0);
}

#[test]
fn a_relation_a_view_reads_cannot_be_dropped_from_under_it() {
    // Measured: `cannot drop view v1 because other objects depend on it`, and
    // the same for a table a view reads. A view left naming a relation that is
    // gone is the dangling reference this refusal prevents.
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v1 AS SELECT a FROM t;
         CREATE VIEW v2 AS SELECT a FROM v1;
         DROP VIEW v1;",
    )
    .expect_err("v2 reads v1");
    assert!(
        matches!(&refused, Error::RelationHasDependents { object_kind, object_name, dependent_kind, dependent_name }
            if *object_kind == ObjectKind::View
                && object_name == "v1"
                && *dependent_kind == ObjectKind::View
                && dependent_name == "v2"),
        "got {refused:?}"
    );

    let refused = parse("CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t; DROP TABLE t;")
        .expect_err("v reads t");
    assert!(
        matches!(&refused, Error::RelationHasDependents { object_kind, dependent_kind, .. }
            if *object_kind == ObjectKind::Table && *dependent_kind == ObjectKind::View),
        "got {refused:?}"
    );

    // A materialized view reading a view blocks it just the same.
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM v;
         DROP VIEW v;",
    )
    .expect_err("m reads v");
    assert!(
        matches!(&refused, Error::RelationHasDependents { dependent_kind, .. }
            if *dependent_kind == ObjectKind::MaterializedView),
        "got {refused:?}"
    );

    // A view nothing reads drops freely.
    parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v1 AS SELECT a FROM t;
         CREATE VIEW v2 AS SELECT a FROM t;
         DROP VIEW v1;",
    )
    .expect("nothing reads v1");
}

#[test]
fn cascade_takes_the_whole_chain_of_readers() {
    // Measured: `DROP ... CASCADE` reports `drop cascades to view v2` and
    // removes the chain, transitively.
    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v1 AS SELECT a FROM t;
         CREATE VIEW v2 AS SELECT a FROM v1;
         CREATE VIEW v3 AS SELECT a FROM v2;
         DROP VIEW v1 CASCADE;",
    )
    .expect("cascade is accepted");
    assert_eq!(db.views().count(), 0, "the whole chain went");

    let db = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v1 AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM v1;
         DROP TABLE t CASCADE;",
    )
    .expect("cascade is accepted");
    assert_eq!(db.tables().count(), 0);
    assert_eq!(db.views().count(), 0);
    assert_eq!(db.materialized_views().count(), 0);
}

#[test]
fn a_name_a_view_only_binds_itself_is_not_a_dependency() {
    // Measured: a view writing `WITH t AS (...) SELECT ... FROM t` reads its
    // own item, not a table called `t`, so PostgreSQL allows dropping that
    // table and the view keeps working. Refusing here would refuse valid input,
    // which is worse than missing a dependency.
    parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (b INT);
         CREATE VIEW v AS WITH t AS (SELECT b FROM u) SELECT b FROM t;
         DROP TABLE t;",
    )
    .expect("the view reads its own item");

    // A derived-table alias is not a relation name either.
    parse(
        "CREATE TABLE t (a INT);
         CREATE TABLE u (b INT);
         CREATE VIEW v AS SELECT b FROM (SELECT b FROM u) AS t;
         DROP TABLE t;",
    )
    .expect("an alias is not a relation name");

    // The same name in another schema is a different relation.
    parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.t (a INT);
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM s.t;
         DROP TABLE public.t;",
    )
    .expect("the view reads s.t, not public.t");
}

#[test]
fn a_cyclic_view_can_still_be_dropped() {
    // A cycle must not wedge the dependency walk: a view reading itself does
    // not block its own drop.
    parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE OR REPLACE VIEW v AS SELECT a FROM v;
         DROP VIEW v;",
    )
    .expect("a self-referencing view drops");

    parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v1 AS SELECT a FROM t;
         CREATE VIEW v2 AS SELECT a FROM v1;
         CREATE OR REPLACE VIEW v1 AS SELECT a FROM v2;
         DROP VIEW v1 CASCADE;",
    )
    .expect("a mutual cycle drops under cascade");
}

#[test]
fn renaming_a_role_carries_a_view_owner_with_it() {
    // Measured: the server repoints the view's owner, and then refuses to drop
    // the renamed role while the view still names it. Leaving the old name on
    // the view would let the role be dropped and a fresh role of the old name
    // silently inherit the view.
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
         ALTER TABLE v OWNER TO r;
         ALTER TABLE m OWNER TO r;
         ALTER ROLE r RENAME TO r2;",
    )
    .expect("the rename is accepted");
    let view = db
        .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("found");
    assert_eq!(db.view_metadata(view).expect("metadata").owner(), Some("r2"));
    let materialized = db
        .materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("found");
    assert_eq!(db.materialized_view_metadata(materialized).expect("metadata").owner(), Some("r2"),);

    let refused = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         ALTER TABLE v OWNER TO r;
         ALTER ROLE r RENAME TO r2;
         DROP ROLE r2;",
    )
    .expect_err("the renamed role still owns the view");
    assert!(matches!(&refused, Error::RoleReferenced { .. }), "got {refused:?}");
}

#[test]
fn a_schema_holding_only_a_view_is_not_empty() {
    // Measured: `cannot drop schema s because other objects depend on it`,
    // detailing the view.
    let refused = parse(
        "CREATE SCHEMA s;
         CREATE TABLE t (a INT);
         CREATE VIEW s.v AS SELECT a FROM t;
         DROP SCHEMA s;",
    )
    .expect_err("the schema holds a view");
    assert!(matches!(&refused, Error::SchemaNotEmpty { .. }), "got {refused:?}");

    let refused = parse(
        "CREATE SCHEMA s;
         CREATE TABLE t (a INT);
         CREATE MATERIALIZED VIEW s.m AS SELECT a FROM t;
         DROP SCHEMA s;",
    )
    .expect_err("the schema holds a materialized view");
    assert!(matches!(&refused, Error::SchemaNotEmpty { .. }), "got {refused:?}");

    parse(
        "CREATE SCHEMA s;
         CREATE TABLE t (a INT);
         CREATE VIEW s.v AS SELECT a FROM t;
         DROP SCHEMA s CASCADE;",
    )
    .expect("cascade empties it");
}

#[test]
fn a_statement_needing_a_table_names_the_view_it_found_instead() {
    // Measured: the server says `"v" is not a table` rather than claiming the
    // relation is absent. Before views were modelled, "not found" was accurate.
    // Now the name exists and only the kind is wrong.
    let schema = "CREATE TABLE t (a INT PRIMARY KEY); CREATE VIEW v AS SELECT a FROM t;";
    let cases = [
        format!("{schema} CREATE POLICY pol ON v USING (true);"),
        format!(
            "{schema}
             CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN END' LANGUAGE plpgsql;
             CREATE TRIGGER tg INSTEAD OF INSERT ON v FOR EACH ROW EXECUTE FUNCTION f();"
        ),
        format!("{schema} CREATE TABLE fk (x INT REFERENCES v(a));"),
    ];
    for sql in cases {
        let refused = parse(&sql).expect_err("a view cannot stand in for a table here");
        assert!(
            matches!(&refused, Error::RelationKindMismatch { expected_kind, actual_kind, object_name }
                if *expected_kind == ObjectKind::Table
                    && *actual_kind == ObjectKind::View
                    && object_name == "v"),
            "got {refused:?}"
        );
    }
}

#[test]
fn an_index_and_a_view_cannot_share_a_name() {
    // Measured: `relation "v" already exists` in both directions, including the
    // index a named UNIQUE constraint creates behind it.
    let refused =
        parse("CREATE TABLE t (a INT); CREATE VIEW v AS SELECT a FROM t; CREATE INDEX v ON t(a);")
            .expect_err("the view holds the name");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. }
            if *object_kind == ObjectKind::Index && *conflicting_kind == ObjectKind::View),
        "got {refused:?}"
    );

    let refused =
        parse("CREATE TABLE t (a INT); CREATE INDEX i ON t(a); CREATE VIEW i AS SELECT a FROM t;")
            .expect_err("the index holds the name");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, .. }
            if *object_kind == ObjectKind::View && *conflicting_kind == ObjectKind::Index),
        "got {refused:?}"
    );

    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE TABLE c (x INT, CONSTRAINT v UNIQUE (x));",
    )
    .expect_err("a constraint-backed index name collides too");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { conflicting_kind, .. }
            if *conflicting_kind == ObjectKind::View),
        "got {refused:?}"
    );
}

#[test]
fn a_table_cannot_be_renamed_onto_a_view_name() {
    let refused = parse(
        "CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE TABLE other (x INT);
         ALTER TABLE other RENAME TO v;",
    )
    .expect_err("the view holds the name");
    assert!(
        matches!(&refused, Error::RelationNameAlreadyTaken { conflicting_kind, .. }
            if *conflicting_kind == ObjectKind::View),
        "got {refused:?}"
    );
}

#[test]
fn dropping_a_granted_view_leaves_the_grant_unresolved() {
    // The house pattern for a reference whose target left: the parse succeeds
    // and the dangling reference is reported on demand, exactly as it is for a
    // renamed table.
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         GRANT SELECT ON v TO r;
         DROP VIEW v;",
    )
    .expect("the drop is accepted");
    assert_eq!(db.unresolved_access_references().expect("the walk answers").count(), 1);
}

#[test]
fn a_view_over_an_unenumerable_relation_answers_nothing() {
    // A table function's columns are not knowable from the SQL text, so a view
    // reading one stays opaque rather than claiming columns. PostgreSQL does
    // resolve such a reference; this is the same conservatism a CTE over a
    // table function already has, and it never invents an answer.
    let schema = "CREATE TABLE t (a INT);
                  CREATE VIEW vf AS SELECT * FROM generate_series(1, 3) AS g(n);";
    assert_eq!(resolve(schema, "SELECT 1 FROM vf", "vf.n"), None);
}

#[test]
fn a_view_over_a_snapshot_carries_no_row_identity() {
    // Measured: PostgreSQL refuses to insert through a view reading a
    // materialized view, because its rows are not a live relation's rows. The
    // non-preserving shape has to travel outwards through the reading view.
    let schema = "CREATE TABLE t (a INT);
                  CREATE MATERIALIZED VIEW m AS SELECT a FROM t;
                  CREATE VIEW v AS SELECT a FROM m;";
    assert_eq!(row_source(schema, "SELECT v.a FROM v"), None);
    // The declared type still resolves through both levels.
    assert_eq!(resolve(schema, "SELECT 1 FROM v", "v.a").as_deref(), Some("t"));
}

#[test]
fn a_with_item_shadows_a_view_of_the_same_name() {
    // Measured: the item wins, as it does over a table of the same name.
    let schema = "CREATE TABLE t (a INT);
                  CREATE TABLE u (b INT);
                  CREATE VIEW v AS SELECT a FROM t;";
    assert_eq!(
        resolve(schema, "WITH v AS (SELECT b FROM u) SELECT 1 FROM v", "v.b").as_deref(),
        Some("u"),
        "the item shadows the view",
    );
    assert_eq!(resolve(schema, "WITH v AS (SELECT b FROM u) SELECT 1 FROM v", "v.a"), None);
}

#[test]
fn a_view_the_search_path_placed_answers_its_bare_name() {
    // A view created without a qualifier while the path selected another schema
    // is stored there, and every statement naming it bare afterwards has to
    // resolve the same way a table lookup does. Comparing the written qualifier
    // against the stored one instead makes such a view unreachable: it cannot
    // be granted on, renamed, given an owner, or dropped.
    let placed = "CREATE ROLE r;
                  CREATE SCHEMA s;
                  CREATE TABLE s.t (a INT);
                  SET search_path TO s;
                  CREATE VIEW v AS SELECT a FROM t;";

    let db = parse(&format!("{placed} GRANT SELECT ON v TO r;"))
        .expect("a grant naming it bare resolves");
    assert_eq!(db.table_grants().count(), 1);

    let db = parse(&format!("{placed} ALTER TABLE v RENAME TO w;")).expect("a rename resolves");
    assert!(
        db.view_by_target(
            TargetName::new("v", false).with_schema("s", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_none()
    );
    assert!(
        db.view_by_target(
            TargetName::new("w", false).with_schema("s", false),
            IdentifierCase::AsWritten
        )
        .expect("unambiguous lookup")
        .is_some()
    );

    let db = parse(&format!("{placed} ALTER TABLE v OWNER TO r;")).expect("an owner resolves");
    let view = db
        .view_by_target(
            TargetName::new("v", false).with_schema("s", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("found");
    assert_eq!(db.view_metadata(view).expect("metadata").owner(), Some("r"));

    let db = parse(&format!("{placed} DROP VIEW v;")).expect("a drop resolves");
    assert_eq!(db.views().count(), 0);

    let refused = parse(&format!("{placed} CREATE POLICY pol ON v USING (true);"))
        .expect_err("a policy still cannot name a view");
    assert!(
        matches!(&refused, Error::RelationKindMismatch { actual_kind, .. }
            if *actual_kind == ObjectKind::View),
        "got {refused:?}"
    );
}

#[test]
fn a_bare_name_inside_a_definition_resolves_where_the_view_was_created() {
    // The definition of a view the path placed in `s` reads `s.t` when it wrote
    // a bare `t`, so dropping `s.t` has to see the dependency. Reading the bare
    // name as the default schema's would miss it.
    let refused = parse(
        "CREATE SCHEMA s;
         CREATE TABLE s.t (a INT);
         SET search_path TO s;
         CREATE VIEW v1 AS SELECT a FROM t;
         CREATE VIEW v2 AS SELECT a FROM v1;
         DROP VIEW v1;",
    )
    .expect_err("v2 reads v1");
    assert!(matches!(&refused, Error::RelationHasDependents { .. }), "got {refused:?}");

    // When the view's own schema holds nothing of that name, the bare name
    // reaches the default schema, and the dependency is seen there instead.
    let refused = parse(
        "CREATE SCHEMA s;
         CREATE TABLE t (a INT);
         SET search_path TO s, public;
         CREATE VIEW v AS SELECT a FROM t;
         RESET search_path;
         DROP TABLE t;",
    )
    .expect_err("the view reads public.t");
    assert!(matches!(&refused, Error::RelationHasDependents { .. }), "got {refused:?}");
}

#[test]
fn a_column_grant_names_the_kind_of_relation_it_covers() {
    // `table` answers the narrow question and cannot tell a grant on a view
    // from a grant on nothing, since both answer nothing. The kind-tagged
    // answer is what a caller needing the target has to ask for.
    let db = parse(
        "CREATE ROLE r;
         CREATE TABLE t (a INT);
         CREATE VIEW v AS SELECT a FROM t;
         GRANT SELECT (a) ON v TO r;",
    )
    .expect("a column grant on a view is accepted");
    assert_eq!(
        db.unresolved_access_references().expect("the walk answers").count(),
        0,
        "the existing view satisfies its grant",
    );
    let grant = db.column_grants().next().expect("one grant");
    assert!(grant.table(&db).is_none(), "the target is not a table");
    assert!(
        matches!(grant.relation(&db), Some(Relation::View(_))),
        "the kind-tagged answer names it",
    );

    // On a table both answer, and they agree.
    let db = parse("CREATE ROLE r; CREATE TABLE t (a INT); GRANT SELECT (a) ON t TO r;")
        .expect("a column grant on a table is accepted");
    let grant = db.column_grants().next().expect("one grant");
    assert_eq!(grant.table(&db).expect("a table target").table_name(), "t");
    assert!(matches!(grant.relation(&db), Some(Relation::Table(_))));
}

#[test]
fn resolving_through_a_deep_chain_of_views_stays_cheap_and_correct() {
    // The cycle guard carries the chain on the stack rather than copying it per
    // reference. This exercises a chain deep enough that a per-reference copy
    // would show up, and pins that the answer is still the base table.
    use core::fmt::Write;

    let mut schema = String::from("CREATE TABLE t (a INT); CREATE VIEW v0 AS SELECT a FROM t;");
    for level in 1..32 {
        write!(schema, " CREATE VIEW v{level} AS SELECT a FROM v{};", level - 1)
            .expect("writing to a String cannot fail");
    }
    assert_eq!(resolve(&schema, "SELECT 1 FROM v31", "v31.a").as_deref(), Some("t"));
}
