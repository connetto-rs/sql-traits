//! A check merges with the check of the same name a parent passes down when
//! the two hold the same stored values, which for a date or time literal
//! depends on the session each was created in, whatever the written text.
//!
//! Every outcome is what PostgreSQL 18 does with the same script, the parent's
//! check read under `Europe/Rome` and the child's under `Asia/Tokyo`, where
//! `'2026-01-01 12:00'` in Rome is `'2026-01-01 20:00'` in Tokyo.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::{
    errors::{ChildMismatch, Error, LookupError, UnattachablePartition},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(script: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(script)
}

fn stored_on(db: &ParserDB, table: &str, name: &str) -> Result<String, LookupError> {
    let table = db.tables().find(|held| held.table_name() == table).expect("table exists");
    let check = table
        .check_constraints(db)?
        .find(|check| check.constraint_name(db).expect("held").as_deref() == Some(name))
        .expect("check exists");
    Ok(check.expression(db)?.to_string())
}

const ROME_PARENTS: &str = "SET TIME ZONE 'Europe/Rome';
CREATE TABLE parent (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 12:00'));
CREATE TABLE unnamed (ts TIMESTAMPTZ, CHECK (ts > '2026-01-01 12:00'));
CREATE TABLE ranged (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 12:00'))
    PARTITION BY RANGE (ts);
SET TIME ZONE 'Asia/Tokyo';";

const ROME_VALUE: &str = "ts > '2026-01-01 11:00:00+00'::TIMESTAMP WITH TIME ZONE";

fn assert_conflict(script: &str, table: &str, constraint: &str) {
    let error = parse(script).expect_err("PostgreSQL refuses");
    assert!(
        matches!(&error, Error::InheritedConstraintConflict { table_name, constraint_name }
            if table_name == table && constraint_name == constraint),
        "{error:?}"
    );
}

fn assert_differs(script: &str) {
    let error = parse(script).expect_err("PostgreSQL refuses");
    let differs = |reason: &ChildMismatch| matches!(reason, ChildMismatch::ConstraintDiffers { constraint_name } if constraint_name == "k");
    assert!(
        match &error {
            Error::InheritanceChildMismatch { reason, .. }
            | Error::PartitionNotAttachable {
                reason: UnattachablePartition::Shape(reason), ..
            } => {
                differs(reason)
            }
            _ => false,
        },
        "{error:?}"
    );
}

#[test]
fn a_child_check_created_with_its_parent_merges_by_stored_value() -> Result<(), Error> {
    assert_conflict(
        &format!(
            "{ROME_PARENTS}
             CREATE TABLE c (CONSTRAINT k CHECK (ts > '2026-01-01 12:00')) INHERITS (parent);"
        ),
        "c",
        "k",
    );
    let db = parse(&format!(
        "{ROME_PARENTS}
         CREATE TABLE c (CONSTRAINT k CHECK (ts > '2026-01-01 20:00')) INHERITS (parent);"
    ))?;
    assert_eq!(stored_on(&db, "c", "k")?, ROME_VALUE);
    Ok(())
}

#[test]
fn a_child_check_written_on_its_column_merges_by_stored_value() -> Result<(), Error> {
    assert_conflict(
        &format!(
            "{ROME_PARENTS}
             CREATE TABLE c (ts TIMESTAMPTZ CONSTRAINT k CHECK (ts > '2026-01-01 12:00'))
                 INHERITS (parent);"
        ),
        "c",
        "k",
    );
    let db = parse(&format!(
        "{ROME_PARENTS}
         CREATE TABLE c (ts TIMESTAMPTZ CONSTRAINT k CHECK (ts > '2026-01-01 20:00'))
             INHERITS (parent);"
    ))?;
    assert_eq!(stored_on(&db, "c", "k")?, ROME_VALUE);
    Ok(())
}

#[test]
fn a_parent_check_written_on_its_column_merges_by_stored_value() -> Result<(), Error> {
    const COLUMN_PARENT: &str = "SET TIME ZONE 'Europe/Rome';
        CREATE TABLE cp (ts TIMESTAMPTZ CONSTRAINT k CHECK (ts > '2026-01-01 12:00'));
        SET TIME ZONE 'Asia/Tokyo';";
    assert_conflict(
        &format!(
            "{COLUMN_PARENT}
             CREATE TABLE c (CONSTRAINT k CHECK (ts > '2026-01-01 12:00')) INHERITS (cp);"
        ),
        "c",
        "k",
    );
    let db = parse(&format!(
        "{COLUMN_PARENT}
         CREATE TABLE c (CONSTRAINT k CHECK (ts > '2026-01-01 20:00')) INHERITS (cp);
         CREATE TABLE d (ts TIMESTAMPTZ CONSTRAINT k CHECK (ts > '2026-01-01 20:00'))
             INHERITS (cp);"
    ))?;
    assert_eq!(stored_on(&db, "c", "k")?, ROME_VALUE);
    assert_eq!(stored_on(&db, "d", "k")?, ROME_VALUE);
    Ok(())
}

#[test]
fn a_child_check_under_the_generated_name_merges_by_stored_value() -> Result<(), Error> {
    assert_conflict(
        &format!(
            "{ROME_PARENTS}
             CREATE TABLE c (CONSTRAINT unnamed_ts_check CHECK (ts > '2026-01-01 12:00'))
                 INHERITS (unnamed);"
        ),
        "c",
        "unnamed_ts_check",
    );
    let db = parse(&format!(
        "{ROME_PARENTS}
         CREATE TABLE c (CONSTRAINT unnamed_ts_check CHECK (ts > '2026-01-01 20:00'))
             INHERITS (unnamed);"
    ))?;
    assert_eq!(stored_on(&db, "c", "unnamed_ts_check")?, ROME_VALUE);
    Ok(())
}

#[test]
fn an_existing_table_inherits_or_attaches_by_stored_value() -> Result<(), Error> {
    assert_differs(&format!(
        "{ROME_PARENTS}
         CREATE TABLE c (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 12:00'));
         ALTER TABLE c INHERIT parent;"
    ));
    assert_differs(&format!(
        "{ROME_PARENTS}
         CREATE TABLE c (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 12:00'));
         ALTER TABLE ranged ATTACH PARTITION c FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');"
    ));
    let db = parse(&format!(
        "{ROME_PARENTS}
         CREATE TABLE c (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 20:00'));
         ALTER TABLE c INHERIT parent;
         CREATE TABLE d (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 20:00'));
         ALTER TABLE ranged ATTACH PARTITION d FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');"
    ))?;
    assert_eq!(stored_on(&db, "c", "k")?, ROME_VALUE);
    assert_eq!(stored_on(&db, "d", "k")?, ROME_VALUE);
    Ok(())
}

#[test]
fn a_check_added_to_a_parent_merges_with_its_children_by_stored_value() -> Result<(), Error> {
    const TOKYO_CHILDREN: &str = "SET TIME ZONE 'Asia/Tokyo';
        CREATE TABLE parent (ts TIMESTAMPTZ);
        CREATE TABLE ranged (ts TIMESTAMPTZ) PARTITION BY RANGE (ts);";
    assert_conflict(
        &format!(
            "{TOKYO_CHILDREN}
             CREATE TABLE c (CONSTRAINT k CHECK (ts > '2026-01-01 12:00')) INHERITS (parent);
             SET TIME ZONE 'Europe/Rome';
             ALTER TABLE parent ADD CONSTRAINT k CHECK (ts > '2026-01-01 12:00');"
        ),
        "c",
        "k",
    );
    let db = parse(&format!(
        "{TOKYO_CHILDREN}
         CREATE TABLE c (CONSTRAINT k CHECK (ts > '2026-01-01 20:00')) INHERITS (parent);
         CREATE TABLE p PARTITION OF ranged (CONSTRAINT k CHECK (ts > '2026-01-01 20:00'))
             FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
         SET TIME ZONE 'Europe/Rome';
         ALTER TABLE parent ADD CONSTRAINT k CHECK (ts > '2026-01-01 12:00');
         ALTER TABLE ranged ADD CONSTRAINT k CHECK (ts > '2026-01-01 12:00');"
    ))?;
    for (table, check) in [("parent", "k"), ("c", "k"), ("ranged", "k"), ("p", "k")] {
        assert_eq!(stored_on(&db, table, check)?, ROME_VALUE);
    }
    Ok(())
}

#[test]
fn two_parents_pass_one_check_name_only_with_one_stored_value() -> Result<(), Error> {
    const PARENTS: &str = "SET TIME ZONE 'Europe/Rome';
        CREATE TABLE pa (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 12:00'));
        SET TIME ZONE 'Asia/Tokyo';
        CREATE TABLE pb (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 12:00'));
        CREATE TABLE pc (ts TIMESTAMPTZ, CONSTRAINT k CHECK (ts > '2026-01-01 20:00'));";
    assert_conflict(&format!("{PARENTS} CREATE TABLE ab () INHERITS (pa, pb);"), "ab", "k");
    let db = parse(&format!("{PARENTS} CREATE TABLE ac () INHERITS (pa, pc);"))?;
    assert_eq!(stored_on(&db, "ac", "k")?, ROME_VALUE);
    Ok(())
}

#[test]
fn checks_read_under_the_one_unset_time_zone_merge_when_written_alike() -> Result<(), Error> {
    let db = parse(
        "CREATE TABLE dp (ts TIMESTAMPTZ, CONSTRAINT d CHECK (ts > '2026-01-01 12:00'));
         CREATE TABLE dc (CONSTRAINT d CHECK (ts > '2026-01-01 12:00')) INHERITS (dp);",
    )?;
    assert!(matches!(stored_on(&db, "dc", "d"), Err(LookupError::CheckLiteralUnreadable { .. })));
    Ok(())
}

#[test]
fn a_merge_whose_values_are_not_known_is_refused() {
    let undecidable = |script: &str| {
        let error = parse(script).expect_err("not decidable here");
        assert!(
            matches!(&error, Error::InheritedCheckUndecidable { table_name, constraint_name, .. }
                if table_name == "dc" && constraint_name == "d"),
            "{error:?}"
        );
    };
    // The parent's zone is the unknown one the session started in, the
    // child's a zone the script sets.
    undecidable(
        "CREATE TABLE dp (ts TIMESTAMPTZ, CONSTRAINT d CHECK (ts > '2026-01-01 12:00'));
         SET TIME ZONE 'UTC';
         CREATE TABLE dc (CONSTRAINT d CHECK (ts > '2026-01-01 12:00')) INHERITS (dp);",
    );
    // Each check reads the time it is created at.
    undecidable(
        "SET TIME ZONE 'UTC';
         CREATE TABLE dp (ts TIMESTAMPTZ, CONSTRAINT d CHECK (ts < 'now'));
         CREATE TABLE dc (CONSTRAINT d CHECK (ts < 'now')) INHERITS (dp);",
    );
    // Written alike, but one zone the input leaves unknown is set between.
    undecidable(
        "CREATE TABLE dp (ts TIMESTAMPTZ, CONSTRAINT d CHECK (ts > '2026-01-01 12:00'));
         SET TIME ZONE INTERVAL '+05:00' HOUR TO MINUTE;
         CREATE TABLE dc (CONSTRAINT d CHECK (ts > '2026-01-01 12:00')) INHERITS (dp);",
    );
}
