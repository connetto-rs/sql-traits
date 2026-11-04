//! Tests that `ALTER TABLE child INHERIT parent` and `NO INHERIT parent` link
//! and unlink an existing table as PostgreSQL does.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 17 and 18.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{ChildMismatch, Error, ObjectKind},
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

/// The reason a refusal gives, after checking the report names both tables
/// and the column or check at fault.
fn mismatch(sql: &str) -> ChildMismatch {
    let error = parse(sql).expect_err("the link is refused");
    let message = error.to_string();
    match error {
        Error::InheritanceChildMismatch { table_name, parent_name, reason }
            if table_name == "c" && parent_name == "p" =>
        {
            let culprit = match &reason {
                ChildMismatch::MissingColumn { column_name }
                | ChildMismatch::ColumnType { column_name }
                | ChildMismatch::ColumnCollation { column_name }
                | ChildMismatch::ColumnNullable { column_name }
                | ChildMismatch::MustBeGenerated { column_name }
                | ChildMismatch::MustNotBeGenerated { column_name } => column_name,
                ChildMismatch::MissingConstraint { constraint_name }
                | ChildMismatch::ConstraintDiffers { constraint_name }
                | ChildMismatch::ConstraintNotInherited { constraint_name }
                | ChildMismatch::ConstraintNotValid { constraint_name } => constraint_name,
            };
            assert!(
                message.contains("`c`")
                    && message.contains("`p`")
                    && message.contains(&format!("`{culprit}`")),
                "{message}"
            );
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
        (
            "CREATE TABLE c (id INT NOT NULL, note TEXT COLLATE \"C\", CONSTRAINT pos CHECK (id >= 0) NO INHERIT);",
            ChildMismatch::ConstraintNotInherited { constraint_name: "pos".to_owned() },
        ),
        (
            "CREATE TABLE c (id INT NOT NULL, note TEXT COLLATE \"C\");
             ALTER TABLE c ADD CONSTRAINT pos CHECK (id >= 0) NOT VALID;",
            ChildMismatch::ConstraintNotValid { constraint_name: "pos".to_owned() },
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
    for (parent, child) in [
        ("CREATE TABLE p (a INT[]);", "CREATE TABLE c (a INT);"),
        ("CREATE DOMAIN d AS INT; CREATE TABLE p (a d);", "CREATE TABLE c (a INT);"),
    ] {
        assert_eq!(
            mismatch(&format!("{parent} {child} ALTER TABLE c INHERIT p;")),
            ChildMismatch::ColumnType { column_name: "a".to_owned() },
            "{parent}"
        );
    }
    // What PostgreSQL stores the same way is the same: array dimensions, an
    // explicit default collation, parentheses, and a quoted name folding alike.
    accepted(
        "CREATE TABLE p (a INT[], t TEXT, CONSTRAINT \"k\" CHECK ((a IS NOT NULL) AND (t > '')));
         CREATE TABLE c (a INT[][], t TEXT COLLATE \"default\", CONSTRAINT k CHECK (a IS NOT NULL AND t > ''));
         ALTER TABLE c INHERIT p;",
    );
    // A parent's unvalidated check is met by a validated one.
    accepted(
        "CREATE TABLE p (a INT); ALTER TABLE p ADD CONSTRAINT k CHECK (a > 0) NOT VALID;
         CREATE TABLE c (a INT, CONSTRAINT k CHECK (a > 0)); ALTER TABLE c INHERIT p;",
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
    for operation in ["INHERIT", "NO INHERIT"] {
        assert!(
            matches!(
                parse(&format!("{base} ALTER TABLE c {operation} missing;")),
                Err(Error::ParentTableNotFound { .. })
            ),
            "{operation}"
        );
    }
    assert!(matches!(
        parse(&format!("{base} CREATE VIEW v AS SELECT 1 AS id; ALTER TABLE c INHERIT v;")),
        Err(Error::AlterActionUnsupportedOnRelation { object_kind: ObjectKind::View, .. })
    ));
    assert!(matches!(
        parse(&format!("{base} CREATE SEQUENCE s; ALTER TABLE c INHERIT s;")),
        Err(Error::AlterActionUnsupportedOnRelation { object_kind: ObjectKind::Sequence, .. })
    ));
    // NO INHERIT names whatever it reaches as no parent.
    for other in [
        "CREATE VIEW v AS SELECT 1 AS id;",
        "CREATE MATERIALIZED VIEW v AS SELECT 1 AS id;",
        "CREATE SEQUENCE v;",
    ] {
        assert!(
            matches!(
                parse(&format!("{base} {other} ALTER TABLE c NO INHERIT v;")),
                Err(Error::NotAParentOf { table_name, parent_name }) if table_name == "c" && parent_name == "v"
            ),
            "{other}"
        );
    }
    // A partitioned table is named as having no such partition.
    assert!(matches!(
        parse(&format!(
            "{base} CREATE TABLE pt (id INT) PARTITION BY LIST (id); ALTER TABLE c NO INHERIT pt;"
        )),
        Err(Error::NotAPartitionOf { table_name, partition_name })
            if table_name == "pt" && partition_name == "c"
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
    // A check received from the unlinked parent alone becomes droppable, one
    // another parent still passes does not.
    let checks = "CREATE TABLE r1 (a INT, CONSTRAINT k CHECK (a > 0));
         CREATE TABLE r2 (a INT, CONSTRAINT k CHECK (a > 0));";
    assert!(
        parse(&format!(
            "{checks} CREATE TABLE one () INHERITS (r1); ALTER TABLE one DROP CONSTRAINT k;"
        ))
        .is_err()
    );
    accepted(&format!(
        "{checks} CREATE TABLE one () INHERITS (r1); ALTER TABLE one NO INHERIT r1;
         ALTER TABLE one DROP CONSTRAINT k;"
    ));
    let two =
        format!("{checks} CREATE TABLE two () INHERITS (r1, r2); ALTER TABLE two NO INHERIT r1;");
    assert!(parse(&format!("{two} ALTER TABLE two DROP CONSTRAINT k;")).is_err());
    accepted(&format!("{two} ALTER TABLE two NO INHERIT r2; ALTER TABLE two DROP CONSTRAINT k;"));
    // An unlinked check stays the child's own when the parent is linked again,
    // so dropping it from the parent leaves the child's.
    let db = accepted(&format!(
        "{checks} CREATE TABLE one () INHERITS (r1); ALTER TABLE one NO INHERIT r1;
         ALTER TABLE one INHERIT r1; ALTER TABLE r1 DROP CONSTRAINT k;"
    ));
    assert_eq!(table(&db, "one").constraints.len(), 1);
    assert!(matches!(
        parse(&format!("{PARENT} {CHILD} ALTER TABLE c NO INHERIT p;")),
        Err(Error::NotAParentOf { table_name, parent_name }) if table_name == "c" && parent_name == "p"
    ));
}

#[test]
fn if_exists_skips_an_absent_table() {
    for operation in ["INHERIT", "NO INHERIT"] {
        accepted(&format!("CREATE TABLE p (a INT); ALTER TABLE IF EXISTS missing {operation} p;"));
    }
}
