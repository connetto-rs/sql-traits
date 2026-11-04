//! Tests that a constraint written without a name answers to the name
//! PostgreSQL gives it, fixed when the constraint is created.
//!
//! Every accepted and refused statement here was measured against PostgreSQL
//! 18, where `NOT NULL` is named too.
#![allow(clippy::expect_used, clippy::panic)]

use std::borrow::Cow;

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::{
    ast::CreateTable,
    dialect::{GenericDialect, PostgreSqlDialect},
};

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

/// The name an accessor answered, owned.
fn owned(name: Result<Option<Cow<'_, str>>, LookupError>) -> Option<String> {
    name.expect("held").map(Cow::into_owned)
}

/// The expected names, owned.
fn names(expected: &[Option<&str>]) -> Vec<Option<String>> {
    expected.iter().map(|name| name.map(str::to_owned)).collect()
}

/// Refuses dropping `name` from table `p` after `setup`, unless it is the
/// name PostgreSQL gave a constraint there.
fn drops(setup: &str, name: &str) -> bool {
    parse(&format!("{setup} ALTER TABLE p DROP CONSTRAINT {name};")).is_ok()
}

#[test]
fn each_kind_of_constraint_answers_to_its_generated_name() {
    for (setup, name) in [
        ("CREATE TABLE p (a INT CHECK (a > 0));", "p_a_check"),
        ("CREATE TABLE p (a INT, CHECK (a > 0));", "p_a_check"),
        ("CREATE TABLE p (a INT CHECK (a > b), b INT);", "p_check"),
        ("CREATE TABLE p (a INT CHECK (true));", "p_check"),
        ("CREATE TABLE p (a INT PRIMARY KEY);", "p_pkey"),
        ("CREATE TABLE p (a INT, b INT UNIQUE);", "p_b_key"),
        ("CREATE TABLE p (d INT, e INT, UNIQUE (d, e));", "p_d_e_key"),
        ("CREATE TABLE r (id INT PRIMARY KEY); CREATE TABLE p (c INT REFERENCES r);", "p_c_fkey"),
        (
            "CREATE TABLE r (x INT, y INT, PRIMARY KEY (x, y));
             CREATE TABLE p (a INT, b INT, FOREIGN KEY (a, b) REFERENCES r);",
            "p_a_b_fkey",
        ),
        ("CREATE TABLE p (a INT NOT NULL);", "p_a_not_null"),
        ("CREATE TABLE p (a SERIAL);", "p_a_not_null"),
        ("CREATE TABLE p (a INT, EXCLUDE USING btree (a WITH =));", "p_a_excl"),
        ("CREATE TABLE p (a INT); ALTER TABLE p ADD CHECK (a > 0);", "p_a_check"),
        ("CREATE TABLE p (a INT); ALTER TABLE p ADD COLUMN b INT CHECK (b > 0);", "p_b_check"),
        ("CREATE TABLE p (a INT); ALTER TABLE p ALTER COLUMN a SET NOT NULL;", "p_a_not_null"),
        (
            "CREATE TABLE p (a INT); CREATE UNIQUE INDEX i ON p (a);
             ALTER TABLE p ADD PRIMARY KEY USING INDEX i;",
            "i",
        ),
    ] {
        assert!(drops(setup, name), "{setup} {name}");
        assert!(!drops(setup, "p_other"), "{setup}");
    }
    // A long name is cut the way PostgreSQL cuts it, and a quoted one keeps
    // its case.
    let long_table = "t".repeat(60);
    let long_column = "c".repeat(60);
    accepted(&format!(
        "CREATE TABLE {long_table} ({long_column} INT CHECK ({long_column} > 0));
         ALTER TABLE {long_table} DROP CONSTRAINT {}_{}_check;",
        "t".repeat(28),
        "c".repeat(28)
    ));
    accepted(
        "CREATE TABLE \"P\" (\"A\" INT CHECK (\"A\" > 0));
         ALTER TABLE \"P\" DROP CONSTRAINT \"P_A_check\";",
    );
}

#[test]
fn a_taken_name_gets_the_next_number() {
    for (setup, name) in [
        ("CREATE TABLE p (a INT CHECK (a > 0), CHECK (a < 9));", "p_a_check1"),
        ("CREATE TABLE p (a INT CHECK (a > 0) CHECK (a > 0));", "p_a_check1"),
        (
            "CREATE TABLE p (a INT); ALTER TABLE p ADD CHECK (a > 0), ADD CHECK (a < 9);",
            "p_a_check1",
        ),
        (
            "CREATE TABLE q (a INT, CONSTRAINT p_a_check CHECK (a > 0));
             CREATE TABLE p (a INT CHECK (a > 0));",
            "p_a_check1",
        ),
        (
            "CREATE TABLE r (id INT PRIMARY KEY);
             CREATE TABLE p (a INT REFERENCES r, FOREIGN KEY (a) REFERENCES r);",
            "p_a_fkey1",
        ),
        ("CREATE TABLE p_pkey (x INT); CREATE TABLE p (a INT PRIMARY KEY);", "p_pkey1"),
        (
            "CREATE TABLE q (a INT, CONSTRAINT p_a_not_null CHECK (a > 0));
             CREATE TABLE p (a INT NOT NULL);",
            "p_a_not_null1",
        ),
    ] {
        assert!(drops(setup, name), "{setup} {name}");
    }
    // An index name is no constraint name, so a check does not avoid it.
    assert!(drops(
        "CREATE TABLE q (a INT); CREATE INDEX p_a_check ON q (a); CREATE TABLE p (a INT CHECK (a > 0));",
        "p_a_check"
    ));
}

#[test]
fn a_generated_name_stays_taken() {
    // A key's name is its index's, so no relation can take it.
    for (setup, relation) in [
        ("CREATE TABLE p (a INT PRIMARY KEY);", "CREATE TABLE p_pkey (x INT);"),
        ("CREATE TABLE p (a INT UNIQUE);", "CREATE INDEX p_a_key ON p (a);"),
    ] {
        assert!(parse(&format!("{setup} {relation}")).is_err(), "{relation}");
    }
    accepted("CREATE TABLE p (a INT CHECK (a > 0)); CREATE TABLE p_a_check (x INT);");
    // No other constraint of the table can take it.
    for added in ["CONSTRAINT p_a_check UNIQUE (a)", "CONSTRAINT p_a_check CHECK (a < 9)"] {
        assert!(
            matches!(
                parse(&format!("CREATE TABLE p (a INT CHECK (a > 0)); ALTER TABLE p ADD {added};")),
                Err(Error::ConstraintAlreadyExists { table_name, constraint_name })
                    if table_name == "p" && constraint_name == "p_a_check"
            ),
            "{added}"
        );
    }
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT PRIMARY KEY); ALTER TABLE p ADD CONSTRAINT p_pkey CHECK (a > 0);"
        ),
        Err(Error::ConstraintAlreadyExists { .. })
    ));
}

#[test]
fn a_generated_name_is_fixed_when_the_constraint_is_created() {
    accepted(
        "CREATE TABLE p (a INT CHECK (a > 0)); ALTER TABLE p RENAME TO q;
         ALTER TABLE q DROP CONSTRAINT p_a_check;",
    );
    accepted(
        "CREATE TABLE p (a INT CHECK (a > 0)); ALTER TABLE p RENAME COLUMN a TO z;
         ALTER TABLE p DROP CONSTRAINT p_a_check;",
    );
    // Dropping the first leaves the second its own name.
    accepted(
        "CREATE TABLE p (a INT, CHECK (a > 0), CHECK (a < 9)); ALTER TABLE p DROP CONSTRAINT p_a_check;
         ALTER TABLE p DROP CONSTRAINT p_a_check1;",
    );
    accepted(
        "CREATE TABLE p (a INT); ALTER TABLE p ADD CHECK (a > 0) NOT VALID;
         ALTER TABLE p VALIDATE CONSTRAINT p_a_check;",
    );
}

#[test]
fn a_copy_keeps_its_parents_name() {
    for child in
        ["CREATE TABLE c () INHERITS (p);", "CREATE TABLE c PARTITION OF p FOR VALUES IN (1);"]
    {
        let parent = if child.contains("PARTITION") {
            "CREATE TABLE p (a INT NOT NULL CHECK (a > 0)) PARTITION BY LIST (a);"
        } else {
            "CREATE TABLE p (a INT NOT NULL CHECK (a > 0));"
        };
        for name in ["p_a_check", "p_a_not_null"] {
            assert!(
                matches!(
                    parse(&format!("{parent} {child} ALTER TABLE c DROP CONSTRAINT {name};")),
                    Err(Error::InheritedConstraintNotDroppable { .. })
                ),
                "{child} {name}"
            );
        }
        assert!(
            parse(&format!("{parent} {child} ALTER TABLE c DROP CONSTRAINT c_a_check;")).is_err()
        );
    }
    // A check written on its own is copied under its name too.
    assert!(matches!(
        parse(
            "CREATE TABLE p (a INT, CHECK (a > 0)); CREATE TABLE c () INHERITS (p);
             ALTER TABLE c DROP CONSTRAINT p_a_check;"
        ),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
    // A child declaring its own NOT NULL names it after itself.
    accepted(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c (a INT NOT NULL) INHERITS (p);
         ALTER TABLE p ALTER COLUMN a DROP NOT NULL; ALTER TABLE c DROP CONSTRAINT c_a_not_null;",
    );
}

#[test]
fn an_unnamed_parent_check_is_required_by_its_generated_name() {
    let attach = "ALTER TABLE p ATTACH PARTITION c FOR VALUES IN (1);";
    for (parent, link) in [
        ("CREATE TABLE p (a INT CHECK (a > 0));", "ALTER TABLE c INHERIT p;"),
        ("CREATE TABLE p (a INT CHECK (a > 0)) PARTITION BY LIST (a);", attach),
    ] {
        assert!(
            parse(&format!("{parent} CREATE TABLE c (a INT CHECK (a > 0)); {link}")).is_err(),
            "{link}: the child's check is named c_a_check"
        );
        accepted(&format!(
            "{parent} CREATE TABLE c (a INT CONSTRAINT p_a_check CHECK (a > 0)); {link}"
        ));
    }
}

#[test]
fn the_name_postgresql_gave_is_readable() {
    let db = accepted(
        "CREATE TABLE r (id INT PRIMARY KEY);
         CREATE TABLE p (a INT PRIMARY KEY CHECK (a > 0) CHECK (a > 0), b INT REFERENCES r,
             c INT CONSTRAINT written NOT NULL, CONSTRAINT kept UNIQUE (b));",
    );
    let p = table(&db, "p");
    let checks: Vec<_> = p
        .check_constraints(&db)
        .expect("held")
        .map(|check| owned(check.constraint_name(&db)))
        .collect();
    assert_eq!(checks, names(&[Some("p_a_check"), Some("p_a_check1")]));
    let keys: Vec<_> =
        p.unique_indices(&db).expect("held").map(|key| owned(key.constraint_name(&db))).collect();
    assert_eq!(keys, names(&[Some("p_pkey"), Some("kept")]));
    let foreign: Vec<_> =
        p.foreign_keys(&db).expect("held").map(|key| owned(key.constraint_name(&db))).collect();
    assert_eq!(foreign, names(&[Some("p_b_fkey")]));
    let not_null: Vec<_> = p
        .columns(&db)
        .expect("held")
        .map(|column| owned(column.not_null_constraint_name(&db)))
        .collect();
    assert_eq!(not_null, names(&[Some("p_a_not_null"), None, Some("written")]));

    // Only PostgreSQL names what it is not told to.
    let generic =
        ParserDB::parse::<GenericDialect>("CREATE TABLE p (a INT CHECK (a > 0));").expect("parses");
    let check = table(&generic, "p").check_constraints(&generic).expect("held").next();
    assert_eq!(check.map(|check| owned(check.constraint_name(&generic))), Some(None));
}

#[test]
fn a_not_null_a_table_states_itself_outlives_its_parents() {
    /// Whether column `a` of `name` requires a value, and the name of its
    /// `NOT NULL`.
    fn requirement(db: &ParserDB, name: &str) -> (bool, Option<String>) {
        let column = table(db, name)
            .columns(db)
            .expect("held")
            .find(|column| column.column_name() == "a")
            .expect("column a");
        (!column.is_nullable(db).expect("held"), owned(column.not_null_constraint_name(db)))
    }
    let named = |name: &str| (true, Some(name.to_owned()));
    let drop = "ALTER TABLE p ALTER COLUMN a DROP NOT NULL;";

    let db = accepted(&format!(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c (a INT NOT NULL) INHERITS (p);
         CREATE TABLE g () INHERITS (c); {drop}"
    ));
    assert_eq!(requirement(&db, "c"), named("c_a_not_null"));
    assert_eq!(requirement(&db, "g"), named("c_a_not_null"));
    let db = accepted(&format!(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c (a INT) INHERITS (p);
         CREATE TABLE g () INHERITS (c); {drop}"
    ));
    assert_eq!(requirement(&db, "c"), (false, None));
    assert_eq!(requirement(&db, "g"), (false, None));
    // Received, then stated by the child itself.
    let db = accepted(&format!(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c () INHERITS (p);
         ALTER TABLE c ALTER COLUMN a SET NOT NULL; {drop}"
    ));
    assert_eq!(requirement(&db, "c"), named("p_a_not_null"));
    // Set by the parent later, and still passed by another parent.
    let db = accepted(
        "CREATE TABLE p (a INT); CREATE TABLE c () INHERITS (p); ALTER TABLE p ALTER COLUMN a SET NOT NULL;",
    );
    assert_eq!(requirement(&db, "c"), named("p_a_not_null"));
    let db = accepted(&format!(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE q (a INT NOT NULL);
         CREATE TABLE c () INHERITS (p, q); {drop}"
    ));
    assert_eq!(requirement(&db, "c"), named("p_a_not_null"));
    accepted(&format!(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c (a INT NOT NULL) INHERITS (p); {drop}
         ALTER TABLE c ALTER COLUMN a DROP NOT NULL;"
    ));
}

#[test]
fn no_inherit_leaves_a_received_not_null_the_tables_own() {
    let one = "CREATE TABLE p (a INT NOT NULL); CREATE TABLE c () INHERITS (p); ALTER TABLE c NO INHERIT p;";
    accepted(&format!("{one} ALTER TABLE c DROP CONSTRAINT p_a_not_null;"));
    let two = "CREATE TABLE p (a INT NOT NULL); CREATE TABLE q (a INT NOT NULL);
               CREATE TABLE c () INHERITS (p, q); ALTER TABLE c NO INHERIT p;";
    assert!(matches!(
        parse(&format!("{two} ALTER TABLE c DROP CONSTRAINT p_a_not_null;")),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
}

#[test]
fn a_name_is_asked_of_the_database_holding_the_constraint() {
    let db = accepted("CREATE TABLE p (a INT NOT NULL CHECK (a > 0)); CREATE INDEX i ON p (a);");
    let p = table(&db, "p");
    // An index backing no constraint names none.
    let index = db.indexes().next().expect("one index");
    assert_eq!(owned(index.constraint_name(&db)), None);
    // A copy of the check answers as the check it copies.
    let check = p.check_constraints(&db).expect("held").next().expect("one check").clone();
    assert_eq!(owned(check.constraint_name(&db)), Some("p_a_check".to_owned()));
    let column = p.columns(&db).expect("held").next().expect("one column");
    // Through a reference or an `Arc` the column answers alike.
    assert_eq!(
        owned(ColumnLike::not_null_constraint_name(&column, &db)),
        Some("p_a_not_null".to_owned())
    );
    let shared = std::sync::Arc::new(column.clone());
    assert_eq!(owned(shared.not_null_constraint_name(&db)), Some("p_a_not_null".to_owned()));
    let other = accepted("CREATE TABLE q (b INT);");
    assert!(column.not_null_constraint_name(&other).is_err());
}

#[test]
fn outside_postgresql_an_unnamed_parent_check_is_matched_by_its_expression() {
    let parse = |sql: &str| ParserDB::parse::<GenericDialect>(sql);
    let parent = "CREATE TABLE p (a INT CHECK (a > 0));";
    assert!(
        parse(&format!("{parent} CREATE TABLE c (a INT CHECK (a > 0)); ALTER TABLE c INHERIT p;"))
            .is_ok()
    );
    assert!(
        parse(&format!("{parent} CREATE TABLE c (a INT CHECK (a > 1)); ALTER TABLE c INHERIT p;"))
            .is_err()
    );
}

#[test]
fn a_child_writing_its_parents_generated_name_holds_the_parents_copy() {
    for (parent, child) in [
        (
            "CREATE TABLE p (a INT CHECK (a > 0));",
            "CREATE TABLE c (a INT, CONSTRAINT p_a_check CHECK (a > 0)) INHERITS (p);",
        ),
        (
            "CREATE TABLE p (a INT, CHECK (a > 0));",
            "CREATE TABLE c (a INT, CONSTRAINT p_a_check CHECK (a > 0)) INHERITS (p);",
        ),
        (
            "CREATE TABLE p (a INT CHECK (a > 0));",
            "CREATE TABLE c (a INT CONSTRAINT p_a_check CHECK (a > 0)) INHERITS (p);",
        ),
    ] {
        let db = accepted(&format!("{parent} {child}"));
        let checks: Vec<_> = table(&db, "c")
            .check_constraints(&db)
            .expect("held")
            .map(|check| owned(check.constraint_name(&db)))
            .collect();
        assert_eq!(checks, names(&[Some("p_a_check")]), "{child}");
        assert!(
            matches!(
                parse(&format!("{parent} {child} ALTER TABLE c DROP CONSTRAINT p_a_check;")),
                Err(Error::InheritedConstraintNotDroppable { .. })
            ),
            "{child}"
        );
    }
    // Under another name, the child's check is its own beside the copy.
    let other = "CREATE TABLE p (a INT CHECK (a > 0));
                 CREATE TABLE c (a INT, CONSTRAINT k CHECK (a > 0)) INHERITS (p);";
    accepted(&format!("{other} ALTER TABLE c DROP CONSTRAINT k;"));
    assert!(matches!(
        parse(&format!("{other} ALTER TABLE c DROP CONSTRAINT p_a_check;")),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
}
