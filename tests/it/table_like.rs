//! Tests that a table built as a copy of another carries that table's
//! columns, and that the copy is only a copy.
//!
//! `CREATE TABLE copy (LIKE original)` parsed and was then discarded, so the
//! model held `copy` with no columns whatsoever: nothing to project, no
//! nullability to honour, nothing for a policy to name.
//!
//! Copying is not inheritance and shares none of its machinery. It duplicates
//! the columns once, at the point the statement runs, and records no link, so
//! a later change to the original must not reach the copy.
//!
//! Every expectation was measured against PostgreSQL 18.4. The parts that are
//! easy to guess wrong: a copy receives `NOT NULL` but not the primary key,
//! unique constraint, check, identity or stored generated expression that
//! may have implied it, and it receives a default only when the statement
//! says `INCLUDING DEFAULTS`.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::column_names::column_names;

/// A source table exercising every column feature a copy might carry.
const SOURCE: &str = "CREATE TABLE src (
     keyed INT PRIMARY KEY,
     uniq INT UNIQUE,
     defaulted INT NOT NULL DEFAULT 5,
     checked INT CHECK (checked > 0),
     counted INT GENERATED ALWAYS AS IDENTITY,
     base INT NOT NULL,
     doubled INT GENERATED ALWAYS AS (base * 2) STORED,
     labelled TEXT COLLATE \"C\",
     plain TEXT
 );";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

fn nullability(database: &ParserDB, table_name: &str) -> Vec<(String, bool)> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| {
            (
                column.column_name().to_owned(),
                column.is_nullable(database).expect("column is in this database"),
            )
        })
        .collect()
}

fn defaults(database: &ParserDB, table_name: &str) -> Vec<(String, Option<String>)> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| {
            (column.column_name().to_owned(), column.default_expression().map(ToString::to_string))
        })
        .collect()
}

/// The name of the `NOT NULL` each column of `target` holds.
fn not_null_names(database: &ParserDB, target: TargetName<'_>) -> Vec<(String, Option<String>)> {
    database
        .table_by_target(target, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| {
            (
                column.column_name().to_owned(),
                column
                    .not_null_constraint_name(database)
                    .expect("column is in this database")
                    .map(std::borrow::Cow::into_owned),
            )
        })
        .collect()
}

/// Pairs each column with the `NOT NULL` name it is expected to hold.
fn named(pairs: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    pairs.iter().map(|(column, name)| ((*column).to_owned(), name.map(str::to_owned))).collect()
}

#[test]
fn a_copy_receives_the_columns_of_the_table_it_copies() {
    let database = database(&format!("{SOURCE} CREATE TABLE bare (LIKE src);"));

    assert_eq!(
        column_names(&database, "bare"),
        [
            "keyed",
            "uniq",
            "defaulted",
            "checked",
            "counted",
            "base",
            "doubled",
            "labelled",
            "plain"
        ]
    );
}

#[test]
fn a_copy_receives_not_null_but_none_of_what_implied_it() {
    let database = database(&format!("{SOURCE} CREATE TABLE bare (LIKE src);"));
    let copy = database
        .table_by_target(TargetName::new("bare", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");

    // `keyed` and `counted` are not nullable because a primary key and an
    // identity each imply it, even though neither is copied.
    assert_eq!(
        nullability(&database, "bare"),
        [
            ("keyed".to_owned(), false),
            ("uniq".to_owned(), true),
            ("defaulted".to_owned(), false),
            ("checked".to_owned(), true),
            ("counted".to_owned(), false),
            ("base".to_owned(), false),
            ("doubled".to_owned(), true),
            ("labelled".to_owned(), true),
            ("plain".to_owned(), true),
        ]
    );

    assert_eq!(copy.primary_key_columns(&database).expect("in database").count(), 0);
    assert_eq!(copy.unique_indices(&database).expect("in database").count(), 0);
    assert_eq!(copy.check_constraints(&database).expect("in database").count(), 0);
    assert_eq!(copy.indices(&database).expect("in database").count(), 0);

    // The original keeps everything it declared.
    let source = database
        .table_by_target(TargetName::new("src", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(source.primary_key_columns(&database).expect("in database").count(), 1);
    assert_eq!(source.check_constraints(&database).expect("in database").count(), 1);
}

#[test]
fn a_copy_keeps_the_names_of_the_not_null_constraints_it_receives() {
    let database = database(
        "CREATE TABLE a (
             x INT NOT NULL,
             y INT CONSTRAINT named_nn NOT NULL,
             z INT PRIMARY KEY,
             w INT GENERATED ALWAYS AS IDENTITY,
             v INT,
             u INT,
             NOT NULL v
         );
         CREATE TABLE b (LIKE a);
         CREATE TABLE b2 (LIKE b);",
    );
    let expected = named(&[
        ("x", Some("a_x_not_null")),
        ("y", Some("named_nn")),
        ("z", Some("a_z_not_null")),
        ("w", Some("a_w_not_null")),
        ("v", Some("a_v_not_null")),
        ("u", None),
    ]);
    assert_eq!(not_null_names(&database, TargetName::new("a", false)), expected);
    assert_eq!(not_null_names(&database, TargetName::new("b", false)), expected);
    assert_eq!(not_null_names(&database, TargetName::new("b2", false)), expected);
}

#[test]
fn a_copy_keeps_the_name_the_source_holds_when_the_statement_runs() {
    // Each row was read back from `pg_constraint` on PostgreSQL 18.7.
    for (sql, copy, expected) in [
        (
            "CREATE TABLE ren (x INT NOT NULL);
             ALTER TABLE ren RENAME CONSTRAINT ren_x_not_null TO renamed_nn;
             CREATE TABLE c (LIKE ren);",
            TargetName::new("c", false),
            named(&[("x", Some("renamed_nn"))]),
        ),
        (
            "CREATE TABLE before (x INT NOT NULL);
             ALTER TABLE before RENAME TO after;
             CREATE TABLE c (LIKE after);",
            TargetName::new("c", false),
            named(&[("x", Some("before_x_not_null"))]),
        ),
        (
            "CREATE TABLE src (x INT NOT NULL);
             ALTER TABLE src RENAME COLUMN x TO y;
             CREATE TABLE c (LIKE src);",
            TargetName::new("c", false),
            named(&[("y", Some("src_x_not_null"))]),
        ),
        (
            "CREATE TABLE src (x INT);
             ALTER TABLE src ALTER COLUMN x SET NOT NULL;
             CREATE TABLE c (LIKE src);",
            TargetName::new("c", false),
            named(&[("x", Some("src_x_not_null"))]),
        ),
        (
            "CREATE TABLE p (q INT NOT NULL);
             CREATE TABLE ch (r INT NOT NULL) INHERITS (p);
             CREATE TABLE c (LIKE ch);",
            TargetName::new("c", false),
            named(&[("q", Some("p_q_not_null")), ("r", Some("ch_r_not_null"))]),
        ),
        (
            "CREATE TYPE pt AS (x INT);
             CREATE TABLE t OF pt (x WITH OPTIONS NOT NULL);
             CREATE TABLE c (LIKE t);",
            TargetName::new("c", false),
            named(&[("x", Some("t_x_not_null"))]),
        ),
        (
            "CREATE TABLE \"Up\" (x INT NOT NULL); CREATE TABLE c (LIKE \"Up\");",
            TargetName::new("c", false),
            named(&[("x", Some("Up_x_not_null"))]),
        ),
        (
            "CREATE SCHEMA s; CREATE TABLE s.a (x INT NOT NULL); CREATE TABLE s.c (LIKE s.a);",
            TargetName::new("c", false).with_schema("s", false),
            named(&[("x", Some("a_x_not_null"))]),
        ),
        (
            // The copied name wins over the one the parent passes.
            "CREATE TABLE a (x INT NOT NULL, y INT NOT NULL);
             CREATE TABLE p (x INT NOT NULL, z INT);
             CREATE TABLE c (LIKE a) INHERITS (p);",
            TargetName::new("c", false),
            named(&[("x", Some("a_x_not_null")), ("z", None), ("y", Some("a_y_not_null"))]),
        ),
    ] {
        assert_eq!(not_null_names(&database(sql), copy), expected, "{sql}");
    }
}

#[test]
fn a_copied_not_null_name_reaches_children_until_the_requirement_is_dropped() {
    // Read back from `pg_constraint` on PostgreSQL 18.7.
    let database = database(
        "CREATE TABLE a (x INT NOT NULL, y INT NOT NULL);
         CREATE TABLE b (LIKE a);
         CREATE TABLE g () INHERITS (b);
         ALTER TABLE b ALTER COLUMN y DROP NOT NULL;
         ALTER TABLE b ALTER COLUMN y SET NOT NULL;",
    );
    let expected = named(&[("x", Some("a_x_not_null")), ("y", Some("b_y_not_null"))]);
    assert_eq!(not_null_names(&database, TargetName::new("b", false)), expected);
    assert_eq!(not_null_names(&database, TargetName::new("g", false)), expected);
}

#[test]
fn a_default_arrives_only_when_the_statement_asks_for_it() {
    let with = database(&format!("{SOURCE} CREATE TABLE c (LIKE src INCLUDING DEFAULTS);"));
    let defaulted = defaults(&with, "c")
        .into_iter()
        .find(|(name, _)| name == "defaulted")
        .expect("column exists");
    assert_eq!(defaulted.1, Some("5".to_owned()));

    // Saying nothing and saying `EXCLUDING DEFAULTS` both leave it behind.
    for clause in ["", " EXCLUDING DEFAULTS"] {
        let without = database(&format!("{SOURCE} CREATE TABLE c (LIKE src{clause});"));
        assert!(
            defaults(&without, "c").into_iter().all(|(_, default)| default.is_none()),
            "`LIKE src{clause}` copies no default"
        );
    }
}

#[test]
fn a_copy_is_not_a_child() {
    let database = database(&format!("{SOURCE} CREATE TABLE bare (LIKE src);"));

    let copy = database
        .table_by_target(TargetName::new("bare", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let source = database
        .table_by_target(TargetName::new("src", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");

    // Copying records no edge in either direction, and every column the copy
    // holds is its own.
    assert_eq!(copy.inherits_from(&database).expect("in database").count(), 0);
    assert_eq!(source.inheritors(&database).expect("in database").count(), 0);
    assert_eq!(copy.local_columns(&database).expect("in database").count(), 9);
}

#[test]
fn a_copy_does_not_follow_the_table_it_copied() {
    // The distinguishing property: a copy is taken once. An inheriting child
    // would have followed both of these changes.
    let database = database(
        "CREATE TABLE src (a INT);
         CREATE TABLE copy (LIKE src);
         ALTER TABLE src ADD COLUMN added TEXT;
         ALTER TABLE src DROP COLUMN a;",
    );

    assert_eq!(column_names(&database, "copy"), ["a"]);
    assert_eq!(column_names(&database, "src"), ["added"]);
}

#[test]
fn a_copy_can_name_a_table_in_another_schema() {
    let database = database(
        "CREATE SCHEMA s1;
         CREATE TABLE s1.other (z INT NOT NULL);
         CREATE TABLE fromschema (LIKE s1.other);",
    );

    assert_eq!(column_names(&database, "fromschema"), ["z"]);
    assert_eq!(nullability(&database, "fromschema"), [("z".to_owned(), false)]);
}

#[test]
fn copying_a_table_the_input_never_created_is_refused() {
    assert!(matches!(
        parse("CREATE TABLE c (LIKE absent_table);"),
        Err(Error::CopiedTableNotFound { ref copied_table, ref table_name, .. })
            if copied_table == "absent_table" && table_name == "c"
    ));
}

#[test]
fn a_copy_and_an_inheritance_in_one_statement_each_contribute() {
    // PostgreSQL puts the parent's columns first and counts the copied ones
    // as the table's own, because a copy is a declaration.
    let database = database(
        "CREATE TABLE lsrc (l1 INT, l2 TEXT);
         CREATE TABLE par (p1 INT);
         CREATE TABLE combo (LIKE lsrc) INHERITS (par);",
    );

    assert_eq!(column_names(&database, "combo"), ["p1", "l1", "l2"]);

    let combo = database
        .table_by_target(TargetName::new("combo", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(
        combo
            .local_columns(&database)
            .expect("in database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<_>>(),
        ["l1", "l2"]
    );
    assert_eq!(
        combo
            .inherits_from(&database)
            .expect("in database")
            .map(|parent| parent.table_name().to_owned())
            .collect::<Vec<_>>(),
        ["par"]
    );
}

#[test]
fn the_spelling_without_parentheses_copies_alike() {
    // Snowflake and BigQuery write the clause outside the column list. The
    // parser accepts it for any dialect, and it names the same copy.
    let database = database("CREATE TABLE src (a INT NOT NULL, b TEXT); CREATE TABLE c LIKE src;");

    assert_eq!(column_names(&database, "c"), ["a", "b"]);
    assert_eq!(nullability(&database, "c"), [("a".to_owned(), false), ("b".to_owned(), true)]);
    assert_eq!(
        database
            .table_by_target(TargetName::new("c", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .inherits_from(&database)
            .expect("in database")
            .count(),
        0
    );
}

#[test]
fn a_table_that_copies_nothing_is_left_alone() {
    let database = database("CREATE TABLE plain (a INT PRIMARY KEY, b TEXT);");

    assert_eq!(column_names(&database, "plain"), ["a", "b"]);
    assert_eq!(
        database
            .table_by_target(TargetName::new("plain", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .primary_key_columns(&database)
            .expect("in database")
            .count(),
        1
    );
}
