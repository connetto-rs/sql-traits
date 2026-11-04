//! Tests that `ALTER TABLE child INHERIT parent` and `NO INHERIT parent` link
//! and unlink an existing table as PostgreSQL does.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{ChildMismatch, Error},
    prelude::*,
};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect};

const PARENT: &str = "CREATE TABLE p (
         id INT NOT NULL,
         note TEXT COLLATE \"C\",
         CONSTRAINT pos CHECK (id >= 0),
         CONSTRAINT own CHECK (id < 1000) NO INHERIT,
         PRIMARY KEY (id)
     );";

const CHILD: &str = "CREATE TABLE c (
         id INT NOT NULL,
         note TEXT COLLATE \"C\",
         extra INT,
         CONSTRAINT pos CHECK (id >= 0)
     );";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn accepted(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .unwrap_or_else(|| panic!("table {name} is recorded"))
}

fn local_columns(db: &ParserDB, name: &str) -> Vec<String> {
    table(db, name)
        .local_columns(db)
        .expect("held")
        .map(|column| column.column_name().to_owned())
        .collect()
}

fn mismatch(sql: &str) -> ChildMismatch {
    match parse(sql) {
        Err(Error::InheritanceChildMismatch { table_name, parent_name, reason })
            if table_name == "c" && parent_name == "p" =>
        {
            reason
        }
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn inherit_links_a_table_that_already_has_the_parents_shape() {
    let db = accepted(&format!(
        "{PARENT} {CHILD} ALTER TABLE c INHERIT p; ALTER TABLE p ADD COLUMN late INT;"
    ));
    let (p, c) = (table(&db, "p"), table(&db, "c"));
    assert_eq!(c.inherits_from(&db).expect("held").collect::<Vec<_>>(), [p]);
    assert_eq!(p.inheritors(&db).expect("held").collect::<Vec<_>>(), [c]);
    // The child keeps declaring what it had, and receives what the parent
    // adds afterwards.
    assert_eq!(local_columns(&db, "c"), ["id", "note", "extra"]);
    assert_eq!(
        c.columns(&db).expect("held").map(ColumnLike::column_name).collect::<Vec<_>>(),
        ["id", "note", "extra", "late"]
    );
    // What the parent passes is now the parent's to change.
    for change in [
        "ALTER TABLE c DROP COLUMN note;",
        "ALTER TABLE c RENAME COLUMN note TO n;",
        "ALTER TABLE c DROP CONSTRAINT pos;",
    ] {
        assert!(
            parse(&format!("{PARENT} {CHILD} ALTER TABLE c INHERIT p; {change}")).is_err(),
            "{change}"
        );
        accepted(&format!("{PARENT} {CHILD} {change}"));
    }
    accepted(&format!("{PARENT} {CHILD} ALTER TABLE ONLY c INHERIT p;"));
}

#[test]
fn inherit_refuses_a_child_without_the_parents_shape() {
    for (child, reason) in [
        (
            "CREATE TABLE c (id INT NOT NULL, CONSTRAINT pos CHECK (id >= 0));",
            ChildMismatch::MissingColumn { column_name: "note".to_owned() },
        ),
        (
            "CREATE TABLE c (id INT NOT NULL, note TEXT, CONSTRAINT pos CHECK (id >= 0));",
            ChildMismatch::ColumnCollation { column_name: "note".to_owned() },
        ),
        (
            "CREATE TABLE c (id INT, note TEXT COLLATE \"C\", CONSTRAINT pos CHECK (id >= 0));",
            ChildMismatch::ColumnNullable { column_name: "id".to_owned() },
        ),
        (
            "CREATE TABLE c (id INT NOT NULL, note TEXT COLLATE \"C\");",
            ChildMismatch::MissingConstraint { constraint_name: "pos".to_owned() },
        ),
        (
            "CREATE TABLE c (id INT NOT NULL, note TEXT COLLATE \"C\", CONSTRAINT pos CHECK (id > 0));",
            ChildMismatch::ConstraintDiffers { constraint_name: "pos".to_owned() },
        ),
    ] {
        assert_eq!(
            mismatch(&format!("{PARENT} {child} ALTER TABLE c INHERIT p;")),
            reason,
            "{child}"
        );
    }
    assert_eq!(
        mismatch(&format!(
            "{PARENT} CREATE TABLE c (id BIGINT NOT NULL, note TEXT COLLATE \"C\", CONSTRAINT pos CHECK (id >= 0));
             ALTER TABLE c INHERIT p;"
        )),
        ChildMismatch::ColumnType { column_name: "id".to_owned() }
    );
    let generated = "CREATE TABLE p (id INT, g INT GENERATED ALWAYS AS (id * 2) STORED);";
    assert_eq!(
        mismatch(&format!("{generated} CREATE TABLE c (id INT, g INT); ALTER TABLE c INHERIT p;")),
        ChildMismatch::MustBeGenerated { column_name: "g".to_owned() }
    );
    assert_eq!(
        mismatch(
            "CREATE TABLE p (a INT); CREATE TABLE c (a INT GENERATED ALWAYS AS (1) STORED);
             ALTER TABLE c INHERIT p;"
        ),
        ChildMismatch::MustNotBeGenerated { column_name: "a".to_owned() }
    );
    // Another generation expression is accepted.
    accepted(&format!(
        "{generated} CREATE TABLE c (id INT, g INT GENERATED ALWAYS AS (id * 3) STORED);
         ALTER TABLE c INHERIT p;"
    ));
    // An unnamed parent column check is met by a check with its expression.
    accepted(
        "CREATE TABLE p (id INT CHECK (id > 0));
        CREATE TABLE c (id INT CONSTRAINT p_id_check CHECK (id > 0));
        ALTER TABLE c INHERIT p;",
    );
    assert_eq!(
        mismatch(
            "CREATE TABLE p (id INT CHECK (id > 0)); CREATE TABLE c (id INT);
             ALTER TABLE c INHERIT p;"
        ),
        ChildMismatch::MissingConstraint { constraint_name: "CHECK (id > 0)".to_owned() }
    );
}

#[test]
fn inherit_refuses_a_parent_postgresql_refuses() {
    let base = format!("{PARENT} {CHILD}");
    assert!(matches!(
        parse(&format!("{base} ALTER TABLE c INHERIT p; ALTER TABLE c INHERIT p;")),
        Err(Error::ParentRepeated { .. })
    ));
    for statement in ["ALTER TABLE p INHERIT c;", "ALTER TABLE c INHERIT c;"] {
        assert!(
            matches!(
                parse(&format!("{base} ALTER TABLE c INHERIT p; {statement}")),
                Err(Error::CircularInheritance { .. })
            ),
            "{statement}"
        );
    }
    assert!(matches!(
        parse(&format!("{base} ALTER TABLE c INHERIT missing;")),
        Err(Error::ParentTableNotFound { .. })
    ));
    assert!(matches!(
        parse(&format!("{base} CREATE VIEW v AS SELECT 1 AS id; ALTER TABLE c INHERIT v;")),
        Err(Error::AlterActionUnsupportedOnRelation { .. })
    ));
    let partitioned = "CREATE TABLE pt (id INT) PARTITION BY RANGE (id);
         CREATE TABLE pp PARTITION OF pt FOR VALUES FROM (0) TO (1);
         CREATE TABLE x (id INT);";
    for (statement, check) in [
        ("ALTER TABLE pp INHERIT x;", "a partition as child"),
        ("ALTER TABLE pt INHERIT x;", "a partitioned child"),
        ("ALTER TABLE x INHERIT pt;", "a partitioned parent"),
        ("ALTER TABLE x INHERIT pp;", "a partition as parent"),
        ("ALTER TABLE pp NO INHERIT pt;", "NO INHERIT on a partition"),
    ] {
        let error = parse(&format!("{partitioned} {statement}")).expect_err(check);
        assert!(
            matches!(
                error,
                Error::InheritanceChangeOnPartition { .. }
                    | Error::InheritanceChangeOnPartitioned { .. }
                    | Error::InheritsFromPartitionedTable { .. }
                    | Error::InheritsFromPartition { .. }
            ),
            "{check}: {error:?}"
        );
    }
    assert!(matches!(
        parse(
            "CREATE TEMP TABLE tmp (id INT); CREATE TABLE x (id INT); ALTER TABLE x INHERIT pg_temp.tmp;"
        ),
        Err(Error::PermanentRelationInheritsTemporary { .. })
    ));
}

#[test]
fn no_inherit_makes_what_the_child_received_its_own() {
    let db = accepted(&format!(
        "{PARENT} {CHILD} ALTER TABLE c INHERIT p; ALTER TABLE p ADD COLUMN late INT;
         ALTER TABLE c NO INHERIT p;"
    ));
    let c = table(&db, "c");
    assert_eq!(c.inherits_from(&db).expect("held").count(), 0);
    assert_eq!(local_columns(&db, "c"), ["id", "note", "extra", "late"]);
    accepted(&format!(
        "{PARENT} {CHILD} ALTER TABLE c INHERIT p; ALTER TABLE c NO INHERIT p;
         ALTER TABLE c DROP COLUMN note; ALTER TABLE c DROP CONSTRAINT pos; DROP TABLE p;"
    ));
    // A column another parent still passes stays received.
    let db = accepted(
        "CREATE TABLE r1 (a INT); CREATE TABLE r2 (a INT);
                 CREATE TABLE rc () INHERITS (r1, r2); ALTER TABLE rc NO INHERIT r1;",
    );
    assert_eq!(local_columns(&db, "rc"), Vec::<String>::new());
    assert!(parse(
        "CREATE TABLE r1 (a INT); CREATE TABLE r2 (a INT);
         CREATE TABLE rc () INHERITS (r1, r2); ALTER TABLE rc NO INHERIT r1; ALTER TABLE rc DROP COLUMN a;"
    )
    .is_err());
    assert!(matches!(
        parse(&format!("{PARENT} {CHILD} ALTER TABLE c NO INHERIT p;")),
        Err(Error::NotAParentOf { table_name, parent_name }) if table_name == "c" && parent_name == "p"
    ));
}
