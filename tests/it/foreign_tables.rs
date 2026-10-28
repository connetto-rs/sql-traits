//! Tests that `CREATE FOREIGN TABLE` is recorded as a relation of its own and
//! that every later statement treats it as PostgreSQL does.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18 with `postgres_fdw`.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, KeyConstraint, ObjectKind},
    prelude::*,
    traits::Relation,
};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect, parser::Parser};

const REMOTE: &str = "CREATE ROLE app;
     CREATE FOREIGN TABLE remote (id INT NOT NULL, amount INT DEFAULT 0 CHECK (amount > 0))
     SERVER warehouse OPTIONS (schema_name 'sales', \"Table_Name\" 'Orders');";

const TRIGGER_FUNCTION: &str =
    "CREATE FUNCTION touch() RETURNS TRIGGER AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn db(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

fn refusal(sql: &str) -> Error {
    match parse(sql) {
        Ok(_) => panic!("{sql} was accepted"),
        Err(error) => error,
    }
}

fn foreign<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.foreign_table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("foreign table {name} is recorded"))
}

#[test]
fn a_foreign_table_is_recorded_apart_from_tables() {
    let db = db(&format!("{REMOTE} CREATE TABLE local (id INT);"));
    let names = |tables: &mut dyn Iterator<Item = &CreateTable>| {
        tables.map(|table| table.table_name().to_string()).collect::<Vec<_>>()
    };
    assert_eq!(names(&mut db.tables()), ["local"]);
    assert_eq!(names(&mut db.foreign_tables()), ["remote"]);
    assert_eq!(names(&mut db.tables_and_foreign_tables()), ["local", "remote"]);

    let target = || TargetName::new("remote", false);
    assert!(db.table_by_target(target(), IdentifierCase::AsWritten).expect("lookup").is_none());
    assert!(
        db.resolve_target_table(target(), IdentifierCase::AsWritten).expect("lookup").is_none()
    );
    let resolved = db
        .resolve_target_foreign_table(target(), IdentifierCase::AsWritten)
        .expect("lookup")
        .expect("the search path reaches it");
    assert_eq!(resolved, foreign(&db, "remote"));
    let local = db.tables().next().expect("one table");
    assert!(!local.is_foreign(&db).expect("held"));
    assert!(local.foreign_source(&db).expect("held").is_none());
}

#[test]
fn a_foreign_table_keeps_its_columns_checks_server_and_options() {
    let db = db(REMOTE);
    let remote = foreign(&db, "remote");
    assert!(remote.is_foreign(&db).expect("held"));

    let columns: Vec<_> = remote
        .columns(&db)
        .expect("held")
        .map(|column| (column.column_name(), column.is_nullable(&db).expect("held")))
        .collect();
    assert_eq!(columns, [("id", false), ("amount", true)]);
    assert_eq!(remote.check_constraints(&db).expect("held").count(), 1);

    let source = remote.foreign_source(&db).expect("held").expect("a foreign table names a server");
    assert_eq!(source.server_name(), "warehouse");
    assert!(!source.server_name_is_quoted());
    assert_eq!(
        source.options(),
        [
            ("schema_name".to_string(), "sales".to_string()),
            ("Table_Name".to_string(), "Orders".to_string())
        ]
    );
    // A reference to a foreign table answers as the table does.
    assert_eq!(
        <&CreateTable as TableLike>::foreign_source(&remote, &db).expect("held"),
        Some(source)
    );

    let quoted = self::db("CREATE FOREIGN TABLE remote (id INT) SERVER \"Warehouse\";");
    let quoted_source = foreign(&quoted, "remote")
        .foreign_source(&quoted)
        .expect("held")
        .expect("a foreign table names a server");
    assert!(quoted_source.server_name_is_quoted());
    assert_eq!(quoted_source.stored_server_name(), "Warehouse");
}

#[test]
fn a_grant_on_a_foreign_table_resolves_to_it() {
    let db = db(&format!(
        "{REMOTE} GRANT SELECT ON remote TO app WITH GRANT OPTION; GRANT SELECT ON remote TO PUBLIC;"
    ));
    let remote = foreign(&db, "remote");
    let app = db.role("app").expect("role");

    let grants: Vec<_> = remote.grants(&db).expect("held").collect();
    assert_eq!(grants.len(), 2);
    for grant in &grants {
        assert!(grant.applies_to_table(remote, &db));
        let relations: Vec<_> = grant.relations(&db).collect();
        assert!(matches!(
            relations.as_slice(),
            [Relation::ForeignTable(table)] if *table == remote
        ));
        assert!(format!("{relations:?}").starts_with("[ForeignTable("));
        // A foreign table is no table, so the narrower question omits it.
        assert_eq!(grant.tables(&db).count(), 0);
    }
    assert!(grants[0].with_grant_option());
    assert!(remote.can_select(app, &db).expect("held"));
}

#[test]
fn a_revoke_on_a_foreign_table_resolves_to_it() {
    let db =
        db(&format!("{REMOTE} GRANT SELECT ON remote TO app; REVOKE SELECT ON remote FROM app;"));
    let remote = foreign(&db, "remote");
    let app = db.role("app").expect("role");
    assert!(!remote.can_select(app, &db).expect("held"));
}

#[test]
fn a_schema_wide_grant_covers_foreign_tables() {
    let db = db("CREATE ROLE app; CREATE SCHEMA s;
         CREATE FOREIGN TABLE s.remote (id INT) SERVER warehouse;
         CREATE FOREIGN TABLE elsewhere (id INT) SERVER warehouse;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO app;");
    let grant = db.table_grants().next().expect("one grant");
    let reached: Vec<_> = grant
        .relations(&db)
        .map(|relation| {
            match relation {
                Relation::ForeignTable(table) => table.table_name(),
                other => panic!("unexpected {other:?}"),
            }
        })
        .collect();
    assert_eq!(reached, ["remote"]);
    assert!(!grant.applies_to_table(foreign(&db, "elsewhere"), &db));
}

#[test]
fn a_foreign_table_shares_the_relation_name_pool() {
    assert!(matches!(
        refusal(&format!("{REMOTE} CREATE TABLE remote (id INT);")),
        Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::Table,
            conflicting_kind: ObjectKind::ForeignTable,
            ..
        }
    ));
    assert!(matches!(
        refusal(
            "CREATE TABLE local (id INT); CREATE FOREIGN TABLE local (id INT) SERVER warehouse;"
        ),
        Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::ForeignTable,
            conflicting_kind: ObjectKind::Table,
            ..
        }
    ));
    assert!(matches!(
        refusal(&format!("{REMOTE} CREATE VIEW remote AS SELECT 1 AS one;")),
        Error::RelationNameAlreadyTaken { conflicting_kind: ObjectKind::ForeignTable, .. }
    ));

    let skipped = db(&format!(
        "{REMOTE} CREATE FOREIGN TABLE IF NOT EXISTS remote (other INT) SERVER elsewhere;"
    ));
    let remote = foreign(&skipped, "remote");
    assert_eq!(remote.columns(&skipped).expect("held").count(), 2);
}

#[test]
fn a_foreign_table_refuses_constraints_enforced_against_stored_rows() {
    let parent = "CREATE TABLE parent (id INT PRIMARY KEY);";
    for (sql, expected) in [
        ("CREATE FOREIGN TABLE f (id INT PRIMARY KEY) SERVER s;", KeyConstraint::PrimaryKey),
        ("CREATE FOREIGN TABLE f (id INT, PRIMARY KEY (id)) SERVER s;", KeyConstraint::PrimaryKey),
        ("CREATE FOREIGN TABLE f (id INT UNIQUE) SERVER s;", KeyConstraint::Unique),
        ("CREATE FOREIGN TABLE f (id INT, UNIQUE (id)) SERVER s;", KeyConstraint::Unique),
        (
            "CREATE FOREIGN TABLE f (id INT, EXCLUDE USING gist (id WITH =)) SERVER s;",
            KeyConstraint::Exclusion,
        ),
        (
            "CREATE FOREIGN TABLE f (id INT REFERENCES parent (id)) SERVER s;",
            KeyConstraint::ForeignKey,
        ),
        (
            "CREATE FOREIGN TABLE f (id INT, FOREIGN KEY (id) REFERENCES parent (id)) SERVER s;",
            KeyConstraint::ForeignKey,
        ),
        (
            "CREATE FOREIGN TABLE f (id INT) SERVER s; ALTER TABLE f ADD PRIMARY KEY (id);",
            KeyConstraint::PrimaryKey,
        ),
        (
            "CREATE FOREIGN TABLE f (id INT) SERVER s; ALTER TABLE f ADD CONSTRAINT u UNIQUE (id);",
            KeyConstraint::Unique,
        ),
        (
            "CREATE FOREIGN TABLE f (id INT) SERVER s;
             ALTER TABLE f ADD CONSTRAINT k FOREIGN KEY (id) REFERENCES parent (id);",
            KeyConstraint::ForeignKey,
        ),
        (
            "CREATE FOREIGN TABLE f (id INT) SERVER s; ALTER TABLE f ADD COLUMN w INT UNIQUE;",
            KeyConstraint::Unique,
        ),
    ] {
        let error = refusal(&format!("{parent} {sql}"));
        assert!(
            matches!(
                &error,
                Error::ConstraintUnsupportedOnForeignTable { table_name, constraint }
                    if table_name == "f" && *constraint == expected
            ),
            "{sql}: {error:?}"
        );
    }
}

#[test]
fn a_foreign_table_is_refused_where_a_table_holding_rows_is_needed() {
    for sql in [
        "CREATE TABLE child (id INT REFERENCES remote (id));",
        "CREATE TABLE child (id INT); ALTER TABLE child ADD FOREIGN KEY (id) REFERENCES remote (id);",
        "CREATE INDEX remote_id ON remote (id);",
        "CREATE POLICY p ON remote USING (true);",
        "DROP TABLE remote;",
    ] {
        let error = refusal(&format!("{REMOTE} {sql}"));
        assert!(
            matches!(
                &error,
                Error::RelationKindMismatch {
                    expected_kind: ObjectKind::Table,
                    actual_kind: ObjectKind::ForeignTable,
                    ..
                }
            ),
            "{sql}: {error:?}"
        );
    }
}

#[test]
fn alter_table_refuses_the_actions_a_foreign_table_cannot_take() {
    for action in [
        "ENABLE ROW LEVEL SECURITY",
        "DISABLE ROW LEVEL SECURITY",
        "FORCE ROW LEVEL SECURITY",
        "NO FORCE ROW LEVEL SECURITY",
        "SET LOGGED",
        "SET UNLOGGED",
        "SET (fillfactor = 50)",
    ] {
        let error = refusal(&format!("{REMOTE} ALTER TABLE remote {action};"));
        assert!(
            matches!(
                &error,
                Error::AlterActionUnsupportedOnRelation {
                    object_kind: ObjectKind::ForeignTable,
                    relation_name,
                    ..
                } if relation_name == "remote"
            ),
            "{action}: {error:?}"
        );
    }
}

#[test]
fn alter_table_changes_a_foreign_table_and_keeps_it_foreign() {
    let db = db(&format!(
        "{REMOTE}
         ALTER TABLE remote ADD COLUMN note TEXT;
         ALTER TABLE remote RENAME COLUMN amount TO total;
         ALTER TABLE remote ADD CONSTRAINT positive_id CHECK (id > 0) NOT VALID;
         ALTER TABLE remote OWNER TO app;
         ALTER TABLE remote RENAME TO archive;"
    ));
    assert_eq!(db.tables().count(), 0);
    let archive = foreign(&db, "archive");
    let columns: Vec<_> =
        archive.columns(&db).expect("held").map(ColumnLike::column_name).collect();
    assert_eq!(columns, ["id", "total", "note"]);
    assert_eq!(archive.check_constraints(&db).expect("held").count(), 2);
    assert_eq!(archive.owner(&db).expect("held"), Some("app"));
    assert_eq!(
        archive.foreign_source(&db).expect("held").map(ForeignTableSource::server_name),
        Some("warehouse")
    );
}

#[test]
fn a_trigger_on_a_foreign_table_is_kept_on_it_and_a_rule_is_refused() {
    let db = db(&format!(
        "{REMOTE} {TRIGGER_FUNCTION}
         CREATE TRIGGER stamp BEFORE INSERT ON remote FOR EACH ROW EXECUTE FUNCTION touch();"
    ));
    let remote = foreign(&db, "remote");
    let trigger = db.triggers().next().expect("one trigger");
    assert_eq!(trigger.table(&db).expect("its table resolves"), remote);
    assert_eq!(remote.triggers(&db).expect("held").count(), 1);

    assert!(matches!(
        refusal(&format!("{REMOTE} CREATE RULE r AS ON INSERT TO remote DO INSTEAD NOTHING;")),
        Error::RuleOnUnsupportedRelation { relation_kind: ObjectKind::ForeignTable, .. }
    ));
}

#[test]
fn a_table_inherits_from_or_copies_a_foreign_table() {
    let db = db(&format!(
        "{REMOTE}
         CREATE TABLE child (extra INT) INHERITS (remote);
         CREATE TABLE copy (LIKE remote);"
    ));
    let lookup = |name| {
        db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("lookup")
            .expect("an ordinary table")
    };
    let child = lookup("child");
    let parents: Vec<_> = child.inherits_from(&db).expect("held").collect();
    assert_eq!(parents, [foreign(&db, "remote")]);
    assert_eq!(child.columns(&db).expect("held").count(), 3);
    assert!(!child.is_foreign(&db).expect("held"));
    assert_eq!(lookup("copy").columns(&db).expect("held").count(), 2);
}

#[test]
fn drop_schema_counts_and_cascades_to_a_foreign_table() {
    let schema = "CREATE SCHEMA s; CREATE FOREIGN TABLE s.remote (id INT) SERVER warehouse;";
    assert!(matches!(refusal(&format!("{schema} DROP SCHEMA s;")), Error::SchemaNotEmpty { .. }));
    assert_eq!(db(&format!("{schema} DROP SCHEMA s CASCADE;")).foreign_tables().count(), 0);
}

#[test]
fn a_foreign_table_keeps_its_identity_and_its_columns_are_reachable_by_identity() {
    let db = db(&format!(
        "{REMOTE} CREATE TABLE local (id INT); CREATE MATERIALIZED VIEW m AS SELECT 1 AS one;"
    ));
    let remote = foreign(&db, "remote");
    let identity = db.table_identity(remote).expect("held");
    assert_eq!(db.foreign_table_by_identity(identity), Some(remote));
    assert_eq!(db.table_by_identity(identity), None);
    assert_eq!(db.table_id(remote), None);
    let local = db.tables().next().expect("one table");
    assert_eq!(db.foreign_table_by_identity(db.table_identity(local).expect("held")), None);
    // The stored-identity lookup answers for tables alone, whatever else holds
    // the name.
    assert_eq!(db.table_by_stored_identity(None, "remote"), None);
    assert_eq!(db.table_by_stored_identity(None, "m"), None);

    let column = remote.columns(&db).expect("held").next().expect("a column");
    let column_identity = db.column_identity(column).expect("held");
    assert_eq!(db.column_by_identity(column_identity), Some(column));
}

#[test]
fn a_foreign_table_survives_resumed_ingestion() {
    let statements = |sql| Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("parses");
    let ingestor = statements(REMOTE)
        .into_iter()
        .try_fold(
            ParserDBIngestor::new::<PostgreSqlDialect>("catalog".to_owned()),
            ParserDBIngestor::apply_statement,
        )
        .expect("applies");
    let before = ingestor.snapshot();
    assert_eq!(before.foreign_tables().count(), 1);

    let after =
        statements("ALTER TABLE remote ADD COLUMN note TEXT; GRANT SELECT ON remote TO app;")
            .into_iter()
            .try_fold(ingestor, ParserDBIngestor::apply_statement)
            .expect("applies")
            .finish();
    let remote = foreign(&after, "remote");
    assert_eq!(remote.columns(&after).expect("held").count(), 3);
    assert_eq!(remote.grants(&after).expect("held").count(), 1);
    assert_eq!(after.tables().count(), 0);
}
