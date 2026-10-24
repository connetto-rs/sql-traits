//! Tests that a table taking its shape from another answers for the columns
//! it receives, and that the edge itself is readable.
//!
//! `CREATE TABLE child (...) INHERITS (parent)` used to parse and then be
//! discarded, so the model held the child with only its locally declared
//! columns and nothing recorded that a parent existed. A partition fared
//! worse: `CREATE TABLE part PARTITION OF parent` produced a table with no
//! columns at all.
//!
//! Every expectation here was measured against PostgreSQL 18.4 rather than
//! read off the documentation, including the parts that are easy to guess
//! wrong. An `INHERITS` child does not receive the parent's primary key,
//! unique constraint, foreign key or identity, but does receive its
//! `NOT NULL`, `DEFAULT`, `CHECK`, collation and stored generated expression.
//! A partition receives every one of them, because PostgreSQL enforces a
//! root's keys across the whole hierarchy rather than one table at a time,
//! and the two links are told apart rather than folded together: a partition
//! does not inherit from its root.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::{BigQueryDialect, PostgreSqlDialect};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_names.rs"));

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

fn local_column_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .local_columns(database)
        .expect("table is in this database")
        .map(|column| column.column_name().to_owned())
        .collect()
}

fn parent_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .inherits_from(database)
        .expect("table is in this database")
        .map(|parent| parent.table_name().to_owned())
        .collect()
}

fn root_name(database: &ParserDB, table_name: &str) -> Option<String> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .partition_root(database)
        .expect("table is in this database")
        .map(|root| root.table_name().to_owned())
}

fn column<'db>(
    table: &'db sqlparser::ast::CreateTable,
    name: &str,
    database: &'db ParserDB,
) -> &'db <ParserDB as DatabaseLike>::Column {
    table
        .column(name, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists")
}

#[test]
fn a_child_receives_the_parent_columns_before_its_own() {
    let database = database(
        "CREATE TABLE docs (id INT PRIMARY KEY, owner_id TEXT);
         CREATE TABLE secret_docs (classification TEXT) INHERITS (docs);",
    );

    assert_eq!(column_names(&database, "secret_docs"), ["id", "owner_id", "classification"]);
    assert_eq!(local_column_names(&database, "secret_docs"), ["classification"]);
    assert_eq!(parent_names(&database, "secret_docs"), ["docs"]);
}

#[test]
fn a_parent_answers_the_tables_inheriting_from_it() {
    let database = database(
        "CREATE TABLE docs (id INT);
         CREATE TABLE secret_docs (classification TEXT) INHERITS (docs);
         CREATE TABLE public_docs (blurb TEXT) INHERITS (docs);
         CREATE TABLE unrelated (id INT);",
    );

    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let children: Vec<_> = docs
        .inheritors(&database)
        .expect("table is in this database")
        .map(|child| child.table_name().to_owned())
        .collect();
    assert_eq!(children, ["public_docs", "secret_docs"]);

    let unrelated = database
        .table_by_target(TargetName::new("unrelated", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(unrelated.inheritors(&database).expect("table is in this database").count(), 0);
}

#[test]
fn only_direct_parents_and_children_are_answered() {
    let database = database(
        "CREATE TABLE g1 (a INT);
         CREATE TABLE g2 (b INT) INHERITS (g1);
         CREATE TABLE g3 (c INT) INHERITS (g2);",
    );

    // The column list transits the whole chain, while the edge accessors stop
    // at one step, so a grandparent is reached through the parent.
    assert_eq!(column_names(&database, "g3"), ["a", "b", "c"]);
    assert_eq!(parent_names(&database, "g3"), ["g2"]);
    assert_eq!(parent_names(&database, "g2"), ["g1"]);

    let g1 = database
        .table_by_target(TargetName::new("g1", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let children: Vec<_> = g1
        .inheritors(&database)
        .expect("table is in this database")
        .map(|child| child.table_name().to_owned())
        .collect();
    assert_eq!(children, ["g2"]);
}

#[test]
fn several_parents_contribute_in_order_and_a_shared_column_merges() {
    let database = database(
        "CREATE TABLE a1 (shared TEXT, only_a INT);
         CREATE TABLE b1 (shared TEXT, only_b INT);
         CREATE TABLE c1 (own_col BOOLEAN) INHERITS (a1, b1);",
    );

    // PostgreSQL emits a notice and keeps one `shared`, in the position the
    // first parent gave it.
    assert_eq!(column_names(&database, "c1"), ["shared", "only_a", "only_b", "own_col"]);
    assert_eq!(local_column_names(&database, "c1"), ["own_col"]);
    assert_eq!(parent_names(&database, "c1"), ["a1", "b1"]);
}

#[test]
fn a_redeclared_column_keeps_the_position_the_parent_gave_it() {
    let database = database(
        "CREATE TABLE p2 (x INT, y TEXT);
         CREATE TABLE c2 (x INT, z BOOLEAN) INHERITS (p2);",
    );

    assert_eq!(column_names(&database, "c2"), ["x", "y", "z"]);
    // A column the child declares counts as its own even where a parent also
    // declares it, which is what `pg_attribute.attislocal` records.
    assert_eq!(local_column_names(&database, "c2"), ["x", "z"]);
}

#[test]
fn a_not_null_survives_whichever_side_states_it() {
    let database = database(
        "CREATE TABLE np (kept INT NOT NULL, loosened INT NOT NULL, plain INT);
         CREATE TABLE nc (loosened INT, tightened INT NOT NULL) INHERITS (np);",
    );

    let table = database
        .table_by_target(TargetName::new("nc", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let nullability: Vec<(String, bool)> = table
        .columns(&database)
        .expect("table is in this database")
        .map(|column| {
            (
                column.column_name().to_owned(),
                column.is_nullable(&database).expect("column is in this database"),
            )
        })
        .collect();

    assert_eq!(
        nullability,
        [
            ("kept".to_owned(), false),
            // A child cannot loosen a `NOT NULL` it inherits.
            ("loosened".to_owned(), false),
            ("plain".to_owned(), true),
            ("tightened".to_owned(), false),
        ]
    );
}

#[test]
fn a_child_receives_defaults_and_checks_but_no_key_of_its_own() {
    let database = database(
        "CREATE TABLE par (
             keyed INT PRIMARY KEY,
             uniq INT UNIQUE,
             defaulted INT NOT NULL DEFAULT 5,
             checked INT CHECK (checked > 0)
         );
         CREATE TABLE chi (own INT) INHERITS (par);",
    );

    let child = database
        .table_by_target(TargetName::new("chi", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");

    assert_eq!(column_names(&database, "chi"), ["keyed", "uniq", "defaulted", "checked", "own"]);

    // A primary key, a unique constraint and the index behind them all stay
    // with the parent.
    assert_eq!(child.primary_key_columns(&database).expect("in database").count(), 0);
    assert_eq!(child.unique_indices(&database).expect("in database").count(), 0);

    // The check travels with the column it is written on.
    assert_eq!(child.check_constraints(&database).expect("in database").count(), 1);

    // The primary key is withheld but the `NOT NULL` it implies is not.
    let keyed = child
        .column("keyed", &database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists");
    assert!(!keyed.is_nullable(&database).expect("column is in this database"));

    let parent = database
        .table_by_target(TargetName::new("par", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(parent.primary_key_columns(&database).expect("in database").count(), 1);
}

#[test]
fn a_check_written_on_its_own_is_inherited_too() {
    let database = database(
        "CREATE TABLE tp (id INT, CONSTRAINT positive CHECK (id > 0));
         CREATE TABLE tc (extra INT) INHERITS (tp);",
    );

    let child = database
        .table_by_target(TargetName::new("tc", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(child.check_constraints(&database).expect("in database").count(), 1);
}

#[test]
fn a_partition_receives_every_column_of_the_table_it_partitions() {
    let database = database(
        "CREATE TABLE evt (id INT, happened_at DATE NOT NULL) PARTITION BY RANGE (happened_at);
         CREATE TABLE evt_2024 PARTITION OF evt
             FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');",
    );

    // A partition cannot declare columns of its own, so every column is the
    // parent's and none is local.
    assert_eq!(column_names(&database, "evt_2024"), ["id", "happened_at"]);
    assert_eq!(local_column_names(&database, "evt_2024"), Vec::<String>::new());

    // The link is a partition link, and not an inheritance one.
    assert_eq!(root_name(&database, "evt_2024").as_deref(), Some("evt"));
    assert_eq!(parent_names(&database, "evt_2024"), Vec::<String>::new());
}

#[test]
fn a_column_a_parent_gains_later_reaches_the_child() {
    let database = database(
        "CREATE TABLE ap (a INT);
         CREATE TABLE ac (c INT) INHERITS (ap);
         ALTER TABLE ap ADD COLUMN b TEXT;",
    );

    // The child's own column already had its place when the parent gained
    // `b`, so the new column lands at the end rather than among the ones the
    // child started with. Measured against PostgreSQL, which answers the same.
    assert_eq!(column_names(&database, "ac"), ["a", "c", "b"]);
    assert_eq!(local_column_names(&database, "ac"), ["c"]);
}

#[test]
fn a_column_a_parent_renames_or_retypes_changes_in_the_child_too() {
    let database = database(
        "CREATE TABLE par (a INT, b INT, c INT);
         CREATE TABLE chi (own INT) INHERITS (par);
         ALTER TABLE par RENAME COLUMN b TO renamed;
         ALTER TABLE par ALTER COLUMN c TYPE BIGINT;",
    );

    assert_eq!(column_names(&database, "chi"), ["a", "renamed", "c", "own"]);

    let child = database
        .table_by_target(TargetName::new("chi", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let retyped = child
        .column("c", &database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists");
    assert_eq!(retyped.data_type(&database), "BIGINT");
}

#[test]
fn a_column_a_parent_drops_leaves_the_child_too() {
    let database = database(
        "CREATE TABLE par (a INT, b INT);
         CREATE TABLE chi (own INT) INHERITS (par);
         CREATE TABLE grandchild (mine INT) INHERITS (chi);
         ALTER TABLE par DROP COLUMN a;",
    );

    assert_eq!(column_names(&database, "chi"), ["b", "own"]);
    assert_eq!(column_names(&database, "grandchild"), ["b", "own", "mine"]);
}

#[test]
fn a_child_cannot_drop_a_column_it_inherits() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (a INT);
             CREATE TABLE chi (own INT) INHERITS (par);
             ALTER TABLE chi DROP COLUMN a;"
        ),
        Err(Error::InheritedColumnNotDroppable { ref table_name, ref column_name })
            if table_name == "chi" && column_name == "a"
    ));

    // Its own column is still its to drop.
    let database = database(
        "CREATE TABLE par (a INT);
         CREATE TABLE chi (own INT) INHERITS (par);
         ALTER TABLE chi DROP COLUMN own;",
    );
    assert_eq!(column_names(&database, "chi"), ["a"]);
}

#[test]
fn a_parent_in_another_schema_is_reached() {
    let database = database(
        "CREATE SCHEMA s1;
         CREATE TABLE s1.base (b INT);
         CREATE TABLE derived (d INT) INHERITS (s1.base);",
    );

    assert_eq!(column_names(&database, "derived"), ["b", "d"]);
    assert_eq!(parent_names(&database, "derived"), ["base"]);
}

#[test]
fn a_parent_the_input_never_created_is_refused() {
    assert!(matches!(
        parse("CREATE TABLE child (x INT) INHERITS (nonexistent_parent);"),
        Err(Error::ParentTableNotFound { ref parent_table, ref child_table })
            if parent_table == "nonexistent_parent" && child_table == "child"
    ));

    // A partition names its parent the same way and is refused the same way.
    assert!(matches!(
        parse("CREATE TABLE part PARTITION OF absent FOR VALUES IN (1);"),
        Err(Error::ParentTableNotFound { ref parent_table, .. }) if parent_table == "absent"
    ));
}

#[test]
fn a_parent_named_before_it_is_created_is_refused() {
    // PostgreSQL resolves the parent while running the statement, so a
    // forward reference fails there too, and a table cannot name itself.
    assert!(matches!(
        parse("CREATE TABLE a (x INT) INHERITS (b); CREATE TABLE b (y INT);"),
        Err(Error::ParentTableNotFound { ref parent_table, .. }) if parent_table == "b"
    ));
    assert!(matches!(
        parse("CREATE TABLE t (x INT) INHERITS (t);"),
        Err(Error::ParentTableNotFound { ref parent_table, .. }) if parent_table == "t"
    ));
}

#[test]
fn redeclaring_an_inherited_column_with_another_type_is_refused() {
    assert!(matches!(
        parse(
            "CREATE TABLE p (x INT, y TEXT);
             CREATE TABLE c (x TEXT) INHERITS (p);"
        ),
        Err(Error::InheritedColumnTypeConflict { ref column_name, ref child_table, .. })
            if column_name == "x" && child_table == "c"
    ));

    // A narrower or wider integer is a different type, and so is a different
    // length on the same one.
    assert!(matches!(
        parse("CREATE TABLE p (x INT); CREATE TABLE c (x BIGINT) INHERITS (p);"),
        Err(Error::InheritedColumnTypeConflict { .. })
    ));
    assert!(matches!(
        parse("CREATE TABLE p (x VARCHAR(10)); CREATE TABLE c (x VARCHAR(20)) INHERITS (p);"),
        Err(Error::InheritedColumnTypeConflict { .. })
    ));
}

#[test]
fn another_spelling_of_the_same_type_is_not_a_conflict() {
    // PostgreSQL resolves both spellings to one type, so redeclaring is a
    // merge rather than a conflict.
    for (parent_type, child_type) in [
        ("INT", "INTEGER"),
        ("VARCHAR(10)", "CHARACTER VARYING(10)"),
        ("BOOL", "BOOLEAN"),
        ("DECIMAL(10,2)", "NUMERIC(10,2)"),
        ("TIMESTAMPTZ", "TIMESTAMP WITH TIME ZONE"),
    ] {
        let sql = format!(
            "CREATE TABLE p (x {parent_type});
             CREATE TABLE c (x {child_type}) INHERITS (p);"
        );
        let parsed = parse(&sql);
        assert!(
            parsed.is_ok(),
            "`{parent_type}` and `{child_type}` name one type, so redeclaring merges"
        );
        assert_eq!(column_names(&parsed.expect("checked above"), "c"), ["x"]);
    }
}

#[test]
fn a_type_the_model_cannot_place_is_accepted_rather_than_guessed_at() {
    // Refusing a legal schema costs more than missing a conflict the model
    // cannot see, so an unfamiliar spelling merges.
    let database = database(
        "CREATE TABLE p (shape GEOMETRY);
         CREATE TABLE c (shape GEOGRAPHY) INHERITS (p);",
    );
    assert_eq!(column_names(&database, "c"), ["shape"]);
}

#[test]
fn dropping_a_parent_needs_cascade_and_takes_the_children_with_it() {
    assert!(matches!(
        parse(
            "CREATE TABLE p (x INT);
             CREATE TABLE c (y INT) INHERITS (p);
             DROP TABLE p;"
        ),
        Err(Error::DropTableInheritedFrom { ref parent_table, ref child_table })
            if parent_table == "p" && child_table == "c"
    ));

    let database = database(
        "CREATE TABLE p (x INT);
         CREATE TABLE c (y INT) INHERITS (p);
         CREATE TABLE grandchild (z INT) INHERITS (c);
         CREATE TABLE untouched (w INT);
         DROP TABLE p CASCADE;",
    );
    let remaining: Vec<_> = database.tables().map(|table| table.table_name().to_owned()).collect();
    assert_eq!(remaining, ["untouched"]);
}

#[test]
fn renaming_a_parent_keeps_the_edge() {
    let database = database(
        "CREATE TABLE p (x INT);
         CREATE TABLE c (y INT) INHERITS (p);
         ALTER TABLE p RENAME TO renamed;",
    );

    assert_eq!(parent_names(&database, "c"), ["renamed"]);
    assert_eq!(column_names(&database, "c"), ["x", "y"]);
}

#[test]
fn a_table_without_a_parent_is_left_alone() {
    let database = database("CREATE TABLE plain (a INT PRIMARY KEY, b TEXT);");

    assert_eq!(column_names(&database, "plain"), ["a", "b"]);
    assert_eq!(local_column_names(&database, "plain"), ["a", "b"]);
    assert_eq!(parent_names(&database, "plain"), Vec::<String>::new());

    let table = database
        .table_by_target(TargetName::new("plain", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(table.primary_key_columns(&database).expect("in database").count(), 1);
}

#[test]
fn the_accessors_answer_the_same_through_a_reference() {
    let database = database(
        "CREATE TABLE docs (id INT);
         CREATE TABLE secret_docs (classification TEXT) INHERITS (docs);
         CREATE TABLE evt (id INT) PARTITION BY RANGE (id);
         CREATE TABLE evt_low PARTITION OF evt FOR VALUES FROM (1) TO (9);",
    );
    let child = database
        .table_by_target(TargetName::new("secret_docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let by_reference = &child;

    assert_eq!(
        TableLike::inherits_from(by_reference, &database)
            .expect("in database")
            .map(|parent| parent.table_name().to_owned())
            .collect::<Vec<_>>(),
        ["docs"]
    );
    assert_eq!(
        TableLike::local_columns(by_reference, &database)
            .expect("in database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<_>>(),
        ["classification"]
    );

    let parent = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let parent_reference = &parent;
    assert_eq!(TableLike::inheritors(parent_reference, &database).expect("in database").count(), 1);

    let root = database
        .table_by_target(TargetName::new("evt", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let root_reference = &root;
    assert_eq!(TableLike::partitions(root_reference, &database).expect("in database").count(), 1);
    assert_eq!(TableLike::partition_strategy(root_reference), Some(PartitionStrategy::Range));
    assert!(TableLike::is_partitioned(root_reference));
    assert!(!TableLike::is_partition(root_reference, &database).expect("in database"));

    let partition = database
        .table_by_target(TargetName::new("evt_low", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let partition_reference = &partition;
    assert_eq!(
        TableLike::partition_root(partition_reference, &database)
            .expect("in database")
            .map(|root| root.table_name().to_owned()),
        Some("evt".to_owned())
    );
    assert!(TableLike::is_partition(partition_reference, &database).expect("in database"));
}

#[test]
fn an_identity_stays_with_the_parent_while_a_stored_expression_comes_down() {
    let database = database(
        "CREATE TABLE par (
             counted INT GENERATED ALWAYS AS IDENTITY,
             base INT NOT NULL,
             doubled INT GENERATED ALWAYS AS (base * 2) STORED,
             labelled TEXT COLLATE \"C\"
         );
         CREATE TABLE chi (own INT) INHERITS (par);",
    );

    assert_eq!(column_names(&database, "chi"), ["counted", "base", "doubled", "labelled", "own"]);

    let child = database
        .table_by_target(TargetName::new("chi", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let parent = database
        .table_by_target(TargetName::new("par", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    // Nullability as PostgreSQL reports it for the child. The identity is
    // withheld but the `NOT NULL` it implies is not, which is why `counted`
    // is not nullable even though nothing spells it.
    for (name, nullable) in
        [("counted", false), ("base", false), ("doubled", true), ("labelled", true)]
    {
        assert_eq!(
            column(child, name, &database)
                .is_nullable(&database)
                .expect("column is in this database"),
            nullable,
            "`{name}` nullability"
        );
    }

    // The declaration itself is the parent's, whatever it says.
    for name in ["counted", "base", "doubled", "labelled"] {
        assert_eq!(
            column(child, name, &database).data_type(&database),
            column(parent, name, &database).data_type(&database),
            "`{name}` keeps the parent's type"
        );
        assert_eq!(
            column(child, name, &database).is_generated(),
            column(parent, name, &database).is_generated(),
            "`{name}` keeps the parent's generated flag"
        );
    }

    // The identity does not reach the child as a key or a sequence of its
    // own, so the child has neither.
    assert_eq!(child.primary_key_columns(&database).expect("in database").count(), 0);
    assert_eq!(child.unique_indices(&database).expect("in database").count(), 0);

    assert_eq!(local_column_names(&database, "chi"), ["own"]);
}

#[test]
fn a_grandparent_reached_through_two_parents_contributes_once() {
    let database = database(
        "CREATE TABLE top (shared INT);
         CREATE TABLE left_branch (l INT) INHERITS (top);
         CREATE TABLE right_branch (r INT) INHERITS (top);
         CREATE TABLE bottom (b INT) INHERITS (left_branch, right_branch);",
    );

    // `shared` arrives down both branches and merges into one column.
    assert_eq!(column_names(&database, "bottom"), ["shared", "l", "r", "b"]);
    assert_eq!(local_column_names(&database, "bottom"), ["b"]);
}

#[test]
fn a_column_dropped_at_the_top_of_a_diamond_goes_exactly_once() {
    // `bottom` is reached down both branches, so the drop has to arrive there
    // once rather than twice.
    let database = database(
        "CREATE TABLE top (shared INT);
         CREATE TABLE left_branch (l INT) INHERITS (top);
         CREATE TABLE right_branch (r INT) INHERITS (top);
         CREATE TABLE bottom (b INT) INHERITS (left_branch, right_branch);
         ALTER TABLE top DROP COLUMN shared;",
    );

    assert_eq!(column_names(&database, "bottom"), ["l", "r", "b"]);
    assert_eq!(column_names(&database, "left_branch"), ["l"]);
}

#[test]
fn a_column_added_to_a_grandparent_reaches_the_grandchild() {
    let database = database(
        "CREATE TABLE g1 (a INT);
         CREATE TABLE g2 (b INT) INHERITS (g1);
         CREATE TABLE g3 (c INT) INHERITS (g2);
         ALTER TABLE g1 ADD COLUMN late TEXT;",
    );

    assert_eq!(column_names(&database, "g2"), ["a", "b", "late"]);
    assert_eq!(column_names(&database, "g3"), ["a", "b", "c", "late"]);
    // The added column is the parent's in both, so neither counts it as local.
    assert_eq!(local_column_names(&database, "g2"), ["b"]);
    assert_eq!(local_column_names(&database, "g3"), ["c"]);
}

#[test]
fn a_renamed_inherited_column_is_still_the_parents() {
    let database = database(
        "CREATE TABLE par (a INT);
         CREATE TABLE chi (own INT) INHERITS (par);
         ALTER TABLE par RENAME COLUMN a TO renamed;",
    );

    assert_eq!(column_names(&database, "chi"), ["renamed", "own"]);
    // The rename has to follow into the record of what the child inherits,
    // or the child starts claiming the column as its own.
    assert_eq!(local_column_names(&database, "chi"), ["own"]);
}

#[test]
fn types_that_differ_only_in_width_or_precision_are_a_conflict() {
    // Each pair names two different PostgreSQL types, so redeclaring is
    // refused rather than merged.
    for (parent_type, child_type) in [
        ("SMALLINT", "INT"),
        ("REAL", "DOUBLE PRECISION"),
        ("UUID", "TEXT"),
        ("CHARACTER VARYING(10)", "CHARACTER VARYING(20)"),
        ("NUMERIC(10,2)", "NUMERIC"),
        ("NUMERIC(10)", "NUMERIC(10,2)"),
        ("VARCHAR(10)", "VARCHAR"),
        ("TIMESTAMP", "TIMESTAMPTZ"),
        ("CHAR(3)", "VARCHAR(3)"),
        // Two shapes of the same idea are still two types, so a typo in the
        // table that collapsed either pair would be caught here.
        ("JSON", "JSONB"),
        ("DATE", "TIMESTAMP"),
        ("BYTEA", "TEXT"),
    ] {
        let sql = format!(
            "CREATE TABLE p (x {parent_type});
             CREATE TABLE c (x {child_type}) INHERITS (p);"
        );
        assert!(
            matches!(parse(&sql), Err(Error::InheritedColumnTypeConflict { .. })),
            "`{parent_type}` and `{child_type}` are different types, so redeclaring is refused"
        );
    }
}

#[test]
fn types_spelled_without_their_optional_size_still_match() {
    for shared_type in [
        "SMALLINT",
        "REAL",
        "DOUBLE PRECISION",
        "UUID",
        "NUMERIC",
        "NUMERIC(10)",
        "VARCHAR",
        "JSON",
        "JSONB",
        "DATE",
        "BYTEA",
    ] {
        let sql = format!(
            "CREATE TABLE p (x {shared_type});
             CREATE TABLE c (x {shared_type}) INHERITS (p);"
        );
        let parsed = parse(&sql);
        assert!(parsed.is_ok(), "`{shared_type}` redeclared as itself merges");
        assert_eq!(column_names(&parsed.expect("checked above"), "c"), ["x"]);
    }
}

#[test]
fn an_identity_spelled_the_other_way_is_withheld_just_the_same() {
    // `INHERITS` is PostgreSQL syntax, but the generic dialect accepts it
    // alongside the `IDENTITY(seed, step)` spelling other databases use. That
    // spelling is a different node, and it has to be withheld from the child
    // too, or the child would carry a sequence of its own.
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE par (counted INT IDENTITY(1,1), plain INT);
         CREATE TABLE chi (own INT) INHERITS (par);",
    )
    .expect("schema parses");

    assert_eq!(column_names(&database, "chi"), ["counted", "plain", "own"]);

    let child = database
        .table_by_target(TargetName::new("chi", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let counted = child
        .column("counted", &database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists");
    // The identity is gone but the `NOT NULL` it implies stays.
    assert!(!counted.is_nullable(&database).expect("column is in this database"));
    assert_eq!(child.primary_key_columns(&database).expect("in database").count(), 0);
}

#[test]
fn an_unbounded_length_compares_like_any_other() {
    // `VARCHAR(MAX)` carries its length as a marker rather than a number, so
    // it needs its own comparison or every redeclaration of it would look
    // like a conflict.
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE p (x VARCHAR(MAX));
         CREATE TABLE c (x VARCHAR(MAX)) INHERITS (p);",
    )
    .expect("the same unbounded length merges");
    assert_eq!(column_names(&database, "c"), ["x"]);

    assert!(matches!(
        ParserDB::parse::<GenericDialect>(
            "CREATE TABLE p (x VARCHAR(MAX));
             CREATE TABLE c (x VARCHAR(10)) INHERITS (p);"
        ),
        Err(Error::InheritedColumnTypeConflict { .. })
    ));
}

#[test]
fn a_partition_receives_the_roots_keys_and_an_inheritor_does_not() {
    // PostgreSQL enforces a root's primary key across every partition, so the
    // partition carries it. An `INHERITS` child may hold a row whose key
    // duplicates one of the parent's, so it carries none of it.
    //
    // Every key of a partitioned table has to contain its partitioning
    // columns, which is why the unique constraint here names `id` as well.
    let database = database(
        "CREATE TABLE target (id INT PRIMARY KEY);
         CREATE TABLE root (
             id INT PRIMARY KEY,
             ref_id INT REFERENCES target (id),
             code TEXT,
             UNIQUE (id, code)
         ) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (100);
         CREATE TABLE parent (
             id INT PRIMARY KEY,
             ref_id INT REFERENCES target (id),
             code TEXT,
             UNIQUE (id, code)
         );
         CREATE TABLE child (extra TEXT) INHERITS (parent);",
    );

    let part = database
        .table_by_target(TargetName::new("part", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(
        part.primary_key_columns(&database)
            .expect("in database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<_>>(),
        ["id"]
    );
    // The key's own index and the one behind `UNIQUE (id, code)`.
    assert_eq!(part.unique_indices(&database).expect("in database").count(), 2);
    assert_eq!(part.foreign_keys(&database).expect("in database").count(), 1);

    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(child.primary_key_columns(&database).expect("in database").count(), 0);
    assert_eq!(child.unique_indices(&database).expect("in database").count(), 0);
    assert_eq!(child.foreign_keys(&database).expect("in database").count(), 0);
}

#[test]
fn a_partition_receives_the_roots_identity() {
    // No accessor reports an identity, so this reads the declaration the
    // model holds. An `INHERITS` child must not carry one, or it would own a
    // sequence of its own, while a partition draws on the root's, which
    // PostgreSQL reports as `attidentity = 'a'` on the partition too.
    let database = database(
        "CREATE TABLE root (id INT GENERATED ALWAYS AS IDENTITY, payload TEXT)
             PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         CREATE TABLE parent (id INT GENERATED ALWAYS AS IDENTITY, payload TEXT);
         CREATE TABLE child (extra TEXT) INHERITS (parent);",
    );

    let carries_identity = |table_name: &str| {
        database
            .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .columns
            .iter()
            .find(|column| column.name.value == "id")
            .expect("column exists")
            .options
            .iter()
            .any(|option| {
                matches!(
                    option.option,
                    sqlparser::ast::ColumnOption::Generated { generation_expr: None, .. }
                )
            })
    };
    assert!(carries_identity("part"));
    assert!(!carries_identity("child"));

    // Either way the `NOT NULL` an identity implies is kept, which PostgreSQL
    // records as a not-null constraint on the partition as well.
    for table_name in ["part", "child"] {
        let table = database
            .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists");
        assert!(!column(table, "id", &database).is_nullable(&database).expect("in database"));
    }
}

#[test]
fn a_root_answers_its_partitions_and_a_parent_its_inheritors() {
    let database = database(
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE part_low PARTITION OF root FOR VALUES FROM (1) TO (9);
         CREATE TABLE part_high PARTITION OF root FOR VALUES FROM (9) TO (99);
         CREATE TABLE parent (id INT);
         CREATE TABLE child (extra TEXT) INHERITS (parent);",
    );

    let root = database
        .table_by_target(TargetName::new("root", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let partitions: Vec<_> = root
        .partitions(&database)
        .expect("in database")
        .map(|partition| partition.table_name().to_owned())
        .collect();
    assert_eq!(partitions, ["part_high", "part_low"]);
    assert_eq!(root.inheritors(&database).expect("in database").count(), 0);

    let parent = database
        .table_by_target(TargetName::new("parent", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert_eq!(parent.partitions(&database).expect("in database").count(), 0);
    assert_eq!(parent.inheritors(&database).expect("in database").count(), 1);
    assert!(!parent.is_partition(&database).expect("in database"));
}

#[test]
fn a_root_with_no_partitions_is_still_partitioned() {
    // PostgreSQL refuses a row into a partitioned table with no partition to
    // route it to, so the root holds no rows of its own from the moment it is
    // written. An empty list of partitions cannot say that.
    let database = database(
        "CREATE TABLE root (id INT) PARTITION BY HASH (id);
         CREATE TABLE plain (id INT);",
    );

    let root = database
        .table_by_target(TargetName::new("root", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(root.is_partitioned());
    assert_eq!(root.partitions(&database).expect("in database").count(), 0);

    let plain = database
        .table_by_target(TargetName::new("plain", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(!plain.is_partitioned());
}

#[test]
fn every_partitioning_strategy_is_read_whatever_its_spelling() {
    // PostgreSQL takes the strategy as a plain identifier and folds its case,
    // so a quoted or mixed-case spelling names the same one. All three
    // spellings of `RANGE` below record `partstrat = 'r'`.
    let database = database(
        "CREATE TABLE by_range (id INT) PARTITION BY RANGE (id);
         CREATE TABLE by_list (region TEXT) PARTITION BY LIST (region);
         CREATE TABLE by_hash (id INT) PARTITION BY HASH (id);
         CREATE TABLE mixed_case (id INT) PARTITION BY rAnGe (id);
         CREATE TABLE quoted (id INT) PARTITION BY \"RANGE\" (id);
         CREATE TABLE several (a INT, b INT) PARTITION BY RANGE (a, b);
         CREATE TABLE computed (a INT, b INT) PARTITION BY RANGE ((a + b));",
    );

    let strategy = |name: &str| {
        database
            .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .partition_strategy()
    };
    assert_eq!(strategy("by_range"), Some(PartitionStrategy::Range));
    assert_eq!(strategy("by_list"), Some(PartitionStrategy::List));
    assert_eq!(strategy("by_hash"), Some(PartitionStrategy::Hash));
    assert_eq!(strategy("mixed_case"), Some(PartitionStrategy::Range));
    assert_eq!(strategy("quoted"), Some(PartitionStrategy::Range));
    assert_eq!(strategy("several"), Some(PartitionStrategy::Range));
    assert_eq!(strategy("computed"), Some(PartitionStrategy::Range));
}

#[test]
fn a_partitioning_expression_of_another_dialect_is_not_a_strategy() {
    // BigQuery writes an ordinary table's storage layout into the very same
    // clause, and such a table does hold its own rows, so it is not a root.
    // The clause reaches this crate as a call, as a bare column, and as a
    // call to a qualified user function, and none of the three names a
    // strategy.
    let database = ParserDB::parse::<BigQueryDialect>(
        "CREATE TABLE by_call (ts TIMESTAMP, id INT64) PARTITION BY DATE(ts);
         CREATE TABLE by_column (ts TIMESTAMP) PARTITION BY _PARTITIONDATE;
         CREATE TABLE by_qualified_call (ts TIMESTAMP) PARTITION BY dataset.udf(ts);",
    )
    .expect("schema parses");

    for name in ["by_call", "by_column", "by_qualified_call"] {
        let table = database
            .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists");
        assert_eq!(table.partition_strategy(), None);
        assert!(!table.is_partitioned());
    }
}

#[test]
fn a_partition_can_itself_be_partitioned() {
    // PostgreSQL answers `relkind = 'p'` and `relispartition = true` for the
    // middle table, so belonging to a root and having partitions are two
    // separate facts about one table.
    //
    // Both partitioning columns have to sit in the key, since each level
    // partitions on one of them.
    let database = database(
        "CREATE TABLE top (id INT, region TEXT, PRIMARY KEY (id, region))
             PARTITION BY RANGE (id);
         CREATE TABLE middle PARTITION OF top
             FOR VALUES FROM (1) TO (9) PARTITION BY LIST (region);
         CREATE TABLE leaf PARTITION OF middle FOR VALUES IN ('eu');",
    );

    let middle = database
        .table_by_target(TargetName::new("middle", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(middle.is_partition(&database).expect("in database"));
    assert!(middle.is_partitioned());
    assert_eq!(middle.partition_strategy(), Some(PartitionStrategy::List));
    assert_eq!(root_name(&database, "middle").as_deref(), Some("top"));

    // The root's key reaches the whole chain, one level at a time.
    let leaf = database
        .table_by_target(TargetName::new("leaf", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    assert!(leaf.is_partition(&database).expect("in database"));
    assert!(!leaf.is_partitioned());
    assert_eq!(
        leaf.primary_key_columns(&database)
            .expect("in database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<_>>(),
        ["id", "region"]
    );
    assert_eq!(root_name(&database, "leaf").as_deref(), Some("middle"));
}
