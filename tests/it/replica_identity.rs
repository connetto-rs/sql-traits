//! Tests that a table's replica identity follows every statement that sets it
//! or reaches the index it names.
//!
//! Every rule asserted here was measured against PostgreSQL 18.6 in Docker,
//! reading `pg_class.relreplident` and `pg_index.indisreplident`. A table
//! starts at `DEFAULT`, `ALTER TABLE ... REPLICA IDENTITY` moves it, and
//! `USING INDEX` takes only a unique, immediate, non-partial index over plain
//! columns that require a value. Dropping that index leaves the setting naming
//! no index, which PostgreSQL treats as `NOTHING`, and renaming it keeps it.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ReplicaIdentityRefusal, RequiredValue},
    prelude::*,
};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn db(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    let target = TargetName::new(name, false);
    db.resolve_target_table(target.clone(), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .or_else(|| {
            db.foreign_table_by_target(target, IdentifierCase::AsWritten)
                .expect("unambiguous lookup")
        })
        .expect("the name resolves")
}

/// The replica identity of `name` after `sql`, spelled the way
/// `relreplident` and the index carrying `indisreplident` read.
fn identity_of(sql: &str, name: &str) -> String {
    let database = db(sql);
    let identity = table(&database, name).replica_identity(&database).expect("t is held");
    match identity {
        ReplicaIdentity::Default => "default".to_owned(),
        ReplicaIdentity::Nothing => "nothing".to_owned(),
        ReplicaIdentity::Full => "full".to_owned(),
        ReplicaIdentity::Index(None) => "index -".to_owned(),
        ReplicaIdentity::Index(Some(ReplicaIdentityIndex::Index(index))) => {
            format!("index {}", index.name().expect("a usable index is named"))
        }
        ReplicaIdentity::Index(Some(ReplicaIdentityIndex::UniqueIndex(key))) => {
            format!(
                "key {}",
                key.constraint_name(&database).expect("the key is held").expect("a key is named")
            )
        }
    }
}

fn refusal(sql: &str) -> Error {
    match parse(sql) {
        Ok(_) => panic!("{sql} was accepted"),
        Err(error) => error,
    }
}

const UNIQUE_INDEX: &str = "CREATE TABLE t (a INT NOT NULL, b INT); CREATE UNIQUE INDEX i ON t (a);
     ALTER TABLE t REPLICA IDENTITY USING INDEX i;";

const UNIQUE_KEY: &str = "CREATE TABLE t (a INT NOT NULL, b INT, CONSTRAINT k UNIQUE (a));
     ALTER TABLE t REPLICA IDENTITY USING INDEX k;";

#[test]
fn a_table_starts_at_default_and_moves_to_each_setting() {
    let table = "CREATE TABLE t (id INT PRIMARY KEY);";
    assert_eq!(identity_of(table, "t"), "default");
    for (setting, read) in [("NOTHING", "nothing"), ("FULL", "full"), ("DEFAULT", "default")] {
        let sql = format!(
            "{table} ALTER TABLE t REPLICA IDENTITY FULL; ALTER TABLE t REPLICA IDENTITY {setting};"
        );
        assert_eq!(identity_of(&sql, "t"), read, "{setting}");
    }
    let both = format!("{table} ALTER TABLE t REPLICA IDENTITY FULL, REPLICA IDENTITY NOTHING;");
    assert_eq!(identity_of(&both, "t"), "nothing");
    let partitioned = "CREATE TABLE t (a INT NOT NULL) PARTITION BY RANGE (a);
         ALTER TABLE t REPLICA IDENTITY FULL;";
    assert_eq!(identity_of(partitioned, "t"), "full");
}

#[test]
fn using_index_names_a_unique_index_or_the_index_of_a_key() {
    assert_eq!(identity_of(UNIQUE_INDEX, "t"), "index i");
    assert_eq!(identity_of(UNIQUE_KEY, "t"), "key k");
    let primary_key = "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t REPLICA IDENTITY USING INDEX t_pkey;";
    assert_eq!(identity_of(primary_key, "t"), "key t_pkey");
    let generated_unique = "CREATE TABLE t (a INT NOT NULL UNIQUE);
         ALTER TABLE t REPLICA IDENTITY USING INDEX t_a_key;";
    assert_eq!(identity_of(generated_unique, "t"), "key t_a_key");
    let table_primary_key = "CREATE TABLE t (id INT, PRIMARY KEY (id));
         ALTER TABLE t REPLICA IDENTITY USING INDEX t_pkey;";
    assert_eq!(identity_of(table_primary_key, "t"), "key t_pkey");
    // The named key is found past an earlier key of the same table.
    let second_key = "CREATE TABLE t (id INT PRIMARY KEY, a INT NOT NULL, CONSTRAINT k UNIQUE (a));
         ALTER TABLE t REPLICA IDENTITY USING INDEX k;";
    assert_eq!(identity_of(second_key, "t"), "key k");
    // Only the key columns must require a value, and a unique index that
    // treats nulls as equal is unique all the same.
    let including = "CREATE TABLE t (a INT NOT NULL, b INT);
         CREATE UNIQUE INDEX i ON t (a) INCLUDE (b);
         ALTER TABLE t REPLICA IDENTITY USING INDEX i;";
    assert_eq!(identity_of(including, "t"), "index i");
    let nulls_equal = "CREATE TABLE t (a INT NOT NULL);
         CREATE UNIQUE INDEX i ON t (a) NULLS NOT DISTINCT;
         ALTER TABLE t REPLICA IDENTITY USING INDEX i;";
    assert_eq!(identity_of(nulls_equal, "t"), "index i");
    let qualified = "CREATE SCHEMA s; CREATE TABLE s.t (a INT NOT NULL);
         CREATE UNIQUE INDEX i ON s.t (a);
         ALTER TABLE s.t REPLICA IDENTITY USING INDEX i;";
    let database = db(qualified);
    let t = database
        .resolve_target_table(
            TargetName::new("t", false).with_schema("s", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("s.t resolves");
    assert!(matches!(
        t.replica_identity(&database),
        Ok(ReplicaIdentity::Index(Some(ReplicaIdentityIndex::Index(_))))
    ));
}

#[test]
fn using_index_refuses_what_postgres_refuses() {
    let refused = |sql: &str| {
        match refusal(sql) {
            Error::ReplicaIdentityIndexRefused { table_name, index_name, reason } => {
                assert_eq!((table_name.as_str(), index_name.as_str()), ("t", "i"), "{sql}");
                reason
            }
            other => panic!("{sql} reported {other:?}"),
        }
    };
    let over = |index: &str| {
        format!(
            "CREATE TABLE t (a INT NOT NULL, b INT); {index}
             ALTER TABLE t REPLICA IDENTITY USING INDEX i;"
        )
    };
    assert_eq!(refused(&over("CREATE INDEX i ON t (a);")), ReplicaIdentityRefusal::NotUnique);
    assert_eq!(
        refused(&over("CREATE UNIQUE INDEX i ON t (a) WHERE a > 0;")),
        ReplicaIdentityRefusal::Partial
    );
    assert_eq!(
        refused(&over("CREATE UNIQUE INDEX i ON t ((a + 1));")),
        ReplicaIdentityRefusal::Expression
    );
    assert_eq!(
        refused(&over("ALTER TABLE t ADD CONSTRAINT i EXCLUDE USING btree (a WITH =);")),
        ReplicaIdentityRefusal::NotUnique
    );
    for deferral in
        ["DEFERRABLE", "DEFERRABLE INITIALLY IMMEDIATE", "DEFERRABLE INITIALLY DEFERRED"]
    {
        assert_eq!(
            refused(&over(&format!("ALTER TABLE t ADD CONSTRAINT i UNIQUE (a) {deferral};"))),
            ReplicaIdentityRefusal::NotImmediate,
            "{deferral}"
        );
    }
    assert_eq!(
        refused(&over("CREATE UNIQUE INDEX i ON t (a, b);")),
        ReplicaIdentityRefusal::NullableColumn("b".to_owned())
    );
    assert_eq!(
        refused(&over("ALTER TABLE t ADD CONSTRAINT i UNIQUE (b);")),
        ReplicaIdentityRefusal::NullableColumn("b".to_owned())
    );
    // PostgreSQL's `REL_18_STABLE` refuses a key column whose requirement is
    // not validated, which 18.6 still accepts.
    let unvalidated = "ALTER TABLE t ADD CONSTRAINT nn NOT NULL b NOT VALID;";
    assert_eq!(
        refused(&over(&format!("{unvalidated} CREATE UNIQUE INDEX i ON t (a, b);"))),
        ReplicaIdentityRefusal::UnvalidatedNotNull("b".to_owned())
    );
    assert_eq!(
        identity_of(
            &over(&format!(
                "{unvalidated} ALTER TABLE t VALIDATE CONSTRAINT nn; CREATE UNIQUE INDEX i ON t (b);"
            )),
            "t"
        ),
        "index i"
    );
}

#[test]
fn using_index_refuses_a_name_that_is_no_index_of_the_table() {
    let missing =
        "CREATE TABLE t (a INT NOT NULL); ALTER TABLE t REPLICA IDENTITY USING INDEX nope;";
    assert!(matches!(
        refusal(missing),
        Error::ReplicaIdentityIndexNotFound { table_name, index_name }
            if table_name == "t" && index_name == "nope"
    ));
    // An unquoted name folds before it is looked up.
    let folded = "CREATE TABLE t (a INT NOT NULL); CREATE UNIQUE INDEX \"Idx\" ON t (a);
         ALTER TABLE t REPLICA IDENTITY USING INDEX Idx;";
    assert!(matches!(refusal(folded), Error::ReplicaIdentityIndexNotFound { .. }));
    let other_table = "CREATE TABLE t (a INT NOT NULL); CREATE TABLE u (a INT NOT NULL);
         CREATE UNIQUE INDEX i ON u (a); ALTER TABLE t REPLICA IDENTITY USING INDEX i;";
    assert!(matches!(
        refusal(other_table),
        Error::ReplicaIdentityIndexOfOtherTable { table_name, index_name }
            if table_name == "t" && index_name == "i"
    ));
    let other_key = "CREATE TABLE t (a INT NOT NULL); CREATE TABLE u (a INT PRIMARY KEY);
         ALTER TABLE t REPLICA IDENTITY USING INDEX u_pkey;";
    assert!(matches!(refusal(other_key), Error::ReplicaIdentityIndexOfOtherTable { .. }));
    let view_index = "CREATE TABLE t (a INT NOT NULL); CREATE MATERIALIZED VIEW m AS SELECT 1 AS a;
         CREATE UNIQUE INDEX mi ON m (a); ALTER TABLE t REPLICA IDENTITY USING INDEX mi;";
    assert!(matches!(refusal(view_index), Error::ReplicaIdentityIndexOfOtherTable { .. }));
    let a_table = "CREATE TABLE t (a INT NOT NULL); CREATE TABLE u (a INT);
         ALTER TABLE t REPLICA IDENTITY USING INDEX u;";
    assert!(matches!(refusal(a_table), Error::RelationKindMismatch { .. }));
}

#[test]
fn dropping_the_index_leaves_the_setting_naming_none() {
    for drop in [
        "DROP INDEX i;",
        "DROP INDEX i; CREATE UNIQUE INDEX i ON t (a);",
        "ALTER TABLE t DROP COLUMN a;",
        "ALTER TABLE t DROP COLUMN a; ALTER TABLE t ADD COLUMN a INT NOT NULL;
         CREATE UNIQUE INDEX i ON t (a);",
    ] {
        assert_eq!(identity_of(&format!("{UNIQUE_INDEX} {drop}"), "t"), "index -", "{drop}");
    }
    for drop in [
        "ALTER TABLE t DROP CONSTRAINT k;",
        "ALTER TABLE t DROP CONSTRAINT k, ADD CONSTRAINT k UNIQUE (a);",
        "ALTER TABLE t DROP CONSTRAINT k; ALTER TABLE t ADD CONSTRAINT k UNIQUE (a);",
        "ALTER TABLE t DROP COLUMN a;",
    ] {
        assert_eq!(identity_of(&format!("{UNIQUE_KEY} {drop}"), "t"), "index -", "{drop}");
    }
    let primary_key = "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t REPLICA IDENTITY USING INDEX t_pkey;
         ALTER TABLE t DROP CONSTRAINT t_pkey;";
    assert_eq!(identity_of(primary_key, "t"), "index -");
}

#[test]
fn the_setting_follows_renames_and_rebuilds() {
    for (follow, read) in [
        ("ALTER INDEX i RENAME TO j;", "index j"),
        ("ALTER TABLE t RENAME COLUMN a TO c;", "index i"),
        ("ALTER TABLE t ALTER COLUMN a TYPE BIGINT;", "index i"),
        ("ALTER TABLE t ADD COLUMN c INT;", "index i"),
    ] {
        assert_eq!(identity_of(&format!("{UNIQUE_INDEX} {follow}"), "t"), read, "{follow}");
    }
    for (follow, read) in [
        ("ALTER TABLE t RENAME CONSTRAINT k TO j;", "key j"),
        ("ALTER INDEX k RENAME TO j;", "key j"),
        ("ALTER TABLE t ALTER COLUMN a TYPE BIGINT;", "key k"),
    ] {
        assert_eq!(identity_of(&format!("{UNIQUE_KEY} {follow}"), "t"), read, "{follow}");
    }
    let renamed = format!("{UNIQUE_INDEX} ALTER TABLE t RENAME TO u;");
    assert_eq!(identity_of(&renamed, "u"), "index i");
    let full = "CREATE TABLE t (a INT); ALTER TABLE t REPLICA IDENTITY FULL;
         ALTER TABLE t ADD CONSTRAINT positive CHECK (a > 0); ALTER TABLE t RENAME TO u;";
    assert_eq!(identity_of(full, "u"), "full");
}

#[test]
fn a_key_column_of_the_identity_keeps_its_required_value() {
    let refused = |sql: &str| {
        assert!(
            matches!(
                refusal(sql),
                Error::RequiredValueNotDroppable {
                    table_name,
                    column_name,
                    reason: RequiredValue::InReplicaIdentity,
                    constraint_name: None,
                } if table_name == "t" && column_name == "a"
            ),
            "{sql}"
        );
    };
    refused(&format!("{UNIQUE_INDEX} ALTER TABLE t ALTER COLUMN a DROP NOT NULL;"));
    refused(&format!("{UNIQUE_KEY} ALTER TABLE t ALTER COLUMN a DROP NOT NULL;"));
    refused(
        "CREATE TABLE t (a INT NOT NULL UNIQUE); ALTER TABLE t REPLICA IDENTITY USING INDEX t_a_key;
         ALTER TABLE t ALTER COLUMN a DROP NOT NULL;",
    );
    refused(
        "CREATE TABLE t (a INT CONSTRAINT nn NOT NULL); CREATE UNIQUE INDEX i ON t (a);
         ALTER TABLE t REPLICA IDENTITY USING INDEX i; ALTER TABLE t DROP CONSTRAINT nn;",
    );
    refused(
        "CREATE TABLE t (a INT, CONSTRAINT nn NOT NULL a); CREATE UNIQUE INDEX i ON t (a);
         ALTER TABLE t REPLICA IDENTITY USING INDEX i; ALTER TABLE t DROP CONSTRAINT nn;",
    );
    // An included column, a dropped identity index and a table read in full
    // leave the requirement free to go.
    db("CREATE TABLE t (a INT NOT NULL, b INT NOT NULL); CREATE UNIQUE INDEX i ON t (a) INCLUDE (b);
        ALTER TABLE t REPLICA IDENTITY USING INDEX i; ALTER TABLE t ALTER COLUMN b DROP NOT NULL;");
    db(&format!("{UNIQUE_INDEX} DROP INDEX i; ALTER TABLE t ALTER COLUMN a DROP NOT NULL;"));
    db(&format!(
        "{UNIQUE_INDEX} ALTER TABLE t REPLICA IDENTITY FULL; ALTER TABLE t ALTER COLUMN a DROP NOT NULL;"
    ));
}

#[test]
fn a_table_below_losing_its_required_value_refuses_while_in_its_identity() {
    let refused = |sql: &str| {
        assert!(
            matches!(
                refusal(sql),
                Error::RequiredValueNotDroppable {
                    table_name,
                    column_name,
                    reason: RequiredValue::InReplicaIdentity,
                    constraint_name: None,
                } if table_name == "c" && column_name == "a"
            ),
            "{sql}"
        );
    };
    let child = "CREATE UNIQUE INDEX ci ON c (a); ALTER TABLE c REPLICA IDENTITY USING INDEX ci;";
    for (parent, drop) in [
        ("CREATE TABLE p (a INT NOT NULL)", "ALTER TABLE p ALTER COLUMN a DROP NOT NULL;"),
        ("CREATE TABLE p (a INT CONSTRAINT nn NOT NULL)", "ALTER TABLE p DROP CONSTRAINT nn;"),
        ("CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a)", "ALTER TABLE p DROP CONSTRAINT nn;"),
    ] {
        refused(&format!("{parent}; CREATE TABLE c () INHERITS (p); {child} {drop}"));
    }
    refused(&format!(
        "CREATE TABLE p (a INT NOT NULL) PARTITION BY LIST (a);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1); {child}
         ALTER TABLE p ALTER COLUMN a DROP NOT NULL;"
    ));
    // A table below that states the requirement itself keeps it.
    for (parent, drop) in [
        ("CREATE TABLE p (a INT NOT NULL)", "ALTER TABLE p ALTER COLUMN a DROP NOT NULL;"),
        ("CREATE TABLE p (a INT CONSTRAINT nn NOT NULL)", "ALTER TABLE p DROP CONSTRAINT nn;"),
        ("CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a)", "ALTER TABLE p DROP CONSTRAINT nn;"),
    ] {
        for own in ["a INT NOT NULL", "a INT, CONSTRAINT cnn NOT NULL a"] {
            db(&format!("{parent}; CREATE TABLE c ({own}) INHERITS (p); {child} {drop}"));
        }
    }
    // The parent's refusal comes first on the table below itself.
    for parent in [
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c () INHERITS (p);",
        "CREATE TABLE p (a INT NOT NULL) PARTITION BY LIST (a);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1);",
    ] {
        let sql = format!("{parent} {child} ALTER TABLE c ALTER COLUMN a DROP NOT NULL;");
        assert!(
            matches!(
                refusal(&sql),
                Error::RequiredValueNotDroppable { reason: RequiredValue::EnforcedByParent, .. }
            ),
            "{sql}"
        );
    }
}

#[test]
fn an_index_a_key_adopts_carries_the_identity_under_the_key() {
    let index = "CREATE TABLE t (a INT NOT NULL); CREATE UNIQUE INDEX i ON t (a);";
    let adopted = format!(
        "{index} ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX i;
         ALTER TABLE t REPLICA IDENTITY USING INDEX k;"
    );
    let adopted_after = format!(
        "{index} ALTER TABLE t REPLICA IDENTITY USING INDEX i;
         ALTER TABLE t ADD CONSTRAINT k UNIQUE USING INDEX i;"
    );
    let unnamed = format!(
        "{index} ALTER TABLE t ADD PRIMARY KEY USING INDEX i;
         ALTER TABLE t REPLICA IDENTITY USING INDEX i;"
    );
    let unnamed_after = format!(
        "{index} ALTER TABLE t REPLICA IDENTITY USING INDEX i;
         ALTER TABLE t ADD PRIMARY KEY USING INDEX i;"
    );
    // PostgreSQL renames the adopted index to the key, keeping the identity.
    for setting in [&adopted, &adopted_after, &unnamed, &unnamed_after] {
        let key = key_of(setting);
        assert_eq!(identity_of(setting, "t"), format!("key {key}"), "{setting}");
        let renamed = format!("{setting} ALTER TABLE t RENAME CONSTRAINT {key} TO j;");
        assert_eq!(identity_of(&renamed, "t"), "key j", "{renamed}");
        let dropped = format!("{renamed} ALTER TABLE t DROP CONSTRAINT j;");
        assert_eq!(identity_of(&dropped, "t"), "index -", "{dropped}");
        // PostgreSQL names the primary key ahead of the replica identity.
        let expected = if setting.contains("PRIMARY KEY") {
            RequiredValue::CoveredByKey
        } else {
            RequiredValue::InReplicaIdentity
        };
        let sql = format!("{setting} ALTER TABLE t ALTER COLUMN a DROP NOT NULL;");
        assert!(
            matches!(
                refusal(&sql),
                Error::RequiredValueNotDroppable { reason, .. } if reason == expected
            ),
            "{sql}"
        );
    }
    // An index of the same name in another schema is not the one adopted.
    db("CREATE SCHEMA a; CREATE SCHEMA b; CREATE TABLE a.t (x INT); CREATE INDEX i ON a.t (x);
        CREATE TABLE b.t (x INT NOT NULL); CREATE UNIQUE INDEX i ON b.t (x);
        ALTER TABLE b.t ADD CONSTRAINT k UNIQUE USING INDEX i;
        ALTER TABLE b.t REPLICA IDENTITY USING INDEX k;");
}

/// The name of the key `sql` adopts, `k` where written and `i` otherwise.
fn key_of(sql: &str) -> &'static str {
    if sql.contains("CONSTRAINT k") { "k" } else { "i" }
}

#[test]
fn a_collated_key_is_a_plain_column() {
    for key in ["a COLLATE \"C\"", "(a COLLATE \"C\")"] {
        let sql = format!(
            "CREATE TABLE t (a TEXT NOT NULL); CREATE UNIQUE INDEX i ON t ({key});
             ALTER TABLE t REPLICA IDENTITY USING INDEX i;"
        );
        assert_eq!(identity_of(&sql, "t"), "index i", "{sql}");
    }
}

#[test]
fn using_index_reaches_an_index_under_the_parse_case() {
    let sql = "CREATE TABLE t (a INT NOT NULL); CREATE UNIQUE INDEX \"Idx\" ON t (a);
         ALTER TABLE t REPLICA IDENTITY USING INDEX idx;";
    let folded = |sql: &str| {
        ParseOptions::default()
            .with_identifier_case(IdentifierCase::Folded)
            .parse::<PostgreSqlDialect>(sql)
            .unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
    };
    let database = folded(sql);
    let identity = table(&database, "t").replica_identity(&database).expect("t is held");
    assert!(
        matches!(identity, ReplicaIdentity::Index(Some(ReplicaIdentityIndex::Index(index)))
            if index.name() == Some("Idx")),
        "{identity:?}"
    );
    let dropped = folded(&format!("{sql} DROP INDEX idx;"));
    let identity = table(&dropped, "t").replica_identity(&dropped).expect("t is held");
    assert!(matches!(identity, ReplicaIdentity::Index(None)), "{identity:?}");
}

#[test]
fn replica_identity_runs_after_the_other_subcommands_of_its_statement() {
    let set_not_null = "CREATE TABLE t (a INT); CREATE UNIQUE INDEX i ON t (a);
         ALTER TABLE t REPLICA IDENTITY USING INDEX i, ALTER COLUMN a SET NOT NULL;";
    assert_eq!(identity_of(set_not_null, "t"), "index i");
    let added_key = "CREATE TABLE t (a INT NOT NULL);
         ALTER TABLE t REPLICA IDENTITY USING INDEX t_a_key, ADD UNIQUE (a);";
    assert_eq!(identity_of(added_key, "t"), "key t_a_key");
    let dropped_key = "CREATE TABLE t (a INT NOT NULL, CONSTRAINT k UNIQUE (a));
         ALTER TABLE t REPLICA IDENTITY USING INDEX k, DROP CONSTRAINT k;";
    assert!(matches!(refusal(dropped_key), Error::ReplicaIdentityIndexNotFound { .. }));
}

#[test]
fn relations_without_a_replica_identity_refuse_it() {
    let view = "CREATE VIEW t AS SELECT 1 AS a; ALTER TABLE t REPLICA IDENTITY FULL;";
    assert!(matches!(refusal(view), Error::AlterActionUnsupportedOnRelation { .. }));
    let foreign = "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w;
         CREATE FOREIGN TABLE t (a INT) SERVER s; ALTER TABLE t REPLICA IDENTITY FULL;";
    assert!(matches!(refusal(foreign), Error::AlterActionUnsupportedOnRelation { .. }));
    db("ALTER TABLE IF EXISTS t REPLICA IDENTITY FULL;");
    db("ALTER TABLE IF EXISTS t REPLICA IDENTITY USING INDEX i;");
}

#[test]
fn a_foreign_table_holds_nothing() {
    // PostgreSQL starts only a table, a partitioned table and a materialized
    // view at `DEFAULT`, so a foreign table reads `NOTHING`, as a partition
    // too.
    let server = "CREATE FOREIGN DATA WRAPPER w; CREATE SERVER s FOREIGN DATA WRAPPER w;";
    assert_eq!(
        identity_of(&format!("{server} CREATE FOREIGN TABLE t (a INT) SERVER s;"), "t"),
        "nothing"
    );
    assert_eq!(
        identity_of(
            &format!(
                "{server} CREATE TABLE p (a INT) PARTITION BY LIST (a);
                 CREATE FOREIGN TABLE t PARTITION OF p FOR VALUES IN (1) SERVER s;"
            ),
            "t"
        ),
        "nothing"
    );
}

#[test]
fn a_reference_to_a_table_answers_its_identity() {
    let database = db(UNIQUE_INDEX);
    let reference = &table(&database, "t");
    assert!(matches!(
        TableLike::replica_identity(reference, &database).expect("t is held"),
        ReplicaIdentity::Index(Some(ReplicaIdentityIndex::Index(index))) if index.name() == Some("i")
    ));
}
