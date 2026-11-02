//! Tests that a constraint added to or dropped from a parent reaches the
//! tables taking their shape from it.
//!
//! `ALTER TABLE parent ADD CONSTRAINT ...` used to record the constraint on
//! the named table alone, so a child kept answering as though the constraint
//! were never added, and a partition kept answering that it had no key at all.
//! Column changes already reached the tables below, so a single statement
//! could leave one half updated.
//!
//! Every expectation here was measured against PostgreSQL 18.4 rather than
//! read off the documentation. An `INHERITS` child receives a check and
//! nothing else, while a partition receives the root's keys and foreign keys
//! too. A key is also an index, and two indexes in one schema cannot share a
//! name, so a partition's copy of a key is given a name of its own while a
//! check and a foreign key keep the parent's.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, InheritedChange},
    prelude::*,
};
use sqlparser::{ast::TableConstraint, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn database(sql: &str) -> ParserDB {
    parse(sql).expect("schema parses")
}

/// The name of each table constraint the table holds, in the order it holds
/// them. No accessor reports a constraint's name, so this reads the
/// declaration the model keeps.
fn constraint_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
        .constraints
        .iter()
        .map(|constraint| {
            match constraint {
                TableConstraint::Check(check) => check.name.as_ref(),
                TableConstraint::PrimaryKey(key) => key.name.as_ref(),
                TableConstraint::Unique(unique) => unique.name.as_ref(),
                TableConstraint::ForeignKey(key) => key.name.as_ref(),
                _ => None,
            }
            .map_or_else(|| "<unnamed>".to_owned(), |name| name.value.clone())
        })
        .collect()
}

fn unique_index_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
        .unique_indices(database)
        .expect("table is in this database")
        .map(|index| IndexLike::name(index).map_or_else(|| "<unnamed>".to_owned(), str::to_owned))
        .collect()
}

fn primary_key_width(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
        .primary_key_columns(database)
        .expect("table is in this database")
        .count()
}

fn column_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| column.column_name().to_owned())
        .collect()
}

fn is_nullable(database: &ParserDB, table_name: &str, column_name: &str) -> bool {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists")
        .is_nullable(database)
        .expect("table is in this database")
}

#[test]
fn a_check_added_to_a_parent_reaches_every_descendant() {
    let database = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0);",
    );

    // The copy keeps the parent's name, which is how one constraint stays
    // recognisable across a whole hierarchy.
    for table_name in ["par", "chi", "gch"] {
        assert_eq!(constraint_names(&database, table_name), ["c1"]);
    }
}

#[test]
fn a_key_added_to_a_parent_stays_with_it() {
    // An `INHERITS` child may hold a row whose key duplicates one of the
    // parent's, so it receives no key, no unique constraint and no foreign
    // key, exactly as it receives none when it is created after them.
    let database = database(
        "CREATE TABLE tgt (id INT PRIMARY KEY);
         CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ADD CONSTRAINT p1 PRIMARY KEY (id);
         ALTER TABLE par ADD CONSTRAINT u1 UNIQUE (code);
         ALTER TABLE par ADD CONSTRAINT f1 FOREIGN KEY (id) REFERENCES tgt (id);",
    );

    assert_eq!(constraint_names(&database, "par"), ["p1", "u1", "f1"]);
    assert_eq!(constraint_names(&database, "chi"), Vec::<String>::new());
    assert_eq!(primary_key_width(&database, "chi"), 0);
    assert_eq!(unique_index_names(&database, "chi"), Vec::<String>::new());
}

#[test]
fn a_key_added_to_a_root_reaches_its_partitions_under_a_name_of_their_own() {
    // PostgreSQL enforces a root's keys across every partition, so the
    // partition carries them, and gives each copy a generated name because
    // the copy is a separate index. A check and a foreign key take no index
    // name, so those copies keep the root's.
    let database = database(
        "CREATE TABLE tgt (id INT PRIMARY KEY, code TEXT UNIQUE);
         CREATE TABLE root (id INT, code TEXT) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE root ADD CONSTRAINT c1 CHECK (id > 0);
         ALTER TABLE root ADD CONSTRAINT p1 PRIMARY KEY (id);
         ALTER TABLE root ADD CONSTRAINT u1 UNIQUE (id, code);
         ALTER TABLE root ADD CONSTRAINT f1 FOREIGN KEY (code) REFERENCES tgt (code);",
    );

    assert_eq!(constraint_names(&database, "root"), ["c1", "p1", "u1", "f1"]);
    assert_eq!(constraint_names(&database, "part"), ["c1", "part_pkey", "part_id_code_key", "f1"]);
    assert_eq!(primary_key_width(&database, "part"), 1);
    assert_eq!(unique_index_names(&database, "part"), ["part_pkey", "part_id_code_key"]);
    assert_eq!(
        database
            .table_by_target(TargetName::new("part", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("table exists")
            .foreign_keys(&database)
            .expect("in database")
            .count(),
        1
    );
}

#[test]
fn a_key_added_to_a_root_reaches_a_partition_of_a_partition() {
    // The walk goes one edge at a time, so each level builds its name from
    // its own table rather than from the root's.
    let database = database(
        "CREATE TABLE top (id INT) PARTITION BY RANGE (id);
         CREATE TABLE middle PARTITION OF top
             FOR VALUES FROM (1) TO (100) PARTITION BY RANGE (id);
         CREATE TABLE leaf PARTITION OF middle FOR VALUES FROM (1) TO (9);
         ALTER TABLE top ADD CONSTRAINT c9 CHECK (id > 0);
         ALTER TABLE top ADD CONSTRAINT p9 PRIMARY KEY (id);",
    );

    assert_eq!(constraint_names(&database, "top"), ["c9", "p9"]);
    assert_eq!(constraint_names(&database, "middle"), ["c9", "middle_pkey"]);
    assert_eq!(constraint_names(&database, "leaf"), ["c9", "leaf_pkey"]);
    assert_eq!(primary_key_width(&database, "leaf"), 1);
}

#[test]
fn a_partitioned_root_may_name_its_key() {
    // A named key on a partitioned root used to make the whole schema
    // unreadable, because the copy kept the root's name and collided with it
    // in the relation pool. Every spelling of a named key is covered, since
    // each reaches the pool by a different route.
    for sql in [
        "CREATE TABLE root (id INT, CONSTRAINT p1 PRIMARY KEY (id)) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);",
        "CREATE TABLE root (id INT CONSTRAINT p1 PRIMARY KEY) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);",
    ] {
        let database = database(sql);
        assert_eq!(unique_index_names(&database, "root"), ["p1"]);
        assert_eq!(unique_index_names(&database, "part"), ["part_pkey"]);
        assert_eq!(primary_key_width(&database, "part"), 1);
    }

    let database = database(
        "CREATE TABLE root (id INT, code TEXT, CONSTRAINT u1 UNIQUE (id, code))
             PARTITION BY RANGE (id);
         CREATE TABLE part_a PARTITION OF root FOR VALUES FROM (1) TO (9);
         CREATE TABLE part_b PARTITION OF root FOR VALUES FROM (9) TO (99);",
    );
    assert_eq!(unique_index_names(&database, "root"), ["u1"]);
    assert_eq!(unique_index_names(&database, "part_a"), ["part_a_id_code_key"]);
    assert_eq!(unique_index_names(&database, "part_b"), ["part_b_id_code_key"]);
}

#[test]
fn a_generated_name_steps_aside_for_one_already_taken() {
    // PostgreSQL appends a counter to a name it builds when the schema
    // already holds it, having tried the bare name first.
    let database = database(
        "CREATE TABLE squat (x INT);
         CREATE INDEX part_pkey ON squat (x);
         CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE root ADD CONSTRAINT p1 PRIMARY KEY (id);",
    );

    assert_eq!(unique_index_names(&database, "part"), ["part_pkey1"]);
}

#[test]
fn a_generated_name_follows_the_quoting_of_the_table() {
    // A name built off a quoted table keeps the case, so it stays reachable
    // only under the spelling that preserves it.
    let database = database(
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE \"MyPart\" PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE root ADD CONSTRAINT p1 PRIMARY KEY (id);",
    );

    assert_eq!(unique_index_names(&database, "\"MyPart\""), ["MyPart_pkey"]);
}

#[test]
fn a_constraint_dropped_from_a_parent_leaves_every_descendant() {
    let database = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0);
         ALTER TABLE par DROP CONSTRAINT c1;",
    );

    for table_name in ["par", "chi", "gch"] {
        assert_eq!(constraint_names(&database, table_name), Vec::<String>::new());
    }
}

#[test]
fn a_table_that_declared_the_constraint_itself_keeps_it() {
    // PostgreSQL merges the parent's constraint into the one the child
    // already declared and records the result as the child's own, so the
    // parent dropping its copy leaves the child's standing.
    let database = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE chi ADD CONSTRAINT same CHECK (id > 0);
         ALTER TABLE par ADD CONSTRAINT same CHECK (id > 0);
         ALTER TABLE par DROP CONSTRAINT same;",
    );

    assert_eq!(constraint_names(&database, "chi"), ["same"]);
    assert_eq!(constraint_names(&database, "par"), Vec::<String>::new());
}

#[test]
fn a_constraint_two_parents_pass_down_survives_one_of_them_dropping_it() {
    // How many parents still pass a constraint down is read from them rather
    // than counted, so the child keeps its copy until the last one lets go.
    let sql = "CREATE TABLE left_par (id INT);
               CREATE TABLE right_par (id INT);
               CREATE TABLE chi () INHERITS (left_par, right_par);
               ALTER TABLE left_par ADD CONSTRAINT dual CHECK (id > 0);
               ALTER TABLE right_par ADD CONSTRAINT dual CHECK (id > 0);";

    let one_dropped = database(&format!("{sql} ALTER TABLE left_par DROP CONSTRAINT dual;"));
    assert_eq!(constraint_names(&one_dropped, "chi"), ["dual"]);

    let both_dropped = database(&format!(
        "{sql}
         ALTER TABLE left_par DROP CONSTRAINT dual;
         ALTER TABLE right_par DROP CONSTRAINT dual;"
    ));
    assert_eq!(constraint_names(&both_dropped, "chi"), Vec::<String>::new());
}

#[test]
fn a_table_cannot_drop_a_constraint_it_receives() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT, CONSTRAINT c1 CHECK (id > 0));
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE chi DROP CONSTRAINT c1;"
        ),
        Err(Error::InheritedConstraintNotDroppable { ref table_name, ref constraint_name })
            if table_name == "chi" && constraint_name == "c1"
    ));

    // Refused even where the child declared an equivalent constraint of its
    // own, which is what PostgreSQL refuses too.
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE chi ADD CONSTRAINT same CHECK (id > 0);
             ALTER TABLE par ADD CONSTRAINT same CHECK (id > 0);
             ALTER TABLE chi DROP CONSTRAINT same;"
        ),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));
}

#[test]
fn a_table_may_drop_a_constraint_of_its_own() {
    let database = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE chi ADD CONSTRAINT own CHECK (id < 5);
         ALTER TABLE chi DROP CONSTRAINT own;",
    );

    assert_eq!(constraint_names(&database, "chi"), Vec::<String>::new());
}

#[test]
fn only_refuses_to_add_a_constraint_when_tables_inherit() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE ONLY par ADD CONSTRAINT c1 CHECK (id > 0);"
        ),
        Err(Error::OnlyRefusedWithChildren {
            ref table_name,
            change: InheritedChange::AddConstraint,
        }) if table_name == "par"
    ));

    // With nothing below it the keyword changes nothing.
    let database = database(
        "CREATE TABLE solo (id INT);
         ALTER TABLE ONLY solo ADD CONSTRAINT s1 CHECK (id > 0);",
    );
    assert_eq!(constraint_names(&database, "solo"), ["s1"]);
}

#[test]
fn only_drops_a_constraint_from_the_named_table_and_leaves_the_copies() {
    // PostgreSQL grants `ONLY` here, because a table below can hold the
    // constraint as its own once nothing passes it down, which is exactly
    // what it then does.
    let detached = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0);
         ALTER TABLE ONLY par DROP CONSTRAINT c1;",
    );

    assert_eq!(constraint_names(&detached, "par"), Vec::<String>::new());
    assert_eq!(constraint_names(&detached, "chi"), ["c1"]);
    // The grandchild still receives its copy from the child, so it keeps it
    // as one it received rather than one of its own.
    assert_eq!(constraint_names(&detached, "gch"), ["c1"]);
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi () INHERITS (par);
             CREATE TABLE gch () INHERITS (chi);
             ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0);
             ALTER TABLE ONLY par DROP CONSTRAINT c1;
             ALTER TABLE gch DROP CONSTRAINT c1;"
        ),
        Err(Error::InheritedConstraintNotDroppable { .. })
    ));

    // The child may now drop what has become its own, and that reaches the
    // grandchild the ordinary way.
    let dropped = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0);
         ALTER TABLE ONLY par DROP CONSTRAINT c1;
         ALTER TABLE chi DROP CONSTRAINT c1;",
    );
    assert_eq!(constraint_names(&dropped, "chi"), Vec::<String>::new());
    assert_eq!(constraint_names(&dropped, "gch"), Vec::<String>::new());
}

#[test]
fn only_refuses_to_add_or_rename_a_column_when_tables_inherit() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE ONLY par ADD COLUMN extra INT;"
        ),
        Err(Error::OnlyRefusedWithChildren { change: InheritedChange::AddColumn, .. })
    ));

    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT, code TEXT);
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE ONLY par RENAME COLUMN code TO label;"
        ),
        Err(Error::OnlyRefusedWithChildren { change: InheritedChange::RenameColumn, .. })
    ));
}

#[test]
fn only_drops_a_column_from_the_named_table_and_leaves_the_copies() {
    let detached = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE ONLY par DROP COLUMN code;",
    );

    assert_eq!(column_names(&detached, "par"), ["id"]);
    assert_eq!(column_names(&detached, "chi"), ["id", "code"]);
    assert_eq!(column_names(&detached, "gch"), ["id", "code"]);

    // The child holds the column as its own now, so it may drop it, and that
    // takes the grandchild's along.
    let dropped = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE ONLY par DROP COLUMN code;
         ALTER TABLE chi DROP COLUMN code;",
    );
    assert_eq!(column_names(&dropped, "chi"), ["id"]);
    assert_eq!(column_names(&dropped, "gch"), ["id"]);
}

#[test]
fn only_alters_a_column_of_the_named_table_alone() {
    // PostgreSQL grants `ONLY` here without complaint, because the tables
    // below still agree on what the column is, only on what it may hold.
    let alone = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par ALTER COLUMN code SET NOT NULL;",
    );
    assert!(!is_nullable(&alone, "par", "code"));
    assert!(is_nullable(&alone, "chi", "code"));

    let reaching = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ALTER COLUMN code SET NOT NULL;",
    );
    assert!(!is_nullable(&reaching, "chi", "code"));
}

#[test]
fn a_renamed_column_carries_the_check_and_a_later_drop_still_reaches_it() {
    // A rename rewrites the check in place on both tables, and the record of
    // where the child's copy came from has to follow that rewrite or the
    // parent's drop would no longer recognise it.
    let renamed = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (code <> '');
         ALTER TABLE par RENAME COLUMN code TO label;",
    );
    assert_eq!(column_names(&renamed, "chi"), ["id", "label"]);
    assert_eq!(constraint_names(&renamed, "chi"), ["c1"]);

    let dropped = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (code <> '');
         ALTER TABLE par RENAME COLUMN code TO label;
         ALTER TABLE par DROP CONSTRAINT c1;",
    );
    assert_eq!(constraint_names(&dropped, "chi"), Vec::<String>::new());
}

#[test]
fn dropping_a_column_takes_the_constraints_on_it_from_every_descendant() {
    let database = database(
        "CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (code <> '');
         ALTER TABLE par ADD CONSTRAINT c2 CHECK (id > 0);
         ALTER TABLE par DROP COLUMN code;",
    );

    assert_eq!(column_names(&database, "chi"), ["id"]);
    assert_eq!(constraint_names(&database, "par"), ["c2"]);
    assert_eq!(constraint_names(&database, "chi"), ["c2"]);
}

#[test]
fn a_constraint_the_table_does_not_hold_is_reported_unless_excused() {
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             ALTER TABLE par DROP CONSTRAINT absent;"
        ),
        Err(Error::DropConstraintNotFound { ref constraint_name, .. })
            if constraint_name == "absent"
    ));

    let database = database(
        "CREATE TABLE par (id INT);
         ALTER TABLE par DROP CONSTRAINT IF EXISTS absent;",
    );
    assert_eq!(constraint_names(&database, "par"), Vec::<String>::new());
}

/// The number of check constraints the table answers, counting the ones
/// written on a column along with the ones in the constraint list.
fn check_count(database: &ParserDB, table_name: &str) -> usize {
    database
        .table_by_target(
            TargetName::parse(table_name).expect("one identifier"),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("table exists")
        .check_constraints(database)
        .expect("table is in this database")
        .count()
}

#[test]
fn a_no_inherit_check_stays_with_the_table_declaring_it() {
    // Measured on PostgreSQL 18.4: whether the child exists before or after
    // the statement, and whichever spelling writes the check, it never
    // travels.
    let after = database(
        "CREATE TABLE par (id INT);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0) NO INHERIT;
         CREATE TABLE chi () INHERITS (par);",
    );
    assert_eq!(constraint_names(&after, "par"), ["c1"]);
    assert_eq!(constraint_names(&after, "chi"), Vec::<String>::new());

    let before = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch () INHERITS (chi);
         ALTER TABLE par ADD CONSTRAINT c1 CHECK (id > 0) NO INHERIT;",
    );
    assert_eq!(constraint_names(&before, "par"), ["c1"]);
    assert_eq!(constraint_names(&before, "chi"), Vec::<String>::new());
    assert_eq!(constraint_names(&before, "gch"), Vec::<String>::new());

    let declared = database(
        "CREATE TABLE par (id INT, CONSTRAINT c1 CHECK (id > 0) NO INHERIT);
         CREATE TABLE chi () INHERITS (par);",
    );
    assert_eq!(constraint_names(&declared, "par"), ["c1"]);
    assert_eq!(constraint_names(&declared, "chi"), Vec::<String>::new());
}

#[test]
fn a_no_inherit_check_on_a_column_stays_while_the_column_travels() {
    // Measured on PostgreSQL 18.4: the child receives the column bare, both
    // when the column is inherited at creation and when it arrives later.
    let declared = database(
        "CREATE TABLE par (id INT CHECK (id > 0) NO INHERIT);
         CREATE TABLE chi () INHERITS (par);",
    );
    assert_eq!(column_names(&declared, "chi"), ["id"]);
    assert_eq!(check_count(&declared, "par"), 1);
    assert_eq!(check_count(&declared, "chi"), 0);

    let added = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE par ADD COLUMN w INT CHECK (w > 0) NO INHERIT;",
    );
    assert_eq!(column_names(&added, "chi"), ["id", "w"]);
    assert_eq!(check_count(&added, "par"), 1);
    assert_eq!(check_count(&added, "chi"), 0);
}

#[test]
fn only_is_granted_for_a_no_inherit_check() {
    // PostgreSQL refuses `ONLY` for a check that would have to reach the
    // tables below, and grants it for one that stays put either way.
    let database = database(
        "CREATE TABLE par (id INT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par ADD CONSTRAINT c1 CHECK (id > 0) NO INHERIT;",
    );
    assert_eq!(constraint_names(&database, "par"), ["c1"]);
    assert_eq!(constraint_names(&database, "chi"), Vec::<String>::new());
}

#[test]
fn a_partitioned_table_refuses_a_no_inherit_check() {
    // PostgreSQL enforces every constraint of a partitioned table on its
    // partitions, so each spelling that would keep a check from them is
    // refused: added later, declared on the table, written on a column, and
    // riding a column added later.
    for sql in [
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         ALTER TABLE root ADD CONSTRAINT c1 CHECK (id > 0) NO INHERIT;",
        "CREATE TABLE root (id INT, CONSTRAINT c1 CHECK (id > 0) NO INHERIT)
             PARTITION BY RANGE (id);",
        "CREATE TABLE root (id INT CHECK (id > 0) NO INHERIT) PARTITION BY RANGE (id);",
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         ALTER TABLE root ADD COLUMN w INT CHECK (w > 0) NO INHERIT;",
    ] {
        assert!(matches!(
            parse(sql),
            Err(Error::NoInheritCheckOnPartitionedTable { ref table_name })
                if table_name == "root"
        ));
    }

    // A partition itself may hold one, because nothing partitions it further.
    let database = database(
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE part ADD CONSTRAINT cp CHECK (id < 100) NO INHERIT;",
    );
    assert_eq!(constraint_names(&database, "part"), ["cp"]);
}

#[test]
fn a_child_constraint_unmergeable_with_an_inherited_one_is_refused() {
    // Measured on PostgreSQL 18.4: an arriving check merges into a child's
    // own of the same name only when the expressions match and the child's
    // is not `NO INHERIT`. Anything else sharing the name is refused, at
    // creation and through `ALTER TABLE` alike.
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT, CONSTRAINT c6 CHECK (id > 0));
             CREATE TABLE chi (id INT, CONSTRAINT c6 CHECK (id > 0) NO INHERIT) INHERITS (par);"
        ),
        Err(Error::InheritedConstraintConflict { ref table_name, ref constraint_name })
            if table_name == "chi" && constraint_name == "c6"
    ));

    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi (CONSTRAINT lc CHECK (id > 0) NO INHERIT) INHERITS (par);
             ALTER TABLE par ADD CONSTRAINT lc CHECK (id > 0);"
        ),
        Err(Error::InheritedConstraintConflict { ref table_name, ref constraint_name })
            if table_name == "chi" && constraint_name == "lc"
    ));

    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi (CONSTRAINT kc CHECK (id > 0)) INHERITS (par);
             ALTER TABLE par ADD CONSTRAINT kc CHECK (id < 5);"
        ),
        Err(Error::InheritedConstraintConflict { ref constraint_name, .. })
            if constraint_name == "kc"
    ));

    // Two parents passing the same name merge when the checks agree and are
    // refused when they do not.
    assert!(matches!(
        parse(
            "CREATE TABLE qa (a INT, CONSTRAINT qc CHECK (a > 0));
             CREATE TABLE qb (a INT, CONSTRAINT qc CHECK (a < 5));
             CREATE TABLE qchild () INHERITS (qa, qb);"
        ),
        Err(Error::InheritedConstraintConflict { ref constraint_name, .. })
            if constraint_name == "qc"
    ));
    let agreeing = database(
        "CREATE TABLE ra (a INT, CONSTRAINT rc CHECK (a > 0));
         CREATE TABLE rb (a INT, CONSTRAINT rc CHECK (a > 0));
         CREATE TABLE rchild () INHERITS (ra, rb);",
    );
    assert_eq!(constraint_names(&agreeing, "rchild"), ["rc"]);
}

#[test]
fn a_parent_no_inherit_check_leaves_the_name_free_for_the_child() {
    // Nothing arrives from the parent, so the child's own constraint under
    // the same name stands, whichever flavour the child writes.
    for child_constraint in
        ["CONSTRAINT c7 CHECK (id > 0) NO INHERIT", "CONSTRAINT c7 CHECK (id > 0)"]
    {
        let database = database(&format!(
            "CREATE TABLE par (id INT, CONSTRAINT c7 CHECK (id > 0) NO INHERIT);
             CREATE TABLE chi (id INT, {child_constraint}) INHERITS (par);"
        ));
        assert_eq!(constraint_names(&database, "par"), ["c7"]);
        assert_eq!(constraint_names(&database, "chi"), ["c7"]);
    }
}

#[test]
fn the_no_inherit_flag_is_read_from_the_check() {
    let database = database(
        "CREATE TABLE par (id INT, CONSTRAINT own CHECK (id > 0) NO INHERIT, CHECK (id < 9));",
    );
    let table = database
        .table_by_target(TargetName::new("par", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists");
    let flags: Vec<bool> = table
        .check_constraints(&database)
        .expect("table is in this database")
        .map(sql_traits::traits::CheckConstraintLike::no_inherit)
        .collect();
    assert_eq!(flags, [true, false]);
}

#[test]
fn only_is_granted_for_a_unique_or_foreign_key_where_tables_inherit() {
    // Measured on PostgreSQL 18.4: neither reaches an `INHERITS` child even
    // without `ONLY`, so there is nothing to withhold and the statement is
    // granted.
    let database = database(
        "CREATE TABLE tgt (id INT PRIMARY KEY);
         CREATE TABLE par (id INT, code TEXT);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par ADD CONSTRAINT u1 UNIQUE (code);
         ALTER TABLE ONLY par ADD CONSTRAINT f1 FOREIGN KEY (id) REFERENCES tgt (id);",
    );
    assert_eq!(constraint_names(&database, "par"), ["u1", "f1"]);
    assert_eq!(constraint_names(&database, "chi"), Vec::<String>::new());
}

#[test]
fn only_leaves_a_unique_constraint_off_existing_partitions() {
    // Measured on PostgreSQL 18.4: the partition standing when the statement
    // runs receives nothing, while one created afterwards receives its copy
    // the ordinary way, under a name of its own.
    let database = database(
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE before_it PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE ONLY root ADD CONSTRAINT u1 UNIQUE (id);
         CREATE TABLE after_it PARTITION OF root FOR VALUES FROM (9) TO (99);",
    );
    assert_eq!(constraint_names(&database, "root"), ["u1"]);
    assert_eq!(constraint_names(&database, "before_it"), Vec::<String>::new());
    assert_eq!(constraint_names(&database, "after_it"), ["after_it_id_key"]);
}

#[test]
fn only_refuses_a_foreign_key_on_a_partitioned_table() {
    // Measured on PostgreSQL 18.4: refused whether or not any partition
    // exists yet.
    assert!(matches!(
        parse(
            "CREATE TABLE tgt (id INT PRIMARY KEY);
             CREATE TABLE root (id INT) PARTITION BY RANGE (id);
             ALTER TABLE ONLY root ADD CONSTRAINT f1 FOREIGN KEY (id) REFERENCES tgt (id);"
        ),
        Err(Error::OnlyForeignKeyOnPartitionedTable { ref table_name }) if table_name == "root"
    ));
}

#[test]
fn only_grants_a_primary_key_where_every_table_below_requires_the_columns() {
    // Measured on PostgreSQL 18.4: the key stays with the named table, and
    // the `NOT NULL` it would imply has to hold below already.
    let inherits = database(
        "CREATE TABLE par (id INT NOT NULL);
         CREATE TABLE chi () INHERITS (par);
         ALTER TABLE ONLY par ADD CONSTRAINT p1 PRIMARY KEY (id);",
    );
    assert_eq!(constraint_names(&inherits, "par"), ["p1"]);
    assert_eq!(constraint_names(&inherits, "chi"), Vec::<String>::new());
    assert_eq!(primary_key_width(&inherits, "chi"), 0);

    let partitioned = database(
        "CREATE TABLE root (id INT NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
         ALTER TABLE ONLY root ADD CONSTRAINT p1 PRIMARY KEY (id);",
    );
    assert_eq!(constraint_names(&partitioned, "root"), ["p1"]);
    assert_eq!(constraint_names(&partitioned, "part"), Vec::<String>::new());
    assert_eq!(primary_key_width(&partitioned, "part"), 0);

    // A grandchild redeclaring the column keeps the requirement the union
    // with the parent's declaration gives it, so it stands in nothing's way.
    let redeclared = database(
        "CREATE TABLE par (id INT NOT NULL);
         CREATE TABLE chi () INHERITS (par);
         CREATE TABLE gch (id INT) INHERITS (chi);
         ALTER TABLE ONLY par ADD CONSTRAINT p1 PRIMARY KEY (id);",
    );
    assert_eq!(constraint_names(&redeclared, "par"), ["p1"]);

    // A table below whose keyed column may still hold nothing is refused.
    assert!(matches!(
        parse(
            "CREATE TABLE par (id INT);
             CREATE TABLE chi () INHERITS (par);
             ALTER TABLE ONLY par ADD CONSTRAINT p1 PRIMARY KEY (id);"
        ),
        Err(Error::OnlyPrimaryKeyOnNullableColumn { ref table_name, ref column_name })
            if table_name == "chi" && column_name == "id"
    ));
    assert!(matches!(
        parse(
            "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
             CREATE TABLE part PARTITION OF root FOR VALUES FROM (1) TO (9);
             ALTER TABLE ONLY root ADD CONSTRAINT p1 PRIMARY KEY (id);"
        ),
        Err(Error::OnlyPrimaryKeyOnNullableColumn { ref table_name, .. })
            if table_name == "part"
    ));
}
