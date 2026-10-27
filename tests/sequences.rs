//! Tests that `CREATE SEQUENCE` records a sequence in the schema it lands in,
//! that the sequence takes its name from the pool tables, views and indexes
//! share, and that it leaves with `DROP SEQUENCE` and with its schema.
//!
//! Every expectation was measured against PostgreSQL 18.4.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// No sequence at all, for comparing against [`sequence_names`].
const NONE: [&str; 0] = [];

/// Every sequence the database holds, as `schema.name` with `public` for one
/// stored without a schema, sorted.
fn sequence_names(database: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> = database
        .sequences()
        .map(|sequence| {
            format!(
                "{}.{}",
                sequence.sequence_schema().unwrap_or("public"),
                sequence.sequence_name()
            )
        })
        .collect();
    names.sort();
    names
}

fn sequence<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Sequence {
    database
        .resolve_target_sequence(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("the name is well formed")
        .expect("the sequence is recorded")
}

#[test]
fn a_created_sequence_is_recorded_beside_the_grant_on_it() {
    let db = parse(
        "CREATE SEQUENCE ticket_seq;
         GRANT USAGE ON SEQUENCE ticket_seq TO PUBLIC;",
    )
    .expect("schema parses");

    assert_eq!(sequence_names(&db), ["public.ticket_seq"]);
    let ticket = sequence(&db, "ticket_seq");
    assert_eq!(ticket.sequence_schema(), None);
    assert!(!ticket.is_temporary());
    assert_eq!(ticket.options(), []);
    assert_eq!(db.table_grants().count(), 1);
}

#[test]
fn the_written_options_are_kept_in_order() {
    let db = parse(
        "CREATE SEQUENCE s AS smallint INCREMENT BY 5 MINVALUE 1 NO MAXVALUE START WITH 10 \
         CACHE 3 CYCLE;",
    )
    .expect("schema parses");

    let options: Vec<String> =
        sequence(&db, "s").options().iter().map(ToString::to_string).collect();
    assert_eq!(
        options,
        [
            " AS SMALLINT",
            " INCREMENT BY 5",
            " MINVALUE 1",
            " NO MAXVALUE",
            " START WITH 10",
            " CACHE 3",
            " CYCLE"
        ]
    );
}

#[test]
fn a_sequence_lands_where_a_table_would() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.qualified;
         SET search_path = missing, app, public;
         CREATE SEQUENCE placed;
         CREATE TEMP SEQUENCE scratch;
         CREATE SEQUENCE \"Quoted\";",
    )
    .expect("schema parses");

    assert_eq!(
        sequence_names(&db),
        ["app.Quoted", "app.placed", "app.qualified", "pg_temp.scratch"]
    );
    let scratch = sequence(&db, "scratch");
    assert!(scratch.is_temporary());
    let quoted = db
        .sequence_by_target(
            TargetName::new("Quoted", true).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("the name is well formed")
        .expect("a quoted name keeps its case");
    assert_eq!(quoted.stored_sequence_name(), "Quoted");
    assert!(
        db.sequence_by_target(TargetName::new("quoted", false), IdentifierCase::AsWritten)
            .expect("the name is well formed")
            .is_none()
    );
}

#[test]
fn a_sequence_is_refused_where_a_table_would_be() {
    assert!(matches!(
        parse("CREATE SEQUENCE nope.s;"),
        Err(Error::SchemaNotFoundForRelation { object_kind: ObjectKind::Sequence, .. })
    ));
    assert!(matches!(
        parse("CREATE SCHEMA app; CREATE TEMP SEQUENCE app.s;"),
        Err(Error::TemporaryRelationInPermanentSchema { object_kind: ObjectKind::Sequence, .. })
    ));
}

#[test]
fn a_sequence_shares_the_relation_name_pool() {
    let taken = |sql: &str| {
        match parse(sql) {
            Err(Error::RelationNameAlreadyTaken { object_kind, conflicting_kind, object_name }) => {
                assert_eq!(object_name, "s");
                Some((object_kind, conflicting_kind))
            }
            other => panic!("expected a taken name, got {other:?}"),
        }
    };

    assert_eq!(
        taken("CREATE SEQUENCE s; CREATE SEQUENCE s;"),
        Some((ObjectKind::Sequence, ObjectKind::Sequence))
    );
    assert_eq!(
        taken("CREATE SEQUENCE s; CREATE TABLE s (id INT);"),
        Some((ObjectKind::Table, ObjectKind::Sequence))
    );
    assert_eq!(
        taken("CREATE TABLE s (id INT); CREATE SEQUENCE s;"),
        Some((ObjectKind::Sequence, ObjectKind::Table))
    );
    assert_eq!(
        taken("CREATE VIEW s AS SELECT 1 AS one; CREATE SEQUENCE s;"),
        Some((ObjectKind::Sequence, ObjectKind::View))
    );
    assert_eq!(
        taken("CREATE TABLE t (id INT); CREATE INDEX s ON t (id); CREATE SEQUENCE s;"),
        Some((ObjectKind::Sequence, ObjectKind::Index))
    );
    assert_eq!(
        taken("CREATE SEQUENCE s; CREATE VIEW s AS SELECT 1 AS one;"),
        Some((ObjectKind::View, ObjectKind::Sequence))
    );
    assert_eq!(
        taken("CREATE SEQUENCE s; CREATE TABLE t (id INT); CREATE INDEX s ON t (id);"),
        Some((ObjectKind::Index, ObjectKind::Sequence))
    );
    // A rename takes its new name from the same pool.
    assert_eq!(
        taken("CREATE SEQUENCE s; CREATE TABLE t (id INT); ALTER TABLE t RENAME TO s;"),
        Some((ObjectKind::Table, ObjectKind::Sequence))
    );
    assert_eq!(
        taken("CREATE SEQUENCE s; CREATE VIEW v AS SELECT 1 AS one; ALTER TABLE v RENAME TO s;"),
        Some((ObjectKind::View, ObjectKind::Sequence))
    );
    assert_eq!(
        taken(
            "CREATE SEQUENCE s; CREATE TABLE t (id INT); CREATE INDEX i ON t (id);
             ALTER INDEX i RENAME TO s;"
        ),
        Some((ObjectKind::Index, ObjectKind::Sequence))
    );

    // Another schema holds its own pool.
    let db = parse("CREATE SCHEMA app; CREATE TABLE s (id INT); CREATE SEQUENCE app.s;")
        .expect("the names live in different schemas");
    assert_eq!(sequence_names(&db), ["app.s"]);
}

#[test]
fn if_not_exists_skips_whatever_relation_holds_the_name() {
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE SEQUENCE s INCREMENT BY 2;
         CREATE SEQUENCE IF NOT EXISTS s INCREMENT BY 3;
         CREATE SEQUENCE IF NOT EXISTS t;
         CREATE SEQUENCE IF NOT EXISTS s OWNED BY missing.id;",
    )
    .expect("every repeated name is skipped");

    assert_eq!(sequence_names(&db), ["public.s"]);
    let options: Vec<String> =
        sequence(&db, "s").options().iter().map(ToString::to_string).collect();
    assert_eq!(options, [" INCREMENT BY 2"]);
}

#[test]
fn drop_sequence_removes_the_sequences_it_names() {
    let db = parse(
        "CREATE SCHEMA app;
         SET search_path = app, public;
         CREATE SEQUENCE a;
         CREATE SEQUENCE b;
         CREATE SEQUENCE keep;
         DROP SEQUENCE a, b;
         DROP SEQUENCE IF EXISTS missing;",
    )
    .expect("schema parses");

    assert_eq!(sequence_names(&db), ["app.keep"]);
}

#[test]
fn drop_sequence_reaches_the_temporary_sequence_first() {
    let db = parse(
        "CREATE SEQUENCE s;
         CREATE TEMP SEQUENCE s;
         DROP SEQUENCE s;",
    )
    .expect("schema parses");

    assert_eq!(sequence_names(&db), ["public.s"]);
}

#[test]
fn each_drop_spelling_refuses_the_other_kind() {
    let mismatch = |sql: &str| {
        match parse(sql) {
            Err(Error::RelationKindMismatch { expected_kind, actual_kind, object_name }) => {
                assert_eq!(object_name, "s");
                (expected_kind, actual_kind)
            }
            other => panic!("expected a kind mismatch, got {other:?}"),
        }
    };

    assert_eq!(
        mismatch("CREATE TABLE s (id INT); DROP SEQUENCE s;"),
        (ObjectKind::Sequence, ObjectKind::Table)
    );
    assert_eq!(
        mismatch("CREATE VIEW s AS SELECT 1 AS one; DROP SEQUENCE s;"),
        (ObjectKind::Sequence, ObjectKind::View)
    );
    assert_eq!(
        mismatch("CREATE SEQUENCE s; DROP TABLE s;"),
        (ObjectKind::Table, ObjectKind::Sequence)
    );
    assert_eq!(
        mismatch("CREATE SEQUENCE s; DROP VIEW s;"),
        (ObjectKind::View, ObjectKind::Sequence)
    );
    assert!(matches!(
        parse("DROP SEQUENCE s;"),
        Err(Error::RelationNotFound { object_kind: ObjectKind::Sequence, object_name })
            if object_name == "s"
    ));
}

#[test]
fn a_schema_holding_a_sequence_is_dropped_only_with_cascade() {
    assert!(matches!(
        parse("CREATE SCHEMA app; CREATE SEQUENCE app.s; DROP SCHEMA app;"),
        Err(Error::SchemaNotEmpty { schema_name }) if schema_name == "app"
    ));

    let db = parse(
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.s;
         CREATE SEQUENCE kept;
         DROP SCHEMA app CASCADE;",
    )
    .expect("cascade takes the sequence along");
    assert_eq!(sequence_names(&db), ["public.kept"]);
}

#[test]
fn a_sequence_name_ends_the_search_path_walk() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.x;
         CREATE TABLE x (id INT);
         SET search_path = app, public;",
    )
    .expect("schema parses");

    let case = IdentifierCase::AsWritten;
    assert!(
        db.resolve_target_table(TargetName::new("x", false), case)
            .expect("the name is well formed")
            .is_none(),
        "the sequence in `app` hides the table in `public`"
    );
    assert_eq!(sequence(&db, "x").sequence_schema(), Some("app"));
    assert!(
        db.sequence_by_target(TargetName::new("x", false), case)
            .expect("the name is well formed")
            .is_none(),
        "the strict lookup reads a bare name as `public`"
    );
}

#[test]
fn a_view_reading_a_sequence_holds_it() {
    assert!(matches!(
        parse(
            "CREATE SEQUENCE s;
             CREATE VIEW v AS SELECT last_value FROM s;
             DROP SEQUENCE s;"
        ),
        Err(Error::RelationHasDependents { object_kind: ObjectKind::Sequence, dependent_name, .. })
            if dependent_name == "v"
    ));

    let db = parse(
        "CREATE SEQUENCE s;
         CREATE VIEW v AS SELECT last_value FROM s;
         DROP SEQUENCE s CASCADE;",
    )
    .expect("cascade takes the view along");
    assert_eq!(sequence_names(&db), NONE);
    assert_eq!(db.views().count(), 0);
}

#[test]
fn a_folding_lookup_reports_two_sequences_it_cannot_tell_apart() {
    let db = parse("CREATE SEQUENCE \"Seq\"; CREATE SEQUENCE seq;").expect("two names as written");
    for lookup in [
        db.sequence_by_target(TargetName::new("seq", false), IdentifierCase::Folded),
        db.resolve_target_sequence(TargetName::new("seq", false), IdentifierCase::Folded),
    ] {
        assert!(matches!(
            lookup,
            Err(LookupError::AmbiguousTableLookup { ref candidates, .. })
                if candidates == &["\"Seq\"".to_string(), "seq".to_string()]
        ));
    }
}
