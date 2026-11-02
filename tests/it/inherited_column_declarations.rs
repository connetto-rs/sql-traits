//! Tests what a child takes from its parent's column declaration, and which
//! changes to a column travel down.
//!
//! Three answers used to be wrong, all from the same cause: what a child
//! receives from a parent's column was decided in two places that disagreed,
//! and neither consulted the rule the create-time copy already used. A child
//! redeclaring a column held no check from the parent. A child could drop a
//! requirement its parent enforces, leaving the two disagreeing about one
//! column. And an identity added to a parent reached an `INHERITS` child, which
//! would give that child a sequence of its own.
//!
//! Two statements PostgreSQL refuses are now refused here too. Removing the
//! requirement to hold a value is refused where a key covers the column, which
//! previously appeared to succeed and did nothing, since the key immediately
//! put the requirement back. Adding an identity is refused while the column may
//! still hold nothing.
//!
//! Every expectation was measured against PostgreSQL 18.4.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, RequiredValue},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

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

fn requires_a_value(database: &ParserDB, table_name: &str, column_name: &str) -> bool {
    !column(database, table_name, column_name).is_nullable(database).expect("in this database")
}

fn column<'db>(
    database: &'db ParserDB,
    table_name: &str,
    column_name: &str,
) -> &'db <ParserDB as DatabaseLike>::Column {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists")
}

fn carries_an_identity(database: &ParserDB, table_name: &str, column_name: &str) -> bool {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns
        .iter()
        .find(|declared| declared.name.value == column_name)
        .expect("column exists")
        .options
        .iter()
        .any(|option| sql_traits::utils::is_identity(&option.option))
}

#[test]
fn a_redeclared_column_still_receives_the_parents_check() {
    let database = database(
        "CREATE TABLE par (id INT CONSTRAINT c1 CHECK (id > 0));
         CREATE TABLE chi (id INT) INHERITS (par);",
    );

    assert_eq!(checks(&database, "par"), 1);
    assert_eq!(checks(&database, "chi"), 1);
}

#[test]
fn a_redeclared_column_keeps_its_own_where_only_one_may_be_stated() {
    // PostgreSQL leaves the child on its own default and lets both checks
    // stand, each under its own name, because a check may be written any
    // number of times while a default may not.
    let database = database(
        "CREATE TABLE par (v INT DEFAULT 1 CONSTRAINT c1 CHECK (v > 0));
         CREATE TABLE chi (v INT DEFAULT 9 CONSTRAINT c2 CHECK (v < 100)) INHERITS (par);",
    );

    assert_eq!(checks(&database, "par"), 1);
    assert_eq!(checks(&database, "chi"), 2);
    assert_eq!(column(&database, "par", "v").default_value().as_deref(), Some("1"));
    assert_eq!(column(&database, "chi", "v").default_value().as_deref(), Some("9"));
}

#[test]
fn a_redeclared_column_receives_no_key_and_no_reference() {
    // The same rule as for a column the child does not redeclare: a key, a
    // unique constraint and a reference stay with the parent, because each
    // would otherwise become one of the child's own.
    let database = database(
        "CREATE TABLE tgt (id INT PRIMARY KEY);
         CREATE TABLE par (u INT CONSTRAINT uq UNIQUE, r INT CONSTRAINT fk REFERENCES tgt (id));
         CREATE TABLE chi (u INT, r INT) INHERITS (par);",
    );

    let chi = database
        .table_by_target(TargetName::new("chi", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(chi.unique_indices(&database).expect("in database").count(), 0);
    assert_eq!(chi.foreign_keys(&database).expect("in database").count(), 0);

    let par = database
        .table_by_target(TargetName::new("par", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(par.unique_indices(&database).expect("in database").count(), 1);
    assert_eq!(par.foreign_keys(&database).expect("in database").count(), 1);
}

#[test]
fn a_redeclared_column_may_require_a_value_the_parent_does_not() {
    let database = database(
        "CREATE TABLE par (v INT);
         CREATE TABLE chi (v INT NOT NULL) INHERITS (par);",
    );

    assert!(requires_a_value(&database, "chi", "v"));
    assert!(!requires_a_value(&database, "par", "v"));
}

#[test]
fn a_child_cannot_stop_requiring_a_value_its_parent_requires() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT NOT NULL);
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE chi ALTER COLUMN id DROP NOT NULL;"
        ),
        Err(Error::RequiredValueNotDroppable {
            ref table_name,
            ref column_name,
            reason: RequiredValue::EnforcedByParent,
        }) if table_name == "chi" && column_name == "id"
    ));
}

#[test]
fn a_child_may_stop_requiring_a_value_it_required_itself() {
    let database = database(
        "CREATE TABLE par (own INT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE chi ALTER COLUMN own SET NOT NULL;
         ALTER TABLE chi ALTER COLUMN own DROP NOT NULL;",
    );

    assert!(!requires_a_value(&database, "chi", "own"));
    assert!(!requires_a_value(&database, "par", "own"));
}

#[test]
fn a_parent_lifting_the_requirement_lifts_it_below_unless_only_is_written() {
    let reaching = database(
        "CREATE TABLE par (id INT NOT NULL);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ALTER COLUMN id DROP NOT NULL;",
    );
    assert!(!requires_a_value(&reaching, "par", "id"));
    assert!(!requires_a_value(&reaching, "chi", "id"));

    let named_only = database(
        "CREATE TABLE par (id INT NOT NULL);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par ALTER COLUMN id DROP NOT NULL;",
    );
    assert!(!requires_a_value(&named_only, "par", "id"));
    assert!(requires_a_value(&named_only, "chi", "id"));
}

#[test]
fn a_column_a_key_covers_cannot_stop_requiring_a_value() {
    // Both spellings of the key, since the requirement reaches the column by a
    // different route in each. This previously appeared to succeed and changed
    // nothing, because the key put the requirement straight back.
    for sql in [
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t ALTER COLUMN id DROP NOT NULL;",
        "CREATE TABLE t (id INT, CONSTRAINT pk PRIMARY KEY (id));
         ALTER TABLE t ALTER COLUMN id DROP NOT NULL;",
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::RequiredValueNotDroppable { reason: RequiredValue::CoveredByKey, .. })
            ),
            "should have been refused:\n{sql}"
        );
    }
}

#[test]
fn an_identity_added_to_a_parent_stays_with_it() {
    let database = database(
        "CREATE TABLE par (id INT NOT NULL);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
    );

    assert!(carries_an_identity(&database, "par", "id"));
    assert!(!carries_an_identity(&database, "chi", "id"));
    // The child still requires a value, because the parent required one before
    // the identity was added.
    assert!(requires_a_value(&database, "chi", "id"));
}

#[test]
fn an_identity_added_to_a_root_reaches_its_partitions() {
    // PostgreSQL enforces the root's identity across every partition, so unlike
    // an inheritor a partition does receive it.
    let database = database(
        "CREATE TABLE root (id INT NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE root ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
    );

    assert!(carries_an_identity(&database, "root", "id"));
    assert!(carries_an_identity(&database, "part", "id"));
}

#[test]
fn a_column_that_may_hold_nothing_cannot_be_given_an_identity() {
    assert!(matches!(
        parse(
            "CREATE TABLE t (id INT);
             ALTER TABLE t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;"
        ),
        Err(Error::IdentityNeedsRequiredValue { ref table_name, ref column_name })
            if table_name == "t" && column_name == "id"
    ));

    // Requiring one first is accepted, which is the sequence PostgreSQL wants.
    let database = database(
        "CREATE TABLE t (id INT);
         ALTER TABLE t ALTER COLUMN id SET NOT NULL;
         ALTER TABLE t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY;",
    );
    assert!(carries_an_identity(&database, "t", "id"));
}

#[test]
fn the_requirement_and_a_default_still_reach_every_descendant() {
    // Guards the walk that decides how far a column change travels: only an
    // identity stops at an inheritor, and everything else must still arrive.
    let required = database(
        "CREATE TABLE par (v INT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ALTER COLUMN v SET NOT NULL;",
    );
    for table in ["par", "chi", "gch"] {
        assert!(requires_a_value(&required, table, "v"), "{table}");
    }

    let defaulted = database(
        "CREATE TABLE par (v INT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ALTER COLUMN v SET DEFAULT 5;",
    );
    for table in ["par", "chi", "gch"] {
        assert_eq!(column(&defaulted, table, "v").default_value().as_deref(), Some("5"), "{table}");
    }
}
