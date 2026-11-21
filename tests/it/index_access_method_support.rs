//! Index access method capabilities.
//! Expectations were measured on PostgreSQL 18.7.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{GenericDialect, PostgreSqlDialect};

/// Unique indexes require a built-in method that supports uniqueness.
#[test]
fn unique_capabilities() {
    for (written, resolved) in [
        ("hash", "hash"),
        ("gist", "gist"),
        ("gin", "gin"),
        ("brin", "brin"),
        ("spgist", "spgist"),
        ("HASH", "hash"),
        ("\"hash\"", "hash"),
    ] {
        let statement =
            format!("CREATE TABLE r (id INT);\nCREATE UNIQUE INDEX ix ON r USING {written} (id);");
        let result = ParserDB::parse::<PostgreSqlDialect>(&statement);
        assert!(
            matches!(&result, Err(Error::AccessMethodWithoutUniqueSupport { access_method }) if access_method == resolved),
            "{written}: {result:?}"
        );
        if let Err(error) = result {
            assert_eq!(
                error.to_string(),
                format!("access method \"{resolved}\" does not support unique indexes")
            );
        }
    }
}

/// Exclusion capabilities apply to table creation and constraint addition.
#[test]
fn exclusion_capabilities() {
    for (written, resolved) in
        [("gin", "gin"), ("brin", "brin"), ("GIN", "gin"), ("\"gin\"", "gin")]
    {
        for statement in [
            format!("CREATE TABLE r (id INT, EXCLUDE USING {written} (id WITH =));"),
            format!(
                "CREATE TABLE r (id INT);\nALTER TABLE r ADD CONSTRAINT ex EXCLUDE USING {written} (id WITH =);"
            ),
        ] {
            let result = ParserDB::parse::<PostgreSqlDialect>(&statement);
            assert!(
                matches!(&result, Err(Error::AccessMethodWithoutExclusionSupport { access_method }) if access_method == resolved),
                "{written}: {result:?}"
            );
            if let Err(error) = result {
                assert_eq!(
                    error.to_string(),
                    format!("access method \"{resolved}\" does not support exclusion constraints")
                );
            }
        }
    }
}

/// Unique capability checks also cover views, partitions and existing index
/// names.
#[test]
fn unique_creation_targets() {
    for statement in [
        "CREATE MATERIALIZED VIEW r AS SELECT 1 AS id;\nCREATE UNIQUE INDEX ix ON r USING hash(id);",
        "CREATE TABLE r (id INT) PARTITION BY RANGE(id);\nCREATE UNIQUE INDEX ix ON r USING hash(id);",
        "CREATE TABLE r (id INT);\nCREATE UNIQUE INDEX ix ON r(id);\nCREATE UNIQUE INDEX IF NOT EXISTS ix ON r USING hash(id);",
    ] {
        let result = ParserDB::parse::<PostgreSqlDialect>(statement);
        assert!(
            matches!(&result, Err(Error::AccessMethodWithoutUniqueSupport { access_method }) if access_method == "hash"),
            "{result:?}"
        );
    }
}

/// A partition's own exclusion must use a capable method.
#[test]
fn partition_exclusion() {
    let result = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT) PARTITION BY RANGE(id);
         CREATE TABLE c PARTITION OF r (EXCLUDE USING gin (id WITH =)) FOR VALUES FROM (0) TO (10);",
    );
    assert!(
        matches!(&result, Err(Error::AccessMethodWithoutExclusionSupport { access_method }) if access_method == "gin"),
        "{result:?}"
    );
}

/// Supported exclusion methods retain their constraints.
#[test]
fn supported_exclusions() -> Result<(), Error> {
    for (method, column_type, operator) in [
        ("btree", "INT", "="),
        ("hash", "INT", "="),
        ("HASH", "INT", "="),
        ("\"hash\"", "INT", "="),
        ("gist", "int4range", "&&"),
        ("spgist", "int4range", "&&"),
    ] {
        let statement = format!(
            "CREATE TABLE r (id {column_type}, CONSTRAINT ex EXCLUDE USING {method} (id WITH {operator}));"
        );
        let database = ParserDB::parse::<PostgreSqlDialect>(&statement)?;
        assert_eq!(database.tables().count(), 1);
        ParserDB::parse::<PostgreSqlDialect>(&format!(
            "{statement}\nALTER TABLE r DROP CONSTRAINT ex;"
        ))?;
    }
    Ok(())
}

/// Supported unique and ordinary indexes remain visible and removable.
#[test]
fn supported_indexes() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT);
         CREATE UNIQUE INDEX ix ON r USING btree (id);",
    )?;
    assert_eq!(database.indexes().filter(|index| index.name() == Some("ix")).count(), 1);
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT);
         CREATE INDEX ix ON r USING hash(id);
         ALTER INDEX ix RENAME TO iy;
         DROP INDEX iy;",
    )?;
    assert_eq!(database.indexes().count(), 0);
    ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT, EXCLUDE USING hash (id WITH =));
         ALTER TABLE r RENAME CONSTRAINT r_id_excl TO ex;
         ALTER TABLE r DROP CONSTRAINT ex;",
    )?;
    Ok(())
}

/// Unknown access methods require a catalog the model does not maintain.
#[test]
fn unknown_methods_remain_available() -> Result<(), Error> {
    for method in ["bloom", "\"HASH\""] {
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE r (id INT);\nCREATE UNIQUE INDEX ix ON r USING {method} (id);"
        ))?;
        assert_eq!(database.indexes().count(), 1);
        let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE TABLE r (id INT, EXCLUDE USING {method} (id WITH =));"
        ))?;
        assert_eq!(database.tables().count(), 1);
    }
    Ok(())
}

/// PostgreSQL capability restrictions do not apply to a generic dialect.
#[test]
fn generic_dialect() -> Result<(), Error> {
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE r (id INT, EXCLUDE USING gin (id WITH =));
         CREATE UNIQUE INDEX ix ON r USING hash (id);",
    )?;
    assert_eq!(database.indexes().count(), 1);
    Ok(())
}

/// Supported methods survive partition attachment and column changes.
#[test]
fn supported_key_lifecycle() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT) PARTITION BY RANGE(id);
         CREATE TABLE c (id INT, CONSTRAINT ex EXCLUDE USING hash (id WITH =));
         ALTER TABLE r ATTACH PARTITION c FOR VALUES FROM (0) TO (10);
         ALTER TABLE r DETACH PARTITION c;
         ALTER TABLE c RENAME COLUMN id TO value;
         ALTER TABLE c ALTER COLUMN value TYPE BIGINT;
         ALTER TABLE c DROP CONSTRAINT ex;",
    )?;
    assert_eq!(database.tables().count(), 2);
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (id INT) PARTITION BY RANGE(id);
         CREATE UNIQUE INDEX ix ON r USING btree(id);
         CREATE TABLE c PARTITION OF r FOR VALUES FROM (0) TO (10);
         ALTER TABLE r DETACH PARTITION c;
         ALTER TABLE r RENAME COLUMN id TO value;
         DROP INDEX ix;",
    )?;
    assert_eq!(database.tables().count(), 2);
    assert!(!database.indexes().any(|index| index.name() == Some("ix")));
    Ok(())
}
