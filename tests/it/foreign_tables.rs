//! Tests that `CREATE FOREIGN TABLE` is recorded as a relation of its own and
//! that every later statement treats it as PostgreSQL does.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18 with `postgres_fdw`.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, KeyConstraint, ObjectKind, UnattachablePartition},
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
fn a_foreign_table_resolves_as_a_relation_of_its_own_kind() {
    let db = db(&format!("{REMOTE} CREATE TABLE local (id INT);"));
    let remote = foreign(&db, "remote");
    let target = || TargetName::new("remote", false);
    assert!(matches!(
        db.resolve_target_relation(target(), IdentifierCase::AsWritten).expect("lookup"),
        Some(Relation::ForeignTable(table)) if table == remote
    ));
    assert!(matches!(
        db.resolve_bound_relation(target()).expect("lookup"),
        Some(Relation::ForeignTable(table)) if table == remote
    ));

    // A statement names the foreign table among what it reads, once however
    // often it names it, and the base-table answer leaves it out.
    let query = sqlparser::parser::Parser::new(&PostgreSqlDialect {})
        .try_with_sql("SELECT * FROM remote, local WHERE id IN (SELECT id FROM remote)")
        .expect("tokenizes")
        .parse_query()
        .expect("parses");
    let relations = query.referenced_relations(&db).expect("resolves");
    assert!(matches!(
        relations.as_slice(),
        [Relation::ForeignTable(first), Relation::Table(second)]
            if *first == remote && second.table_name() == "local"
    ));
    assert_eq!(query.referenced_tables(&db).expect("resolves"), [table(&db, "local")]);
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
fn drop_foreign_table_drops_only_a_foreign_table() {
    let dropped = db(&format!("{REMOTE} DROP FOREIGN TABLE remote;"));
    assert_eq!(dropped.foreign_tables().count(), 0);
    assert_eq!(
        db(&format!("{REMOTE} DROP FOREIGN TABLE IF EXISTS nowhere;")).foreign_tables().count(),
        1
    );

    for (sql, actual_kind) in [
        ("CREATE TABLE local (id INT); DROP FOREIGN TABLE local;", ObjectKind::Table),
        ("CREATE VIEW v AS SELECT 1 AS one; DROP FOREIGN TABLE v;", ObjectKind::View),
        (
            "CREATE MATERIALIZED VIEW m AS SELECT 1 AS one; DROP FOREIGN TABLE m;",
            ObjectKind::MaterializedView,
        ),
    ] {
        let error = refusal(sql);
        assert!(
            matches!(
                &error,
                Error::RelationKindMismatch { expected_kind: ObjectKind::ForeignTable, actual_kind: kind, .. }
                    if *kind == actual_kind
            ),
            "{sql}: {error:?}"
        );
    }
    assert!(matches!(
        refusal("DROP FOREIGN TABLE nowhere;"),
        Error::RelationNotFound { object_kind: ObjectKind::ForeignTable, object_name } if object_name == "nowhere"
    ));

    // A name of another kind anywhere in the list refuses the whole statement.
    let mixed = "CREATE FOREIGN TABLE f5 (id INT) SERVER s; CREATE TABLE local (id INT);";
    assert!(matches!(
        refusal(&format!("{mixed} DROP FOREIGN TABLE f5, local;")),
        Error::RelationKindMismatch { actual_kind: ObjectKind::Table, .. }
    ));
}

#[test]
fn drop_foreign_table_takes_what_depends_on_it_only_under_cascade() {
    let viewed = format!("{REMOTE} CREATE VIEW recent AS SELECT id FROM remote;");
    assert!(matches!(
        refusal(&format!("{viewed} DROP FOREIGN TABLE remote;")),
        Error::RelationHasDependents { object_kind: ObjectKind::ForeignTable, object_name, .. }
            if object_name == "remote"
    ));
    let cascaded = db(&format!("{viewed} DROP FOREIGN TABLE remote CASCADE;"));
    assert_eq!(cascaded.foreign_tables().count(), 0);
    assert_eq!(cascaded.views().count(), 0);

    let inherited = format!("{REMOTE} CREATE TABLE child () INHERITS (remote);");
    assert!(matches!(
        refusal(&format!("{inherited} DROP FOREIGN TABLE remote;")),
        Error::DropTableInheritedFrom { .. }
    ));
    let cascaded = db(&format!("{inherited} DROP FOREIGN TABLE remote CASCADE;"));
    assert_eq!(cascaded.tables().count(), 0);
    assert_eq!(cascaded.foreign_tables().count(), 0);
}

const PARTITIONED: &str = "CREATE TABLE target (id INT PRIMARY KEY);
     CREATE TABLE orders (id INT NOT NULL) PARTITION BY RANGE (id);
     CREATE FOREIGN TABLE remote_orders (id INT NOT NULL) SERVER warehouse;";

const ATTACHED: &str =
    "ALTER TABLE orders ATTACH PARTITION remote_orders FOR VALUES FROM (0) TO (10);";

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("table {name} is recorded"))
}

#[test]
fn an_attached_foreign_partition_stays_foreign_and_is_listed_as_a_partition() {
    let db = db(&format!(
        "{PARTITIONED} {ATTACHED}
         CREATE INDEX orders_id ON orders (id);
         ALTER TABLE orders ADD COLUMN note TEXT;
         ALTER TABLE orders ADD CONSTRAINT positive CHECK (id >= 0);"
    ));
    let orders = table(&db, "orders");
    let remote = foreign(&db, "remote_orders");
    assert_eq!(orders.partitions(&db).expect("held").collect::<Vec<_>>(), [remote]);
    assert_eq!(remote.partition_root(&db).expect("held"), Some(orders));
    assert!(remote.is_foreign(&db).expect("held"));
    assert_eq!(remote.columns(&db).expect("held").count(), 2);
    assert_eq!(remote.check_constraints(&db).expect("held").count(), 1);
    assert_eq!(remote.indices(&db).expect("held").count(), 0);
}

#[test]
fn a_foreign_table_is_not_attached_below_a_unique_index() {
    for root in [
        "CREATE TABLE keyed (id INT PRIMARY KEY) PARTITION BY RANGE (id);",
        "CREATE TABLE keyed (id INT, UNIQUE (id)) PARTITION BY RANGE (id);",
        "CREATE TABLE keyed (id INT NOT NULL) PARTITION BY RANGE (id);
         CREATE UNIQUE INDEX keyed_id ON keyed (id);",
    ] {
        let sql = format!(
            "{root} CREATE FOREIGN TABLE f (id INT NOT NULL) SERVER s;
             ALTER TABLE keyed ATTACH PARTITION f FOR VALUES FROM (0) TO (10);"
        );
        let error = refusal(&sql);
        assert!(
            matches!(
                &error,
                Error::PartitionNotAttachable {
                    table_name,
                    partition_name,
                    reason: UnattachablePartition::ForeignTableUnderUniqueIndex,
                } if table_name == "keyed" && partition_name == "f"
            ),
            "{root}: {error:?}"
        );
    }

    // A partitioned table carrying a foreign partition cannot take the index
    // either.
    let error = refusal(&format!(
        "{PARTITIONED} {ATTACHED}
         CREATE TABLE keyed (id INT PRIMARY KEY) PARTITION BY RANGE (id);
         ALTER TABLE keyed ATTACH PARTITION orders FOR VALUES FROM (0) TO (100);"
    ));
    assert!(
        matches!(
            &error,
            Error::UniqueIndexOverForeignPartition { table_name, partition_name }
                if table_name == "orders" && partition_name == "remote_orders"
        ),
        "{error:?}"
    );
}

#[test]
fn a_foreign_table_is_not_attached_below_a_foreign_key() {
    let error = refusal(
        "CREATE TABLE target (id INT PRIMARY KEY);
         CREATE TABLE referencing (id INT REFERENCES target (id)) PARTITION BY RANGE (id);
         CREATE FOREIGN TABLE f (id INT) SERVER s;
         ALTER TABLE referencing ATTACH PARTITION f FOR VALUES FROM (0) TO (10);",
    );
    assert!(
        matches!(
            &error,
            Error::ConstraintUnsupportedOnForeignTable { table_name, constraint: KeyConstraint::ForeignKey }
                if table_name == "f"
        ),
        "{error:?}"
    );
}

#[test]
fn a_unique_index_does_not_reach_a_foreign_partition() {
    let nested = "CREATE TABLE outer_root (id INT NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE middle PARTITION OF outer_root FOR VALUES FROM (0) TO (100)
             PARTITION BY RANGE (id);
         CREATE FOREIGN TABLE leaf (id INT NOT NULL) SERVER s;
         ALTER TABLE middle ATTACH PARTITION leaf FOR VALUES FROM (0) TO (10);";
    for (setup, statement, table_name, partition_name) in [
        (PARTITIONED, "CREATE UNIQUE INDEX orders_uq ON orders (id);", "orders", "remote_orders"),
        (PARTITIONED, "ALTER TABLE orders ADD PRIMARY KEY (id);", "orders", "remote_orders"),
        (
            PARTITIONED,
            "ALTER TABLE orders ADD CONSTRAINT u UNIQUE (id);",
            "orders",
            "remote_orders",
        ),
        (PARTITIONED, "ALTER TABLE orders ADD COLUMN code INT UNIQUE;", "orders", "remote_orders"),
        (nested, "CREATE UNIQUE INDEX outer_uq ON outer_root (id);", "outer_root", "leaf"),
    ] {
        let attached = if setup == PARTITIONED { ATTACHED } else { "" };
        let sql = format!("{setup} {attached} {statement}");
        let error = refusal(&sql);
        assert!(
            matches!(
                &error,
                Error::UniqueIndexOverForeignPartition { table_name: table, partition_name: partition }
                    if table == table_name && partition == partition_name
            ),
            "{statement}: {error:?}"
        );
    }

    let error = refusal(&format!(
        "{PARTITIONED} {ATTACHED}
         ALTER TABLE orders ADD CONSTRAINT k FOREIGN KEY (id) REFERENCES target (id);"
    ));
    assert!(
        matches!(
            &error,
            Error::ConstraintUnsupportedOnForeignTable { table_name, constraint: KeyConstraint::ForeignKey }
                if table_name == "remote_orders"
        ),
        "{error:?}"
    );
}

#[test]
fn a_foreign_partition_goes_with_its_partitioned_table_and_leaves_it_alone() {
    let partitioned = format!("{PARTITIONED} {ATTACHED}");
    let dropped_root = db(&format!("{partitioned} DROP TABLE orders;"));
    assert_eq!(dropped_root.foreign_tables().count(), 0);

    let dropped_partition = db(&format!("{partitioned} DROP FOREIGN TABLE remote_orders;"));
    assert_eq!(
        table(&dropped_partition, "orders").partitions(&dropped_partition).expect("held").count(),
        0
    );

    // With the foreign partition gone, the unique index is accepted again.
    db(&format!(
        "{partitioned} DROP FOREIGN TABLE remote_orders; ALTER TABLE orders ADD PRIMARY KEY (id);"
    ));
}

const PARENT: &str = "CREATE TABLE parent (
         id INT NOT NULL,
         note TEXT DEFAULT 'x',
         CONSTRAINT positive CHECK (id > 0),
         CONSTRAINT parent_pk PRIMARY KEY (id)
     );";

#[test]
fn a_foreign_table_inherits_a_parent_columns_and_checks_but_not_its_keys() {
    let db = db(&format!(
        "{PARENT}
         CREATE TABLE target (id INT PRIMARY KEY);
         CREATE TABLE keyed (id INT REFERENCES target (id), code INT UNIQUE);
         CREATE FOREIGN TABLE child (extra INT) INHERITS (parent) SERVER warehouse;
         CREATE FOREIGN TABLE keyed_child () INHERITS (keyed) SERVER warehouse;"
    ));
    let parent = table(&db, "parent");
    let child = foreign(&db, "child");
    assert!(child.is_foreign(&db).expect("held"));
    assert_eq!(child.inherits_from(&db).expect("held").collect::<Vec<_>>(), [parent]);
    assert_eq!(parent.inheritors(&db).expect("held").collect::<Vec<_>>(), [child]);

    let columns: Vec<_> = child
        .columns(&db)
        .expect("held")
        .map(|column| (column.column_name(), column.is_nullable(&db).expect("held")))
        .collect();
    assert_eq!(columns, [("id", false), ("note", true), ("extra", true)]);
    assert_eq!(child.check_constraints(&db).expect("held").count(), 1);
    assert!(!child.has_primary_key(&db).expect("held"));

    let keyed_child = foreign(&db, "keyed_child");
    assert_eq!(keyed_child.foreign_keys(&db).expect("held").count(), 0);
    assert_eq!(keyed_child.unique_indices(&db).expect("held").count(), 0);
}

#[test]
fn a_foreign_table_inherits_from_several_parents_and_from_a_foreign_table() {
    let db = db(&format!(
        "{PARENT}
         CREATE TABLE coded (code INT CHECK (code <> 0), CHECK (code > 1) NO INHERIT);
         CREATE FOREIGN TABLE both_parents (id INT, note TEXT) INHERITS (parent, coded)
             SERVER warehouse;
         CREATE FOREIGN TABLE remote_parent (a INT) SERVER warehouse;
         CREATE FOREIGN TABLE remote_child (b INT) INHERITS (remote_parent) SERVER warehouse;"
    ));
    let both = foreign(&db, "both_parents");
    let names: Vec<_> = both.columns(&db).expect("held").map(ColumnLike::column_name).collect();
    assert_eq!(names, ["id", "note", "code"]);
    // The `NO INHERIT` check stays with its table.
    assert_eq!(both.check_constraints(&db).expect("held").count(), 2);

    let remote_child = foreign(&db, "remote_child");
    assert_eq!(
        remote_child.inherits_from(&db).expect("held").collect::<Vec<_>>(),
        [foreign(&db, "remote_parent")]
    );
    assert_eq!(remote_child.columns(&db).expect("held").count(), 2);
}

#[test]
fn a_foreign_table_inherits_only_from_a_table_postgresql_lets_it() {
    for (sql, check) in [
        ("CREATE FOREIGN TABLE c () INHERITS (nowhere) SERVER s;", "an absent parent"),
        (
            "CREATE VIEW v AS SELECT 1 AS a; CREATE FOREIGN TABLE c () INHERITS (v) SERVER s;",
            "a view",
        ),
        (
            "CREATE TEMP TABLE t (id INT); CREATE FOREIGN TABLE c () INHERITS (t) SERVER s;",
            "a temporary table",
        ),
        (
            &format!("{PARENT} CREATE FOREIGN TABLE c (id BIGINT) INHERITS (parent) SERVER s;"),
            "a column type conflict",
        ),
    ] {
        assert!(parse(sql).is_err(), "{check} was accepted");
    }
}

#[test]
fn a_parent_change_reaches_its_foreign_child() {
    let inherited =
        format!("{PARENT} CREATE FOREIGN TABLE child () INHERITS (parent) SERVER warehouse;");
    let altered = db(&format!(
        "{inherited}
         ALTER TABLE parent ADD COLUMN added INT;
         ALTER TABLE parent ADD CONSTRAINT small CHECK (id < 100);
         CREATE UNIQUE INDEX parent_note ON parent (note);
         ALTER TABLE parent ENABLE ROW LEVEL SECURITY;"
    ));
    let child = foreign(&altered, "child");
    let names: Vec<_> =
        child.columns(&altered).expect("held").map(ColumnLike::column_name).collect();
    assert_eq!(names, ["id", "note", "added"]);
    assert_eq!(child.check_constraints(&altered).expect("held").count(), 2);
    assert_eq!(child.indices(&altered).expect("held").count(), 0);

    // A read of the parent reaches the child's remote rows, which the base
    // table answer leaves out as `tables()` does.
    let query = sqlparser::parser::Parser::new(&PostgreSqlDialect {})
        .try_with_sql("SELECT * FROM parent")
        .expect("tokenizes")
        .parse_query()
        .expect("parses");
    assert_eq!(query.referenced_tables(&altered).expect("resolves"), [table(&altered, "parent")]);

    assert!(matches!(
        refusal(&format!("{inherited} DROP TABLE parent;")),
        Error::DropTableInheritedFrom { .. }
    ));
    let cascaded = db(&format!("{inherited} DROP TABLE parent CASCADE;"));
    assert_eq!(cascaded.foreign_tables().count(), 0);
    let child_dropped = db(&format!("{inherited} DROP FOREIGN TABLE child;"));
    assert_eq!(
        table(&child_dropped, "parent").inheritors(&child_dropped).expect("held").count(),
        0
    );
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
