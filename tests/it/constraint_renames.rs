//! Tests that `ALTER TABLE ... RENAME CONSTRAINT` renames a constraint, written
//! or given its name by PostgreSQL, on the table and on every copy below it,
//! and that `ALTER INDEX ... RENAME TO` on the index behind a key renames the
//! key with it.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 18, and against 17 where `NOT NULL` is not involved.
#![allow(clippy::expect_used, clippy::panic)]

extern crate alloc;

use alloc::borrow::Cow;

use sql_traits::{
    errors::{Error, InheritedChange, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn accepted(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

/// Whether `table` can drop a constraint called `name` after `setup`.
fn drops(setup: &str, table: &str, name: &str) -> bool {
    parse(&format!("{setup} ALTER TABLE {table} DROP CONSTRAINT {name};")).is_ok()
}

fn owned(name: Result<Option<Cow<'_, str>>, LookupError>) -> Option<String> {
    name.expect("held").map(Cow::into_owned)
}

#[test]
fn a_constraint_answers_to_its_new_name() {
    for (setup, old) in [
        ("CREATE TABLE p (a INT CHECK (a > 0));", "p_a_check"),
        ("CREATE TABLE p (a INT CONSTRAINT k0 CHECK (a > 0));", "k0"),
        ("CREATE TABLE p (a INT, CONSTRAINT k0 CHECK (a > 0));", "k0"),
        ("CREATE TABLE p (a INT NOT NULL);", "p_a_not_null"),
        ("CREATE TABLE p (a INT PRIMARY KEY);", "p_pkey"),
    ] {
        let renamed = format!("{setup} ALTER TABLE p RENAME CONSTRAINT {old} TO k;");
        assert!(drops(&renamed, "p", "k"), "{setup}");
        assert!(!drops(&renamed, "p", old), "{setup}");
    }
    let db = accepted(
        "CREATE TABLE p (a INT NOT NULL CHECK (a > 0));
         ALTER TABLE p RENAME CONSTRAINT p_a_check TO positive;
         ALTER TABLE p RENAME CONSTRAINT p_a_not_null TO required;",
    );
    let p = db
        .table_by_target(TargetName::new("p", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("held");
    let check = p.check_constraints(&db).expect("held").next().expect("one check");
    assert_eq!(owned(check.constraint_name(&db)), Some("positive".to_owned()));
    let column = p.columns(&db).expect("held").next().expect("one column");
    assert_eq!(owned(column.not_null_constraint_name(&db)), Some("required".to_owned()));
    // The generated name is free again, so the next unnamed check takes it.
    assert!(drops(
        "CREATE TABLE p (a INT CHECK (a > 0)); ALTER TABLE p RENAME CONSTRAINT p_a_check TO k;
         ALTER TABLE p ADD CHECK (a < 9);",
        "p",
        "p_a_check"
    ));
}

#[test]
fn a_renamed_key_moves_its_index_name() {
    let renamed =
        "CREATE TABLE p (a INT PRIMARY KEY); ALTER TABLE p RENAME CONSTRAINT p_pkey TO pk;";
    assert!(parse(&format!("{renamed} CREATE TABLE pk (x INT);")).is_err());
    accepted(&format!("{renamed} CREATE TABLE p_pkey (x INT);"));
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT PRIMARY KEY); CREATE TABLE pk (x INT);
             ALTER TABLE p RENAME CONSTRAINT p_pkey TO pk;"
        ),
        Err(Error::RelationNameAlreadyTaken { .. })
    ));
}

/// PostgreSQL renames a primary key or unique constraint together with the
/// index behind it, whichever of the two the statement names.
#[test]
fn renaming_a_key_index_renames_the_key() {
    for (setup, table, old) in [
        ("CREATE TABLE p (a INT PRIMARY KEY);", "p", "p_pkey"),
        ("CREATE TABLE p (a INT UNIQUE);", "p", "p_a_key"),
        ("CREATE TABLE p (a INT CONSTRAINT u UNIQUE);", "p", "u"),
        ("CREATE TABLE p (a INT, CONSTRAINT u PRIMARY KEY (a));", "p", "u"),
        ("CREATE SCHEMA app; CREATE TABLE app.p (a INT PRIMARY KEY);", "app.p", "app.p_pkey"),
        (
            "CREATE SCHEMA app; CREATE TABLE app.p (a INT PRIMARY KEY); SET search_path TO app;",
            "p",
            "p_pkey",
        ),
    ] {
        let renamed = format!("{setup} ALTER INDEX {old} RENAME TO k;");
        let (qualifier, old_bare) = old.rsplit_once('.').unwrap_or(("", old));
        let new = if qualifier.is_empty() { "k".to_owned() } else { format!("{qualifier}.k") };
        assert!(drops(&renamed, table, "k"), "{setup}");
        assert!(parse(&format!("{renamed} ALTER INDEX {new} RENAME TO k2;")).is_ok(), "{setup}");
        assert!(!drops(&renamed, table, old_bare), "{setup}");
    }

    let db = accepted("CREATE TABLE p (a INT PRIMARY KEY); ALTER INDEX p_pkey RENAME TO pk;");
    let p = db
        .table_by_target(TargetName::new("p", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("held");
    let key = p.unique_indices(&db).expect("held").next().expect("one key");
    assert_eq!(owned(key.constraint_name(&db)), Some("pk".to_owned()));
    // The old name leaves the schema's pool of relation names, the new one
    // joins it.
    accepted(
        "CREATE TABLE p (a INT PRIMARY KEY); ALTER INDEX p_pkey RENAME TO pk; CREATE TABLE p_pkey (x INT);",
    );
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT PRIMARY KEY); ALTER INDEX p_pkey RENAME TO pk; CREATE TABLE pk (x INT);"
        ),
        Err(Error::RelationNameAlreadyTaken { .. })
    ));
}

/// Each refusal names the clash PostgreSQL 18 reports first, a relation of
/// the schema before a constraint of the table.
#[test]
fn renaming_a_key_index_onto_a_held_name_is_refused() {
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT PRIMARY KEY, c INT CHECK (c > 0));
             ALTER INDEX p_pkey RENAME TO p_c_check;"
        ),
        Err(Error::ConstraintAlreadyExists { table_name, constraint_name })
            if table_name == "p" && constraint_name == "p_c_check"
    ));
    for taken in [
        "CREATE TABLE p (a INT PRIMARY KEY); CREATE TABLE q (x INT); ALTER INDEX p_pkey RENAME TO q;",
        "CREATE TABLE p (a INT, b INT, CONSTRAINT k UNIQUE (a), CONSTRAINT l UNIQUE (b));
         ALTER INDEX k RENAME TO l;",
    ] {
        assert!(
            matches!(
                parse(taken),
                Err(Error::RelationNameAlreadyTaken { object_kind: ObjectKind::UniqueIndex, .. })
            ),
            "{taken}"
        );
    }
}

/// A partition holds its own index under its own name. A rename of the
/// partitioned table's index leaves it alone, and the partition's own index
/// can be renamed through `ALTER INDEX` too.
#[test]
fn a_partitioned_key_index_and_a_partition_key_index_rename_apart() {
    let partitioned = "CREATE TABLE pt (a INT PRIMARY KEY) PARTITION BY RANGE (a);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);";
    for (rename, expected) in [
        ("ALTER INDEX pt_pkey RENAME TO ptk;", ["ptk", "pt1_pkey"]),
        ("ALTER INDEX pt1_pkey RENAME TO y;", ["pt_pkey", "y"]),
    ] {
        let db = accepted(&format!("{partitioned} {rename}"));
        let names: Vec<_> = ["pt", "pt1"]
            .into_iter()
            .map(|name| {
                let table = db
                    .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
                    .expect("unambiguous lookup")
                    .expect("held");
                let key = table.unique_indices(&db).expect("held").next().expect("one key");
                owned(key.constraint_name(&db))
            })
            .collect();
        assert_eq!(names, expected.map(|name| Some(name.to_owned())), "{rename}");
    }
}

/// Only a check or `NOT NULL` received from a parent is the parent's alone to
/// rename. A partition renames the key and the foreign key it received.
#[test]
fn a_partition_renames_the_keys_it_received() {
    let partitioned = "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE pt (a INT PRIMARY KEY REFERENCES r, c INT CHECK (c > 0))
             PARTITION BY RANGE (a);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);";
    for (old, new) in [("pt1_pkey", "x"), ("pt_a_fkey", "f2")] {
        let renamed = format!("{partitioned} ALTER TABLE pt1 RENAME CONSTRAINT {old} TO {new};");
        accepted(&format!("{renamed} ALTER TABLE pt1 RENAME CONSTRAINT {new} TO {new}b;"));
        assert!(matches!(
            parse(&format!("{renamed} ALTER TABLE pt1 RENAME CONSTRAINT {old} TO {new}b;")),
            Err(Error::RenameConstraintNotFound { .. })
        ));
    }
    assert!(matches!(
        parse(&format!("{partitioned} ALTER TABLE pt1 RENAME CONSTRAINT pt_c_check TO c2;")),
        Err(Error::InheritedConstraintNotRenamable { table_name, constraint_name })
            if table_name == "pt1" && constraint_name == "pt_c_check"
    ));
}

#[test]
fn every_copy_below_is_renamed_with_it() {
    for (parent, child) in [
        ("CREATE TABLE p (a INT NOT NULL CHECK (a > 0));", "CREATE TABLE c () INHERITS (p);"),
        (
            "CREATE TABLE p (a INT NOT NULL CHECK (a > 0));",
            "CREATE TABLE c (a INT CONSTRAINT p_a_check CHECK (a > 0)) INHERITS (p);",
        ),
        (
            "CREATE TABLE p (a INT NOT NULL CHECK (a > 0)) PARTITION BY LIST (a);",
            "CREATE TABLE c PARTITION OF p FOR VALUES IN (1);",
        ),
    ] {
        for old in ["p_a_check", "p_a_not_null"] {
            let renamed = format!("{parent} {child} ALTER TABLE p RENAME CONSTRAINT {old} TO k;");
            assert!(
                matches!(
                    parse(&format!("{renamed} ALTER TABLE c DROP CONSTRAINT k;")),
                    Err(Error::InheritedConstraintNotDroppable { .. })
                ),
                "{child} {old}"
            );
            assert!(
                matches!(
                    parse(&format!("{renamed} ALTER TABLE c DROP CONSTRAINT {old};")),
                    Err(Error::DropConstraintNotFound { .. })
                ),
                "{child} {old}"
            );
        }
    }
}

#[test]
fn a_rename_postgresql_refuses_is_refused() {
    let family = "CREATE TABLE p (a INT CHECK (a > 0), b INT CHECK (b > 0));
                  CREATE TABLE c () INHERITS (p);";
    assert!(matches!(
        parse(&format!("{family} ALTER TABLE c RENAME CONSTRAINT p_a_check TO k;")),
        Err(Error::InheritedConstraintNotRenamable { table_name, constraint_name })
            if table_name == "c" && constraint_name == "p_a_check"
    ));
    let only = parse(&format!("{family} ALTER TABLE ONLY p RENAME CONSTRAINT p_a_check TO k;"))
        .expect_err("copies below keep the old name");
    assert!(only.to_string().contains("rename a constraint of"), "{only}");
    assert!(matches!(
        only,
        Error::OnlyRefusedWithChildren { change: InheritedChange::RenameConstraint, .. }
    ));
    assert!(matches!(
        parse(&format!("{family} ALTER TABLE p RENAME CONSTRAINT p_a_check TO p_b_check;")),
        Err(Error::ConstraintAlreadyExists { table_name, constraint_name })
            if table_name == "p" && constraint_name == "p_b_check"
    ));
    assert!(matches!(
        parse("CREATE TABLE p (a INT); ALTER TABLE p RENAME CONSTRAINT nope TO k;"),
        Err(Error::RenameConstraintNotFound { table_name, constraint_name })
            if table_name == "p" && constraint_name == "nope"
    ));
    accepted("ALTER TABLE IF EXISTS absent RENAME CONSTRAINT nope TO k;");
    // Without children, ONLY changes nothing.
    accepted(
        "CREATE TABLE p (a INT CHECK (a > 0)); ALTER TABLE ONLY p RENAME CONSTRAINT p_a_check TO k;",
    );
}

#[test]
fn each_written_kind_of_constraint_is_renamed() {
    let setup = "CREATE TABLE r (id INT PRIMARY KEY);
        CREATE TABLE p (a INT, b INT, c INT, d INT, CONSTRAINT u UNIQUE (b),
            CONSTRAINT pk PRIMARY KEY (a), CONSTRAINT f FOREIGN KEY (c) REFERENCES r,
            CONSTRAINT x EXCLUDE USING btree (d WITH =));";
    for old in ["u", "pk", "f", "x"] {
        let renamed = format!("{setup} ALTER TABLE p RENAME CONSTRAINT {old} TO {old}2;");
        assert!(drops(&renamed, "p", &format!("{old}2")), "{old}");
        assert!(!drops(&renamed, "p", old), "{old}");
    }
    // A key adopting an index answers to the index's name until renamed.
    assert!(drops(
        "CREATE TABLE p (a INT); CREATE UNIQUE INDEX i ON p (a);
         ALTER TABLE p ADD PRIMARY KEY USING INDEX i; ALTER TABLE p RENAME CONSTRAINT i TO j;",
        "p",
        "j"
    ));
    // A check kept from the tables below renames without them, ONLY or not.
    let kept = "CREATE TABLE p (a INT, CONSTRAINT own CHECK (a > 0) NO INHERIT);
                CREATE TABLE c () INHERITS (p);";
    for only in ["", "ONLY "] {
        assert!(drops(
            &format!("{kept} ALTER TABLE {only}p RENAME CONSTRAINT own TO own2;"),
            "p",
            "own2"
        ));
    }
}
