//! Tests that temporary relations live in PostgreSQL's session schema.
//!
//! Every rule asserted here was measured against PostgreSQL 18.4 in Docker. A
//! temporary relation sits in `pg_temp`, which every relation lookup searches
//! before the path unless the path names it, so a temporary relation and a
//! permanent one may share a name and a bare name reaches the temporary one. A
//! view reading a temporary relation becomes temporary itself, and the
//! spellings that would mix the two kinds of storage are refused.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
    structs::View,
};
use sqlparser::{
    ast::{CreateTable, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn db(sql: &str) -> ParserDB {
    parse(sql).expect("the schema builds")
}

fn table<'db>(db: &'db ParserDB, schema: Option<&str>, name: &str) -> Option<&'db CreateTable> {
    let target = TargetName::new(name, false);
    let target = match schema {
        Some(schema) => target.with_schema(schema, false),
        None => target,
    };
    db.table_by_target(target, IdentifierCase::AsWritten).expect("unambiguous lookup")
}

fn view<'db>(db: &'db ParserDB, schema: Option<&str>, name: &str) -> Option<&'db View> {
    let target = TargetName::new(name, false);
    let target = match schema {
        Some(schema) => target.with_schema(schema, false),
        None => target,
    };
    db.view_by_target(target, IdentifierCase::AsWritten).expect("unambiguous lookup")
}

fn column_count(db: &ParserDB, table: &CreateTable) -> usize {
    table.columns(db).expect("in this database").count()
}

fn bare_table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.resolve_target_table(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the name resolves")
}

fn bare_view<'db>(db: &'db ParserDB, name: &str) -> &'db View {
    db.resolve_target_view(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the name resolves")
}

fn view_node(sql: &str) -> sqlparser::ast::CreateView {
    let mut statements = Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("parses");
    let Some(Statement::CreateView(node)) = statements.pop() else {
        panic!("expected a view declaration");
    };
    node
}

#[test]
fn every_temporary_spelling_lands_in_pg_temp() {
    for spelling in ["TEMP", "TEMPORARY", "LOCAL TEMPORARY", "GLOBAL TEMPORARY"] {
        let db = db(&format!("CREATE {spelling} TABLE docs (id INT);"));
        let docs = table(&db, Some("pg_temp"), "docs").expect("stored in pg_temp");
        assert!(docs.is_temporary(), "{spelling}");
        assert!(table(&db, None, "docs").is_none(), "{spelling} left a permanent table");
    }
}

#[test]
fn naming_pg_temp_creates_a_temporary_table() {
    for qualifier in ["pg_temp", "PG_TEMP"] {
        let db = db(&format!("CREATE TABLE {qualifier}.docs (id INT);"));
        let docs = table(&db, Some("pg_temp"), "docs").expect("stored in pg_temp");
        assert!(docs.is_temporary());
        assert!(docs.temporary, "the stored node says what the server holds");
    }
}

#[test]
fn a_quoted_upper_case_pg_temp_is_an_ordinary_absent_schema() {
    assert!(matches!(
        parse(r#"CREATE TABLE "PG_TEMP".docs (id INT);"#),
        Err(Error::SchemaNotFoundForRelation { .. })
    ));
}

#[test]
fn a_permanent_relation_is_not_temporary() {
    let db = db("CREATE TABLE docs (id INT); CREATE VIEW v AS SELECT id FROM docs;");
    assert!(!table(&db, None, "docs").expect("stored").is_temporary());
    assert!(!view(&db, None, "v").expect("stored").is_temporary());
}

#[test]
fn a_temporary_relation_refuses_an_ordinary_schema() {
    for sql in [
        "CREATE SCHEMA app; CREATE TEMP TABLE app.docs (id INT);",
        "CREATE SCHEMA app; CREATE TEMP VIEW app.v AS SELECT 1 AS one;",
        "CREATE TEMP VIEW public.v AS SELECT 1 AS one;",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::TemporaryRelationInPermanentSchema { .. })),
            "{sql} was accepted"
        );
    }
}

#[test]
fn an_absent_schema_is_reported_before_the_temporary_one() {
    assert!(matches!(
        parse("CREATE TEMP TABLE nope.docs (id INT);"),
        Err(Error::SchemaNotFoundForRelation { .. })
    ));
}

#[test]
fn a_path_starting_with_pg_temp_creates_temporary_relations() {
    let db = db("SET search_path TO pg_temp, public;
         CREATE TABLE docs (id INT);
         CREATE VIEW v AS SELECT 1 AS one;
         CREATE MATERIALIZED VIEW m AS SELECT 1 AS one;");
    assert!(table(&db, Some("pg_temp"), "docs").expect("temporary table").is_temporary());
    assert!(view(&db, Some("pg_temp"), "v").expect("temporary view").is_temporary());
    let m = db
        .materialized_view_by_target(
            TargetName::new("m", false).with_schema("pg_temp", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("temporary materialized view");
    assert!(m.is_temporary());
}

#[test]
fn a_path_naming_pg_temp_later_creates_in_the_earlier_schema() {
    let db = db("CREATE SCHEMA app;
         SET search_path TO app, pg_temp;
         CREATE TABLE docs (id INT);");
    assert!(table(&db, Some("app"), "docs").is_some());
}

#[test]
fn a_temporary_and_a_permanent_relation_share_a_name() {
    let db = db("CREATE TABLE docs (id INT);
         CREATE TEMP TABLE docs (id INT, note TEXT);
         CREATE VIEW v AS SELECT 1 AS one;
         CREATE TEMP VIEW v AS SELECT 1 AS one, 2 AS two;
         CREATE TABLE t (id INT);
         CREATE TEMP VIEW t AS SELECT 1 AS one;");
    assert_eq!(column_count(&db, table(&db, None, "docs").expect("permanent")), 1);
    assert_eq!(column_count(&db, table(&db, Some("pg_temp"), "docs").expect("temporary")), 2);
    assert!(view(&db, None, "v").is_some());
    assert!(view(&db, Some("pg_temp"), "v").is_some());
    assert!(table(&db, None, "t").is_some());
    assert!(view(&db, Some("pg_temp"), "t").is_some());
}

#[test]
fn a_bare_name_reaches_the_temporary_relation_first() {
    let db = db("CREATE TABLE docs (id INT);
         CREATE TEMP TABLE docs (id INT, note TEXT);
         CREATE VIEW v AS SELECT 1 AS one;
         CREATE TEMP VIEW v AS SELECT 1 AS one, 2 AS two;");
    assert!(bare_table(&db, "docs").is_temporary());
    assert!(bare_view(&db, "v").is_temporary());
}

#[test]
fn a_path_naming_pg_temp_searches_it_at_that_position() {
    let db = db("CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE TEMP TABLE docs (id INT, note TEXT);
         SET search_path TO app, pg_temp;");
    assert!(!bare_table(&db, "docs").is_temporary());
}

#[test]
fn two_temporary_relations_still_share_one_pool() {
    assert!(matches!(
        parse("CREATE TEMP TABLE t (id INT); CREATE TEMP VIEW t AS SELECT 1 AS one;"),
        Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::View,
            conflicting_kind: ObjectKind::Table,
            ..
        })
    ));
}

#[test]
fn a_view_reading_a_temporary_relation_becomes_temporary() {
    for definition in [
        "SELECT id FROM t",
        "SELECT id FROM pg_temp.t",
        "SELECT 1 AS one WHERE EXISTS (SELECT 1 FROM t)",
        "SELECT id FROM tv",
    ] {
        let db = db(&format!(
            "CREATE TEMP TABLE t (id INT);
             CREATE TEMP VIEW tv AS SELECT id FROM t;
             CREATE VIEW v AS {definition};"
        ));
        assert!(view(&db, None, "v").is_none(), "{definition} left a permanent view");
        assert!(view(&db, Some("pg_temp"), "v").expect("promoted").is_temporary(), "{definition}");
    }
}

#[test]
fn a_with_name_shadowing_a_temporary_relation_does_not_promote() {
    let db = db("CREATE TEMP TABLE t (id INT);
         CREATE VIEW v AS WITH t AS (SELECT 1 AS id) SELECT id FROM t;");
    assert!(!view(&db, None, "v").expect("permanent").is_temporary());
}

#[test]
fn a_qualified_read_of_the_permanent_relation_does_not_promote() {
    let db = db("CREATE TABLE t (id INT);
         CREATE TEMP TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM public.t;");
    assert!(!view(&db, None, "v").expect("permanent").is_temporary());
}

#[test]
fn a_promoted_view_refuses_an_ordinary_schema() {
    assert!(matches!(
        parse("CREATE TEMP TABLE t (id INT); CREATE VIEW public.v AS SELECT id FROM t;"),
        Err(Error::TemporaryRelationInPermanentSchema { object_kind: ObjectKind::View, .. })
    ));
}

#[test]
fn a_temporary_materialized_view_spelling_is_refused() {
    assert!(matches!(
        parse("CREATE TEMP MATERIALIZED VIEW m AS SELECT 1 AS one;"),
        Err(Error::TemporaryMaterializedView { .. })
    ));
}

#[test]
fn a_materialized_view_cannot_read_a_temporary_relation() {
    for sql in [
        "CREATE TEMP TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
        "CREATE TEMP VIEW t AS SELECT 1 AS id; CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
        "CREATE TEMP TABLE t (id INT); CREATE MATERIALIZED VIEW pg_temp.m AS SELECT id FROM t;",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::MaterializedViewReadsTemporaryRelation { .. })),
            "{sql} was accepted"
        );
    }
}

#[test]
fn materialized_if_not_exists_checks_the_creation_schema() {
    let db = db("CREATE TEMP TABLE m (id INT);
         CREATE MATERIALIZED VIEW IF NOT EXISTS m AS SELECT 1 AS one;");
    assert!(
        db.materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_some()
    );
}

#[test]
fn a_replacement_stays_within_its_own_storage() {
    let beside_temporary = db("CREATE TEMP VIEW v AS SELECT 1 AS one;
         CREATE OR REPLACE VIEW v AS SELECT 2 AS other;");
    assert!(view(&beside_temporary, None, "v").is_some());
    assert!(view(&beside_temporary, Some("pg_temp"), "v").is_some());

    let beside_permanent = db("CREATE VIEW v AS SELECT 1 AS one;
         CREATE OR REPLACE TEMP VIEW v AS SELECT 2 AS other;");
    assert!(view(&beside_permanent, None, "v").is_some());
    assert!(view(&beside_permanent, Some("pg_temp"), "v").is_some());

    let promoted = db("CREATE TEMP TABLE t (id INT);
         CREATE VIEW v AS SELECT 1 AS id;
         CREATE OR REPLACE VIEW v AS SELECT id FROM t;");
    assert!(view(&promoted, None, "v").is_some());
    assert!(view(&promoted, Some("pg_temp"), "v").is_some());

    let replaced = db("CREATE TEMP VIEW v AS SELECT 1 AS one;
         CREATE OR REPLACE VIEW pg_temp.v AS SELECT 1 AS one, 2 AS two;");
    assert_eq!(replaced.views().count(), 1);
    assert_eq!(
        view(&replaced, Some("pg_temp"), "v").expect("replaced").definition().to_string(),
        "SELECT 1 AS one, 2 AS two"
    );
}

#[test]
fn a_replacement_reaches_only_the_schema_it_creates_in() {
    let db = db("CREATE SCHEMA app;
         CREATE VIEW app.v AS SELECT 1 AS one;
         SET search_path TO public, app;
         CREATE OR REPLACE VIEW v AS SELECT 2 AS other;");
    assert!(view(&db, None, "v").is_some());
    assert!(view(&db, Some("app"), "v").is_some());
}

#[test]
fn a_bare_drop_takes_the_temporary_relation() {
    let bare = db("CREATE TABLE t (id INT);
         CREATE TEMP TABLE t (id INT);
         DROP TABLE t;
         CREATE VIEW v AS SELECT 1 AS one;
         CREATE TEMP VIEW v AS SELECT 1 AS one;
         DROP VIEW v;");
    assert!(table(&bare, None, "t").is_some());
    assert!(table(&bare, Some("pg_temp"), "t").is_none());
    assert!(view(&bare, None, "v").is_some());
    assert!(view(&bare, Some("pg_temp"), "v").is_none());

    let qualified =
        db("CREATE TABLE t (id INT); CREATE TEMP TABLE t (id INT); DROP TABLE public.t;");
    assert!(table(&qualified, None, "t").is_none());
    assert!(table(&qualified, Some("pg_temp"), "t").is_some());
}

#[test]
fn a_bare_rename_takes_the_temporary_relation() {
    let db = db("CREATE TABLE t (id INT);
         CREATE TEMP TABLE t (id INT);
         ALTER TABLE t RENAME TO u;");
    assert!(table(&db, None, "t").is_some());
    assert!(table(&db, Some("pg_temp"), "u").is_some());
}

#[test]
fn a_temporary_rename_checks_only_the_temporary_pool() {
    let beside_permanent = db("CREATE TABLE u (id INT);
         CREATE TEMP TABLE t (id INT);
         ALTER TABLE t RENAME TO u;");
    assert!(table(&beside_permanent, None, "u").is_some());
    assert!(table(&beside_permanent, Some("pg_temp"), "u").is_some());

    assert!(
        parse(
            "CREATE TEMP TABLE t (id INT);
             CREATE TEMP TABLE u (id INT);
             ALTER TABLE t RENAME TO u;"
        )
        .is_err()
    );
}

#[test]
fn indexes_on_temporary_and_permanent_tables_share_a_name() {
    let two = "CREATE TABLE p (id INT);
         CREATE TEMP TABLE t (id INT);
         CREATE INDEX i ON p (id);
         CREATE INDEX i ON t (id);";
    let db_two = db(two);
    assert_eq!(db_two.indexes().count(), 2);

    let dropped = db(&format!("{two} DROP INDEX i;"));
    let remaining: Vec<_> = dropped.indexes().collect();
    assert_eq!(remaining.len(), 1);
    assert!(!IndexLike::table(remaining[0], &dropped).is_temporary());

    let renamed = db(&format!("{two} ALTER INDEX i RENAME TO j;"));
    for index in renamed.indexes() {
        let on_temporary = IndexLike::table(index, &renamed).is_temporary();
        assert_eq!(index.name(), Some(if on_temporary { "j" } else { "i" }));
    }
}

#[test]
fn a_foreign_key_never_crosses_storage() {
    for sql in [
        "CREATE TEMP TABLE p (id INT PRIMARY KEY); CREATE TABLE c (id INT REFERENCES p);",
        "CREATE TABLE p (id INT PRIMARY KEY); CREATE TEMP TABLE c (id INT REFERENCES p);",
        "CREATE TABLE p (id INT PRIMARY KEY);
         CREATE TEMP TABLE c (id INT);
         ALTER TABLE c ADD CONSTRAINT fk FOREIGN KEY (id) REFERENCES public.p (id);",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::ForeignKeyCrossesTemporaryStorage { .. })),
            "{sql} was accepted"
        );
    }
    let both_temporary = db("CREATE TEMP TABLE p (id INT PRIMARY KEY);
         CREATE TEMP TABLE c (id INT REFERENCES p);");
    let child = table(&both_temporary, Some("pg_temp"), "c").expect("stored");
    let key = child.foreign_keys(&both_temporary).expect("in db").next().expect("one key");
    assert!(key.referenced_table(&both_temporary).expect("resolves").is_temporary());
}

#[test]
fn a_bare_drop_reaching_another_kind_in_pg_temp_is_refused() {
    for (sql, expected_kind, actual_kind) in [
        (
            "CREATE TEMP TABLE v (id INT); CREATE VIEW v AS SELECT 1 AS one; DROP VIEW v;",
            ObjectKind::View,
            ObjectKind::Table,
        ),
        (
            "CREATE TEMP VIEW v AS SELECT 1 AS one; CREATE TABLE v (id INT); DROP TABLE v;",
            ObjectKind::Table,
            ObjectKind::View,
        ),
        (
            "CREATE TEMP TABLE m (id INT);
             CREATE MATERIALIZED VIEW m AS SELECT 1 AS one;
             DROP MATERIALIZED VIEW m;",
            ObjectKind::MaterializedView,
            ObjectKind::Table,
        ),
        (
            "CREATE TABLE other (id INT);
             CREATE INDEX i ON other (id);
             CREATE TEMP TABLE i (id INT);
             DROP INDEX i;",
            ObjectKind::Index,
            ObjectKind::Table,
        ),
    ] {
        let refused = parse(sql);
        assert!(
            matches!(
                refused,
                Err(Error::RelationKindMismatch { expected_kind: expected, actual_kind: actual, .. })
                    if expected == expected_kind && actual == actual_kind
            ),
            "{sql} gave {refused:?}"
        );
    }
}

#[test]
fn a_bare_table_statement_reaches_the_temporary_table_past_a_permanent_view() {
    let renamed = db("CREATE TEMP TABLE t (id INT);
         CREATE VIEW t AS SELECT 1 AS one;
         ALTER TABLE t RENAME TO u;");
    assert!(table(&renamed, Some("pg_temp"), "u").is_some());
    assert!(view(&renamed, None, "t").is_some());

    let dropped = db("CREATE TEMP TABLE t (id INT);
         CREATE VIEW t AS SELECT 1 AS one;
         DROP TABLE t;");
    assert!(table(&dropped, Some("pg_temp"), "t").is_none());
    assert!(view(&dropped, None, "t").is_some());
}

#[test]
fn a_table_target_shadowed_by_a_temporary_view_is_refused() {
    for sql in [
        "CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;
         CREATE TEMP VIEW t AS SELECT 1 AS one;
         CREATE TABLE t (id INT);
         CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION touch();",
        "CREATE TEMP VIEW t AS SELECT 1 AS id;
         CREATE TABLE t (id INT PRIMARY KEY);
         CREATE TABLE c (pid INT REFERENCES t (id));",
        "CREATE TEMP VIEW p AS SELECT 1 AS x; CREATE TABLE p (x INT); CREATE TABLE c () INHERITS (p);",
        "CREATE TEMP VIEW t AS SELECT 1 AS id; CREATE TABLE t (id INT); CREATE INDEX i ON t (id);",
        "CREATE TEMP VIEW t AS SELECT 1 AS id; CREATE TABLE t (id INT); CREATE POLICY p ON t USING (true);",
    ] {
        assert!(parse(sql).is_err(), "{sql} was accepted");
    }
}

#[test]
fn inheritance_never_makes_a_permanent_child_of_a_temporary_parent() {
    for sql in [
        "CREATE TEMP TABLE p (id INT); CREATE TABLE c () INHERITS (p);",
        "CREATE TEMP TABLE p (id INT) PARTITION BY RANGE (id);
         CREATE TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);",
    ] {
        assert!(
            matches!(parse(sql), Err(Error::PermanentRelationInheritsTemporary { .. })),
            "{sql} was accepted"
        );
    }
    assert!(matches!(
        parse(
            "CREATE TABLE p (id INT) PARTITION BY RANGE (id);
             CREATE TEMP TABLE c PARTITION OF p FOR VALUES FROM (1) TO (2);"
        ),
        Err(Error::TemporaryPartitionOfPermanent { .. })
    ));
    for sql in [
        "CREATE TABLE p (id INT); CREATE TEMP TABLE c () INHERITS (p);",
        "CREATE TEMP TABLE p (id INT); CREATE TEMP TABLE c () INHERITS (p);",
    ] {
        let db = db(sql);
        assert!(table(&db, Some("pg_temp"), "c").expect("temporary child").is_temporary());
    }
}

#[test]
fn a_temporary_view_keeps_a_permanent_table_from_being_dropped() {
    assert!(matches!(
        parse("CREATE TABLE t (id INT); CREATE TEMP VIEW v AS SELECT id FROM t; DROP TABLE t;"),
        Err(Error::RelationHasDependents { .. })
    ));
}

#[test]
fn a_declaration_read_on_its_own_tells_the_two_storages_apart() {
    let ordinary = View::from_node(&view_node("CREATE VIEW v AS SELECT 1 AS result"))
        .expect("usable declaration");
    let temporary = View::from_node(&view_node("CREATE TEMP VIEW v AS SELECT 1 AS result"))
        .expect("usable declaration");
    assert_ne!(ordinary, temporary);
    assert!(!ordinary.is_temporary());
    assert!(temporary.is_temporary());
    assert_eq!(temporary.declaration().schema(), Some("pg_temp"));
}
