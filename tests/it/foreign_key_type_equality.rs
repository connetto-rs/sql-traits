//! Foreign keys require compatible equality operators.
//! Expectations were measured on PostgreSQL 18.7.

#![expect(clippy::unwrap_used, reason = "Tests assert successful model construction and lookup.")]
#![expect(clippy::panic, reason = "A failed model construction reports its SQL input.")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Every measured built-in pair follows its referenced key's equality family.
#[test]
fn built_in_matrix() {
    let types = [
        "int2",
        "int4",
        "int8",
        "numeric",
        "float4",
        "float8",
        "text",
        "varchar",
        "char(3)",
        "uuid",
        "date",
        "timestamp",
        "timestamptz",
        "bool",
        "int4[]",
        "int8[]",
        "text[]",
    ];
    for (referenced_index, referenced) in types.iter().enumerate() {
        for (referencing_index, referencing) in types.iter().enumerate() {
            let accepted = match referenced_index {
                0..=2 => referencing_index <= 2,
                3 => referencing_index <= 3,
                4..=5 => referencing_index <= 5,
                6..=8 => (6..=8).contains(&referencing_index),
                10..=12 => (10..=12).contains(&referencing_index),
                _ => referencing_index == referenced_index,
            };
            let sql = format!(
                "CREATE TABLE r (id {referenced} PRIMARY KEY);\nCREATE TABLE c (x {referencing} REFERENCES r);"
            );
            assert_outcome(&sql, accepted, usize::from(accepted));
        }
    }
}

/// Unknown names retain the model's open-world acceptance.
#[test]
fn unknown_type_names() {
    for (host, referenced) in [
        ("mystery", "mystery"),
        ("mystery", "int"),
        ("int", "mystery"),
        ("mystery[]", "int"),
        ("int", "mystery[]"),
        ("hstore", "hstore"),
    ] {
        let sql = format!(
            "CREATE TABLE r (id {referenced} PRIMARY KEY);\nCREATE TABLE c (x {host} REFERENCES r);"
        );
        assert_outcome(&sql, true, 1);
    }
}

/// Measured declaration and alteration events preserve equality compatibility.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "The measured lifecycle cases form one regression matrix."
)]
fn lifecycle_cases() {
    let cases = [
        (
            r"CREATE TYPE mood AS ENUM ('one');
CREATE TABLE r (id mood PRIMARY KEY);
CREATE TABLE c (x mood REFERENCES r);",
            true,
            1,
        ),
        (
            r"CREATE TYPE mood AS ENUM ('one');
CREATE TYPE other AS ENUM ('one');
CREATE TABLE r (id mood PRIMARY KEY);
CREATE TABLE c (x other REFERENCES r);",
            false,
            0,
        ),
        (
            r"CREATE DOMAIN identifier AS int4;
CREATE TABLE r (id identifier PRIMARY KEY);
CREATE TABLE c (x int8 REFERENCES r);",
            true,
            1,
        ),
        (
            r"CREATE DOMAIN identifier AS int4;
CREATE TABLE r (id int8 PRIMARY KEY);
CREATE TABLE c (x identifier REFERENCES r);",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id int4 PRIMARY KEY);
CREATE TABLE c (x int4 REFERENCES r);
ALTER TABLE r ALTER COLUMN id TYPE text;",
            false,
            0,
        ),
        (
            r"CREATE TABLE r (id int4 PRIMARY KEY);
CREATE TABLE c (x int4 REFERENCES r);
ALTER TABLE c ALTER COLUMN x TYPE text;",
            false,
            0,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x text, FOREIGN KEY (x) REFERENCES r);",
            false,
            0,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x text);
ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES r;",
            false,
            0,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c ();
ALTER TABLE c ADD COLUMN x text REFERENCES r;",
            false,
            0,
        ),
        (
            r"CREATE TABLE r (id bigint PRIMARY KEY);
CREATE TABLE c (x int, FOREIGN KEY (x) REFERENCES r);",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id bigint PRIMARY KEY);
CREATE TABLE c (x int);
ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES r;",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id bigint PRIMARY KEY);
CREATE TABLE c ();
ALTER TABLE c ADD COLUMN x int REFERENCES r;",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r);
ALTER TABLE r ALTER COLUMN id TYPE bigint;",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r);
ALTER TABLE c ALTER COLUMN x TYPE bigint;",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r);
ALTER TABLE r RENAME COLUMN id TO ident;
ALTER TABLE c RENAME COLUMN x TO val;
ALTER TABLE r RENAME TO parent;
ALTER TABLE c RENAME TO child;",
            true,
            1,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r);
ALTER TABLE c DROP CONSTRAINT c_x_fkey;
ALTER TABLE c ALTER COLUMN x TYPE text;",
            true,
            0,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r);
ALTER TABLE c DROP COLUMN x;",
            true,
            0,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r);
DROP TABLE c;
DROP TABLE r;",
            true,
            0,
        ),
        (
            r"CREATE TABLE r (id int PRIMARY KEY);
CREATE TABLE c (x text REFERENCES r) PARTITION BY RANGE (x);",
            false,
            0,
        ),
        (
            r"CREATE TABLE r (id bigint PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r) PARTITION BY RANGE (x);
CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (0) TO (10);
ALTER TABLE c DETACH PARTITION c1;",
            true,
            2,
        ),
        (
            r"CREATE TABLE r (id bigint PRIMARY KEY);
CREATE TABLE c (x int REFERENCES r) PARTITION BY RANGE (x);
CREATE TABLE c1 (x int);
ALTER TABLE c ATTACH PARTITION c1 FOR VALUES FROM (0) TO (10);
ALTER TABLE c DETACH PARTITION c1;",
            true,
            2,
        ),
        (
            r"CREATE TYPE mood AS ENUM ('one');
CREATE TABLE r (id mood[] PRIMARY KEY);
CREATE TABLE c (x mood[] REFERENCES r);",
            true,
            1,
        ),
        (
            r"CREATE DOMAIN identifier AS int4;
CREATE TABLE r (id int4 PRIMARY KEY);
CREATE TABLE c (x identifier REFERENCES r);",
            true,
            1,
        ),
        (
            r"CREATE DOMAIN identifier AS text;
CREATE TABLE r (id int4 PRIMARY KEY);
CREATE TABLE c (x identifier REFERENCES r);",
            false,
            0,
        ),
        (
            r"CREATE DOMAIN identifier AS int4;
CREATE DOMAIN nested AS identifier;
CREATE TABLE r (id nested PRIMARY KEY);
CREATE TABLE c (x int8 REFERENCES r);",
            true,
            1,
        ),
    ];
    for (sql, accepted, expected_keys) in cases {
        assert_outcome(sql, accepted, expected_keys);
    }
}

/// Checks the public key model or the complete incompatibility diagnostic.
fn assert_outcome(sql: &str, accepted: bool, expected_keys: usize) {
    let result = ParserDB::parse::<PostgreSqlDialect>(sql);
    if accepted {
        let database = result.unwrap_or_else(|error| panic!("{sql}\n{error}"));
        let count: usize =
            database.tables().map(|table| table.foreign_keys(&database).unwrap().count()).sum();
        assert_eq!(count, expected_keys, "{sql}");
    } else {
        let error = result.unwrap_err();
        assert_eq!(error.to_string(), "foreign key constraint \"c_x_fkey\" cannot be implemented");
        assert!(
            matches!(
                error,
                Error::ForeignKeyCannotBeImplemented {
                    constraint_name,
                    host_table,
                    host_column,
                    referenced_table,
                    referenced_column,
                } if constraint_name.as_ref() == "c_x_fkey"
                    && host_table.as_ref() == "c"
                    && host_column.as_ref() == "x"
                    && referenced_table.as_ref() == "r"
                    && referenced_column.as_ref() == "id"
            ),
            "{sql}"
        );
    }
}

/// Explicit operator classes remain unknown without an operator-family catalog.
#[test]
fn explicit_operator_classes() {
    for host_type in ["text", "varchar", "int"] {
        let sql = format!(
            "CREATE TABLE r (id text);\nCREATE UNIQUE INDEX key_index ON r (id text_pattern_ops);\nCREATE TABLE c (x {host_type} REFERENCES r (id));"
        );
        assert_outcome(&sql, true, 1);
    }
}

/// Retyping an inherited column checks incoming foreign keys of descendants.
#[test]
fn inherited_type_changes() {
    for (new_type, accepted) in [("text", false), ("bigint", true)] {
        let sql = format!(
            "CREATE TABLE ancestor (id int);\nCREATE TABLE r (PRIMARY KEY(id)) INHERITS (ancestor);\nCREATE TABLE c (x int REFERENCES r);\nALTER TABLE ancestor ALTER COLUMN id TYPE {new_type};"
        );
        assert_outcome(&sql, accepted, usize::from(accepted));
    }
}

/// Incompatibility diagnostics retain declared and renamed constraint names.
#[test]
fn named_constraints() {
    for sql in [
        "CREATE TABLE r (id int PRIMARY KEY);\nCREATE TABLE c (x text CONSTRAINT typed_fk REFERENCES r);",
        "CREATE TABLE r (id int PRIMARY KEY);\nCREATE TABLE c (x text, CONSTRAINT typed_fk FOREIGN KEY (x) REFERENCES r);",
        "CREATE TABLE r (id int PRIMARY KEY);\nCREATE TABLE c (x int REFERENCES r);\nALTER TABLE c RENAME CONSTRAINT c_x_fkey TO typed_fk;\nALTER TABLE c ALTER COLUMN x TYPE text;",
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(sql).unwrap_err();
        assert_eq!(error.to_string(), "foreign key constraint \"typed_fk\" cannot be implemented");
        assert!(matches!(
            error,
            Error::ForeignKeyCannotBeImplemented {
                constraint_name,
                host_table,
                host_column,
                referenced_table,
                referenced_column,
            } if constraint_name.as_ref() == "typed_fk"
                && host_table.as_ref() == "c"
                && host_column.as_ref() == "x"
                && referenced_table.as_ref() == "r"
                && referenced_column.as_ref() == "id"
        ));
    }
}
