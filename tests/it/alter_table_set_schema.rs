//! Moving relations between schemas.

#![expect(
    clippy::expect_used,
    reason = "Missing objects and unexpected acceptance fail these tests."
)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Tables retain their identity and columns through a schema move.
#[test]
fn table_moves() -> Result<(), Error> {
    for target in ["p", "ONLY p"] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA o; CREATE TABLE p (id INT PRIMARY KEY); ALTER TABLE {target} SET SCHEMA o;"
        ))?;
        assert!(
            database
                .table_by_target(
                    TargetName::new("p", false).with_schema("public", false),
                    IdentifierCase::AsWritten
                )?
                .is_none()
        );
        assert!(
            database
                .table_by_target(
                    TargetName::new("p", false).with_schema("o", false),
                    IdentifierCase::AsWritten
                )?
                .is_some()
        );
    }
    Ok(())
}

/// Creating a catalog relation requires the existing system-schema privilege.
#[test]
fn catalog_creation_is_refused() {
    let error = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE pg_catalog.x (id int);")
        .expect_err("catalog creation is refused");
    assert!(matches!(error, Error::SystemSchemaRelationRefused { .. }));
    let diagnostic = error.postgres_error().expect("PostgreSQL diagnostic");
    assert_eq!(diagnostic.sqlstate().to_string(), "42501");
    assert_eq!(diagnostic.message(), "permission denied to create \"pg_catalog.x\"");
}

/// Missing relations and moves to the current schema are harmless when
/// permitted.
#[test]
fn no_op_moves() -> Result<(), Error> {
    for statement in ["ALTER TABLE", "ALTER FOREIGN TABLE", "ALTER VIEW", "ALTER MATERIALIZED VIEW"]
    {
        ParserDB::parse::<PostgreSqlDialect>(&format!("{statement} IF EXISTS p SET SCHEMA nope;"))?;
    }
    ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE p (id INT); ALTER TABLE p SET SCHEMA public;",
    )?;
    Ok(())
}

/// Every supported relation kind can move through its statement spelling.
#[test]
fn relation_kinds_move() -> Result<(), Error> {
    for (declaration, alteration) in [
        ("CREATE VIEW p AS SELECT 1 AS id", "ALTER VIEW p"),
        ("CREATE VIEW p AS SELECT 1 AS id", "ALTER TABLE p"),
        ("CREATE MATERIALIZED VIEW p AS SELECT 1 AS id", "ALTER MATERIALIZED VIEW p"),
        ("CREATE MATERIALIZED VIEW p AS SELECT 1 AS id", "ALTER TABLE p"),
        ("CREATE SEQUENCE p", "ALTER TABLE p"),
        ("CREATE TABLE p (id INT) PARTITION BY RANGE (id)", "ALTER TABLE p"),
        (
            "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w; CREATE FOREIGN TABLE p (id INT) SERVER s",
            "ALTER FOREIGN TABLE p",
        ),
        (
            "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w; CREATE FOREIGN TABLE p (id INT) SERVER s",
            "ALTER TABLE p",
        ),
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA o; {declaration}; {alteration} SET SCHEMA o;"
        ))?;
        assert!(database.tables().all(|table| table.table_schema() == Some("o")));
        assert!(database.views().all(|view| view.view_schema() == Some("o")));
        assert!(database.materialized_views().all(|view| view.view_schema() == Some("o")));
        assert!(database.sequences().all(|sequence| sequence.sequence_schema() == Some("o")));
    }
    Ok(())
}

/// References retain their bound targets after relations and owned sequences
/// move.
#[test]
fn references_follow() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE TABLE p (id SERIAL PRIMARY KEY); CREATE TABLE c (id INT REFERENCES p(id)); CREATE VIEW v AS SELECT id FROM p; ALTER TABLE p SET SCHEMA o; CREATE TABLE after_move (id INT REFERENCES o.p(id));",
    )?;
    for table in database.tables() {
        for key in table.foreign_keys(&database)? {
            assert_eq!(key.referenced_table_name().schema(), Some("o"));
        }
    }
    assert!(database.views().next().expect("view").definition().to_string().contains("o.p"));
    let parent = database
        .table_by_target(
            TargetName::new("p", false).with_schema("o", false),
            IdentifierCase::AsWritten,
        )?
        .expect("moved parent");
    let column = parent.column("id", &database, IdentifierCase::AsWritten)?.expect("serial column");
    assert_eq!(
        column.default_expression().map(ToString::to_string).as_deref(),
        Some("nextval('o.p_id_seq'::REGCLASS)")
    );
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE SEQUENCE q; CREATE TABLE p (id INT DEFAULT nextval('q'::regclass)); ALTER TABLE q SET SCHEMA o;",
    )?;
    let table = database.tables().next().expect("table");
    let column = table.columns(&database)?.next().expect("column");
    assert_eq!(
        column.default_expression().map(ToString::to_string).as_deref(),
        Some("nextval('o.q'::REGCLASS)")
    );
    Ok(())
}

/// PostgreSQL reports schema move failures with its native error text.
#[test]
fn refusals() {
    for (sql, state, message) in [
        ("ALTER TABLE p SET SCHEMA nope", "42P01", "relation \"p\" does not exist"),
        (
            "CREATE TABLE p (id INT); ALTER TABLE p SET SCHEMA nope",
            "3F000",
            "schema \"nope\" does not exist",
        ),
        ("CREATE TABLE p (id INT); ALTER VIEW p SET SCHEMA nope", "42809", "\"p\" is not a view"),
        (
            "CREATE TABLE p (id INT); ALTER MATERIALIZED VIEW p SET SCHEMA nope",
            "42809",
            "\"p\" is not a materialized view",
        ),
        (
            "CREATE TABLE p (id INT); ALTER FOREIGN TABLE p SET SCHEMA nope",
            "42809",
            "\"p\" is not a foreign table",
        ),
        (
            "CREATE TABLE p (id INT PRIMARY KEY); ALTER TABLE p_pkey SET SCHEMA nope",
            "42809",
            "cannot change schema of index \"p_pkey\"",
        ),
        (
            "CREATE TYPE ty AS (id INT); ALTER TABLE ty SET SCHEMA nope",
            "42809",
            "\"ty\" is a composite type",
        ),
        (
            "CREATE SCHEMA o; CREATE TEMP TABLE p (id INT); ALTER TABLE p SET SCHEMA o",
            "0A000",
            "cannot move objects into or out of temporary schemas",
        ),
        (
            "CREATE TABLE p (id INT); ALTER TABLE p SET SCHEMA pg_temp",
            "0A000",
            "cannot move objects into or out of temporary schemas",
        ),
        (
            "CREATE TABLE p (id INT); ALTER TABLE p SET SCHEMA pg_toast",
            "0A000",
            "cannot move objects into or out of TOAST schema",
        ),
        (
            "CREATE SCHEMA o; CREATE TABLE p (id SERIAL); ALTER TABLE p_id_seq SET SCHEMA o",
            "0A000",
            "cannot move an owned sequence into another schema",
        ),
        (
            "CREATE SCHEMA o; CREATE TABLE p (id INT); CREATE TABLE o.p (id INT); ALTER TABLE p SET SCHEMA o",
            "42P07",
            "relation \"p\" already exists in schema \"o\"",
        ),
        (
            "CREATE SCHEMA o; CREATE TABLE p (id INT); CREATE DOMAIN o.p AS INT; ALTER TABLE p SET SCHEMA o",
            "42710",
            "type \"p\" already exists in schema \"o\"",
        ),
        (
            "CREATE SCHEMA o; CREATE TABLE p (id INT); CREATE DOMAIN o._p AS INT; ALTER TABLE p SET SCHEMA o",
            "42710",
            "type \"_p\" already exists in schema \"o\"",
        ),
        (
            "CREATE SCHEMA o; CREATE TABLE p (id INT); CREATE INDEX zz ON p(id); CREATE INDEX aa ON p(id); CREATE TABLE o.zz (id INT); CREATE TABLE o.aa (id INT); ALTER TABLE p SET SCHEMA o",
            "42P07",
            "relation \"zz\" already exists in schema \"o\"",
        ),
        (
            "CREATE SCHEMA o; CREATE TABLE p (id SERIAL); CREATE SEQUENCE o.p_id_seq; ALTER TABLE p SET SCHEMA o",
            "42P07",
            "relation \"p_id_seq\" already exists in schema \"o\"",
        ),
        (
            "CREATE TABLE p (id INT); ALTER TABLE p SET SCHEMA o, ADD COLUMN x INT",
            "42601",
            "syntax error at or near \",\"",
        ),
        (
            "CREATE TABLE p (id INT); ALTER TABLE p ADD COLUMN x INT, SET SCHEMA o",
            "42601",
            "syntax error at or near \"SCHEMA\"",
        ),
        (
            "CREATE TABLE p (id INT); alter table p add column x int, set schema o",
            "42601",
            "syntax error at or near \"schema\"",
        ),
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(sql).expect_err(sql);
        assert_refusal(&error, state, message);
    }
}

/// Restrictions precede the same-schema no-op.
#[test]
fn restricted_same_schema_moves() {
    for (sql, message) in [
        (
            "CREATE TEMP TABLE p(id INT); ALTER TABLE p SET SCHEMA pg_temp;",
            "cannot move objects into or out of temporary schemas",
        ),
        (
            "CREATE TABLE p(id SERIAL); ALTER TABLE p_id_seq SET SCHEMA public;",
            "cannot move an owned sequence into another schema",
        ),
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(sql).expect_err(sql);
        assert_refusal(&error, "0A000", message);
    }
}

/// Generated indexes and materialized-view indexes reserve destination names.
#[test]
fn dependent_index_collisions() {
    for sql in [
        "CREATE SCHEMA o; CREATE TABLE p(id INT PRIMARY KEY); CREATE TABLE o.p_pkey(id INT); ALTER TABLE p SET SCHEMA o;",
        "CREATE SCHEMA o; CREATE MATERIALIZED VIEW p AS SELECT 1 AS id; CREATE INDEX p_pkey ON p(id); CREATE TABLE o.p_pkey(id INT); ALTER MATERIALIZED VIEW p SET SCHEMA o;",
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(sql).expect_err(sql);
        assert!(matches!(error, Error::SetSchemaNameCollision { type_collision: false, .. }));
        let postgres = error.postgres_error().expect(sql);
        assert_eq!(postgres.sqlstate().to_string(), "42P07");
        assert_eq!(postgres.message(), "relation \"p_pkey\" already exists in schema \"o\"");
    }
}

/// Quoted and system namespaces retain their destination identity.
#[test]
fn destination_names() -> Result<(), Error> {
    for (setup, target, schema, quoted) in [
        ("CREATE SCHEMA \"O\";", "\"O\"", "O", true),
        ("", "pg_catalog", "pg_catalog", false),
        ("", "information_schema", "information_schema", false),
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{setup} CREATE TABLE p(id INT); ALTER TABLE p SET SCHEMA {target};"
        ))?;
        assert!(
            database
                .table_by_target(
                    TargetName::new("p", false).with_schema(schema, quoted),
                    IdentifierCase::AsWritten
                )?
                .is_some()
        );
    }
    Ok(())
}

/// A schema move carries indexes and owned sequences while leaving unrelated
/// sequences.
#[test]
fn dependent_objects_move() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE SEQUENCE independent; CREATE TABLE p(id SERIAL PRIMARY KEY, generated BIGINT GENERATED ALWAYS AS IDENTITY, other INT DEFAULT nextval('independent'::regclass)); CREATE SEQUENCE attached OWNED BY p.other; CREATE INDEX ix ON p(other); ALTER TABLE p SET SCHEMA o;",
    )?;
    for sequence in database.sequences() {
        assert_eq!(
            sequence.sequence_schema(),
            if sequence.sequence_name() == "independent" { None } else { Some("o") },
            "{}",
            sequence.sequence_name()
        );
    }
    for index in database.indexes() {
        assert_eq!(index.schema(), Some("o"));
    }
    Ok(())
}

/// Moving a parent leaves inheritance children and partitions in their schemas.
#[test]
fn children_stay() -> Result<(), Error> {
    for setup in [
        "CREATE TABLE p(id INT); CREATE TABLE c() INHERITS(p);",
        "CREATE TABLE p(id INT) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0) TO(10);",
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA o; {setup} ALTER TABLE p SET SCHEMA o;"
        ))?;
        assert!(
            database
                .table_by_target(
                    TargetName::new("c", false).with_schema("public", false),
                    IdentifierCase::AsWritten
                )?
                .is_some()
        );
        assert!(
            database
                .table_by_target(
                    TargetName::new("p", false).with_schema("o", false),
                    IdentifierCase::AsWritten
                )?
                .is_some()
        );
    }
    Ok(())
}

/// Index collision order includes constraint-backed and independent indexes.
#[test]
fn index_creation_order() {
    for indexes in [
        "CREATE INDEX zz ON p(id); ALTER TABLE p ADD CONSTRAINT aa UNIQUE(id);",
        "ALTER TABLE p ADD CONSTRAINT zz UNIQUE(id); CREATE INDEX aa ON p(id);",
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(&format!("CREATE SCHEMA o; CREATE TABLE p(id INT); {indexes} CREATE TABLE o.zz(id INT); CREATE TABLE o.aa(id INT); ALTER TABLE p SET SCHEMA o;")).expect_err("collision");
        assert!(
            matches!(&error, Error::SetSchemaNameCollision { object_name, type_collision: false, .. } if object_name == "zz")
        );
        let postgres = error.postgres_error().expect("PostgreSQL diagnostic");
        assert_eq!(postgres.sqlstate().to_string(), "42P07");
        assert_eq!(postgres.message(), "relation \"zz\" already exists in schema \"o\"");
    }
}

/// Row types and generated arrays remain usable through their destination
/// names.
#[test]
fn row_types_move() -> Result<(), Error> {
    for declaration in [
        "CREATE TABLE p(id INT)",
        "CREATE VIEW p AS SELECT 1 AS id",
        "CREATE MATERIALIZED VIEW p AS SELECT 1 AS id",
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA o; {declaration}; CREATE TABLE holder(row_value p, array_value _p); ALTER TABLE p SET SCHEMA o; CREATE TABLE later(row_value o.p, array_value o._p);"
        ))?;
        let holder = database
            .table_by_target(TargetName::new("holder", false), IdentifierCase::AsWritten)?
            .expect("holder");
        for column in holder.columns(&database)? {
            assert!(
                column.data_type(&database).to_string().contains("o.p"),
                "{}",
                column.data_type(&database)
            );
        }
        let setup = format!("CREATE SCHEMA o; {declaration}; ALTER TABLE p SET SCHEMA o;");
        let moved = ParserDB::parse::<PostgreSqlDialect>(&format!("{setup} DROP TYPE o._p;"));
        assert!(matches!(moved, Err(Error::TypeDropRequiredBy { .. })), "{moved:?}");
        let previous =
            ParserDB::parse::<PostgreSqlDialect>(&format!("{setup} DROP TYPE public._p;"));
        assert!(
            matches!(&previous, Err(Error::TypeNotFound { type_name }) if type_name == "public._p"),
            "{previous:?}"
        );
    }
    Ok(())
}

/// A moved view retains its readers and owned sequences.
#[test]
fn view_dependents_move() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE VIEW p AS SELECT 1 AS id; CREATE VIEW reader AS SELECT id FROM p; CREATE SEQUENCE attached OWNED BY p.id; ALTER VIEW p SET SCHEMA o;",
    )?;
    assert_eq!(database.sequences().next().expect("owned sequence").sequence_schema(), Some("o"));
    assert!(
        database
            .views()
            .find(|view| view.view_name() == "reader")
            .expect("reader")
            .definition()
            .to_string()
            .contains("o.p")
    );
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE MATERIALIZED VIEW p AS SELECT 1 AS id; CREATE INDEX ix ON p(id); ALTER MATERIALIZED VIEW p SET SCHEMA o;",
    )?;
    assert_eq!(database.materialized_view_indexes().next().expect("index").schema(), Some("o"));
    Ok(())
}

/// Other dialects reject schema relocation.
#[test]
fn non_postgres_dialects_refuse() {
    use sqlparser::dialect::{GenericDialect, MySqlDialect, SQLiteDialect};
    let sql = "CREATE TABLE p(id INT); ALTER TABLE p SET SCHEMA o;";
    for error in [
        ParserDB::parse::<GenericDialect>(sql).expect_err("generic syntax"),
        ParserDB::parse::<MySqlDialect>(sql).expect_err("MySQL syntax"),
        ParserDB::parse::<SQLiteDialect>(sql).expect_err("SQLite syntax"),
    ] {
        assert!(matches!(error, Error::SqlParserError { .. }));
        assert!(error.to_string().contains("syntax error"), "{error}");
    }
}

/// Exclusion indexes share the creation order of other indexes.
#[test]
fn exclusion_index_creation_order() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE TABLE p(id INT); CREATE INDEX zz ON p(id); ALTER TABLE p ADD CONSTRAINT aa EXCLUDE USING btree(id WITH =); CREATE TABLE o.zz(id INT); CREATE TABLE o.aa(id INT); ALTER TABLE p SET SCHEMA o;"
    ).expect_err("index collision");
    assert!(
        matches!(&error, Error::SetSchemaNameCollision { object_name, type_collision: false, .. } if object_name == "zz"),
        "{error:?}"
    );
    let postgres = error.postgres_error().expect("PostgreSQL diagnostic");
    assert_eq!(postgres.sqlstate().to_string(), "42P07");
    assert_eq!(postgres.message(), "relation \"zz\" already exists in schema \"o\"");
}

/// Domains do not reserve destination names for indexes or sequences.
#[test]
fn domains_allow_index_and_sequence_moves() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE TABLE p(id SERIAL); CREATE INDEX ix ON p(id); CREATE DOMAIN o.ix AS INT; CREATE DOMAIN o.p_id_seq AS INT; ALTER TABLE p SET SCHEMA o;",
    )?;
    assert_eq!(database.indexes().next().expect("index").schema(), Some("o"));
    assert_eq!(database.sequences().next().expect("sequence").sequence_schema(), Some("o"));
    Ok(())
}

/// Checks the typed refusal and its PostgreSQL diagnostic.
fn assert_refusal(error: &Error, state: &str, message: &str) {
    let expected_variant = match state {
        "42P01" => matches!(error, Error::RelationNotFound { .. }),
        "3F000" => matches!(error, Error::SchemaNotFound { .. }),
        "42P07" => matches!(error, Error::SetSchemaNameCollision { type_collision: false, .. }),
        "42710" => matches!(error, Error::SetSchemaNameCollision { type_collision: true, .. }),
        "42601" => matches!(error, Error::SetSchemaCommandNotAlone { .. }),
        "42809" if message.contains("is not a") => {
            matches!(error, Error::RelationKindMismatch { .. })
        }
        "42809" => matches!(error, Error::SetSchemaWrongKind { .. }),
        "0A000" if message.contains("owned sequence") => {
            matches!(error, Error::SetSchemaOwnedSequence { .. })
        }
        "0A000" => {
            matches!(error, Error::SetSchemaRestrictedNamespace { toast } if *toast == message.contains("TOAST"))
        }
        _ => false,
    };
    assert!(expected_variant, "{error:?}");
    let postgres = error.postgres_error().expect("PostgreSQL diagnostic");
    assert_eq!(postgres.sqlstate().to_string(), state, "{error:?}");
    assert_eq!(postgres.message(), message, "{error:?}");
}

/// Every relation namespace holder conflicts before dependent names.
#[test]
fn relation_name_holders() {
    for holder in [
        "CREATE TABLE o.p(id INT)",
        "CREATE VIEW o.p AS SELECT 1 AS id",
        "CREATE SEQUENCE o.p",
        "CREATE TYPE o.p AS (id INT)",
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(&format!("CREATE SCHEMA o; CREATE TABLE p(id SERIAL PRIMARY KEY); {holder}; CREATE DOMAIN o._p AS INT; ALTER TABLE p SET SCHEMA o;")).expect_err("relation collision");
        assert_refusal(&error, "42P07", "relation \"p\" already exists in schema \"o\"");
    }
    let error = ParserDB::parse::<PostgreSqlDialect>("CREATE SCHEMA o; CREATE TABLE p(id INT); CREATE TYPE o.p AS ENUM('one'); ALTER TABLE p SET SCHEMA o;").expect_err("enum collision");
    assert_refusal(&error, "42710", "type \"p\" already exists in schema \"o\"");
}

/// Statement kinds are checked before the destination schema.
#[test]
fn view_kind_refusals() {
    for (declaration, statement, message) in [
        ("CREATE MATERIALIZED VIEW p AS SELECT 1 AS id", "ALTER VIEW p", "\"p\" is not a view"),
        (
            "CREATE VIEW p AS SELECT 1 AS id",
            "ALTER FOREIGN TABLE p",
            "\"p\" is not a foreign table",
        ),
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{declaration}; {statement} SET SCHEMA nope;"
        ))
        .expect_err("wrong relation kind");
        assert_refusal(&error, "42809", message);
    }
    let error = ParserDB::parse::<PostgreSqlDialect>("CREATE SCHEMA o; CREATE TABLE p(id INT GENERATED ALWAYS AS IDENTITY); ALTER TABLE p_id_seq SET SCHEMA o;").expect_err("owned identity sequence");
    assert_refusal(&error, "0A000", "cannot move an owned sequence into another schema");
}

/// Foreign defaults and reusable constraint names do not enlarge the move.
#[test]
fn unrelated_sequences_and_constraints_stay() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE TABLE other(id INT); CREATE SEQUENCE q OWNED BY other.id; CREATE TABLE p(id INT DEFAULT nextval('q'::regclass), CONSTRAINT positive CHECK(id > 0)); CREATE TABLE o.other(id INT, CONSTRAINT positive CHECK(id > 0)); ALTER TABLE p SET SCHEMA o;",
    )?;
    assert_eq!(database.sequences().next().expect("other sequence").sequence_schema(), None);
    let moved = database
        .table_by_target(
            TargetName::new("p", false).with_schema("o", false),
            IdentifierCase::AsWritten,
        )?
        .expect("moved table");
    assert_eq!(
        moved
            .column("id", &database, IdentifierCase::AsWritten)?
            .expect("column")
            .default_expression()
            .map(ToString::to_string)
            .as_deref(),
        Some("nextval('q'::REGCLASS)")
    );
    Ok(())
}

/// Policies, triggers and rules retain their moved relation.
#[test]
fn attached_metadata_follows() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE TABLE p(id INT); CREATE POLICY policy ON p USING(id > 0); CREATE FUNCTION f() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; CREATE TRIGGER audit AFTER INSERT ON p FOR EACH ROW EXECUTE FUNCTION f(); CREATE RULE keep AS ON DELETE TO p DO INSTEAD NOTHING; ALTER TABLE p SET SCHEMA o;",
    )?;
    assert_eq!(database.policies().next().expect("policy").table_name.to_string(), "o.p");
    assert_eq!(database.triggers().next().expect("trigger").table_name.to_string(), "o.p");
    assert_eq!(database.rules().next().expect("rule").table_name.to_string(), "o.p");
    Ok(())
}

/// Built-in base types reserve names in the catalog namespace.
#[test]
fn builtin_type_collision() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE public.int4(id INT); ALTER TABLE public.int4 SET SCHEMA pg_catalog;",
    )
    .expect_err("built-in type collision");
    assert_refusal(&error, "42710", "type \"int4\" already exists in schema \"pg_catalog\"");
}

/// A child can move independently of its parent.
#[test]
fn children_move_independently() -> Result<(), Error> {
    for setup in [
        "CREATE TABLE p(id INT); CREATE TABLE c() INHERITS(p);",
        "CREATE TABLE p(id INT) PARTITION BY RANGE(id); CREATE TABLE c PARTITION OF p FOR VALUES FROM(0) TO(10);",
    ] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE SCHEMA o; {setup} ALTER TABLE c SET SCHEMA o;"
        ))?;
        assert!(
            database
                .table_by_target(
                    TargetName::new("c", false).with_schema("o", false),
                    IdentifierCase::AsWritten
                )?
                .is_some()
        );
        assert!(
            database
                .table_by_target(
                    TargetName::new("p", false).with_schema("public", false),
                    IdentifierCase::AsWritten
                )?
                .is_some()
        );
    }
    Ok(())
}

/// Two schema moves are a syntax error even when the relation is absent.
#[test]
fn repeated_schema_commands() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "ALTER TABLE IF EXISTS p SET SCHEMA o, SET SCHEMA public;",
    )
    .expect_err("combined schema moves");
    assert_refusal(&error, "42601", "syntax error at or near \",\"");
}

/// Explicitly unowned sequences stay while their defaults' tables move.
#[test]
fn explicitly_unowned_sequence_stays() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA o; CREATE SEQUENCE q OWNED BY NONE; CREATE TABLE p(id INT DEFAULT nextval('q')); ALTER TABLE p SET SCHEMA o;",
    )?;
    assert_eq!(database.sequences().next().expect("unowned sequence").sequence_schema(), None);
    Ok(())
}
