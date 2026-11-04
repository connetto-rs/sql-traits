//! Tests that a partition key names columns PostgreSQL can partition by, and
//! that a unique constraint or index on a partitioned table covers it.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn refusal(sql: &str) -> Error {
    match parse(sql) {
        Ok(_) => panic!("{sql} was accepted"),
        Err(error) => error,
    }
}

#[test]
fn a_partition_key_names_a_column_of_its_table() {
    for (sql, column) in [
        ("CREATE TABLE k (id INT) PARTITION BY RANGE (nope);", "nope"),
        // A quoted name keeps its case, so it reaches no folded column.
        ("CREATE TABLE k (a INT) PARTITION BY RANGE (\"A\");", "A"),
        ("CREATE TABLE k (a INT) PARTITION BY RANGE ((a + nope));", "nope"),
        ("CREATE TABLE k (a TEXT) PARTITION BY RANGE (lower(nope));", "nope"),
    ] {
        let error = refusal(sql);
        assert!(
            matches!(
                &error,
                Error::PartitionKeyColumnNotFound { table_name, column_name }
                    if table_name == "k" && column_name == column
            ),
            "{sql}: {error:?}"
        );
    }
    for sql in [
        "CREATE TABLE k (a INT) PARTITION BY RANGE (A);",
        "CREATE TABLE k (a INT, b INT) PARTITION BY RANGE ((a + b));",
        "CREATE TABLE k (a INT) PARTITION BY RANGE (a, a);",
        "CREATE TABLE k (a TEXT) PARTITION BY RANGE (lower(a));",
    ] {
        parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"));
    }
}

#[test]
fn a_partition_key_takes_no_generated_column() {
    for sql in [
        "CREATE TABLE k (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED) PARTITION BY RANGE (b);",
        "CREATE TABLE k (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED)
             PARTITION BY RANGE ((b + 1));",
        "CREATE TABLE k (a INT, b INT GENERATED ALWAYS AS (a * 2) STORED)
             PARTITION BY RANGE (abs(b));",
    ] {
        let error = refusal(sql);
        assert!(
            matches!(
                &error,
                Error::GeneratedColumnInPartitionKey { table_name, column_name }
                    if table_name == "k" && column_name == "b"
            ),
            "{sql}: {error:?}"
        );
    }
}

#[test]
fn a_list_partition_key_has_one_column() {
    assert!(matches!(
        refusal("CREATE TABLE k (a INT, b INT) PARTITION BY LIST (a, b);"),
        Error::ListPartitionKeyHasManyColumns { table_name } if table_name == "k"
    ));
}

#[test]
fn a_unique_constraint_on_a_partitioned_table_covers_its_key() {
    let partitioned = "CREATE TABLE r (a INT, b INT) PARTITION BY RANGE (a);
         CREATE TABLE rp PARTITION OF r FOR VALUES FROM (0) TO (10);";
    for (sql, table) in [
        ("CREATE TABLE k (id INT, v INT, UNIQUE (v)) PARTITION BY RANGE (id);", "k"),
        ("CREATE TABLE k (id INT, v INT PRIMARY KEY) PARTITION BY RANGE (id);", "k"),
        ("CREATE TABLE k (a INT, b INT, PRIMARY KEY (a)) PARTITION BY RANGE (a, b);", "k"),
        ("CREATE TABLE k (a INT, b INT, UNIQUE (b)) PARTITION BY HASH (a);", "k"),
        ("CREATE TABLE k (a INT, b INT, EXCLUDE USING btree (b WITH =)) PARTITION BY RANGE (a);", "k"),
        // An expression entry covers no key column.
        ("CREATE TABLE k (a INT, b INT, EXCLUDE USING btree ((a + 1) WITH =)) PARTITION BY RANGE (a);", "k"),
        (&format!("{partitioned} ALTER TABLE r ADD PRIMARY KEY (b);"), "r"),
        (&format!("{partitioned} ALTER TABLE r ADD COLUMN c INT UNIQUE;"), "r"),
        (&format!("{partitioned} CREATE UNIQUE INDEX ON r (b);"), "r"),
        (&format!("{partitioned} CREATE UNIQUE INDEX ON r ((a + 1));"), "r"),
        // A partition that is itself partitioned receives the root's key and
        // has to cover its own partition key with it as well.
        (
            "CREATE TABLE root (a INT, b INT, PRIMARY KEY (a)) PARTITION BY RANGE (a);
             CREATE TABLE sub PARTITION OF root FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (b);",
            "sub",
        ),
    ] {
        let error = refusal(sql);
        assert!(
            matches!(
                &error,
                Error::UniqueConstraintMissesPartitionKey { table_name } if table_name == table
            ),
            "{sql}: {error:?}"
        );
    }
    for sql in [
        "CREATE TABLE k (a INT, b INT, UNIQUE (a)) PARTITION BY RANGE ((a + b));",
        "CREATE TABLE k (a INT, b INT, UNIQUE (a)) PARTITION BY RANGE (abs(a));",
    ] {
        assert!(
            matches!(
                refusal(sql),
                Error::UniqueConstraintOverExpressionPartitionKey { table_name } if table_name == "k"
            ),
            "{sql}"
        );
    }

    for sql in [
        "CREATE TABLE k (a INT, b INT, UNIQUE (a, b)) PARTITION BY RANGE (a);",
        "CREATE TABLE k (a INT, b INT, PRIMARY KEY (b, a)) PARTITION BY RANGE (a, b);",
        "CREATE TABLE k (a INT PRIMARY KEY, b INT) PARTITION BY LIST (a);",
        "CREATE TABLE k (a INT, b INT, EXCLUDE USING btree (a WITH =)) PARTITION BY RANGE (a);",
        "CREATE TABLE k (a INT, b INT, EXCLUDE USING btree (a WITH =, (b + 1) WITH =))
             PARTITION BY RANGE (a);",
        &format!("{partitioned} CREATE UNIQUE INDEX ON rp (b);"),
        &format!("{partitioned} CREATE UNIQUE INDEX ON r (a, b);"),
        &format!("{partitioned} CREATE UNIQUE INDEX ON r (a) WHERE b > 0;"),
        &format!("{partitioned} ALTER TABLE r ADD CONSTRAINT u UNIQUE (a, b);"),
        "CREATE TABLE root (a INT, b INT, PRIMARY KEY (a)) PARTITION BY RANGE (a);
         CREATE TABLE sub PARTITION OF root FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (a);",
    ] {
        parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"));
    }
}
