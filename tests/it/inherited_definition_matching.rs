//! Tests that a child's own column or check is matched against its parent's
//! the way PostgreSQL compares what it stores: types by identity, checks by
//! their parsed expression and folded name.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn accepted(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

#[test]
fn a_check_merges_however_it_is_parenthesized_or_quoted() {
    for (parent, child) in [
        ("CONSTRAINT k CHECK (a > 0)", "CONSTRAINT k CHECK ((a > 0))"),
        ("CONSTRAINT k CHECK ((a > 0) AND (b > 0))", "CONSTRAINT k CHECK (a > 0 AND b > 0)"),
        ("CONSTRAINT \"k\" CHECK (a > 0)", "CONSTRAINT k CHECK (a > 0)"),
        ("CONSTRAINT k CHECK (A > 0)", "CONSTRAINT k CHECK (\"a\" > 0)"),
        ("CONSTRAINT k CHECK (p.a > 0)", "CONSTRAINT k CHECK (c.a > 0)"),
    ] {
        let database = accepted(&format!(
            "CREATE TABLE p (a INT, b INT, {parent});
             CREATE TABLE c (a INT, b INT, {child}) INHERITS (p);"
        ));
        // Merged, so the child holds one check and cannot drop it.
        let c = database
            .table_by_target(TargetName::new("c", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("held");
        assert_eq!(c.constraints.len(), 1, "{child}");
        assert!(
            parse(&format!(
                "CREATE TABLE p (a INT, b INT, {parent});
                 CREATE TABLE c (a INT, b INT, {child}) INHERITS (p);
                 ALTER TABLE c DROP CONSTRAINT k;"
            ))
            .is_err(),
            "{child}"
        );
    }
    // The tree decides, so moving the parentheses is another check.
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT, b INT, CONSTRAINT k CHECK ((a + b) * 2 > 0));
             CREATE TABLE c (a INT, b INT, CONSTRAINT k CHECK (a + b * 2 > 0)) INHERITS (p);"
        ),
        Err(Error::InheritedConstraintConflict { .. })
    ));
    // A check a parent adds later merges with the child's the same way.
    accepted(
        "CREATE TABLE p (a INT); CREATE TABLE c () INHERITS (p);
         ALTER TABLE c ADD CONSTRAINT k CHECK ((a > 0));
         ALTER TABLE p ADD CONSTRAINT k CHECK (a > 0);",
    );
}

#[test]
fn an_array_or_a_domain_is_its_own_type() {
    for (parent, child) in [
        ("CREATE TABLE p (a INT[]);", "CREATE TABLE c (a INT) INHERITS (p);"),
        ("CREATE TABLE p (a INT);", "CREATE TABLE c (a INT[]) INHERITS (p);"),
        ("CREATE DOMAIN d AS INT; CREATE TABLE p (a d);", "CREATE TABLE c (a INT) INHERITS (p);"),
        ("CREATE TABLE p (a TEXT[]);", "CREATE TABLE c (a INT[]) INHERITS (p);"),
    ] {
        assert!(
            matches!(
                parse(&format!("{parent} {child}")),
                Err(Error::InheritedColumnTypeConflict { .. })
            ),
            "{parent} {child}"
        );
    }
    // Dimensions are not part of an array type, and a domain matches itself.
    accepted("CREATE TABLE p (a INT[]); CREATE TABLE c (a INTEGER[][]) INHERITS (p);");
    accepted("CREATE DOMAIN d AS INT; CREATE TABLE p (a d); CREATE TABLE c (a d) INHERITS (p);");
}

#[test]
fn a_column_a_parent_adds_merges_with_the_childs_own() {
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT); CREATE TABLE c (b TEXT) INHERITS (p);
             ALTER TABLE p ADD COLUMN b INT;"
        ),
        Err(Error::InheritedColumnTypeConflict { .. })
    ));
    // The child keeps a column it declared when the parent drops its own, and
    // what reads that column keeps reading it.
    for setup in [
        "CREATE TABLE p (a INT); CREATE TABLE c (b INT) INHERITS (p); ALTER TABLE p ADD COLUMN b INT;",
        "CREATE TABLE p (a INT, b INT); CREATE TABLE c (b INT) INHERITS (p);",
    ] {
        assert!(
            parse(&format!("{setup} ALTER TABLE c DROP COLUMN b;")).is_err(),
            "{setup}: received from the parent while it holds the column"
        );
        accepted(&format!("{setup} ALTER TABLE p DROP COLUMN b; ALTER TABLE c DROP COLUMN b;"));
        accepted(&format!(
            "{setup} CREATE VIEW v AS SELECT b FROM ONLY c; ALTER TABLE p DROP COLUMN b;"
        ));
    }
    // Below a table keeping its copy, the column stays received from it.
    assert!(
        parse(
            "CREATE TABLE p (a INT, b INT); CREATE TABLE c (b INT) INHERITS (p);
             CREATE TABLE g () INHERITS (c); ALTER TABLE p DROP COLUMN b; ALTER TABLE g DROP COLUMN b;"
        )
        .is_err()
    );
    // A column another parent still passes stays received, until the last
    // parent drops it.
    let two = "CREATE TABLE p1 (a INT); CREATE TABLE p2 (a INT);
               CREATE TABLE c () INHERITS (p1, p2); ALTER TABLE p1 DROP COLUMN a;";
    assert!(parse(&format!("{two} ALTER TABLE c DROP COLUMN a;")).is_err());
    assert!(parse(&format!("{two} ALTER TABLE c ADD COLUMN a TEXT;")).is_err());
    accepted(&format!("{two} ALTER TABLE p2 DROP COLUMN a; ALTER TABLE c ADD COLUMN a TEXT;"));
    // A table reached through two losing parents loses it too.
    accepted(
        "CREATE TABLE p (a INT, b INT); CREATE TABLE l () INHERITS (p); CREATE TABLE r () INHERITS (p);
         CREATE TABLE g () INHERITS (l, r); ALTER TABLE p DROP COLUMN b; ALTER TABLE g ADD COLUMN b TEXT;",
    );
}

#[test]
fn only_a_table_or_foreign_table_can_be_inherited_from() {
    for (relation, object_kind) in [
        ("CREATE VIEW v AS SELECT 1 AS a;", ObjectKind::View),
        ("CREATE MATERIALIZED VIEW v AS SELECT 1 AS a;", ObjectKind::MaterializedView),
        ("CREATE SEQUENCE v;", ObjectKind::Sequence),
    ] {
        let error = parse(&format!("{relation} CREATE TABLE c () INHERITS (v);"))
            .expect_err("only a table is a parent");
        assert!(
            matches!(
                &error,
                Error::InheritsFromNonTable { table_name, parent_name, object_kind: kind }
                    if table_name == "c" && parent_name == "v" && *kind == object_kind
            ),
            "{relation}: {error:?}"
        );
    }
}
