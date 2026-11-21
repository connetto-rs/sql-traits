//! Primary keys adopted by attached partitions.
//! Expectations were measured on PostgreSQL 18.7.
#![expect(clippy::expect_used, reason = "Test schemas and catalog lookups must succeed")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const ATTACHED: &str = "CREATE TABLE r (id INT PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE r1 (id INT NOT NULL, CONSTRAINT own PRIMARY KEY(id));
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);";

/// Parses a PostgreSQL schema.
fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// Checks the exact inherited-key refusal.
fn inherited(sql: &str, expected_table: &str, expected_constraint: &str) {
    assert!(
        matches!(parse(sql), Err(Error::InheritedConstraintNotDroppable { table_name, constraint_name })
        if table_name == expected_table && constraint_name == expected_constraint),
        "{sql}"
    );
}

/// Reads the primary-key columns through the public catalog.
fn primary_columns(database: &ParserDB, name: &str) -> Vec<String> {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("table exists")
        .primary_key_columns(database)
        .expect("table belongs to database")
        .map(|column| column.column_name().to_owned())
        .collect()
}

/// Reads the names of every unique index through the public catalog.
fn unique_names(database: &ParserDB, name: &str) -> Vec<String> {
    let mut names: Vec<_> = database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("table exists")
        .unique_indices(database)
        .expect("table belongs to database")
        .map(|index| index.name().expect("index is named").to_owned())
        .collect();
    names.sort();
    names
}

/// Both syntactic key locations preserve the adopted name and inherited status.
#[test]
fn all_key_spellings_are_adopted() {
    for parent in ["id INT PRIMARY KEY", "id INT, PRIMARY KEY(id)"] {
        for child in
            ["id INT CONSTRAINT own PRIMARY KEY", "id INT NOT NULL, CONSTRAINT own PRIMARY KEY(id)"]
        {
            let sql = format!(
                "CREATE TABLE r ({parent}) PARTITION BY RANGE(id);\nCREATE TABLE r1 ({child});\nALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);"
            );
            let database = parse(&sql).expect("equal primary key attaches");
            assert_eq!(primary_columns(&database, "r1"), ["id"]);
            assert_eq!(unique_names(&database, "r1"), ["own"]);
            inherited(&format!("{sql}\nALTER TABLE r1 DROP CONSTRAINT own;"), "r1", "own");
        }
    }
}

/// An incompatible primary key prevents attachment.
#[test]
fn different_primary_key_is_refused() {
    assert!(
        matches!(parse("CREATE TABLE r (id INT PRIMARY KEY, v INT NOT NULL) PARTITION BY RANGE(id);
CREATE TABLE r1 (id INT NOT NULL, v INT PRIMARY KEY);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);"),
        Err(Error::MultiplePrimaryKeys { table_name }) if table_name == "r1")
    );
}

/// Unique constraints and standalone indexes cannot replace a primary key.
#[test]
fn unique_keys_remain_independent() {
    for (declaration, drop, expected_names) in [
        (
            "CREATE TABLE r1 (id INT NOT NULL, CONSTRAINT own UNIQUE(id));",
            "ALTER TABLE r1 DROP CONSTRAINT own;",
            &["own", "r1_pkey"][..],
        ),
        (
            "CREATE TABLE r1 (id INT NOT NULL);\nCREATE UNIQUE INDEX own ON r1(id);",
            "DROP INDEX own;",
            &["r1_pkey"][..],
        ),
    ] {
        let sql = format!(
            "CREATE TABLE r (id INT PRIMARY KEY) PARTITION BY RANGE(id);\n{declaration}\nALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);"
        );
        let database = parse(&sql).expect("partition attaches");
        assert_eq!(unique_names(&database, "r1"), expected_names);
        let database = parse(&format!("{sql}\n{drop}")).expect("own key remains independent");
        assert_eq!(primary_columns(&database, "r1"), ["id"]);
        assert_eq!(unique_names(&database, "r1"), ["r1_pkey"]);
    }
}

/// A primary key cannot replace a parent's unique constraint.
#[test]
fn primary_key_under_unique_parent_stays_independent() {
    let sql = "CREATE TABLE r (id INT UNIQUE) PARTITION BY RANGE(id);
CREATE TABLE r1 (id INT CONSTRAINT own PRIMARY KEY);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);";
    assert_eq!(unique_names(&parse(sql).expect("partition attaches"), "r1"), ["own", "r1_id_key"]);
    let database =
        parse(&format!("{sql}\nALTER TABLE r1 DROP CONSTRAINT own;")).expect("own key drops");
    assert_eq!(primary_columns(&database, "r1"), Vec::<String>::new());
    assert_eq!(unique_names(&database, "r1"), ["r1_id_key"]);
}

/// Detachment releases the adopted key without changing its name.
#[test]
fn detachment_preserves_and_releases_the_key() {
    let detached = format!("{ATTACHED}\nALTER TABLE r DETACH PARTITION r1;");
    let database = parse(&detached).expect("partition detaches");
    assert_eq!(unique_names(&database, "r1"), ["own"]);
    let database = parse(&format!("{detached}\nALTER TABLE r1 DROP CONSTRAINT own;"))
        .expect("detached key drops");
    assert_eq!(primary_columns(&database, "r1"), Vec::<String>::new());
    inherited(
        &format!(
            "{detached}\nALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);\nALTER TABLE r1 DROP CONSTRAINT own;"
        ),
        "r1",
        "own",
    );
}

/// Adoption reserves only the existing key's name.
#[test]
fn generated_name_is_free_and_adopted_index_is_protected() {
    let database = parse(&format!("{ATTACHED}\nCREATE INDEX r1_pkey ON r1(id);"))
        .expect("generated name is free");
    assert_eq!(unique_names(&database, "r1"), ["own"]);
    assert!(matches!(parse(&format!("{ATTACHED}\nDROP INDEX own;")),
        Err(Error::IndexRequiredByConstraint { index_name, table_name }) if index_name == "own" && table_name == "r1"));
}

/// Renames preserve the inherited key relationship.
#[test]
fn renames_keep_adopted_keys_inherited() {
    for (rename, table, constraint) in [
        ("ALTER TABLE r1 RENAME CONSTRAINT own TO renamed;", "r1", "renamed"),
        ("ALTER INDEX own RENAME TO renamed;", "r1", "renamed"),
        ("ALTER TABLE r RENAME COLUMN id TO renamed;", "r1", "own"),
        ("ALTER TABLE r1 RENAME TO renamed;", "renamed", "own"),
    ] {
        inherited(
            &format!("{ATTACHED}\n{rename}\nALTER TABLE {table} DROP CONSTRAINT {constraint};"),
            table,
            constraint,
        );
    }
}

/// Dropping the parent key removes its adopted copy.
#[test]
fn parent_drop_removes_adopted_keys() {
    for parent in ["id INT PRIMARY KEY", "id INT, PRIMARY KEY(id)"] {
        for child in
            ["id INT CONSTRAINT own PRIMARY KEY", "id INT NOT NULL, CONSTRAINT own PRIMARY KEY(id)"]
        {
            for scope in ["", "ONLY "] {
                let database = parse(&format!("CREATE TABLE r ({parent}) PARTITION BY RANGE(id);\nCREATE TABLE r1 ({child});\nALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);\nALTER TABLE {scope}r DROP CONSTRAINT r_pkey;\nCREATE INDEX own ON r1(id);")).expect("parent drop releases adopted index name");
                assert_eq!(primary_columns(&database, "r1"), Vec::<String>::new());
            }
        }
    }
    let database = parse(
        "CREATE TABLE r (id INT PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE r1 (id INT CONSTRAINT own PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE r11 (id INT NOT NULL, CONSTRAINT leaf PRIMARY KEY(id));
ALTER TABLE r1 ATTACH PARTITION r11 FOR VALUES FROM (0) TO (5);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r DROP CONSTRAINT r_pkey;
CREATE INDEX own ON r1(id);
CREATE INDEX leaf ON r11(id);",
    )
    .expect("parent drop releases every adopted index name");
    assert_eq!(primary_columns(&database, "r11"), Vec::<String>::new());
}

/// Foreign keys can reference the adopted primary key.
#[test]
fn adopted_key_supports_foreign_keys() {
    let database = parse(&format!("{ATTACHED}\nCREATE TABLE c (id INT REFERENCES r1(id));"))
        .expect("adopted key supports references");
    assert_eq!(primary_columns(&database, "r1"), ["id"]);
}

/// Keys added before attachment are adopted as inherited constraints.
#[test]
fn altered_keys_are_adopted() {
    inherited(
        "CREATE TABLE r (id INT) PARTITION BY RANGE(id);
ALTER TABLE r ADD PRIMARY KEY(id);
CREATE TABLE r1 (id INT NOT NULL);
ALTER TABLE r1 ADD CONSTRAINT own PRIMARY KEY(id);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r1 DROP CONSTRAINT own;",
        "r1",
        "own",
    );
}

/// Every existing partition level keeps its key's inherited status.
#[test]
fn multilevel_attachment_and_detachment() {
    let attached = "CREATE TABLE r (id INT PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE r1 (id INT NOT NULL, CONSTRAINT own PRIMARY KEY(id)) PARTITION BY RANGE(id);
CREATE TABLE r11 PARTITION OF r1 FOR VALUES FROM (0) TO (5);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);";
    let database = parse(attached).expect("partitioned child attaches");
    assert_eq!(unique_names(&database, "r1"), ["own"]);
    assert_eq!(unique_names(&database, "r11"), ["r11_pkey"]);
    inherited(&format!("{attached}\nALTER TABLE r1 DROP CONSTRAINT own;"), "r1", "own");
    inherited(&format!("{attached}\nALTER TABLE r11 DROP CONSTRAINT r11_pkey;"), "r11", "r11_pkey");
    let detached = format!("{attached}\nALTER TABLE r DETACH PARTITION r1;");
    inherited(&format!("{detached}\nALTER TABLE r11 DROP CONSTRAINT r11_pkey;"), "r11", "r11_pkey");
    let database = parse(&format!(
        "{detached}\nALTER TABLE r1 DROP CONSTRAINT own;\nCREATE INDEX r11_pkey ON r11(id);"
    ))
    .expect("detached parent drops its copies");
    assert_eq!(primary_columns(&database, "r11"), Vec::<String>::new());
}

/// Partitions created directly hold inherited primary keys.
#[test]
fn created_partition_key_is_inherited() {
    inherited(
        "CREATE TABLE r (id INT PRIMARY KEY) PARTITION BY RANGE(id);
CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
ALTER TABLE r1 DROP CONSTRAINT r1_pkey;",
        "r1",
        "r1_pkey",
    );
}
