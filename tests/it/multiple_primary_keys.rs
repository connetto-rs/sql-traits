//! A table holds at most one primary key.
//! Expectations were measured on PostgreSQL 18.7.
#![expect(clippy::expect_used, reason = "tests require successful schema lookups")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Every declaration and alteration refuses a second primary key.
#[test]
#[expect(clippy::too_many_lines, reason = "the case table covers primary key DDL events")]
fn second_primary_key_is_refused() {
    let cases = [
        ("CREATE TABLE r (id INT PRIMARY KEY PRIMARY KEY);", "r"),
        (
            "CREATE TABLE r (id INT);
ALTER TABLE r ADD COLUMN v INT PRIMARY KEY PRIMARY KEY;",
            "r",
        ),
        ("CREATE TABLE r (id INT PRIMARY KEY, v INT PRIMARY KEY);", "r"),
        ("CREATE TABLE r (id INT PRIMARY KEY, PRIMARY KEY (id));", "r"),
        (
            "CREATE TABLE r (id INT, CONSTRAINT a PRIMARY KEY (id), CONSTRAINT b PRIMARY KEY (id));",
            "r",
        ),
        ("CREATE TABLE r (id INT, PRIMARY KEY (id), PRIMARY KEY (id));", "r"),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r ADD PRIMARY KEY (id);",
            "r",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r ADD PRIMARY KEY (v);",
            "r",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY);
ALTER TABLE r ADD COLUMN v INT PRIMARY KEY;",
            "r",
        ),
        (
            "CREATE TABLE r (id INT);
ALTER TABLE r ADD PRIMARY KEY (id), ADD PRIMARY KEY (id);",
            "r",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT NOT NULL);
CREATE UNIQUE INDEX ix ON r (v);
ALTER TABLE r ADD CONSTRAINT other PRIMARY KEY USING INDEX ix;",
            "r",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r (PRIMARY KEY (v)) FOR VALUES FROM (0) TO (10);",
            "r1",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r (PRIMARY KEY (id)) FOR VALUES FROM (0) TO (10);",
            "r1",
        ),
        (
            "CREATE TABLE r (id INT, v INT, PRIMARY KEY (id)) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r (PRIMARY KEY (id)) FOR VALUES FROM (0) TO (10);",
            "r1",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r (id PRIMARY KEY) FOR VALUES FROM (0) TO (10);",
            "r1",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT NOT NULL, v INT PRIMARY KEY);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);",
            "r1",
        ),
        (
            "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT, v INT PRIMARY KEY);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r ADD PRIMARY KEY (id);",
            "r1",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r RENAME CONSTRAINT r_pkey TO original;
ALTER TABLE r ADD PRIMARY KEY (v);",
            "r",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r RENAME TO renamed;
ALTER TABLE renamed ADD PRIMARY KEY (v);",
            "renamed",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r RENAME COLUMN id TO identifier;
ALTER TABLE r ADD PRIMARY KEY (v);",
            "r",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
ALTER TABLE r DETACH PARTITION r1;
ALTER TABLE r1 ADD PRIMARY KEY (v);",
            "r1",
        ),
    ];
    for (sql, table_name) in cases {
        let error =
            ParserDB::parse::<PostgreSqlDialect>(sql).expect_err("second primary key refused");
        assert!(
            matches!(&error, Error::MultiplePrimaryKeys { table_name: actual } if actual == table_name),
            "{sql}\n{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("multiple primary keys for table \"{table_name}\" are not allowed")
        );
    }
}

/// Adding the first primary key preserves its public column list.
#[test]
#[expect(clippy::too_many_lines, reason = "the case table covers primary key DDL events")]
fn first_primary_key_is_accepted() {
    let cases = [
        (
            "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT PRIMARY KEY, v INT);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r ADD PRIMARY KEY (id);
ALTER TABLE r DROP CONSTRAINT r_pkey;
ALTER TABLE r1 ADD PRIMARY KEY (v);",
            "r1",
            "v",
        ),
        (
            "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT PRIMARY KEY, v INT);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r ADD PRIMARY KEY (id);",
            "r1",
            "id",
        ),
        (
            "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT, v INT, PRIMARY KEY (id));
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r ADD PRIMARY KEY (id);",
            "r1",
            "id",
        ),
        (
            "CREATE TABLE r (id INT, v INT);
ALTER TABLE r ADD PRIMARY KEY (v);",
            "r",
            "v",
        ),
        (
            "CREATE TABLE r (id INT);
ALTER TABLE r ADD COLUMN v INT PRIMARY KEY;",
            "r",
            "v",
        ),
        (
            "CREATE TABLE r (id INT, v INT NOT NULL);
CREATE UNIQUE INDEX ix ON r (v);
ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY USING INDEX ix;",
            "r",
            "v",
        ),
        (
            "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r (PRIMARY KEY (v)) FOR VALUES FROM (0) TO (10);",
            "r1",
            "v",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);",
            "r1",
            "id",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT PRIMARY KEY, v INT);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);",
            "r1",
            "id",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT NOT NULL, v INT);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);",
            "r1",
            "id",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
CREATE TABLE child (PRIMARY KEY (v)) INHERITS (r);",
            "child",
            "v",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r DROP CONSTRAINT r_pkey;
ALTER TABLE r ADD PRIMARY KEY (v);",
            "r",
            "v",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
ALTER TABLE r DROP COLUMN id;
ALTER TABLE r ADD PRIMARY KEY (v);",
            "r",
            "v",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
ALTER TABLE r DROP CONSTRAINT r_pkey;
ALTER TABLE r1 ADD PRIMARY KEY (v);",
            "r1",
            "v",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY, v INT);
DROP TABLE r;
CREATE TABLE r (id INT, v INT PRIMARY KEY);",
            "r",
            "v",
        ),
    ];
    for (sql, table_name, column_name) in cases {
        let database = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema accepted");
        let table = database
            .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
            .expect("table lookup")
            .expect("table exists");
        let columns: Vec<_> = table
            .primary_key_columns(&database)
            .expect("primary key columns")
            .map(ColumnLike::column_name)
            .collect();
        assert_eq!(columns, [column_name], "{sql}");
    }
}

/// An equal primary key adopted by a parent keeps its name and becomes
/// inherited.
#[test]
fn adopted_column_primary_key_is_inherited() {
    let sql = "CREATE TABLE r (id INT, v INT) PARTITION BY RANGE (id);
CREATE TABLE r1 (id INT CONSTRAINT own PRIMARY KEY, v INT);
ALTER TABLE r ATTACH PARTITION r1 FOR VALUES FROM (0) TO (10);
ALTER TABLE r ADD PRIMARY KEY (id);
ALTER TABLE r1 DROP CONSTRAINT own;";
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(sql),
        Err(Error::InheritedConstraintNotDroppable { table_name, constraint_name })
            if table_name == "r1" && constraint_name == "own"
    ));
}
