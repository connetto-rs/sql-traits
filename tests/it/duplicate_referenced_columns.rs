//! Foreign keys refuse duplicate referenced columns.
//! Expectations were measured on PostgreSQL 18.7.
#![expect(clippy::expect_used, reason = "Tests require successful schema lookups")]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

/// Every declaration route rejects duplicate targets before key and width
/// lookup.
#[test]
fn duplicate_targets_are_refused() {
    for sql in [
        "CREATE TABLE r (a INT, b INT, UNIQUE(a,b));
         CREATE TABLE c (x INT,y INT, FOREIGN KEY(x,y) REFERENCES r(a,a));",
        "CREATE TABLE r (a INT, b INT, UNIQUE(a,b));
         CREATE TABLE c (x INT,y INT);
         ALTER TABLE c ADD FOREIGN KEY(x,y) REFERENCES r(a,a);",
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE c (x INT, FOREIGN KEY(x,x) REFERENCES r(id,id));",
        "CREATE TABLE r (a INT, b INT, UNIQUE(a,b));
         CREATE TABLE c (x INT,y INT, FOREIGN KEY(x,y) REFERENCES r(A,\"a\"));",
        "CREATE TABLE r (a INT, b INT, UNIQUE(a,b));
         CREATE TABLE c (x INT,y INT);
         ALTER TABLE c ADD FOREIGN KEY(x,y) REFERENCES r(A,\"a\");",
        "CREATE TABLE r (a INT,b INT,UNIQUE(a,b));
         CREATE TABLE c (x INT REFERENCES r(a,a));",
        "CREATE TABLE r (a INT,b INT,UNIQUE(a,b));
         CREATE TABLE c (x INT);
         ALTER TABLE c ADD COLUMN y INT REFERENCES r(a,a);",
        "CREATE TABLE r (a INT,b INT,UNIQUE(a,b));
         CREATE TABLE c (x INT,y INT) PARTITION BY RANGE(x);
         CREATE TABLE c1 PARTITION OF c (FOREIGN KEY(x,y) REFERENCES r(a,a)) FOR VALUES FROM(0) TO(10);",
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(sql).expect_err("duplicate targets fail");
        assert!(matches!(error, Error::DuplicateReferencedColumnsForForeignKey), "{sql}");
        assert_eq!(error.to_string(), "foreign key referenced-columns list must not contain duplicates");
    }
}

/// Accepted lists preserve their column identities and positions.
fn assert_columns(database: &ParserDB, host: &[&str], referenced: &[&str]) {
    let child = database
        .table_by_target(TargetName::new("c", false), IdentifierCase::AsWritten)
        .expect("unambiguous name")
        .expect("child exists");
    let key = child
        .foreign_keys(database)
        .expect("child belongs to database")
        .next()
        .expect("foreign key exists");
    assert_eq!(
        key.host_columns(database)
            .expect("host columns exist")
            .map(ColumnLike::column_name)
            .collect::<Vec<_>>(),
        host
    );
    assert_eq!(
        key.referenced_columns(database)
            .expect("target columns exist")
            .map(ColumnLike::column_name)
            .collect::<Vec<_>>(),
        referenced
    );
}

/// Quoted uppercase targets remain distinct from folded lowercase targets.
#[test]
fn distinct_quoted_targets_are_accepted() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (\"A\" INT, a INT, UNIQUE(\"A\",a));
         CREATE TABLE c (x INT,y INT, FOREIGN KEY(x,y) REFERENCES r(\"A\",a));",
    )?;
    assert_columns(&database, &["x", "y"], &["A", "a"]);
    Ok(())
}

/// Referencing columns may repeat in both engines.
#[test]
fn duplicate_host_columns_are_accepted() -> Result<(), Error> {
    let sql = "CREATE TABLE r (a INT, b INT, UNIQUE(a,b));
               CREATE TABLE c (x INT, FOREIGN KEY(x,x) REFERENCES r(a,b));";
    assert_columns(&ParserDB::parse::<PostgreSqlDialect>(sql)?, &["x", "x"], &["a", "b"]);
    assert_columns(&ParserDB::parse::<SQLiteDialect>(sql)?, &["x", "x"], &["a", "b"]);
    Ok(())
}

/// Altering a table preserves distinct quoted target names.
#[test]
fn altered_distinct_quoted_targets_are_accepted() -> Result<(), Error> {
    let database = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE r (\"A\" INT, a INT, UNIQUE(\"A\",a));
         CREATE TABLE c (x INT,y INT);
         ALTER TABLE c ADD FOREIGN KEY(x,y) REFERENCES r(\"A\",a);",
    )?;
    assert_columns(&database, &["x", "y"], &["A", "a"]);
    Ok(())
}

/// An absent target column is diagnosed before repeated target names.
#[test]
fn missing_target_precedes_duplicate_target() {
    assert!(matches!(
        ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE r(a INT,b INT,UNIQUE(a,b));
             CREATE TABLE c(x INT,y INT,FOREIGN KEY(x,y) REFERENCES r(missing,missing));"
        ),
        Err(Error::ReferencedColumnNotFoundForForeignKey {
            referenced_column, referenced_table, host_table
        }) if referenced_column == "missing" && referenced_table == "r" && host_table == "c"
    ));
}

/// SQLite stores duplicate targets until a statement uses the foreign key.
#[test]
fn sqlite_accepts_duplicate_targets() -> Result<(), Error> {
    let database = ParserDB::parse::<SQLiteDialect>(
        "CREATE TABLE r (a INT, b INT, UNIQUE(a,b));
         CREATE TABLE c (x INT,y INT, FOREIGN KEY(x,y) REFERENCES r(a,a));",
    )?;
    assert_columns(&database, &["x", "y"], &["a", "a"]);
    Ok(())
}
