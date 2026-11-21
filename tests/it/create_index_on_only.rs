//! `CREATE INDEX ON ONLY` indexes a partitioned table without its existing
//! partitions. Expectations were measured on PostgreSQL 18.7.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const ROOT: &str = "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);\n";
const PARTITION: &str = "CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);\n";
const LATER: &str = "CREATE TABLE r2 PARTITION OF r FOR VALUES FROM (10) TO (20);\n";
const FOREIGN_SERVER: &str =
    "CREATE FOREIGN DATA WRAPPER w;\nCREATE SERVER s FOREIGN DATA WRAPPER w;\n";

/// Every standalone index, materialized views' included, with whether it is
/// valid, sorted by name.
fn indexes(sql: &str) -> Result<Vec<(String, bool)>, Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(sql)?;
    let mut indexes = database
        .indexes()
        .map(|index| Ok((index.name().unwrap_or_default().to_owned(), index.is_valid(&database)?)))
        .chain(database.materialized_view_indexes().map(|index| {
            Ok((index.name().unwrap_or_default().to_owned(), index.is_valid(&database)?))
        }))
        .collect::<Result<Vec<_>, Error>>()?;
    indexes.sort();
    Ok(indexes)
}

fn owned(expected: &[(&str, bool)]) -> Vec<(String, bool)> {
    expected.iter().map(|(name, valid)| ((*name).to_owned(), *valid)).collect()
}

/// The index stays on the partitioned table and is invalid while it has a
/// partition, foreign or not, and an intermediate partition copies nothing to
/// its own partitions.
#[test]
fn existing_partitions_get_no_copy() -> Result<(), Error> {
    for sql in [
        format!("{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);"),
        format!(
            "{FOREIGN_SERVER}CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE FOREIGN TABLE f PARTITION OF r FOR VALUES FROM (10) TO (20) SERVER s;
CREATE INDEX pix ON ONLY r (id);"
        ),
        format!(
            "{ROOT}CREATE TABLE p1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF p1 FOR VALUES FROM (0) TO (5);
CREATE INDEX pix ON ONLY p1 (id);"
        ),
    ] {
        assert_eq!(indexes(&sql)?, owned(&[("pix", false)]), "{sql}");
    }
    Ok(())
}

/// Where `ONLY` leaves nothing out, the index is an ordinary valid one.
#[test]
fn only_without_partitions_is_valid() -> Result<(), Error> {
    for (sql, name) in [
        (format!("{ROOT}CREATE INDEX pix ON ONLY r (id);"), "pix"),
        ("CREATE TABLE t (id INT);\nCREATE INDEX tix ON ONLY t (id);".to_owned(), "tix"),
        (
            "CREATE TABLE t (id INT);\nCREATE TABLE c () INHERITS (t);\nCREATE INDEX tix ON ONLY t (id);"
                .to_owned(),
            "tix",
        ),
        (format!("{ROOT}{PARTITION}CREATE INDEX lix ON ONLY r1 (id);"), "lix"),
        (
            "CREATE MATERIALIZED VIEW mv AS SELECT 1 AS x;\nCREATE INDEX mix ON ONLY mv (x);"
                .to_owned(),
            "mix",
        ),
    ] {
        assert_eq!(indexes(&sql)?, owned(&[(name, true)]), "{sql}");
    }
    Ok(())
}

/// Partitions created or attached afterwards receive valid copies, through
/// every level, and the parent keeps the validity it was created with.
#[test]
fn later_partitions_receive_valid_copies() -> Result<(), Error> {
    for (sql, expected) in [
        (
            format!("{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);\n{LATER}"),
            &[("pix", false), ("r2_id_idx", true)][..],
        ),
        (
            format!("{ROOT}CREATE INDEX pix ON ONLY r (id);\n{PARTITION}"),
            &[("pix", true), ("r1_id_idx", true)],
        ),
        (
            format!(
                "{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);
CREATE TABLE r2 (id INT, v INT);
ALTER TABLE r ATTACH PARTITION r2 FOR VALUES FROM (10) TO (20);"
            ),
            &[("pix", false), ("r2_id_idx", true)],
        ),
        (
            format!(
                "{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);
CREATE TABLE r2 (id INT, v INT);
CREATE INDEX own ON r2 (id);
ALTER TABLE r ATTACH PARTITION r2 FOR VALUES FROM (10) TO (20);"
            ),
            &[("own", true), ("pix", false)],
        ),
        (
            format!(
                "{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);
ALTER TABLE r DETACH PARTITION r1;
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);"
            ),
            &[("pix", false), ("r1_id_idx", true)],
        ),
        (
            format!("{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);\nDROP TABLE r1;"),
            &[("pix", false)],
        ),
        (
            format!(
                "{ROOT}CREATE TABLE p1 PARTITION OF r FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (id);
CREATE INDEX pix ON ONLY r (id);
CREATE TABLE r11 PARTITION OF p1 FOR VALUES FROM (0) TO (5);
CREATE TABLE p2 PARTITION OF r FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (id);
CREATE TABLE r21 PARTITION OF p2 FOR VALUES FROM (10) TO (20);"
            ),
            &[("p2_id_idx", true), ("pix", false), ("r21_id_idx", true)],
        ),
    ] {
        assert_eq!(indexes(&sql)?, owned(expected), "{sql}");
    }
    Ok(())
}

/// Renaming the index, its table or a column of it keeps the validity.
#[test]
fn validity_survives_renames() -> Result<(), Error> {
    for (statement, name) in [
        ("ALTER INDEX pix RENAME TO q;", "q"),
        ("ALTER TABLE r RENAME TO s;", "pix"),
        ("ALTER TABLE r RENAME COLUMN id TO key;", "pix"),
    ] {
        let sql = format!("{ROOT}{PARTITION}CREATE INDEX pix ON ONLY r (id);\n{statement}");
        assert_eq!(indexes(&sql)?, owned(&[(name, false)]), "{sql}");
    }
    Ok(())
}

/// An equal index already on a partition stays independent and droppable.
#[test]
fn equal_partition_index_is_not_adopted() -> Result<(), Error> {
    let sql = format!(
        "{ROOT}{PARTITION}CREATE INDEX own ON r1 (id);\nCREATE INDEX pix ON ONLY r (id);\nDROP INDEX own;"
    );
    assert_eq!(indexes(&sql)?, owned(&[("pix", false)]));
    Ok(())
}

/// A unique index created only on the parent never reaches the foreign
/// partition, so nothing refuses it.
#[test]
fn unique_only_index_above_foreign_partition() -> Result<(), Error> {
    let sql = format!(
        "{FOREIGN_SERVER}CREATE TABLE r (id INT) PARTITION BY RANGE (id);
CREATE FOREIGN TABLE f PARTITION OF r FOR VALUES FROM (10) TO (20) SERVER s;
CREATE UNIQUE INDEX pix ON ONLY r (id);"
    );
    assert_eq!(indexes(&sql)?, owned(&[("pix", false)]));
    Ok(())
}

/// A foreign key cannot reference the columns of an invalid unique index, and
/// can reference those of a valid one.
#[test]
fn foreign_keys_need_a_valid_unique_index() -> Result<(), Error> {
    let reference = "CREATE TABLE c (rid INT REFERENCES r (id));";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{ROOT}{PARTITION}CREATE UNIQUE INDEX pix ON ONLY r (id);\n{reference}"
        )),
        Err(Error::ReferencedColumnsNotUniqueForForeignKey { .. })
    ));
    for sql in [
        format!("{ROOT}CREATE UNIQUE INDEX pix ON ONLY r (id);\n{PARTITION}{reference}"),
        format!(
            "{ROOT}{PARTITION}CREATE UNIQUE INDEX pix ON ONLY r (id);
CREATE UNIQUE INDEX pix2 ON r (id);\n{reference}"
        ),
    ] {
        ParserDB::parse::<PostgreSqlDialect>(&sql)?;
    }
    Ok(())
}

/// A key adopting an index checks the index before refusing the partitioned
/// table, as PostgreSQL does.
#[test]
fn adoption_checks_the_index_before_the_partitioned_table() {
    for (statement, expected) in [
        (
            "CREATE UNIQUE INDEX pix ON ONLY r (id);\nALTER TABLE r ADD CONSTRAINT k UNIQUE USING INDEX pix;",
            "index \"pix\" is not valid",
        ),
        (
            "CREATE INDEX pix ON ONLY r (id);\nALTER TABLE r ADD CONSTRAINT k PRIMARY KEY USING INDEX pix;",
            "index \"pix\" is not valid",
        ),
        (
            "ALTER TABLE r ADD CONSTRAINT k UNIQUE USING INDEX nope;",
            "index \"nope\" does not exist",
        ),
        (
            "CREATE INDEX pix ON r (id);\nALTER TABLE r ADD CONSTRAINT k UNIQUE USING INDEX pix;",
            "\"pix\" is not a unique index",
        ),
        (
            "ALTER TABLE r ADD CONSTRAINT u UNIQUE (id);\nALTER TABLE r ADD CONSTRAINT k UNIQUE USING INDEX u;",
            "index \"u\" is already associated with a constraint",
        ),
        (
            "CREATE UNIQUE INDEX pix ON r (id);\nALTER TABLE r ADD CONSTRAINT k UNIQUE USING INDEX pix;",
            "ALTER TABLE / ADD CONSTRAINT USING INDEX is not supported on partitioned tables",
        ),
    ] {
        let sql = format!("{ROOT}{PARTITION}{statement}");
        let error = ParserDB::parse::<PostgreSqlDialect>(&sql).err().map(|error| error.to_string());
        assert_eq!(error.as_deref(), Some(expected), "{sql}");
    }
}
