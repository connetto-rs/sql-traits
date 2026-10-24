//! Tests that a constraint written on a column can be dropped by its name.
//!
//! `ALTER TABLE ... DROP CONSTRAINT` used to search only the table's own
//! constraint list, so a constraint the input wrote on a column instead was
//! reported absent and the whole schema failed to load, even though the model
//! held that constraint and answered for it. It reproduced on a table with no
//! parent and no children, so it was never about inheritance.
//!
//! Every expectation was measured against PostgreSQL 18.4. Two of the seven
//! options a name may precede are not constraints at all, a `DEFAULT` and a
//! `NULL`, and naming either is refused rather than quietly removing it.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_nullability.rs"));

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

fn checks(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .check_constraints(database)
        .expect("table is in this database")
        .count()
}

fn unique_indices(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .unique_indices(database)
        .expect("table is in this database")
        .count()
}

fn foreign_keys(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .foreign_keys(database)
        .expect("table is in this database")
        .count()
}

#[test]
fn a_check_written_on_a_column_is_droppable_by_name() {
    let database = database(
        "CREATE TABLE t (id INT CONSTRAINT c1 CHECK (id > 0));
         ALTER TABLE t DROP CONSTRAINT c1;",
    );
    assert_eq!(checks(&database, "t"), 0);
}

#[test]
fn a_key_written_on_a_column_is_droppable_by_name_and_takes_its_index() {
    let unique = database(
        "CREATE TABLE t (id INT CONSTRAINT u1 UNIQUE);
         ALTER TABLE t DROP CONSTRAINT u1;",
    );
    assert_eq!(unique_indices(&unique, "t"), 0);

    // The requirement to hold a value stays behind, which is what PostgreSQL
    // leaves: it records that separately from the key.
    let primary = database(
        "CREATE TABLE t (id INT CONSTRAINT p1 PRIMARY KEY);
         ALTER TABLE t DROP CONSTRAINT p1;",
    );
    assert_eq!(unique_indices(&primary, "t"), 0);
    assert_eq!(
        primary
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .primary_key_columns(&primary)
            .expect("in database")
            .count(),
        0
    );
    assert!(accepts_nothing(&primary, "t", "id"));
}

#[test]
fn a_reference_written_on_a_column_is_droppable_by_name() {
    let database = database(
        "CREATE TABLE o (id INT PRIMARY KEY);
         CREATE TABLE t (ref_id INT CONSTRAINT f1 REFERENCES o (id));
         ALTER TABLE t DROP CONSTRAINT f1;",
    );
    assert_eq!(foreign_keys(&database, "t"), 0);
}

#[test]
fn a_named_requirement_to_hold_a_value_is_droppable_by_name() {
    // PostgreSQL 18 records `NOT NULL` as a constraint of its own, so naming it
    // removes it and the column may then hold nothing.
    let database = database(
        "CREATE TABLE t (id INT CONSTRAINT nn NOT NULL);
         ALTER TABLE t DROP CONSTRAINT nn;",
    );
    assert!(!accepts_nothing(&database, "t", "id"));
}

#[test]
fn a_name_that_precedes_something_other_than_a_constraint_is_refused() {
    // PostgreSQL accepts the name in both spellings and records nothing for it,
    // so dropping by that name reports the constraint absent.
    for sql in [
        "CREATE TABLE t (id INT CONSTRAINT c1 DEFAULT 7);
         ALTER TABLE t DROP CONSTRAINT c1;",
        "CREATE TABLE t (id INT CONSTRAINT c1 NULL);
         ALTER TABLE t DROP CONSTRAINT c1;",
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::DropConstraintNotFound { ref constraint_name, .. })
                    if constraint_name == "c1"
            ),
            "should have been refused:\n{sql}"
        );
    }

    // The default itself survives, rather than being removed by a statement
    // that was refused.
    let excused = database(
        "CREATE TABLE t (id INT CONSTRAINT c1 DEFAULT 7);
         ALTER TABLE t DROP CONSTRAINT IF EXISTS c1;",
    );
    assert_eq!(
        excused
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .column("id", &excused, IdentifierCase::AsWritten)
            .expect("lookup")
            .expect("column exists")
            .default_value()
            .as_deref(),
        Some("7")
    );
}

#[test]
fn dropping_one_from_a_parent_reaches_every_descendant() {
    let database = database(
        "CREATE TABLE par (id INT CONSTRAINT c1 CHECK (id > 0));
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par DROP CONSTRAINT c1;",
    );

    for table in ["par", "chi", "gch"] {
        assert_eq!(checks(&database, table), 0, "{table} still holds it");
    }
}

#[test]
fn a_descendant_cannot_drop_one_it_received() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT CONSTRAINT c1 CHECK (id > 0));
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE chi DROP CONSTRAINT c1;"
        ),
        Err(Error::InheritedConstraintNotDroppable { ref table_name, ref constraint_name })
            if table_name == "chi" && constraint_name == "c1"
    ));
}

#[test]
fn only_leaves_each_direct_descendants_copy_as_its_own() {
    let detached = database(
        "CREATE TABLE par (id INT CONSTRAINT c1 CHECK (id > 0));
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par DROP CONSTRAINT c1;",
    );
    assert_eq!(checks(&detached, "par"), 0);
    assert_eq!(checks(&detached, "chi"), 1);

    // Nothing passes it down any more, so the child may now drop it.
    let dropped = database(
        "CREATE TABLE par (id INT CONSTRAINT c1 CHECK (id > 0));
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par DROP CONSTRAINT c1;
         ALTER TABLE chi DROP CONSTRAINT c1;",
    );
    assert_eq!(checks(&dropped, "chi"), 0);
}

#[test]
fn a_copy_two_parents_write_survives_one_of_them_dropping_it() {
    let sql = "CREATE TABLE left_par (id INT CONSTRAINT dual CHECK (id > 0));
               CREATE TABLE right_par (id INT CONSTRAINT dual CHECK (id > 0));
               CREATE TABLE chi () INHERITS (left_par, right_par);";

    let one = database(&format!("{sql} ALTER TABLE left_par DROP CONSTRAINT dual;"));
    assert_eq!(checks(&one, "chi"), 1);

    let both = database(&format!(
        "{sql}
         ALTER TABLE left_par DROP CONSTRAINT dual;
         ALTER TABLE right_par DROP CONSTRAINT dual;"
    ));
    assert_eq!(checks(&both, "chi"), 0);
}

#[test]
fn dropping_a_roots_key_takes_the_copy_its_partition_renamed() {
    // A partition's copy of a key carries a name of its own, so the walk has to
    // recognise it by shape rather than by name.
    let database = database(
        "CREATE TABLE root (id INT CONSTRAINT p1 PRIMARY KEY) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE root DROP CONSTRAINT p1;",
    );

    assert_eq!(unique_indices(&database, "root"), 0);
    assert_eq!(unique_indices(&database, "part"), 0);
    // As on the root, the requirement to hold a value stays.
    for table in ["root", "part"] {
        assert!(accepts_nothing(&database, table, "id"), "{table}");
    }
}

#[test]
fn an_inheritor_never_received_a_key_so_dropping_the_parents_leaves_it_alone() {
    let database = database(
        "CREATE TABLE par (id INT CONSTRAINT u1 UNIQUE);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par DROP CONSTRAINT u1;",
    );

    assert_eq!(unique_indices(&database, "par"), 0);
    assert_eq!(unique_indices(&database, "chi"), 0);
}

#[test]
fn a_name_no_side_of_the_declaration_holds_is_reported_unless_excused() {
    assert!(matches!(
        parse(
            "CREATE TABLE t (id INT CONSTRAINT c1 CHECK (id > 0));
             ALTER TABLE t DROP CONSTRAINT absent;"
        ),
        Err(Error::DropConstraintNotFound { ref constraint_name, .. }) if constraint_name == "absent"
    ));

    let excused = database(
        "CREATE TABLE t (id INT CONSTRAINT c1 CHECK (id > 0));
         ALTER TABLE t DROP CONSTRAINT IF EXISTS absent;",
    );
    assert_eq!(checks(&excused, "t"), 1);
}
