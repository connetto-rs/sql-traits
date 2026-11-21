//! Tests that a foreign key keeps the key it checks its rows against.
//!
//! PostgreSQL binds a foreign key to one unique index of the referenced table
//! when the key is declared, the primary key for a key naming no columns and
//! otherwise the oldest key over the same columns, and refuses to drop that
//! index without `CASCADE`, which takes the foreign key along. Every
//! expectation was measured against PostgreSQL 18.7.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn foreign_keys(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .foreign_keys(database)
        .expect("table is in this database")
        .count()
}

fn unique_keys(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .unique_indices(database)
        .expect("table is in this database")
        .count()
}

fn refused_for(result: Result<ParserDB, Error>, key: &str, table: &str, referencing: &str) {
    let error = result.err();
    assert!(
        matches!(
            &error,
            Some(Error::KeyReferenced { key_name, table_name, referencing_table })
                if (key_name.as_str(), table_name.as_str(), referencing_table.as_str())
                    == (key, table, referencing)
        ),
        "expected `{key}` of `{table}` refused for `{referencing}`, got {error:?}",
    );
}

const UNIQUE_THEN_PRIMARY: &str = "CREATE TABLE r (id INT NOT NULL);
     ALTER TABLE r ADD CONSTRAINT u UNIQUE (id);
     ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY (id);";

#[test]
fn dropping_the_referenced_primary_key_is_refused() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE r DROP CONSTRAINT pk;",
        ),
        "pk",
        "r",
        "c",
    );
}

#[test]
fn cascade_takes_the_foreign_key_along() {
    let database = parse(
        "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
         CREATE TABLE c (x INT REFERENCES r (id));
         ALTER TABLE r DROP CONSTRAINT pk CASCADE;",
    )
    .expect("the drop cascades");
    assert_eq!((unique_keys(&database, "r"), foreign_keys(&database, "c")), (0, 0));
}

#[test]
fn if_exists_does_not_soften_the_refusal() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE r DROP CONSTRAINT IF EXISTS pk;",
        ),
        "pk",
        "r",
        "c",
    );
}

#[test]
fn a_name_spelled_another_way_reaches_the_same_key() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
             CREATE TABLE c (x INT REFERENCES R (ID));
             ALTER TABLE r DROP CONSTRAINT PK;",
        ),
        "pk",
        "r",
        "c",
    );
}

#[test]
fn a_self_reference_holds_its_own_table_s_key() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY, p INT REFERENCES r (id));
             ALTER TABLE r DROP CONSTRAINT pk;",
        ),
        "pk",
        "r",
        "r",
    );
    let database = parse(
        "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY, p INT REFERENCES r (id));
         ALTER TABLE r DROP CONSTRAINT pk CASCADE;",
    )
    .expect("the drop cascades");
    assert_eq!((unique_keys(&database, "r"), foreign_keys(&database, "r")), (0, 0));
}

#[test]
fn the_oldest_key_over_the_columns_is_the_one_held() {
    refused_for(
        parse(&format!(
            "{UNIQUE_THEN_PRIMARY}
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE r DROP CONSTRAINT u;"
        )),
        "u",
        "r",
        "c",
    );
    let database = parse(&format!(
        "{UNIQUE_THEN_PRIMARY}
         CREATE TABLE c (x INT REFERENCES r (id));
         ALTER TABLE r DROP CONSTRAINT pk;"
    ))
    .expect("the younger key is free");
    assert_eq!(foreign_keys(&database, "c"), 1);
}

#[test]
fn a_key_naming_no_columns_holds_the_primary_key() {
    refused_for(
        parse(&format!(
            "{UNIQUE_THEN_PRIMARY}
             CREATE TABLE c (x INT REFERENCES r);
             ALTER TABLE r DROP CONSTRAINT pk;"
        )),
        "pk",
        "r",
        "c",
    );
    parse(&format!(
        "{UNIQUE_THEN_PRIMARY}
         CREATE TABLE c (x INT REFERENCES r);
         ALTER TABLE r DROP CONSTRAINT u;"
    ))
    .expect("the older unique key is free");
}

#[test]
fn a_key_added_after_the_foreign_key_is_free() {
    parse(
        "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
         CREATE TABLE c (x INT REFERENCES r (id));
         ALTER TABLE r ADD CONSTRAINT u UNIQUE (id);
         ALTER TABLE r DROP CONSTRAINT u;",
    )
    .expect("the younger key is free");
}

#[test]
fn an_older_unique_index_is_the_one_held() {
    let created = "CREATE TABLE r (id INT NOT NULL);
         CREATE UNIQUE INDEX ix ON r (id);
         ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY (id);
         CREATE TABLE c (x INT REFERENCES r (id));";
    parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT pk;"))
        .expect("the younger primary key is free");
    refused_for(parse(&format!("{created} DROP INDEX ix;")), "ix", "r", "c");
}

#[test]
fn drop_index_cascade_takes_the_foreign_key_along() {
    let database = parse(
        "CREATE TABLE r (id INT NOT NULL);
         CREATE UNIQUE INDEX ix ON r (id);
         CREATE TABLE c (x INT REFERENCES r (id));
         DROP INDEX ix CASCADE;",
    )
    .expect("the drop cascades");
    assert_eq!(foreign_keys(&database, "c"), 0);
}

#[test]
fn columns_in_another_order_still_hold_the_key() {
    refused_for(
        parse(
            "CREATE TABLE r (a INT, b INT, CONSTRAINT u UNIQUE (a, b));
             CREATE TABLE c (x INT, y INT, FOREIGN KEY (x, y) REFERENCES r (b, a));
             ALTER TABLE r DROP CONSTRAINT u;",
        ),
        "u",
        "r",
        "c",
    );
}

#[test]
fn renaming_the_key_keeps_it_held() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE r RENAME CONSTRAINT pk TO pk2;
             ALTER TABLE r DROP CONSTRAINT pk2;",
        ),
        "pk2",
        "r",
        "c",
    );
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER INDEX pk RENAME TO pk3;
             ALTER TABLE r DROP CONSTRAINT pk3;",
        ),
        "pk3",
        "r",
        "c",
    );
}

#[test]
fn a_dropped_foreign_key_or_host_frees_the_key() {
    parse(
        "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
         CREATE TABLE c (x INT CONSTRAINT f REFERENCES r (id));
         ALTER TABLE c DROP CONSTRAINT f;
         ALTER TABLE r DROP CONSTRAINT pk;",
    )
    .expect("no foreign key is left");
    parse(
        "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
         CREATE TABLE c (x INT REFERENCES r (id));
         DROP TABLE c;
         ALTER TABLE r DROP CONSTRAINT pk;",
    )
    .expect("no foreign key is left");
}

#[test]
fn a_partition_s_copy_of_the_key_is_held_by_a_key_referencing_the_partition() {
    let created = "CREATE TABLE r (id INT, CONSTRAINT pk PRIMARY KEY (id)) PARTITION BY RANGE (id);
         CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
         CREATE TABLE c (x INT REFERENCES r1 (id));";
    refused_for(parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT pk;")), "pk", "r", "c");
    let database = parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT pk CASCADE;"))
        .expect("the drop cascades");
    assert_eq!(foreign_keys(&database, "c"), 0);
}

#[test]
fn a_key_referencing_the_partitioned_table_holds_its_key() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT, CONSTRAINT pk PRIMARY KEY (id)) PARTITION BY RANGE (id);
             CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE r DROP CONSTRAINT pk;",
        ),
        "pk",
        "r",
        "c",
    );
}

#[test]
fn a_partitioned_host_loses_its_key_and_every_copy_under_cascade() {
    let created = "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
         CREATE TABLE c (x INT, CONSTRAINT f FOREIGN KEY (x) REFERENCES r (id))
             PARTITION BY RANGE (x);
         CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (0) TO (10);";
    refused_for(parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT pk;")), "pk", "r", "c");
    let database = parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT pk CASCADE;"))
        .expect("the drop cascades");
    assert_eq!((foreign_keys(&database, "c"), foreign_keys(&database, "c1")), (0, 0));
}

#[test]
fn a_detached_partition_keeps_the_key_its_copy_held() {
    let created = format!(
        "{UNIQUE_THEN_PRIMARY}
         CREATE TABLE c (x INT REFERENCES r) PARTITION BY RANGE (x);
         CREATE TABLE c1 PARTITION OF c FOR VALUES FROM (0) TO (10);
         ALTER TABLE c DETACH PARTITION c1;"
    );
    parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT u;"))
        .expect("the older unique key is free");
    refused_for(
        parse(&format!("{created} DROP TABLE c; ALTER TABLE r DROP CONSTRAINT pk;")),
        "pk",
        "r",
        "c1",
    );
}

#[test]
fn a_type_change_binds_the_key_again_to_the_oldest() {
    let referenced = format!(
        "{UNIQUE_THEN_PRIMARY}
         CREATE TABLE c (x BIGINT REFERENCES r);
         ALTER TABLE r ALTER COLUMN id TYPE BIGINT;"
    );
    refused_for(parse(&format!("{referenced} ALTER TABLE r DROP CONSTRAINT u;")), "u", "r", "c");
    parse(&format!("{referenced} ALTER TABLE r DROP CONSTRAINT pk;"))
        .expect("the primary key is free");
    parse(&format!(
        "{UNIQUE_THEN_PRIMARY}
         CREATE TABLE c (x INT REFERENCES r);
         ALTER TABLE c ALTER COLUMN x TYPE BIGINT;
         ALTER TABLE r DROP CONSTRAINT pk;"
    ))
    .expect("the host's type change binds the key again");
    refused_for(
        parse(
            "CREATE TABLE r (id INT NOT NULL, v INT);
             ALTER TABLE r ADD CONSTRAINT u UNIQUE (id);
             ALTER TABLE r ADD CONSTRAINT pk PRIMARY KEY (id);
             CREATE TABLE c (x INT REFERENCES r);
             ALTER TABLE r ALTER COLUMN v TYPE BIGINT;
             ALTER TABLE r DROP CONSTRAINT pk;",
        ),
        "pk",
        "r",
        "c",
    );
}

#[test]
fn a_unique_column_holds_the_key_naming_it() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT u UNIQUE);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE r DROP CONSTRAINT u;",
        ),
        "u",
        "r",
        "c",
    );
}

#[test]
fn keys_over_other_columns_are_free() {
    let created = "CREATE TABLE r (
             id INT PRIMARY KEY,
             code INT,
             name TEXT,
             CONSTRAINT positive CHECK (id > 0),
             CONSTRAINT code_key UNIQUE (code)
         );
         CREATE UNIQUE INDEX name_ix ON r (name);
         CREATE TABLE c (x INT REFERENCES r (code));";
    parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT r_pkey; DROP INDEX name_ix;"))
        .expect("neither key covers the referenced column");
    refused_for(
        parse(&format!("{created} ALTER TABLE r DROP CONSTRAINT code_key;")),
        "code_key",
        "r",
        "c",
    );
}

#[test]
fn only_does_not_soften_the_refusal() {
    refused_for(
        parse(
            "CREATE TABLE r (id INT CONSTRAINT pk PRIMARY KEY);
             CREATE TABLE c (x INT REFERENCES r (id));
             ALTER TABLE ONLY r DROP CONSTRAINT pk;",
        ),
        "pk",
        "r",
        "c",
    );
}

#[test]
fn a_host_type_change_binds_a_table_level_key_again() {
    let changed = format!(
        "{UNIQUE_THEN_PRIMARY}
         CREATE TABLE c (x INT, FOREIGN KEY (x) REFERENCES r);
         ALTER TABLE c ALTER COLUMN x TYPE BIGINT;"
    );
    parse(&format!("{changed} ALTER TABLE r DROP CONSTRAINT pk;"))
        .expect("the primary key is free");
    refused_for(parse(&format!("{changed} ALTER TABLE r DROP CONSTRAINT u;")), "u", "r", "c");
}

#[test]
fn sqlite_drops_an_index_a_foreign_key_reads() {
    ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE r (id INT);
         CREATE UNIQUE INDEX ix ON r (id);
         CREATE TABLE c (x INT REFERENCES r (id));
         DROP INDEX ix;",
    )
    .expect("SQLite checks a foreign key's target only when rows change");
}
