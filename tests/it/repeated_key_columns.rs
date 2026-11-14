//! Tests that a primary key or unique constraint naming one column twice is
//! refused under PostgreSQL and MySQL, and that an index naming one column
//! twice is refused under MySQL and kept under PostgreSQL.
//!
//! Every expectation was measured against PostgreSQL 18.6, MySQL 8.4 and
//! SQLite 3. PostgreSQL's `CREATE TABLE` checks the key columns in order, each
//! for being declared and then for repeating an earlier one, while its
//! `ALTER TABLE ... ADD` looks for a repeat across the whole list before it
//! looks for an undeclared column. MySQL checks in order in both. SQLite keeps
//! every repeat.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, KeyConstraint, LookupError},
    prelude::*,
};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect, SQLiteDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn assert_repeated(sql: &str, table: &str, column: &str, kind: KeyConstraint) {
    let error = parse(sql).expect_err("PostgreSQL refuses a key naming a column twice");
    assert!(
        matches!(
            &error,
            Error::RepeatedKeyColumn { table_name, column_name, constraint }
                if table_name == table && column_name == column && *constraint == kind
        ),
        "{sql}: {error:?}"
    );
}

#[test]
fn a_key_constraint_naming_a_column_twice_is_refused() {
    for (sql, table, column, kind) in [
        ("CREATE TABLE u1 (b INT, UNIQUE (b, b));", "u1", "b", KeyConstraint::Unique),
        ("CREATE TABLE u2 (a INT, PRIMARY KEY (a, a));", "u2", "a", KeyConstraint::PrimaryKey),
        (
            "CREATE TABLE u3 (b INT, c INT, CONSTRAINT u3_bcb UNIQUE (b, c, b));",
            "u3",
            "b",
            KeyConstraint::Unique,
        ),
        (
            "CREATE TABLE v (a INT, b INT); ALTER TABLE v ADD CONSTRAINT v_bb UNIQUE (b, b);",
            "v",
            "b",
            KeyConstraint::Unique,
        ),
        (
            "CREATE TABLE w (a INT); ALTER TABLE w ADD PRIMARY KEY (a, a);",
            "w",
            "a",
            KeyConstraint::PrimaryKey,
        ),
        (
            "CREATE TABLE p (b INT); CREATE TABLE c (UNIQUE (b, b)) INHERITS (p);",
            "c",
            "b",
            KeyConstraint::Unique,
        ),
    ] {
        assert_repeated(sql, table, column, kind);
    }
}

#[test]
fn key_columns_repeat_when_their_stored_names_match() {
    assert_repeated("CREATE TABLE t (b INT, UNIQUE (b, B));", "t", "B", KeyConstraint::Unique);
    assert_repeated("CREATE TABLE t (b INT, UNIQUE (b, \"b\"));", "t", "b", KeyConstraint::Unique);
    parse("CREATE TABLE t (\"B\" INT, b INT, UNIQUE (b, \"B\"));")
        .expect("`b` and `\"B\"` are two columns");
}

#[test]
fn create_table_checks_each_key_column_before_the_next() {
    let error = parse("CREATE TABLE t (b INT, UNIQUE (z, b, b));").expect_err("z is not declared");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound {
            table_name, column_name }) if table_name == "t" && column_name == "z"),
        "got {error:?}"
    );
    assert_repeated("CREATE TABLE t (b INT, UNIQUE (b, b, z));", "t", "b", KeyConstraint::Unique);
}

#[test]
fn alter_table_looks_for_a_repeat_before_an_undeclared_column() {
    assert_repeated(
        "CREATE TABLE t (b INT); ALTER TABLE t ADD UNIQUE (z, b, b);",
        "t",
        "b",
        KeyConstraint::Unique,
    );
    assert_repeated(
        "CREATE TABLE t (b INT); ALTER TABLE t ADD PRIMARY KEY (z, z);",
        "t",
        "z",
        KeyConstraint::PrimaryKey,
    );
}

#[test]
fn an_index_or_include_list_naming_a_column_twice_is_kept() {
    for sql in [
        "CREATE TABLE t (a INT); CREATE INDEX t_a_a ON t (a, a);",
        "CREATE TABLE t (b INT, c INT); CREATE UNIQUE INDEX t_b_c_b ON t (b, c, b);",
        "CREATE TABLE t (b INT, UNIQUE (b) INCLUDE (b));",
        "CREATE TABLE t (b INT, c INT, UNIQUE (b) INCLUDE (c, c));",
    ] {
        let refusal = parse(sql).err();
        assert!(refusal.is_none(), "{sql}: {refusal:?}");
    }
}

#[test]
fn a_foreign_table_refuses_the_key_before_its_columns_are_read() {
    let error = parse(
        "CREATE FOREIGN TABLE f (a INT) SERVER s;
         ALTER TABLE f ADD UNIQUE (a, a);",
    )
    .expect_err("a foreign table holds no unique constraint");
    assert!(
        matches!(
            &error,
            Error::ConstraintUnsupportedOnForeignTable { table_name, constraint: KeyConstraint::Unique }
                if table_name == "f"
        ),
        "got {error:?}"
    );
}

#[test]
fn mysql_refuses_a_repeat_in_every_key_checking_each_column_in_turn() {
    let mysql = ParserDB::parse::<MySqlDialect>;
    for (sql, column, kind) in [
        ("CREATE TABLE t (b INT, UNIQUE (b, b));", "b", KeyConstraint::Unique),
        ("CREATE TABLE t (a INT, PRIMARY KEY (a, a));", "a", KeyConstraint::PrimaryKey),
        ("CREATE TABLE t (b INT); ALTER TABLE t ADD UNIQUE (b, b, z);", "b", KeyConstraint::Unique),
    ] {
        let error = mysql(sql).expect_err("MySQL refuses a key naming a column twice");
        assert!(
            matches!(&error, Error::RepeatedKeyColumn { table_name, column_name, constraint }
                if table_name == "t" && column_name == column && *constraint == kind),
            "{sql}: {error:?}"
        );
    }

    let error = mysql("CREATE TABLE t (b INT); ALTER TABLE t ADD UNIQUE (z, b, b);")
        .expect_err("z is not declared");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound {
            table_name, column_name }) if table_name == "t" && column_name == "z"),
        "got {error:?}"
    );

    for sql in [
        "CREATE TABLE t (a INT); CREATE INDEX t_a_a ON t (a, a);",
        "CREATE TABLE t (a INT); CREATE UNIQUE INDEX u ON t (a, a);",
        "CREATE TABLE t (a INT, KEY (a, a));",
        "CREATE TABLE t (a INT); ALTER TABLE t ADD INDEX (a, a);",
    ] {
        let error = mysql(sql).expect_err("MySQL refuses an index naming a column twice");
        assert!(
            matches!(&error, Error::RepeatedIndexColumn { table_name, column_name }
                if table_name == "t" && column_name == "a"),
            "{sql}: {error:?}"
        );
    }
}

#[test]
fn sqlite_keeps_a_key_naming_a_column_twice() {
    for sql in [
        "CREATE TABLE t (b INT, UNIQUE (b, b));",
        "CREATE TABLE u (a INT, PRIMARY KEY (a, a));",
        "CREATE TABLE v (a INT); CREATE INDEX v_a_a ON v (a, a);",
    ] {
        let refusal = ParserDB::parse::<SQLiteDialect>(sql).err();
        assert!(refusal.is_none(), "{sql}: {refusal:?}");
    }
}
