//! Tests that `CREATE SEQUENCE` records a sequence in the schema it lands in,
//! that the sequence takes its name from the pool tables, views and indexes
//! share, and that it leaves with `DROP SEQUENCE`, with its schema, and with
//! the column an `OWNED BY` ties it to.
//!
//! Every expectation was measured against PostgreSQL 18.4.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
    traits::grant::GrantTarget,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// No sequence at all, for comparing against [`sequence_names`].
const NONE: [&str; 0] = [];

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/sequence_names.rs"));

fn sequence<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Sequence {
    database
        .resolve_target_sequence(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("the name is well formed")
        .expect("the sequence is recorded")
}

/// The relation and column owning a sequence, if any, a view column named by
/// its position.
fn owner_of(database: &ParserDB, name: &str) -> Option<(String, String)> {
    sequence(database, name).owner(database).map(|owner| {
        match owner {
            SequenceOwner::Column(column) => {
                (
                    ColumnLike::table(column, database).table_name().to_string(),
                    column.column_name().to_string(),
                )
            }
            SequenceOwner::ViewColumn { view, ordinal } => {
                (view.view_name().to_string(), format!("#{ordinal}"))
            }
        }
    })
}

#[test]
fn a_created_sequence_is_recorded_and_reached_by_the_grants_on_it() {
    let db = parse(
        "CREATE SEQUENCE ticket_seq;
         GRANT USAGE ON SEQUENCE ticket_seq TO PUBLIC;
         GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO PUBLIC;",
    )
    .expect("schema parses");

    assert_eq!(sequence_names(&db), ["public.ticket_seq"]);
    let ticket = sequence(&db, "ticket_seq");
    assert_eq!(ticket.sequence_schema(), None);
    assert!(!ticket.is_temporary());
    assert_eq!(ticket.options(), []);
    for grant in db.table_grants() {
        assert_eq!(relations_of(&db, grant), ["sequence public.ticket_seq"]);
    }
    assert_eq!(db.table_grants().count(), 2);
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
fn drop_sequence_removes_the_sequence_and_the_grants_on_it() {
    let db = parse(
        "CREATE SCHEMA app;
         SET search_path = app, public;
         CREATE SEQUENCE a;
         CREATE SEQUENCE b;
         CREATE SEQUENCE keep;
         GRANT USAGE ON SEQUENCE a TO PUBLIC;
         GRANT USAGE ON SEQUENCE a, keep TO PUBLIC;
         GRANT SELECT ON keep TO PUBLIC;
         GRANT USAGE ON SCHEMA app TO PUBLIC;
         DROP SEQUENCE a, b;
         DROP SEQUENCE IF EXISTS missing;",
    )
    .expect("schema parses");

    assert_eq!(sequence_names(&db), ["app.keep"]);
    let targets: Vec<String> = db
        .table_grants()
        .map(|grant| {
            match grant.target() {
                GrantTarget::Sequences(names) => {
                    format!(
                        "sequences {}",
                        names.map(|name| name.to_string()).collect::<Vec<_>>().join(",")
                    )
                }
                GrantTarget::Tables(names) => {
                    format!(
                        "tables {}",
                        names.map(|name| name.to_string()).collect::<Vec<_>>().join(",")
                    )
                }
                GrantTarget::Schemas(_) => "schemas".to_string(),
                GrantTarget::Other => "other".to_string(),
            }
        })
        .collect();
    assert_eq!(targets, ["sequences app.keep", "tables app.keep", "schemas"]);
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
fn a_grant_spelled_for_a_table_may_name_a_sequence() {
    let db = parse(
        "CREATE SEQUENCE s;
         GRANT SELECT, UPDATE ON s TO PUBLIC;",
    )
    .expect("a sequence is a legal target of the table spelling");
    assert_eq!(db.unresolved_access_references().expect("targets are well formed").count(), 0);
    let grant = db.table_grants().next().expect("the grant is recorded");
    assert_eq!(relations_of(&db, grant), ["sequence public.s"]);
    let relation = grant.relations(&db).next().expect("the grant reaches the sequence");
    assert!(format!("{relation:?}").starts_with("Sequence("), "{relation:?}");
}

/// Every relation a grant covers, as `kind schema.name`, in the order the
/// grant reaches them.
fn relations_of(
    database: &ParserDB,
    grant: &<ParserDB as DatabaseLike>::TableGrant,
) -> Vec<String> {
    grant.relations(database).map(|relation| describe(&relation)).collect()
}

#[test]
fn a_grant_reaches_the_sequences_it_names_or_sweeps() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (id INT);
         CREATE SEQUENCE app.a;
         CREATE SEQUENCE app.b;
         CREATE SEQUENCE elsewhere;
         SET search_path = app, public;
         GRANT USAGE ON SEQUENCE a, elsewhere TO PUBLIC;
         GRANT SELECT ON ALL SEQUENCES IN SCHEMA app TO PUBLIC;
         GRANT SELECT ON ALL TABLES IN SCHEMA app TO PUBLIC;
         SET search_path = public;",
    )
    .expect("schema parses");
    let relations: Vec<Vec<String>> =
        db.table_grants().map(|grant| relations_of(&db, grant)).collect();

    // One grant per sequence named, one per sequence the schema held, and one
    // on the table, since `ALL TABLES` leaves sequences out.
    assert_eq!(
        relations,
        [
            ["sequence app.a"],
            ["sequence public.elsewhere"],
            ["sequence app.a"],
            ["sequence app.b"],
            ["table app.t"],
        ]
    );

    // The names were bound where they reached when the grant ran, so they
    // keep naming those sequences after the path changes.
    let written: Vec<String> = db
        .table_grants()
        .take(2)
        .flat_map(|grant| {
            let GrantTarget::Sequences(targets) = grant.target() else {
                panic!("expected a sequence target");
            };
            targets.map(|target| target.to_string()).collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(written, ["app.a", "elsewhere"]);
}

#[test]
fn owned_by_ties_the_sequence_to_its_column() {
    let db = parse(
        "CREATE TABLE t (id INT, other INT);
         CREATE SEQUENCE s OWNED BY t.id;
         CREATE SEQUENCE free OWNED BY NONE;",
    )
    .expect("schema parses");

    assert_eq!(owner_of(&db, "s"), Some(("t".to_string(), "id".to_string())));
    assert_eq!(owner_of(&db, "free"), None);
    assert_eq!(sequence(&db, "s").options(), []);
}

#[test]
fn an_owned_sequence_leaves_with_its_table() {
    let db = parse(
        "CREATE TABLE t (id INT);
         CREATE SEQUENCE s OWNED BY t.id;
         CREATE SEQUENCE kept;
         DROP TABLE t;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&db), ["public.kept"]);

    let renamed = parse(
        "CREATE TABLE t (id INT);
         CREATE SEQUENCE s OWNED BY t.id;
         ALTER TABLE t RENAME TO u;
         DROP TABLE u;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&renamed), NONE, "the rename keeps the tie");

    let schema = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (id INT);
         CREATE SEQUENCE app.s OWNED BY app.t.id;
         DROP SCHEMA app CASCADE;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&schema), NONE);
}

#[test]
fn an_owned_sequence_leaves_with_its_column_alone() {
    let db = parse(
        "CREATE TABLE t (id INT, other INT, third INT);
         CREATE SEQUENCE by_id OWNED BY t.id;
         CREATE SEQUENCE by_other OWNED BY t.other;
         ALTER TABLE t DROP COLUMN third;
         ALTER TABLE t RENAME COLUMN other TO renamed;
         ALTER TABLE t ALTER COLUMN id TYPE BIGINT;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&db), ["public.by_id", "public.by_other"]);
    assert_eq!(owner_of(&db, "by_other"), Some(("t".to_string(), "renamed".to_string())));

    let dropped = parse(
        "CREATE TABLE t (id INT, other INT);
         CREATE SEQUENCE by_id OWNED BY t.id;
         CREATE SEQUENCE by_other OWNED BY t.other;
         ALTER TABLE t RENAME COLUMN other TO renamed;
         ALTER TABLE t DROP COLUMN renamed;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&dropped), ["public.by_id"]);
}

#[test]
fn owned_by_is_resolved_and_checked() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.t (id INT);
         SET search_path = app, public;
         CREATE SEQUENCE s OWNED BY t.id;",
    )
    .expect("the table is reached through the path");
    assert_eq!(sequence(&db, "s").sequence_schema(), Some("app"));
    assert_eq!(owner_of(&db, "s"), Some(("t".to_string(), "id".to_string())));

    assert!(matches!(
        parse("CREATE SEQUENCE s OWNED BY t.id;"),
        Err(Error::RelationNotFoundForSequenceOwner { relation_name, sequence_name })
            if relation_name == "t" && sequence_name == "s"
    ));
    assert!(matches!(
        parse("CREATE TABLE t (id INT); CREATE SEQUENCE s OWNED BY t.nope;"),
        Err(Error::ColumnNotFoundForSequenceOwner { column_name, relation_name, sequence_name })
            if column_name == "nope" && relation_name == "t" && sequence_name == "s"
    ));
    assert!(matches!(
        parse("CREATE TABLE t (id INT); CREATE SEQUENCE s OWNED BY t;"),
        Err(Error::InvalidSequenceOwner { sequence_name, owner })
            if sequence_name == "s" && owner == "t"
    ));
    for sql in [
        "CREATE SCHEMA app; CREATE TABLE t (id INT); CREATE SEQUENCE app.s OWNED BY t.id;",
        "CREATE TABLE t (id INT); CREATE TEMP SEQUENCE s OWNED BY t.id;",
        "CREATE TEMP TABLE t (id INT); CREATE SEQUENCE s OWNED BY t.id;",
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::SequenceOwnerInAnotherSchema { ref sequence_name, ref relation_name })
                    if sequence_name == "s" && relation_name == "t"
            ),
            "{sql}"
        );
    }
    for (sql, kind) in [
        (
            "CREATE MATERIALIZED VIEW m AS SELECT 1 AS id; CREATE SEQUENCE s OWNED BY m.id;",
            ObjectKind::MaterializedView,
        ),
        ("CREATE SEQUENCE m; CREATE SEQUENCE s OWNED BY m.last_value;", ObjectKind::Sequence),
        (
            "CREATE TABLE t (id INT); CREATE INDEX m ON t (id); CREATE SEQUENCE s OWNED BY m.id;",
            ObjectKind::Index,
        ),
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::SequenceOwnerKindUnsupported { ref relation_name, relation_kind, .. })
                    if relation_name == "m" && relation_kind == kind
            ),
            "{sql}"
        );
    }
}

#[test]
fn a_view_column_may_own_a_sequence() {
    let db = parse(
        "CREATE VIEW v AS SELECT 1 AS a, 2 AS b;
         CREATE SEQUENCE s OWNED BY v.b;
         ALTER TABLE v RENAME COLUMN b TO renamed;
         CREATE OR REPLACE VIEW v AS SELECT 1 AS a, 2 AS renamed, 3 AS c;",
    )
    .expect("schema parses");
    assert_eq!(owner_of(&db, "s"), Some(("v".to_string(), "#1".to_string())));

    let dropped = parse(
        "CREATE VIEW v AS SELECT 1 AS a;
         CREATE SEQUENCE s OWNED BY v.a;
         DROP VIEW v;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&dropped), NONE);

    let cascaded = parse(
        "CREATE TABLE t (id INT);
         CREATE VIEW v AS SELECT id FROM t;
         CREATE SEQUENCE s OWNED BY v.id;
         DROP TABLE t CASCADE;",
    )
    .expect("schema parses");
    assert_eq!(sequence_names(&cascaded), NONE);

    assert!(matches!(
        parse("CREATE VIEW v AS SELECT 1 AS a; CREATE SEQUENCE s OWNED BY v.nope;"),
        Err(Error::ColumnNotFoundForSequenceOwner { column_name, relation_name, .. })
            if column_name == "nope" && relation_name == "v"
    ));
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
fn a_view_holds_the_sequence_its_path_reached() {
    for (schema, sequence) in [("app", "app.s"), ("\"App\"", "\"App\".s")] {
        let refused = parse(&format!(
            "CREATE SCHEMA {schema};
             CREATE SEQUENCE {sequence};
             SET search_path = {schema};
             CREATE VIEW public.v AS SELECT last_value FROM s;
             SET search_path = public;
             DROP SEQUENCE {sequence};"
        ));
        assert!(
            matches!(
                refused,
                Err(Error::RelationHasDependents { object_kind: ObjectKind::Sequence, ref dependent_name, .. })
                    if dependent_name == "v"
            ),
            "{sequence}: {refused:?}"
        );
    }
}

#[test]
fn a_sequence_reports_its_stored_names() {
    let db = parse(
        "CREATE SCHEMA \"App\";
         CREATE SCHEMA Other;
         CREATE SEQUENCE \"App\".s;
         CREATE SEQUENCE Other.\"T\";",
    )
    .expect("schema parses");
    let case = IdentifierCase::AsWritten;
    let quoted = db
        .sequence_by_target(TargetName::new("s", false).with_schema("App", true), case)
        .expect("the name is well formed")
        .expect("the quoted schema keeps its case");
    assert_eq!(quoted.stored_sequence_schema().as_deref(), Some("App"));
    assert_eq!(quoted.target_name().to_string(), "\"App\".s");
    let folded = db
        .sequence_by_target(TargetName::new("T", true).with_schema("other", false), case)
        .expect("the name is well formed")
        .expect("the unquoted schema folds");
    assert_eq!(folded.stored_sequence_schema().as_deref(), Some("other"));
    assert_eq!(folded.stored_sequence_name(), "T");
    assert!(db.table_by_stored_identity(Some("App"), "s").is_none(), "a sequence is no table");
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

/// The kind and name of a relation, as `kind schema.name`.
fn describe(relation: &Relation<'_, ParserDB>) -> String {
    match relation {
        Relation::Table(table) => {
            format!("table {}.{}", table.table_schema().unwrap_or("public"), table.table_name())
        }
        Relation::View(view) => {
            format!("view {}.{}", view.view_schema().unwrap_or("public"), view.view_name())
        }
        Relation::MaterializedView(view) => {
            format!(
                "materialized view {}.{}",
                view.view_schema().unwrap_or("public"),
                view.view_name()
            )
        }
        Relation::Sequence(sequence) => {
            format!(
                "sequence {}.{}",
                sequence.sequence_schema().unwrap_or("public"),
                sequence.sequence_name()
            )
        }
    }
}

#[test]
fn a_sequence_is_the_relation_its_name_reaches() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.x;
         CREATE TABLE x (id INT);
         SET search_path = app, public;",
    )
    .expect("schema parses");
    let reached = |name| {
        db.resolve_target_relation(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("well formed")
            .map(|relation| describe(&relation))
    };
    assert_eq!(
        reached("x").as_deref(),
        Some("sequence app.x"),
        "the first schema holding the name"
    );
    assert_eq!(reached("absent"), None);

    let bound = db
        .resolve_bound_relation(TargetName::new("x", false).with_schema("app", false))
        .expect("well formed")
        .map(|relation| describe(&relation));
    assert_eq!(bound.as_deref(), Some("sequence app.x"));
}

#[test]
fn a_statement_reading_a_sequence_reports_it() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE SEQUENCE app.x;
         CREATE TABLE x (id INT);
         SET search_path = app, public;",
    )
    .expect("schema parses");
    let query = sqlparser::parser::Parser::new(&PostgreSqlDialect {})
        .try_with_sql("SELECT last_value FROM x, public.x AS t")
        .and_then(|mut parser| parser.parse_query())
        .expect("query parses");
    let read: Vec<String> =
        query.referenced_relations(&db).expect("names resolve").iter().map(describe).collect();
    assert_eq!(read, ["sequence app.x", "table public.x"]);
}

#[test]
fn a_view_column_it_cannot_list_is_refused_as_an_owner() {
    assert!(matches!(
        parse(
            "CREATE VIEW v AS SELECT * FROM generate_series(1, 3);
             CREATE SEQUENCE s OWNED BY v.generate_series;"
        ),
        Err(Error::UnsupportedSequenceOwner { sequence_name, owner })
            if sequence_name == "s" && owner == "v.generate_series"
    ));

    // The declaration's own column list names the column, so it is found.
    let db = parse(
        "CREATE VIEW v (series) AS SELECT * FROM generate_series(1, 3);
         CREATE SEQUENCE s OWNED BY v.series;",
    )
    .expect("the declared column list names the owner");
    assert_eq!(owner_of(&db, "s"), Some(("v".to_string(), "#0".to_string())));
}
