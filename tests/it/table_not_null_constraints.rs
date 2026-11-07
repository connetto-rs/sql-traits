//! The table-level `NOT NULL` constraint PostgreSQL 18 records of its own,
//! spelled `CONSTRAINT name NOT NULL column NO INHERIT` in the constraint
//! list and merged with the column's own `NOT NULL` into one named
//! requirement.
//!
//! Every expectation here was measured against PostgreSQL 18 by reading
//! `pg_constraint` and `pg_attribute.attnotnull` after each statement. The
//! load-bearing ones are that the two spellings are one constraint carrying
//! one name, that a child receives the requirement as a copy of the parent's
//! constraint under the parent's name and cannot drop or rename it, and that
//! `NO INHERIT` keeps the requirement with the table declaring it.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, InheritedChange, LookupError, RequiredValue},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::column_nullability::accepts_nothing;

fn parse_db(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn make_database(sql: &str) -> ParserDB {
    parse_db(sql).expect("schema parses")
}

/// The name the `NOT NULL` constraint `column` of `table` answers to.
fn not_null_name(database: &ParserDB, table_name: &str, column_name: &str) -> Option<String> {
    let table = database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    table
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists")
        .not_null_constraint_name(database)
        .expect("table is in this database")
        .map(Into::into)
}

#[test]
fn a_written_table_level_not_null_keeps_its_name() {
    let database = make_database("CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);");
    assert!(accepts_nothing(&database, "p", "a"));
    assert_eq!(not_null_name(&database, "p", "a"), Some("nn".to_owned()));
}

#[test]
fn an_unnamed_table_level_not_null_takes_the_postgresql_name() {
    let database = make_database("CREATE TABLE p (a INT, NOT NULL a);");
    assert!(accepts_nothing(&database, "p", "a"));
    assert_eq!(not_null_name(&database, "p", "a"), Some("p_a_not_null".to_owned()));
}

#[test]
fn the_two_spellings_of_one_not_null_are_one_constraint() {
    for sql in [
        "CREATE TABLE p (a INT NOT NULL, NOT NULL a);",
        "CREATE TABLE p (a INT, NOT NULL a, NOT NULL a);",
    ] {
        let database = make_database(sql);
        assert!(accepts_nothing(&database, "p", "a"));
        assert_eq!(not_null_name(&database, "p", "a"), Some("p_a_not_null".to_owned()), "{sql}");
        assert!(
            parse_db(format!("{sql} ALTER TABLE p DROP CONSTRAINT p_a_not_null;").as_str()).is_ok(),
            "the one constraint drops under its one name for {sql}"
        );
    }
}

#[test]
fn conflicting_written_names_for_one_not_null_are_refused() {
    assert!(matches!(
        parse_db("CREATE TABLE p (a INT CONSTRAINT x NOT NULL, CONSTRAINT y NOT NULL a);"),
        Err(Error::ConflictingNotNullConstraintNames { ref table_name, ref column_name, ref first_name, ref second_name })
            if table_name == "p"
                && column_name == "a"
                && first_name == "x"
                && second_name == "y"
    ));
}

#[test]
fn a_no_inherit_not_null_beside_another_spelling_is_refused() {
    for sql in [
        "CREATE TABLE p (a INT NOT NULL, NOT NULL a NO INHERIT);",
        "CREATE TABLE p (a INT, NOT NULL a NO INHERIT, NOT NULL a);",
    ] {
        assert!(
            matches!(
                parse_db(sql),
                Err(Error::ConflictingNoInheritNotNull { ref table_name, ref column_name })
                    if table_name == "p" && column_name == "a"
            ),
            "{sql}"
        );
    }
}

#[test]
fn a_written_name_wins_over_the_generated_one_for_a_keyed_column() {
    let database = make_database("CREATE TABLE p (a INT PRIMARY KEY, CONSTRAINT nn NOT NULL a);");
    assert!(accepts_nothing(&database, "p", "a"));
    assert_eq!(not_null_name(&database, "p", "a"), Some("nn".to_owned()));
}

#[test]
fn a_no_inherit_not_null_is_refused_on_a_partitioned_table() {
    assert!(matches!(
        parse_db("CREATE TABLE p (a INT, b INT, NOT NULL a NO INHERIT) PARTITION BY LIST (b);"),
        Err(Error::NoInheritNotNullOnPartitionedTable { ref table_name })
            if table_name == "p"
    ));

    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, b INT) PARTITION BY LIST (b);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NO INHERIT;",
        ),
        Err(Error::NoInheritNotNullOnPartitionedTable { ref table_name })
            if table_name == "p"
    ));
}

#[test]
fn a_child_receives_the_table_level_not_null_of_its_parent() {
    let database = make_database(
        "CREATE TABLE p (a INT, NOT NULL a);
         CREATE TABLE c () INHERITS (p);",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("p_a_not_null".to_owned()));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a);
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE c DROP CONSTRAINT p_a_not_null;",
        ),
        Err(Error::InheritedConstraintNotDroppable { ref constraint_name, .. })
            if constraint_name == "p_a_not_null"
    ));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a);
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE c RENAME CONSTRAINT p_a_not_null TO own;",
        ),
        Err(Error::InheritedConstraintNotRenamable { ref constraint_name, .. })
            if constraint_name == "p_a_not_null"
    ));
}

#[test]
fn a_child_of_a_no_inherit_not_null_stays_nullable() {
    let database = make_database(
        "CREATE TABLE p (a INT, NOT NULL a NO INHERIT);
         CREATE TABLE c () INHERITS (p);",
    );
    assert!(!accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), None);
}

#[test]
fn inheriting_a_parent_with_a_no_inherit_not_null_is_accepted() {
    let database = make_database(
        "CREATE TABLE p (a INT, NOT NULL a NO INHERIT);
         CREATE TABLE c (a INT);
         ALTER TABLE c INHERIT p;",
    );
    assert!(!accepts_nothing(&database, "c", "a"));
    assert!(accepts_nothing(&database, "p", "a"));
}

#[test]
fn adding_a_not_null_constraint_reaches_the_children() {
    let database = make_database(
        "CREATE TABLE p (a INT);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("nn".to_owned()));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT);
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;
             ALTER TABLE c DROP CONSTRAINT nn;",
        ),
        Err(Error::InheritedConstraintNotDroppable { ref constraint_name, .. })
            if constraint_name == "nn"
    ));
}

#[test]
fn adding_a_no_inherit_not_null_keeps_the_children_nullable() {
    let database = make_database(
        "CREATE TABLE p (a INT);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NO INHERIT;",
    );
    assert!(accepts_nothing(&database, "p", "a"));
    assert!(!accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "p", "a"), Some("nn".to_owned()));
}

#[test]
fn a_not_null_added_not_valid_is_validated_later() {
    let added = make_database(
        "CREATE TABLE p (a INT);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NOT VALID;",
    );
    assert!(accepts_nothing(&added, "p", "a"));

    assert!(
        parse_db(
            "CREATE TABLE p (a INT);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NOT VALID;
             ALTER TABLE p VALIDATE CONSTRAINT nn;",
        )
        .is_ok(),
        "VALIDATE CONSTRAINT accepts a not-null"
    );
}

#[test]
fn dropping_not_null_removes_the_table_level_constraint() {
    assert!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a);
             ALTER TABLE p ALTER COLUMN a DROP NOT NULL;",
        )
        .is_ok(),
    );
    let database = make_database(
        "CREATE TABLE p (a INT, NOT NULL a);
         ALTER TABLE p ALTER COLUMN a DROP NOT NULL;",
    );
    assert!(!accepts_nothing(&database, "p", "a"));
    assert_eq!(not_null_name(&database, "p", "a"), None);
}

#[test]
fn dropping_a_table_level_not_null_leaves_the_column_nullable() {
    let dropped = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);
         ALTER TABLE p DROP CONSTRAINT nn;",
    );
    assert!(!accepts_nothing(&dropped, "p", "a"));
    assert_eq!(not_null_name(&dropped, "p", "a"), None);

    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT NOT NULL, PRIMARY KEY (a));
             ALTER TABLE p DROP CONSTRAINT p_a_not_null;",
        ),
        Err(Error::RequiredValueNotDroppable {
            ref reason,
            ..
        }) if *reason == RequiredValue::CoveredByKey
    ));
}

#[test]
fn setting_not_null_over_a_no_inherit_requirement_is_refused() {
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a NO INHERIT);
             ALTER TABLE p ALTER COLUMN a SET NOT NULL;",
        ),
        Err(Error::NotNullNoInheritStatusChange {
            ref constraint_name,
            ..
        }) if constraint_name == "p_a_not_null"
    ));
}

#[test]
fn renaming_a_table_level_not_null_renames_the_constraint() {
    let renamed = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);
         ALTER TABLE p RENAME CONSTRAINT nn TO mm;",
    );
    assert!(accepts_nothing(&renamed, "p", "a"));
    assert_eq!(not_null_name(&renamed, "p", "a"), Some("mm".to_owned()));

    let dropped = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);
         ALTER TABLE p RENAME CONSTRAINT nn TO mm;
         ALTER TABLE p DROP CONSTRAINT mm;",
    );
    assert!(!accepts_nothing(&dropped, "p", "a"));
    assert_eq!(not_null_name(&dropped, "p", "a"), None);
}

#[test]
fn dropping_a_column_takes_its_table_level_not_null_with_it() {
    let database = make_database(
        "CREATE TABLE p (a INT, b INT, CONSTRAINT nn NOT NULL a);
         ALTER TABLE p DROP COLUMN a;",
    );
    assert_eq!(not_null_name(&database, "p", "b"), None);
    assert!(
        parse_db(
            "CREATE TABLE p (a INT, b INT, CONSTRAINT nn NOT NULL a);
             ALTER TABLE p DROP COLUMN a;
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL b;",
        )
        .is_ok(),
        "the name is free again"
    );
}

#[test]
fn adding_a_second_not_null_to_a_column_that_requires_a_value_is_refused() {
    for (sql, existing_name) in [
        ("CREATE TABLE p (a INT NOT NULL);", "p_a_not_null"),
        ("CREATE TABLE p (a INT, NOT NULL a);", "p_a_not_null"),
        ("CREATE TABLE p (a INT PRIMARY KEY);", "p_a_not_null"),
        ("CREATE TABLE p (a INT CONSTRAINT nn NOT NULL);", "nn"),
    ] {
        assert!(
            matches!(
                parse_db(format!("{sql} ALTER TABLE p ADD CONSTRAINT second NOT NULL a;").as_str()),
                Err(Error::NotNullConstraintAlreadyExists {
                    ref constraint_name,
                    existing_name: ref inner_name,
                    ..
                }) if *inner_name == existing_name && constraint_name == "second"
            ),
            "{sql}"
        );
        // Restating it unnamed or under its own name changes nothing.
        for restated in [
            "ALTER TABLE p ADD NOT NULL a;".to_owned(),
            format!("ALTER TABLE p ADD CONSTRAINT {existing_name} NOT NULL a;"),
        ] {
            let database = make_database(&format!("{sql} {restated}"));
            assert_eq!(
                not_null_name(&database, "p", "a").as_deref(),
                Some(existing_name),
                "{restated}"
            );
        }
    }
}

#[test]
fn adding_a_not_null_of_another_no_inherit_status_is_refused() {
    // A status differing from the requirement the column already holds is the
    // refusal, the same status being the already-existing fault instead.
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a NO INHERIT);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;",
        ),
        Err(Error::NotNullNoInheritStatusChange {
            ref constraint_name,
            ..
        }) if constraint_name == "p_a_not_null"
    ));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT NOT NULL);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NO INHERIT;",
        ),
        Err(Error::NotNullNoInheritStatusChange {
            ref constraint_name,
            ..
        }) if constraint_name == "p_a_not_null"
    ));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a NO INHERIT);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NO INHERIT;",
        ),
        Err(Error::NotNullConstraintAlreadyExists {
            ref existing_name,
            ..
        }) if existing_name == "p_a_not_null"
    ));
}

#[test]
fn a_table_level_not_null_naming_a_missing_column_is_refused() {
    assert!(matches!(
        parse_db("CREATE TABLE p (b INT, NOT NULL a);"),
        Err(Error::IdentifierLookupError(
            LookupError::ColumnNotFound { ref column_name, .. }
        )) if column_name == "a"
    ));
}

#[test]
fn a_table_created_below_a_table_level_not_null_receives_the_copy() {
    let database = make_database(
        "CREATE TABLE p (a INT);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;
         CREATE TABLE c () INHERITS (p);",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("nn".to_owned()));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE c DROP CONSTRAINT nn;",
        ),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
}

#[test]
fn a_child_declaring_its_own_not_null_keeps_it_when_the_parent_adds_one() {
    let database = make_database(
        "CREATE TABLE p (a INT);
         CREATE TABLE c (a INT NOT NULL) INHERITS (p);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("c_a_not_null".to_owned()));
    assert!(
        parse_db(
            "CREATE TABLE p (a INT);
             CREATE TABLE c (a INT NOT NULL) INHERITS (p);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;
             ALTER TABLE c DROP CONSTRAINT c_a_not_null;",
        )
        .is_ok(),
        "the child's own requirement stays its own"
    );
}

#[test]
fn a_parent_dropping_its_not_null_leaves_nothing_in_the_child() {
    for sql in [
        "CREATE TABLE p (a INT, NOT NULL a);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE p ALTER COLUMN a DROP NOT NULL;",
        "CREATE TABLE p (a INT, NOT NULL a);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE p DROP CONSTRAINT p_a_not_null;",
    ] {
        let database = make_database(sql);
        assert!(!accepts_nothing(&database, "c", "a"), "{sql}");
        assert_eq!(not_null_name(&database, "c", "a"), None, "{sql}");
    }
}

#[test]
fn only_dropping_a_not_null_leaves_the_childs_copy_its_own() {
    let database = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE ONLY p DROP CONSTRAINT nn;",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("nn".to_owned()));
    assert!(
        parse_db(
            "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE ONLY p DROP CONSTRAINT nn;
             ALTER TABLE c DROP CONSTRAINT nn;",
        )
        .is_ok(),
        "the copy is the child's own now"
    );
}

#[test]
fn setting_not_null_on_the_parent_reaches_the_child() {
    let database = make_database(
        "CREATE TABLE p (a INT);
         CREATE TABLE c () INHERITS (p);
         ALTER TABLE p ALTER COLUMN a SET NOT NULL;",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("p_a_not_null".to_owned()));
}

#[test]
fn only_validating_a_not_null_is_refused_while_children_hold_it() {
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT);
             CREATE TABLE c () INHERITS (p);
             ALTER TABLE p ADD CONSTRAINT nn NOT NULL a NOT VALID;
             ALTER TABLE ONLY p VALIDATE CONSTRAINT nn;",
        ),
        Err(Error::OnlyRefusedWithChildren {
            ref change,
            ..
        }) if *change == InheritedChange::ValidateConstraint
    ));
}

#[test]
fn a_partition_receives_the_table_level_not_null_of_its_root() {
    let database = make_database(
        "CREATE TABLE p (a INT, b INT, NOT NULL a) PARTITION BY LIST (b);
         CREATE TABLE c PARTITION OF p FOR VALUES IN (1);",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("p_a_not_null".to_owned()));
}

#[test]
fn renaming_a_column_renames_the_not_null_column_it_holds() {
    let database = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a);
         ALTER TABLE p RENAME COLUMN a TO b;",
    );
    assert!(accepts_nothing(&database, "p", "b"));
    assert_eq!(not_null_name(&database, "p", "b"), Some("nn".to_owned()));
}

#[test]
fn a_like_copy_receives_the_table_level_not_null() {
    let database = make_database(
        "CREATE TABLE o (a INT, NOT NULL a);
         CREATE TABLE l (LIKE o);",
    );
    assert!(accepts_nothing(&database, "l", "a"));
}

#[test]
fn a_written_name_on_the_column_spelling_answers_for_the_pair() {
    for sql in [
        "CREATE TABLE p (a INT CONSTRAINT x NOT NULL, NOT NULL a);",
        "CREATE TABLE p (a INT CONSTRAINT x NOT NULL, CONSTRAINT x NOT NULL a);",
    ] {
        let database = make_database(sql);
        assert!(accepts_nothing(&database, "p", "a"));
        assert_eq!(not_null_name(&database, "p", "a"), Some("x".to_owned()), "{sql}");
    }
}

#[test]
fn validating_an_already_validated_not_null_is_a_no_op() {
    assert!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a);
             ALTER TABLE p VALIDATE CONSTRAINT p_a_not_null;",
        )
        .is_ok()
    );
}

#[test]
fn a_no_inherit_not_null_is_still_the_table_own_requirement() {
    let database = make_database("CREATE TABLE p (a INT, NOT NULL a NO INHERIT);");
    assert!(accepts_nothing(&database, "p", "a"));

    let dropped = make_database(
        "CREATE TABLE p (a INT, NOT NULL a NO INHERIT);
         ALTER TABLE p ALTER COLUMN a DROP NOT NULL;",
    );
    assert!(!accepts_nothing(&dropped, "p", "a"));
    assert_eq!(not_null_name(&dropped, "p", "a"), None);
}

#[test]
fn dropping_a_table_level_not_null_follows_postgresql_below() {
    // A key covering the column keeps it required.
    for sql in [
        "CREATE TABLE p (a INT PRIMARY KEY, CONSTRAINT nn NOT NULL a);",
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a, PRIMARY KEY (a));",
    ] {
        assert!(
            matches!(
                parse_db(&format!("{sql} ALTER TABLE p DROP CONSTRAINT nn;")),
                Err(Error::RequiredValueNotDroppable { ref column_name, reason: RequiredValue::CoveredByKey, .. })
                    if column_name == "a"
            ),
            "{sql}"
        );
    }
    let family =
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a); CREATE TABLE c () INHERITS (p);";
    // Under ONLY the child keeps the copy, now its own.
    let only = make_database(&format!("{family} ALTER TABLE ONLY p DROP CONSTRAINT nn;"));
    assert!(accepts_nothing(&only, "c", "a"));
    assert_eq!(not_null_name(&only, "c", "a"), Some("nn".to_owned()));
    make_database(&format!(
        "{family} ALTER TABLE ONLY p DROP CONSTRAINT nn; ALTER TABLE c DROP CONSTRAINT nn;"
    ));
    // A child and grandchild that only received it lose it.
    let deep = make_database(&format!(
        "{family} CREATE TABLE g () INHERITS (c); ALTER TABLE p DROP CONSTRAINT nn;"
    ));
    assert!(!accepts_nothing(&deep, "c", "a"));
    assert!(!accepts_nothing(&deep, "g", "a"));
    // Added by ALTER TABLE, the copy goes the same way.
    let added = make_database(
        "CREATE TABLE p (a INT); CREATE TABLE c () INHERITS (p);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a; ALTER TABLE p DROP CONSTRAINT nn;",
    );
    assert!(!accepts_nothing(&added, "c", "a"));
    // A child stating its own keeps it, under its own name.
    let own = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a); CREATE TABLE c (a INT NOT NULL) INHERITS (p);
         ALTER TABLE p DROP CONSTRAINT nn;",
    );
    assert_eq!(not_null_name(&own, "c", "a"), Some("c_a_not_null".to_owned()));
    // Another parent still passing it keeps it received.
    let two = "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a); CREATE TABLE q (a INT NOT NULL);
               CREATE TABLE c () INHERITS (p, q); ALTER TABLE p DROP CONSTRAINT nn;";
    assert!(accepts_nothing(&make_database(two), "c", "a"));
    assert!(matches!(
        parse_db(&format!("{two} ALTER TABLE c DROP CONSTRAINT nn;")),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
    // A NO INHERIT one never reached the child.
    let kept = make_database(
        "CREATE TABLE p (a INT, CONSTRAINT nn NOT NULL a NO INHERIT); CREATE TABLE c () INHERITS (p);
         ALTER TABLE p DROP CONSTRAINT nn;",
    );
    assert!(!accepts_nothing(&kept, "p", "a"));
    assert!(!accepts_nothing(&kept, "c", "a"));
}

#[test]
fn a_not_null_added_to_a_parent_replaces_a_childs_null_option() {
    let database = make_database(
        "CREATE TABLE p (a INT DEFAULT 1); CREATE TABLE c (a INT NULL DEFAULT 1) INHERITS (p);
         ALTER TABLE p ADD CONSTRAINT nn NOT NULL a;",
    );
    assert!(accepts_nothing(&database, "c", "a"));
    assert_eq!(not_null_name(&database, "c", "a"), Some("nn".to_owned()));
}

#[test]
fn a_paired_not_null_renames_as_one_constraint() {
    for (sql, old) in [
        ("CREATE TABLE p (a INT NOT NULL, NOT NULL a);", "p_a_not_null"),
        ("CREATE TABLE p (a INT CONSTRAINT x NOT NULL, NOT NULL a);", "x"),
        ("CREATE TABLE p (a INT NOT NULL, CONSTRAINT x NOT NULL a);", "x"),
    ] {
        let database = make_database(&format!("{sql} ALTER TABLE p RENAME CONSTRAINT {old} TO k;"));
        assert_eq!(not_null_name(&database, "p", "a"), Some("k".to_owned()), "{sql}");
        make_database(&format!(
            "{sql} ALTER TABLE p RENAME CONSTRAINT {old} TO k; ALTER TABLE p DROP CONSTRAINT k;"
        ));
    }
}

#[test]
fn a_child_receives_an_unnamed_table_level_not_null_under_its_parents_name() {
    let database =
        make_database("CREATE TABLE p (a INT, NOT NULL a); CREATE TABLE c () INHERITS (p);");
    assert_eq!(not_null_name(&database, "c", "a"), Some("p_a_not_null".to_owned()));
    assert!(matches!(
        parse_db(
            "CREATE TABLE p (a INT, NOT NULL a); CREATE TABLE c () INHERITS (p); ALTER TABLE c DROP CONSTRAINT p_a_not_null;"
        ),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
}

#[test]
fn a_table_level_not_null_reaches_children_like_a_column_one() {
    // Another parent still requiring it through its table-level spelling
    // keeps the child's requirement when the first parent drops its own.
    let two = make_database(
        "CREATE TABLE p (a INT NOT NULL); CREATE TABLE q (a INT, NOT NULL a);
         CREATE TABLE c () INHERITS (p, q); ALTER TABLE p ALTER COLUMN a DROP NOT NULL;",
    );
    assert!(accepts_nothing(&two, "c", "a"));
    // A child stating its own names it after itself.
    let own = make_database(
        "CREATE TABLE p (a INT, NOT NULL a); CREATE TABLE c (a INT NOT NULL) INHERITS (p);",
    );
    assert_eq!(not_null_name(&own, "c", "a"), Some("c_a_not_null".to_owned()));
    // A copied column carrying other options requires a value too.
    let with_default = make_database(
        "CREATE TABLE p (a INT DEFAULT 1, NOT NULL a); CREATE TABLE c () INHERITS (p);",
    );
    assert!(accepts_nothing(&with_default, "c", "a"));
}
