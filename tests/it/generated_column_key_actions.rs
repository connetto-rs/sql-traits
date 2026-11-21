//! Foreign key actions on expression-generated columns.
//! Expectations were measured on PostgreSQL 18.7.

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

const ACTIONS: [&str; 10] = [
    "UPDATE CASCADE",
    "UPDATE SET NULL",
    "UPDATE SET DEFAULT",
    "DELETE SET NULL",
    "DELETE SET DEFAULT",
    "DELETE CASCADE",
    "UPDATE RESTRICT",
    "UPDATE NO ACTION",
    "DELETE RESTRICT",
    "DELETE NO ACTION",
];

/// Produces each declaration path for a generated referencing column.
fn declarations(action: &str) -> [String; 6] {
    [
        format!(
            "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED REFERENCES r ON {action});"
        ),
        format!(
            "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED, FOREIGN KEY(g) REFERENCES r ON {action});"
        ),
        format!(
            "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED);\nALTER TABLE c ADD FOREIGN KEY(g) REFERENCES r ON {action};"
        ),
        format!(
            "CREATE TABLE c (id INT);\nALTER TABLE c ADD COLUMN g INT GENERATED ALWAYS AS (id + 1) STORED REFERENCES r ON {action};"
        ),
        format!(
            "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED, FOREIGN KEY(id,g) REFERENCES r(id,other) ON {action});"
        ),
        format!(
            "CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED) PARTITION BY RANGE(id);\nCREATE TABLE leaf PARTITION OF c FOR VALUES FROM (0) TO (10);\nALTER TABLE c ADD CONSTRAINT fk FOREIGN KEY(g) REFERENCES r ON {action};"
        ),
    ]
}

/// Reads a table's foreign keys through the public catalog interface.
#[expect(clippy::unwrap_used, reason = "Each fixture declares the inspected table")]
fn key_count(database: &ParserDB, name: &str) -> Result<usize, Error> {
    let table =
        database.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)?;
    Ok(table.unwrap().foreign_keys(database)?.count())
}

/// Every declaration refuses actions that would assign a generated value.
#[test]
fn generated_referencing_actions() -> Result<(), Error> {
    let mut mismatches = Vec::new();
    for (position, action) in ACTIONS.iter().enumerate() {
        for declaration in declarations(action) {
            let sql = format!(
                "CREATE TABLE r (id INT PRIMARY KEY, other INT, UNIQUE(id, other));\n{declaration}"
            );
            let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
            if position < 5 {
                let expected = matches!(
                    (position, &result),
                    (0..=2, Err(Error::InvalidForeignKeyUpdateActionForGeneratedColumn))
                        | (3..=4, Err(Error::InvalidForeignKeyDeleteActionForGeneratedColumn))
                );
                if !expected {
                    mismatches.push(sql);
                }
            } else {
                let database = result?;
                assert_eq!(key_count(&database, "c")?, 1);
                if declaration.contains("leaf") {
                    assert_eq!(key_count(&database, "leaf")?, 1);
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "accepted forbidden actions {mismatches:#?}");
    Ok(())
}

/// A generated referenced column permits every referential action.
#[test]
fn generated_referenced_actions() -> Result<(), Error> {
    for action in ACTIONS {
        let sql = format!(
            "CREATE TABLE r (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED UNIQUE);\nCREATE TABLE c (g INT REFERENCES r(g) ON {action});"
        );
        let database = ParserDB::parse::<PostgreSqlDialect>(&sql)?;
        assert_eq!(key_count(&database, "c")?, 1);
    }
    Ok(())
}

/// Partitions receive an allowed key on creation and attachment.
#[test]
fn partitions_and_key_lifecycle() -> Result<(), Error> {
    let base = "CREATE TABLE r (id INT PRIMARY KEY);
CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id + 1) STORED) PARTITION BY RANGE(id);
ALTER TABLE c ADD CONSTRAINT fk FOREIGN KEY(g) REFERENCES r ON DELETE CASCADE;";
    for partition in [
        "CREATE TABLE leaf PARTITION OF c FOR VALUES FROM (0) TO (10);",
        "CREATE TABLE leaf (id INT, g INT GENERATED ALWAYS AS(id+2) STORED);\nALTER TABLE c ATTACH PARTITION leaf FOR VALUES FROM (0) TO (10);",
    ] {
        let sql = format!("{base}\n{partition}");
        let database = ParserDB::parse::<PostgreSqlDialect>(&sql)?;
        assert_eq!(key_count(&database, "leaf")?, 1);
    }
    let sql = format!(
        "{base}
CREATE TABLE leaf PARTITION OF c FOR VALUES FROM (0) TO (10);
ALTER TABLE c DETACH PARTITION leaf;
ALTER TABLE leaf RENAME COLUMN g TO renamed;
ALTER TABLE leaf RENAME CONSTRAINT fk TO renamed_fk;
ALTER TABLE leaf DROP CONSTRAINT renamed_fk;"
    );
    let database = ParserDB::parse::<PostgreSqlDialect>(&sql)?;
    assert_eq!(key_count(&database, "c")?, 1);
    assert_eq!(key_count(&database, "leaf")?, 0);
    Ok(())
}

/// Identity columns permit assignments by foreign key actions.
#[test]
fn identity_is_not_expression_generated() -> Result<(), Error> {
    for declaration in [
        "CREATE TABLE c (g INT GENERATED ALWAYS AS IDENTITY REFERENCES r ON UPDATE CASCADE);",
        "CREATE TABLE c (g INT NOT NULL REFERENCES r ON UPDATE CASCADE);\nALTER TABLE c ALTER COLUMN g ADD GENERATED ALWAYS AS IDENTITY;",
    ] {
        let sql = format!("CREATE TABLE r (id INT PRIMARY KEY);\n{declaration}");
        let database = ParserDB::parse::<PostgreSqlDialect>(&sql)?;
        assert_eq!(key_count(&database, "c")?, 1);
    }
    Ok(())
}

/// Only the referencing columns restrict actions, using identifier case rules.
#[test]
fn referencing_columns_and_update_precedence() -> Result<(), Error> {
    for declaration in [
        "CREATE TABLE c(id INT, g INT GENERATED ALWAYS AS(id+1) STORED, FOREIGN KEY(g) REFERENCES r ON UPDATE CASCADE ON DELETE SET NULL);",
        "CREATE TABLE c(id INT, \"Generated\" INT GENERATED ALWAYS AS(id+1) STORED, FOREIGN KEY(\"Generated\") REFERENCES r ON UPDATE CASCADE);",
        "CREATE TABLE c(id INT, G INT GENERATED ALWAYS AS(id+1) STORED, FOREIGN KEY(g) REFERENCES r ON UPDATE CASCADE);",
    ] {
        let sql = format!("CREATE TABLE r(id INT PRIMARY KEY);\n{declaration}");
        let result = ParserDB::parse::<PostgreSqlDialect>(&sql);
        assert!(matches!(result, Err(Error::InvalidForeignKeyUpdateActionForGeneratedColumn)));
    }
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r(id INT PRIMARY KEY);
CREATE TABLE c(id INT REFERENCES r ON UPDATE CASCADE ON DELETE SET NULL, g INT GENERATED ALWAYS AS(id+1) STORED);",
    )?;
    assert_eq!(key_count(&database, "c")?, 1);
    Ok(())
}
