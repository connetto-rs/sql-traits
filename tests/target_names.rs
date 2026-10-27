//! Tests that an object's target reads back as the name ingestion bound.
//!
//! A policy, a trigger, a foreign key and a grant all name a table. Reading
//! that name applies no resolution and cannot fail, so a caller honouring
//! rules of its own can resolve it itself. The name carries the qualifier of
//! the table it reached when the object was created and the quoting the SQL
//! wrote on the table name. These tests pin that reading, including the case
//! where resolution fails.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*, traits::grant::GrantTarget};
use sqlparser::{ast::Action, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

/// Reads a grant's target names out of the one kind the test expects it to
/// name, failing when the grant names another.
macro_rules! names_in {
    ($grant:expr, $kind:ident) => {
        match $grant.target() {
            GrantTarget::$kind(names) => Some(names.collect::<Vec<_>>()),
            _ => None,
        }
        .expect(concat!("the grant targets ", stringify!($kind)))
    };
}

/// The only `docs` table lives in schema `app`, the schema puts `app` on the
/// search path, and the policy names `docs` unqualified. The reader hands back
/// the name qualified with the schema the path reached, which is the table
/// the policy stays on.
#[test]
fn policy_target_reads_back_bound_to_the_schema_the_search_path_reached() {
    let db = parse(
        "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE app.docs (id INT);
         CREATE POLICY docs_sel ON docs USING (true);",
    );
    let policy = db.policies().next().expect("the policy exists");

    let target = policy.target_table_name();
    assert_eq!(target.name(), "docs");
    assert!(!target.name_is_quoted());
    assert_eq!(target.schema(), Some("app"), "the search path reached app");
    assert_eq!(target.to_string(), "app.docs");

    // The bound name resolves in the catalog without the caller reassembling
    // the parts or reaching for the concrete parser node.
    let resolved = db
        .resolve_target_table(target, IdentifierCase::AsWritten)
        .expect("the name is unambiguous");
    assert_eq!(resolved.expect("the qualified name finds it").table_schema(), Some("app"));

    let table = policy.table(&db).expect("the bound target resolves");
    assert_eq!(table.table_schema(), Some("app"));
}

/// Without the schema on the path, the same policy names a table that cannot
/// be found, and the database refuses it too.
#[test]
fn policy_on_a_table_off_the_search_path_is_refused() {
    let refused = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE POLICY docs_sel ON docs USING (true);",
    );

    assert!(
        matches!(refused, Err(Error::TableNotFoundForPolicy { ref table_name, .. }) if table_name == "docs"),
        "got {refused:?}"
    );
}

#[test]
fn policy_target_preserves_quoting_and_qualification() {
    let db = parse(
        "CREATE SCHEMA \"App\";
         CREATE TABLE \"App\".\"Docs\" (id INT);
         CREATE POLICY docs_sel ON \"App\".\"Docs\" USING (true);",
    );
    let policy = db.policies().next().expect("the policy exists");

    let target = policy.target_table_name();
    assert_eq!(target.name(), "Docs");
    assert!(target.name_is_quoted());
    assert_eq!(target.schema(), Some("App"));
    assert!(target.schema_is_quoted());
    assert_eq!(target.to_string(), "\"App\".\"Docs\"", "and it renders back as SQL text");

    // Reading through the blanket implementation for references answers the
    // same, since a caller holding `&&Policy` is the common case behind an
    // iterator.
    let by_reference = &policy;
    assert_eq!(by_reference.target_table_name(), target);
}

/// A trigger target that does not resolve cannot reach a built schema, since
/// the parse refuses it with `TableNotFoundForTrigger`. What the reader still
/// buys here is the name without a parser node, and without the failure mode
/// that [`TriggerLike::table`] carries for the caller who only wants a name.
#[test]
fn trigger_target_reads_back_unqualified() {
    let db = parse(
        "CREATE TABLE docs (id INT);
         CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER docs_touch AFTER INSERT ON docs
         FOR EACH ROW EXECUTE FUNCTION touch();",
    );
    let trigger = db.triggers().next().expect("the trigger exists");

    let target = trigger.target_table_name();
    assert_eq!(target.name(), "docs");
    assert!(!target.name_is_quoted());
    assert_eq!(target.schema(), None);

    let by_reference = &trigger;
    assert_eq!(by_reference.target_table_name(), target);
}

/// An unresolvable trigger target is refused outright, unlike the policy above
/// which is recorded and only refuses on resolution.
#[test]
fn trigger_target_that_does_not_resolve_is_refused() {
    let refused = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE app.docs (id INT);
         CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER docs_touch AFTER INSERT ON docs
         FOR EACH ROW EXECUTE FUNCTION touch();",
    );

    assert!(
        matches!(refused, Err(Error::TableNotFoundForTrigger { ref table_name, .. }) if table_name == "docs"),
        "got {refused:?}"
    );
}

#[test]
fn trigger_target_preserves_quoting_and_qualification() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.\"Docs\" (id INT);
         CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER docs_touch AFTER INSERT ON app.\"Docs\"
         FOR EACH ROW EXECUTE FUNCTION touch();",
    );
    let trigger = db.triggers().next().expect("the trigger exists");

    let target = trigger.target_table_name();
    assert_eq!(target.name(), "Docs");
    assert!(target.name_is_quoted());
    assert_eq!(target.schema(), Some("app"));
    assert!(!target.schema_is_quoted());
    assert_eq!(target.to_string(), "app.\"Docs\"");
}

#[test]
fn foreign_key_reference_preserves_quoting_and_qualification() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE app.\"Docs\" (id INT PRIMARY KEY);
         CREATE TABLE notes (doc_id INT, FOREIGN KEY (doc_id) REFERENCES app.\"Docs\"(id));",
    );
    let notes = db
        .table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the host table exists");
    let foreign_key =
        notes.foreign_keys(&db).expect("the host table is known").next().expect("one key");

    let target = foreign_key.referenced_table_name();
    assert_eq!(target.name(), "Docs");
    assert!(target.name_is_quoted());
    assert_eq!(target.schema(), Some("app"));
    assert!(!target.schema_is_quoted());
    assert_eq!(target.to_string(), "app.\"Docs\"");
}

#[test]
fn foreign_key_reference_reads_back_unqualified() {
    let db = parse(
        "CREATE TABLE docs (id INT PRIMARY KEY);
         CREATE TABLE notes (doc_id INT, FOREIGN KEY (doc_id) REFERENCES docs(id));",
    );
    let notes = db
        .table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the host table exists");
    let foreign_key =
        notes.foreign_keys(&db).expect("the host table is known").next().expect("one key");

    let target = foreign_key.referenced_table_name();
    assert_eq!(target.name(), "docs");
    assert!(!target.name_is_quoted());
    assert_eq!(target.schema(), None);
}

#[test]
fn grant_lists_its_table_targets_as_written() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE TABLE users (id INT);
         CREATE TABLE app.\"Posts\" (id INT);
         CREATE ROLE reader;
         GRANT SELECT ON users, app.\"Posts\" TO reader;",
    );
    let grant = db.table_grants().next().expect("the grant exists");
    let targets = names_in!(grant, Tables);

    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].name(), "users");
    assert!(!targets[0].name_is_quoted());
    assert_eq!(targets[0].schema(), None);
    assert_eq!(targets[1].name(), "Posts");
    assert!(targets[1].name_is_quoted());
    assert_eq!(targets[1].schema(), Some("app"));
    assert!(!targets[1].schema_is_quoted());
}

/// A column grant is the same statement seen through another trait, and it
/// must not lose the tables past the first the way the resolving reader does.
#[test]
fn column_grant_lists_every_table_target() {
    let db = parse(
        "CREATE TABLE users (id INT, name TEXT);
         CREATE TABLE posts (id INT, name TEXT);
         CREATE ROLE reader;
         GRANT SELECT (id, name) ON users, posts TO reader;",
    );
    let grant = db.column_grants().next().expect("the grant exists");
    let targets = names_in!(grant, Tables);
    let names: Vec<_> = targets.iter().map(TargetName::name).collect();

    assert_eq!(names, ["users", "posts"]);
}

#[test]
fn schema_wide_grant_names_the_schema_not_the_tables() {
    let db = parse(
        "CREATE TABLE public.users (id INT);
         CREATE ROLE reader;
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO reader;",
    );
    let grant = db.table_grants().next().expect("the grant exists");
    let schemas = names_in!(grant, TablesInSchema);

    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name(), "public");
    assert_eq!(schemas[0].schema(), None);
}

#[test]
fn grant_target_reads_back_when_it_does_not_resolve() {
    let sql = "CREATE ROLE reader;
               GRANT SELECT ON absent_table TO reader;";

    assert!(
        matches!(
            ParserDB::parse::<PostgreSqlDialect>(sql),
            Err(Error::TableNotFoundForGrant { ref table_name }) if table_name == "absent_table"
        ),
        "a closed world refuses a grant on a table nothing creates"
    );

    let db = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
        .expect("an open world records it");
    let grant = db.table_grants().next().expect("the grant exists");
    let targets = names_in!(grant, Tables);

    assert_eq!(targets.first().expect("one target").name(), "absent_table");
    assert!(grant.tables(&db).next().is_none(), "the target resolves to nothing");
}

/// `ALL PRIVILEGES` on a schema carries `CREATE`, and on the tables of a
/// schema it carries none, so the two grants must not read alike.
#[test]
fn direct_schema_grant_names_the_schema_apart_from_its_tables() {
    let db = parse(
        "CREATE SCHEMA app;
         CREATE SCHEMA \"Ops\";
         GRANT ALL ON ALL TABLES IN SCHEMA app TO PUBLIC;
         GRANT CREATE ON SCHEMA app, \"Ops\" TO PUBLIC;",
    );
    let grants: Vec<_> = db.table_grants().collect();

    assert!(matches!(grants[0].target(), GrantTarget::TablesInSchema(_)));
    let schemas = names_in!(grants[1], Schemas);

    assert_eq!(schemas.len(), 2);
    assert_eq!(schemas[0].name(), "app");
    assert!(!schemas[0].name_is_quoted());
    assert_eq!(schemas[1].name(), "Ops");
    assert!(schemas[1].name_is_quoted());
}

/// A revoke that leaves part of a schema grant standing keeps it naming the
/// schema, with only the privileges the revoke spared.
#[test]
fn partial_revoke_of_a_schema_grant_keeps_the_schema_target() {
    let db = parse(
        "CREATE SCHEMA app;
         GRANT USAGE, CREATE ON SCHEMA app TO PUBLIC;
         REVOKE CREATE ON SCHEMA app FROM PUBLIC;",
    );
    let grant = db.table_grants().next().expect("the usage grant survives");
    let schemas = names_in!(grant, Schemas);

    assert_eq!(schemas.iter().map(TargetName::name).collect::<Vec<_>>(), ["app"]);
    assert_eq!(grant.privileges(&db).cloned().collect::<Vec<_>>(), [Action::Usage]);
}

#[test]
fn sequence_grants_name_sequences_and_the_schemas_they_sweep() {
    let db = parse(
        "CREATE SEQUENCE ticket_seq;
         CREATE SCHEMA app;
         CREATE SEQUENCE app.order_seq;
         GRANT USAGE ON SEQUENCE ticket_seq, app.order_seq TO PUBLIC;
         GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO PUBLIC;",
    );
    let grants: Vec<_> = db.table_grants().collect();

    let sequences = names_in!(grants[0], Sequences);
    assert_eq!(sequences.len(), 2);
    assert_eq!((sequences[0].name(), sequences[0].schema()), ("ticket_seq", None));
    assert_eq!((sequences[1].name(), sequences[1].schema()), ("order_seq", Some("app")));

    let schemas = names_in!(grants[1], SequencesInSchema);
    assert_eq!(schemas.iter().map(TargetName::name).collect::<Vec<_>>(), ["public"]);
}

#[test]
fn grant_on_an_undescribed_object_kind_is_other() {
    let db = parse(
        "CREATE ROLE reader;
         GRANT CONNECT ON DATABASE app TO reader;",
    );
    let grant = db.table_grants().next().expect("the grant exists");

    assert!(matches!(grant.target(), GrantTarget::Other));
}

/// The point of the whole exercise: a caller can classify every policy in a
/// schema knowing only that it holds a catalog, with no mention of `ParserDB`
/// and no reach for a parser node. Reading the target and resolving it are both
/// trait methods, so this function compiles against any catalog.
fn policy_targets_by_schema<DB: DatabaseLike>(database: &DB) -> Vec<(String, Option<String>)> {
    database
        .policies()
        .map(|policy| {
            let target = policy.target_table_name();
            let schema = database
                .resolve_target_table(target.clone(), IdentifierCase::AsWritten)
                .expect("the target is unambiguous")
                .and_then(|table| table.table_schema().map(ToString::to_string));
            (target.to_string(), schema)
        })
        .collect()
}

#[test]
fn a_generic_catalog_resolves_every_policy_target() {
    let db = parse(
        "CREATE SCHEMA app;
         SET search_path TO app;
         CREATE TABLE app.docs (id INT);
         CREATE TABLE app.\"Notes\" (id INT);
         CREATE POLICY docs_sel ON docs USING (true);
         CREATE POLICY notes_sel ON app.\"Notes\" USING (true);",
    );

    let mut targets = policy_targets_by_schema(&db);
    targets.sort();

    assert_eq!(
        targets,
        [
            // Written qualified and quoted, and the quoting survives the round
            // trip.
            ("app.\"Notes\"".to_string(), Some("app".to_string())),
            // Written unqualified, bound into `app` by the search path.
            ("app.docs".to_string(), Some("app".to_string())),
        ]
    );
}

#[test]
fn function_target_preserves_quoting_and_qualification() {
    let db = parse(
        "CREATE SCHEMA \"Auth\";
         CREATE FUNCTION \"Auth\".\"UID\"() RETURNS TEXT LANGUAGE sql AS 'SELECT ''x''';",
    );
    let function =
        db.functions().find(|function| function.name() == "UID").expect("the function exists");

    let target = function.target_name();
    assert_eq!(target.name(), "UID");
    assert!(target.name_is_quoted());
    assert_eq!(target.schema(), Some("Auth"));
    assert!(target.schema_is_quoted());
    assert_eq!(target.to_string(), "\"Auth\".\"UID\"");
}

#[test]
fn function_target_reads_back_unqualified() {
    let db = parse("CREATE FUNCTION uid() RETURNS TEXT LANGUAGE sql AS 'SELECT ''x''';");
    let function =
        db.functions().find(|function| function.name() == "uid").expect("the function exists");

    let target = function.target_name();
    assert_eq!(target.name(), "uid");
    assert!(!target.name_is_quoted());
    assert_eq!(target.schema(), None);
}

/// An owned target name keeps both parts and their quoting after the text it
/// was read from is gone.
#[test]
fn an_owned_target_name_keeps_both_parts_and_their_quoting() {
    let target = {
        let schema = String::from("my.schema");
        let name = String::from("Docs");
        TargetName::new(&name, true).with_schema(&schema, true).into_owned()
    };

    assert_eq!(target.schema(), Some("my.schema"));
    assert!(target.schema_is_quoted());
    assert_eq!(target.name(), "Docs");
    assert!(target.name_is_quoted());
    assert_eq!(target.to_string(), "\"my.schema\".\"Docs\"");
}
