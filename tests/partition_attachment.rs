//! Tests that `ALTER TABLE root ATTACH PARTITION part` leaves the catalog
//! PostgreSQL 18 leaves.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ObjectKind, UnattachablePartition},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_names.rs"));

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

fn table<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreateTable {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
}

fn root_name(database: &ParserDB, name: &str) -> Option<String> {
    table(database, name)
        .partition_root(database)
        .expect("table is in this database")
        .map(|root| root.table_name().to_owned())
}

fn primary_key(database: &ParserDB, name: &str) -> Vec<String> {
    table(database, name)
        .primary_key_columns(database)
        .expect("table is in this database")
        .map(|column| column.column_name().to_owned())
        .collect()
}

fn unique_index_names(database: &ParserDB, name: &str) -> Vec<String> {
    let mut names: Vec<String> = table(database, name)
        .unique_indices(database)
        .expect("table is in this database")
        .map(|index| index.name().expect("every key here is named").to_owned())
        .collect();
    names.sort();
    names
}

fn foreign_key_names(database: &ParserDB, name: &str) -> Vec<String> {
    let mut names: Vec<String> = table(database, name)
        .foreign_keys(database)
        .expect("table is in this database")
        .map(|key| key.foreign_key_name().expect("every key here is named").to_owned())
        .collect();
    names.sort();
    names
}

fn carries_identity(database: &ParserDB, name: &str, column: &str) -> bool {
    table(database, name)
        .columns
        .iter()
        .find(|declared| declared.name.value == column)
        .expect("column exists")
        .options
        .iter()
        .any(|option| sql_traits::utils::is_identity(&option.option))
}

fn refusal(sql: &str) -> UnattachablePartition {
    match parse(sql) {
        Err(Error::PartitionNotAttachable { table_name, partition_name, reason }) => {
            assert_eq!((table_name.as_str(), partition_name.as_str()), ("p", "c"), "{sql}");
            reason
        }
        other => panic!("{sql} answered {other:?}"),
    }
}

#[test]
fn an_attached_table_answers_as_a_partition_of_the_root() {
    let database = database(
        "CREATE TABLE p (a INT, b TEXT) PARTITION BY RANGE (a);
         CREATE TABLE c (b TEXT, a INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (9);",
    );

    assert_eq!(root_name(&database, "c").as_deref(), Some("p"));
    assert!(table(&database, "c").is_partition(&database).expect("in database"));
    let partitions: Vec<_> = table(&database, "p")
        .partitions(&database)
        .expect("in database")
        .map(|partition| partition.table_name().to_owned())
        .collect();
    assert_eq!(partitions, ["c"]);
    assert_eq!(table(&database, "p").inheritors(&database).expect("in database").count(), 0);

    // The table keeps its own column order, and every column now belongs to the
    // root.
    assert_eq!(column_names(&database, "c"), ["b", "a"]);
    assert_eq!(table(&database, "c").local_columns(&database).expect("in database").count(), 0);
}

#[test]
fn an_attached_table_answers_like_one_created_as_a_partition() {
    let attached = database(
        "CREATE TABLE p (a INT, b TEXT) PARTITION BY LIST (a);
         CREATE TABLE c (a INT, b TEXT);
         ALTER TABLE ONLY p ATTACH PARTITION c DEFAULT;",
    );
    let created = database(
        "CREATE TABLE p (a INT, b TEXT) PARTITION BY LIST (a);
         CREATE TABLE c PARTITION OF p DEFAULT;",
    );

    for database in [&attached, &created] {
        assert_eq!(root_name(database, "c").as_deref(), Some("p"));
        assert_eq!(column_names(database, "c"), ["a", "b"]);
        assert_eq!(table(database, "c").local_columns(database).expect("in database").count(), 0);
    }
    assert_eq!(table(&attached, "c").for_values, table(&created, "c").for_values);
}

#[test]
fn the_roots_keys_and_foreign_keys_reach_the_attached_table() {
    let database = database(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (
             id INT PRIMARY KEY,
             code TEXT,
             rid INT REFERENCES r (id),
             sid INT,
             UNIQUE (id, code),
             CONSTRAINT p_sid_fkey FOREIGN KEY (sid) REFERENCES r (id)
         ) PARTITION BY RANGE (id);
         CREATE TABLE c (id INT NOT NULL, code TEXT, rid INT, sid INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (9);",
    );

    assert_eq!(primary_key(&database, "c"), ["id"]);
    assert_eq!(unique_index_names(&database, "c"), ["c_id_code_key", "c_pkey"]);
    let foreign_keys: Vec<_> = table(&database, "c")
        .foreign_keys(&database)
        .expect("in database")
        .map(|key| {
            let columns: Vec<&str> = key
                .host_columns(&database)
                .expect("in database")
                .map(ColumnLike::column_name)
                .collect();
            (columns, key.foreign_key_name())
        })
        .collect();
    assert_eq!(foreign_keys.len(), 2);
    assert!(foreign_keys.contains(&(vec!["sid"], Some("p_sid_fkey"))));
    assert!(foreign_keys.iter().any(|(columns, _)| columns == &["rid"]));
}

#[test]
fn a_key_the_attached_table_already_holds_is_kept_rather_than_doubled() {
    // A key written inline on the root matches one written as a table
    // constraint below.
    let database = database(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (id INT PRIMARY KEY, code TEXT UNIQUE, rid INT REFERENCES r (id))
             PARTITION BY LIST (id);
         CREATE TABLE c (
             id INT NOT NULL,
             code TEXT,
             rid INT,
             CONSTRAINT mine PRIMARY KEY (id),
             CONSTRAINT theirs FOREIGN KEY (rid) REFERENCES r (id)
         );
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );

    assert_eq!(primary_key(&database, "c"), ["id"]);
    assert_eq!(unique_index_names(&database, "c"), ["c_code_key", "mine"]);
    assert_eq!(foreign_key_names(&database, "c"), ["theirs"]);
}

#[test]
fn a_unique_constraint_does_not_stand_in_for_the_roots_primary_key() {
    let database = database(
        "CREATE TABLE p (id INT PRIMARY KEY) PARTITION BY LIST (id);
         CREATE TABLE c (id INT NOT NULL CONSTRAINT c_id_key UNIQUE);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );

    assert_eq!(primary_key(&database, "c"), ["id"]);
    assert_eq!(unique_index_names(&database, "c"), ["c_id_key", "c_pkey"]);
}

#[test]
fn a_generated_key_name_steps_past_a_taken_one() {
    let database = database(
        "CREATE TABLE p (id INT PRIMARY KEY) PARTITION BY LIST (id);
         CREATE TABLE c (id INT NOT NULL);
         CREATE TABLE c_pkey (x INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );

    assert_eq!(unique_index_names(&database, "c"), ["c_pkey1"]);
}

#[test]
fn the_roots_identity_reaches_the_attached_table_but_not_its_partitions() {
    let database = database(
        "CREATE TABLE p (id INT GENERATED ALWAYS AS IDENTITY, r INT) PARTITION BY RANGE (id);
         CREATE TABLE c (id INT NOT NULL, r INT) PARTITION BY LIST (r);
         CREATE TABLE cc PARTITION OF c FOR VALUES IN (1);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (9);",
    );

    assert!(carries_identity(&database, "c", "id"));
    assert!(!carries_identity(&database, "cc", "id"));
}

#[test]
fn the_roots_keys_reach_the_partitions_of_an_attached_partitioned_table() {
    let database = database(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (
             id INT PRIMARY KEY,
             rid INT,
             CONSTRAINT p_rid_fkey FOREIGN KEY (rid) REFERENCES r
         ) PARTITION BY RANGE (id);
         CREATE TABLE c (id INT NOT NULL, rid INT) PARTITION BY LIST (id);
         CREATE TABLE cc PARTITION OF c FOR VALUES IN (1);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES FROM (1) TO (9);",
    );

    assert_eq!(root_name(&database, "cc").as_deref(), Some("c"));
    assert_eq!(unique_index_names(&database, "c"), ["c_pkey"]);
    assert_eq!(unique_index_names(&database, "cc"), ["cc_pkey"]);
    assert_eq!(foreign_key_names(&database, "c"), ["p_rid_fkey"]);
    assert_eq!(foreign_key_names(&database, "cc"), ["p_rid_fkey"]);
}

#[test]
fn a_default_stays_with_the_table_that_declares_it() {
    let database = database(
        "CREATE TABLE p (a INT DEFAULT 5, b INT DEFAULT 6) PARTITION BY LIST (a);
         CREATE TABLE c (a INT DEFAULT 7, b INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );

    let default = |column: &str| {
        table(&database, "c")
            .columns
            .iter()
            .find(|declared| declared.name.value == column)
            .expect("column exists")
            .options
            .iter()
            .find_map(|option| {
                match &option.option {
                    sqlparser::ast::ColumnOption::Default(expr) => Some(expr.to_string()),
                    _ => None,
                }
            })
    };
    assert_eq!(default("a").as_deref(), Some("7"));
    assert_eq!(default("b"), None);
}

#[test]
fn a_matching_check_becomes_the_roots_to_drop() {
    let database = database(
        "CREATE TABLE p (id INT, CONSTRAINT pos CHECK (id > 0)) PARTITION BY LIST (id);
         CREATE TABLE c (id INT, CONSTRAINT pos CHECK (id > 0), CONSTRAINT own CHECK (id < 9));
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );
    assert_eq!(table(&database, "c").check_constraints(&database).expect("in database").count(), 2);

    let sql = "CREATE TABLE p (id INT, CONSTRAINT pos CHECK (id > 0)) PARTITION BY LIST (id);
         CREATE TABLE c (id INT, CONSTRAINT pos CHECK (id > 0), CONSTRAINT own CHECK (id < 9));
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);";
    assert!(matches!(
        parse(&format!("{sql} ALTER TABLE c DROP CONSTRAINT pos;")),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
    parse(&format!("{sql} ALTER TABLE c DROP CONSTRAINT own;")).expect("its own check is its own");
}

#[test]
fn later_changes_to_the_root_reach_the_attached_table() {
    let database = database(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (id INT) PARTITION BY LIST (id);
         CREATE TABLE c (id INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);
         ALTER TABLE p ADD COLUMN note TEXT;
         ALTER TABLE p ADD CONSTRAINT p_id_fkey FOREIGN KEY (id) REFERENCES r (id);
         ALTER TABLE p RENAME TO q;",
    );

    assert_eq!(column_names(&database, "c"), ["id", "note"]);
    assert_eq!(foreign_key_names(&database, "c"), ["p_id_fkey"]);
    assert_eq!(root_name(&database, "c").as_deref(), Some("q"));
    assert!(matches!(
        parse(
            "CREATE TABLE p (id INT) PARTITION BY LIST (id);
             CREATE TABLE c (id INT);
             ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);
             ALTER TABLE c DROP COLUMN id;"
        ),
        Err(Error::InheritedColumnNotDroppable { .. })
    ));
}

#[test]
fn a_pg_dump_of_a_partitioned_table_reads_back() {
    // `pg_dump` writes every partition as a plain table and attaches it
    // afterwards.
    let database = database(
        "CREATE TABLE public.p (
             id integer NOT NULL,
             k integer NOT NULL,
             rid integer,
             note text DEFAULT 'x'::text,
             CONSTRAINT pos CHECK ((k > 0))
         )
         PARTITION BY RANGE (k);
         CREATE TABLE public.c1 (
             id integer CONSTRAINT p_id_not_null NOT NULL,
             k integer CONSTRAINT p_k_not_null NOT NULL,
             rid integer,
             note text DEFAULT 'x'::text,
             CONSTRAINT pos CHECK ((k > 0))
         );
         CREATE TABLE public.cdef (
             id integer CONSTRAINT p_id_not_null NOT NULL,
             k integer CONSTRAINT p_k_not_null NOT NULL,
             rid integer,
             note text DEFAULT 'x'::text,
             CONSTRAINT pos CHECK ((k > 0))
         );
         ALTER TABLE public.p ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;
         CREATE TABLE public.r (id integer NOT NULL);
         ALTER TABLE ONLY public.p ATTACH PARTITION public.c1 FOR VALUES FROM (1) TO (10);
         ALTER TABLE ONLY public.p ATTACH PARTITION public.cdef DEFAULT;
         ALTER TABLE ONLY public.r ADD CONSTRAINT r_pkey PRIMARY KEY (id);
         ALTER TABLE public.p ADD CONSTRAINT p_rid_fkey FOREIGN KEY (rid) REFERENCES public.r(id);",
    );

    for partition in ["c1", "cdef"] {
        assert_eq!(root_name(&database, partition).as_deref(), Some("p"));
        assert!(carries_identity(&database, partition, "id"));
        assert_eq!(foreign_key_names(&database, partition), ["p_rid_fkey"]);
    }
}

#[test]
fn an_absent_root_is_excused_by_if_exists_and_an_absent_partition_never() {
    let database = database(
        "CREATE TABLE c (id INT);
         ALTER TABLE IF EXISTS p ATTACH PARTITION c FOR VALUES IN (1);",
    );
    assert_eq!(root_name(&database, "c"), None);

    assert!(matches!(
        parse("CREATE TABLE c (id INT); ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);"),
        Err(Error::AlterTableNotFound { table_name }) if table_name == "p"
    ));
    assert!(matches!(
        parse(
            "CREATE TABLE p (id INT) PARTITION BY LIST (id);
             ALTER TABLE IF EXISTS p ATTACH PARTITION c FOR VALUES IN (1);"
        ),
        Err(Error::RelationNotFound { object_kind: ObjectKind::Table, object_name })
            if object_name == "c"
    ));
}

#[test]
fn only_a_table_can_be_attached_and_only_to_a_partitioned_one() {
    let refused = |sql: &str, kind: ObjectKind, relation: &str| {
        match parse(sql) {
            Err(Error::AlterActionUnsupportedOnRelation { object_kind, relation_name, .. }) => {
                assert_eq!((object_kind, relation_name.as_str()), (kind, relation), "{sql}");
            }
            other => panic!("{sql} answered {other:?}"),
        }
    };
    refused(
        "CREATE TABLE p (id INT); CREATE TABLE c (id INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
        ObjectKind::Table,
        "p",
    );
    refused(
        "CREATE TABLE p (id INT) PARTITION BY LIST (id); CREATE VIEW c AS SELECT 1 AS id;
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
        ObjectKind::View,
        "c",
    );
    refused(
        "CREATE TABLE p (id INT) PARTITION BY LIST (id);
         CREATE MATERIALIZED VIEW c AS SELECT 1 AS id;
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
        ObjectKind::MaterializedView,
        "c",
    );
}

#[test]
fn a_table_already_linked_elsewhere_is_refused() {
    let root = "CREATE TABLE p (id INT) PARTITION BY LIST (id);";
    let attach = "ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);";
    let cases = [
        (
            "CREATE TABLE c (id INT); ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (2);",
            UnattachablePartition::AlreadyPartition,
        ),
        (
            "CREATE TABLE q (id INT); CREATE TABLE c () INHERITS (q);",
            UnattachablePartition::InheritanceChild,
        ),
        (
            "CREATE TABLE c (id INT); CREATE TABLE k () INHERITS (c);",
            UnattachablePartition::InheritanceParent,
        ),
    ];
    for (setup, expected) in cases {
        assert_eq!(refusal(&format!("{root} {setup} {attach}")), expected, "{setup}");
    }

    assert_eq!(
        refusal(
            "CREATE TABLE c (id INT) PARTITION BY LIST (id);
             CREATE TABLE p PARTITION OF c FOR VALUES IN (1) PARTITION BY LIST (id);
             ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);"
        ),
        UnattachablePartition::Circular
    );
    assert!(matches!(
        parse(
            "CREATE TABLE p (id INT) PARTITION BY LIST (id);
               ALTER TABLE p ATTACH PARTITION p FOR VALUES IN (1);"
        ),
        Err(Error::PartitionNotAttachable { reason: UnattachablePartition::Circular, .. })
    ));
}

#[test]
fn temporary_and_permanent_tables_do_not_mix() {
    assert!(matches!(
        parse(
            "CREATE TABLE p (id INT) PARTITION BY LIST (id); CREATE TEMP TABLE c (id INT);
             ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);"
        ),
        Err(Error::TemporaryPartitionOfPermanent { table_name, parent_name })
            if table_name == "c" && parent_name == "p"
    ));
    assert!(matches!(
        parse(
            "CREATE TEMP TABLE p (id INT) PARTITION BY LIST (id); CREATE TABLE c (id INT);
             ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);"
        ),
        Err(Error::PermanentRelationInheritsTemporary { table_name, parent_name })
            if table_name == "c" && parent_name == "p"
    ));
    database(
        "CREATE TEMP TABLE p (id INT) PARTITION BY LIST (id); CREATE TEMP TABLE c (id INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );
    database(
        "CREATE TABLE p (id INT) PARTITION BY LIST (id); CREATE UNLOGGED TABLE c (id INT);
         ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);",
    );
}

#[test]
fn the_columns_have_to_match_the_roots() {
    let attach = "ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);";
    let column = |name: &str| name.to_owned();
    let cases = [
        ("CREATE TABLE p (id INT) PARTITION BY LIST (id); CREATE TABLE c (id INT, x INT);",
         UnattachablePartition::ExtraColumn { column_name: column("x") }),
        ("CREATE TABLE p (id INT, y INT) PARTITION BY LIST (id); CREATE TABLE c (id INT);",
         UnattachablePartition::MissingColumn { column_name: column("y") }),
        ("CREATE TABLE p (id INT) PARTITION BY LIST (id);
          CREATE TABLE c (id INT GENERATED ALWAYS AS IDENTITY);",
         UnattachablePartition::IdentityColumn { column_name: column("id") }),
        ("CREATE TABLE p (id INT NOT NULL) PARTITION BY LIST (id); CREATE TABLE c (id INT);",
         UnattachablePartition::NullableColumn { column_name: column("id") }),
        ("CREATE TABLE p (id INT PRIMARY KEY) PARTITION BY LIST (id); CREATE TABLE c (id INT);",
         UnattachablePartition::NullableColumn { column_name: column("id") }),
        ("CREATE TABLE p (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED) PARTITION BY LIST (a);
          CREATE TABLE c (a INT, b INT);",
         UnattachablePartition::GenerationMismatch {
             column_name: column("b"),
             generated_in_root: true,
         }),
        ("CREATE TABLE p (a INT, b INT) PARTITION BY LIST (a);
          CREATE TABLE c (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED);",
         UnattachablePartition::GenerationMismatch {
             column_name: column("b"),
             generated_in_root: false,
         }),
        ("CREATE TABLE p (id INT, b INT, PRIMARY KEY (id)) PARTITION BY LIST (id);
          CREATE TABLE c (id INT NOT NULL, b INT PRIMARY KEY);",
         UnattachablePartition::OtherPrimaryKey),
    ];
    for (setup, expected) in cases {
        assert_eq!(refusal(&format!("{setup} {attach}")), expected, "{setup}");
    }

    assert!(matches!(
        parse(&format!(
            "CREATE TABLE p (id INT) PARTITION BY LIST (id); CREATE TABLE c (id BIGINT); {attach}"
        )),
        Err(Error::InheritedColumnTypeConflict { column_name, child_table, .. })
            if column_name == "id" && child_table == "c"
    ));
    assert!(matches!(
        parse(&format!(
            "CREATE TABLE p (a TEXT COLLATE \"C\") PARTITION BY LIST (a);
             CREATE TABLE c (a TEXT); {attach}"
        )),
        Err(Error::InheritedColumnCollationConflict { .. })
    ));
    // A generated column may compute its value its own way.
    database(&format!(
        "CREATE TABLE p (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED) PARTITION BY LIST (a);
         CREATE TABLE c (a INT, b INT GENERATED ALWAYS AS (a * 3) STORED); {attach}"
    ));
}

#[test]
fn the_roots_checks_have_to_be_held_already() {
    let attach = "ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);";
    let root = "CREATE TABLE p (id INT, CONSTRAINT ck CHECK (id > 0)) PARTITION BY LIST (id);";

    assert_eq!(
        refusal(&format!("{root} CREATE TABLE c (id INT); {attach}")),
        UnattachablePartition::MissingCheck {
            constraint: "CONSTRAINT ck CHECK (id > 0)".to_owned()
        }
    );
    assert_eq!(
        refusal(&format!(
            "{root} CREATE TABLE c (id INT, CONSTRAINT other CHECK (id > 0)); {attach}"
        )),
        UnattachablePartition::MissingCheck {
            constraint: "CONSTRAINT ck CHECK (id > 0)".to_owned()
        }
    );
    for held in ["CONSTRAINT ck CHECK (id > 1)", "CONSTRAINT ck CHECK (id > 0) NO INHERIT"] {
        assert!(
            matches!(
                parse(&format!("{root} CREATE TABLE c (id INT, {held}); {attach}")),
                Err(Error::InheritedConstraintConflict { table_name, constraint_name })
                    if table_name == "c" && constraint_name == "ck"
            ),
            "{held}"
        );
    }
    assert!(matches!(
        parse(&format!(
            "{root} CREATE TABLE c (id INT);
             ALTER TABLE c ADD CONSTRAINT ck CHECK (id > 0) NOT VALID; {attach}"
        )),
        Err(Error::NotValidConstraintConflict { .. })
    ));
    database(&format!(
        "CREATE TABLE p (id INT) PARTITION BY LIST (id);
         ALTER TABLE p ADD CONSTRAINT ck CHECK (id > 0) NOT VALID;
         CREATE TABLE c (id INT, CONSTRAINT ck CHECK (id > 0)); {attach}"
    ));
    // A check written on the column is held whichever way the table writes it.
    database(&format!(
        "CREATE TABLE p (id INT CONSTRAINT ck CHECK (id > 0)) PARTITION BY LIST (id);
         CREATE TABLE c (id INT, CONSTRAINT ck CHECK (id > 0)); {attach}"
    ));
}
