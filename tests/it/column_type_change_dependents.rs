//! Tests that `ALTER TABLE ... ALTER COLUMN ... TYPE` is refused while
//! anything outside the table reads the column.
//!
//! PostgreSQL has no `CASCADE` form for a type change and refuses it while a
//! view, a materialized view, a rule, a bound SQL function body, a policy or a
//! trigger names the column, even when the type stays the same. The indexes,
//! checks and foreign keys involving the column are rebuilt instead. The
//! expected outcomes are the ones PostgreSQL 18 gives for the same
//! statements.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

const TABLE: &str = "CREATE TABLE t (a INT, b INT);
     CREATE FUNCTION tf() RETURNS TRIGGER LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$;";

#[test]
fn a_reader_naming_the_column_refuses_the_type_change() {
    for (reader, kind, name) in [
        ("CREATE VIEW v AS SELECT a FROM t;", ObjectKind::View, "v"),
        ("CREATE VIEW v AS SELECT b FROM t WHERE a > 0;", ObjectKind::View, "v"),
        ("CREATE VIEW v AS SELECT * FROM t;", ObjectKind::View, "v"),
        ("CREATE MATERIALIZED VIEW m AS SELECT a FROM t;", ObjectKind::MaterializedView, "m"),
        (
            "CREATE TABLE u (a INT); CREATE VIEW v AS SELECT b FROM t JOIN u USING (a);",
            ObjectKind::View,
            "v",
        ),
        (
            "CREATE TABLE log (x INT);
             CREATE RULE r AS ON INSERT TO t DO ALSO INSERT INTO log VALUES (new.a);",
            ObjectKind::Rule,
            "r",
        ),
        (
            "CREATE TABLE s (x INT);
             CREATE RULE r AS ON INSERT TO s DO ALSO INSERT INTO t (a) VALUES (new.x);",
            ObjectKind::Rule,
            "r",
        ),
        (
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql BEGIN ATOMIC SELECT a FROM t; END;",
            ObjectKind::Function,
            "f",
        ),
        (
            "CREATE FUNCTION f() RETURNS INT LANGUAGE sql RETURN (SELECT a FROM t LIMIT 1);",
            ObjectKind::Function,
            "f",
        ),
        ("CREATE POLICY p ON t USING (a > 0);", ObjectKind::Policy, "p"),
        (
            "CREATE TRIGGER g BEFORE UPDATE OF a ON t FOR EACH ROW EXECUTE FUNCTION tf();",
            ObjectKind::Trigger,
            "g",
        ),
        (
            "CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.a > 0) EXECUTE FUNCTION tf();",
            ObjectKind::Trigger,
            "g",
        ),
    ] {
        let sql = format!("{TABLE} {reader} ALTER TABLE t ALTER COLUMN a TYPE BIGINT;");
        let error = parse(&sql).expect_err(reader);
        assert!(
            matches!(
                &error,
                Error::ColumnTypeChangeBlocked { table_name, column_name, dependent_kind, dependent_name }
                    if table_name == "t" && column_name == "a" && *dependent_kind == kind && dependent_name == name
            ),
            "{reader}: {error:?}"
        );
    }
}

#[test]
fn keeping_the_same_type_is_refused_as_well() {
    let error = parse(&format!(
        "{TABLE} CREATE VIEW v AS SELECT a FROM t; ALTER TABLE t ALTER COLUMN a SET DATA TYPE INT;"
    ))
    .expect_err("PostgreSQL refuses the change whatever the new type");
    assert!(matches!(error, Error::ColumnTypeChangeBlocked { .. }), "{error:?}");
}

#[test]
fn a_reader_not_naming_the_column_lets_the_type_change() {
    for reader in [
        "CREATE VIEW v AS SELECT b FROM t;",
        "CREATE VIEW v AS SELECT t FROM t;",
        "CREATE VIEW v AS SELECT count(*) AS n FROM t;",
        "CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT a FROM t';",
        "CREATE INDEX t_a ON t (a); ALTER TABLE t ADD CHECK (a > 0);",
        "CREATE TRIGGER g BEFORE UPDATE ON t FOR EACH ROW WHEN (NEW.b > 0) EXECUTE FUNCTION tf();",
        "CREATE POLICY p ON t USING (b > 0);",
    ] {
        let sql = format!("{TABLE} {reader} ALTER TABLE t ALTER COLUMN a TYPE BIGINT;");
        parse(&sql).expect(reader);
    }
}

#[test]
fn a_foreign_key_from_another_table_lets_the_type_change() {
    parse(
        "CREATE TABLE t (a INT PRIMARY KEY);
         CREATE TABLE u (x INT REFERENCES t (a));
         ALTER TABLE t ALTER COLUMN a TYPE BIGINT;",
    )
    .expect("the foreign key is rebuilt");
}

#[test]
fn a_view_on_a_table_inheriting_the_column_refuses_the_type_change() {
    let error = parse(
        "CREATE TABLE p (a INT, b INT);
         CREATE TABLE c () INHERITS (p);
         CREATE VIEW v AS SELECT a FROM c;
         ALTER TABLE p ALTER COLUMN a TYPE BIGINT;",
    )
    .expect_err("the child's column changes type with the parent's");
    assert!(
        matches!(&error, Error::ColumnTypeChangeBlocked { dependent_name, .. } if dependent_name == "v"),
        "{error:?}"
    );
}

#[test]
fn a_reader_of_another_column_of_a_view_reading_the_table_is_still_a_reader() {
    let error = parse(&format!(
        "{TABLE}
         CREATE VIEW v AS SELECT a FROM t;
         CREATE VIEW w AS SELECT b FROM t;
         ALTER TABLE t ALTER COLUMN b TYPE BIGINT;"
    ))
    .expect_err("w reads b");
    assert!(
        matches!(&error, Error::ColumnTypeChangeBlocked { dependent_name, .. } if dependent_name == "w"),
        "{error:?}"
    );
}
