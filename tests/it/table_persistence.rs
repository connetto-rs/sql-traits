//! Tests that a table's persistence follows every statement that sets it.
//!
//! Every rule asserted here was measured against PostgreSQL 18.6 in Docker. A
//! table starts permanent or unlogged as `CREATE TABLE` wrote it, and `ALTER
//! TABLE ... SET UNLOGGED` and `SET LOGGED` move it between the two. A
//! temporary or partitioned table never changes, and a permanent table never
//! references an unlogged one through a foreign key.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{
    errors::{Error, FixedPersistence},
    prelude::*,
};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn db(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.resolve_target_table(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the name resolves")
}

fn unlogged_after(sql: &str, name: &str) -> bool {
    table(&db(sql), name).is_unlogged()
}

fn refusal(sql: &str) -> Error {
    match parse(sql) {
        Ok(_) => panic!("{sql} was accepted"),
        Err(error) => error,
    }
}

#[test]
fn persistence_starts_where_create_table_left_it() {
    assert!(unlogged_after("CREATE UNLOGGED TABLE t (id INT);", "t"));
    assert!(!unlogged_after("CREATE TABLE t (id INT);", "t"));
    assert!(!unlogged_after("CREATE TEMP TABLE t (id INT);", "t"));
}

#[test]
fn a_reference_to_a_table_reports_its_persistence() {
    let database = db("CREATE UNLOGGED TABLE cache (id INT); CREATE TABLE events (id INT);");
    for (name, unlogged) in [("cache", true), ("events", false)] {
        let table = table(&database, name);
        assert_eq!(<&CreateTable as TableLike>::is_unlogged(&table), unlogged, "{name}");
    }
}

#[test]
fn each_alter_table_moves_persistence_to_the_state_it_names() {
    let staging = "CREATE TABLE staging (id INT); ALTER TABLE staging SET UNLOGGED;";
    assert!(unlogged_after(staging, "staging"));
    assert!(!unlogged_after(&format!("{staging} ALTER TABLE staging SET LOGGED;"), "staging"));
    assert!(!unlogged_after("CREATE UNLOGGED TABLE t (id INT); ALTER TABLE t SET LOGGED;", "t"));
    assert!(unlogged_after("CREATE UNLOGGED TABLE t (id INT); ALTER TABLE t SET UNLOGGED;", "t"));
    assert!(!unlogged_after("CREATE TABLE t (id INT); ALTER TABLE ONLY t SET LOGGED;", "t"));
}

#[test]
fn persistence_survives_the_statements_that_rebuild_a_table() {
    let database = db("CREATE TABLE staging (id INT);
         ALTER TABLE staging SET UNLOGGED;
         ALTER TABLE staging ADD COLUMN note TEXT;
         ALTER TABLE staging RENAME COLUMN note TO remark;
         ALTER TABLE staging ADD CONSTRAINT positive CHECK (id > 0);
         ALTER TABLE staging RENAME TO landing;");
    assert!(table(&database, "landing").is_unlogged());
}

#[test]
fn a_table_takes_no_persistence_from_the_tables_it_derives_from() {
    let parents = "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE plain (id INT PRIMARY KEY);
         CREATE UNLOGGED TABLE base (id INT PRIMARY KEY);";
    assert!(unlogged_after(
        &format!(
            "{parents} CREATE UNLOGGED TABLE part PARTITION OF root FOR VALUES FROM (0) TO (10);"
        ),
        "part"
    ));
    assert!(!unlogged_after(
        &format!("{parents} CREATE TABLE part PARTITION OF root FOR VALUES FROM (0) TO (10);"),
        "part"
    ));
    assert!(!unlogged_after(&format!("{parents} CREATE TABLE child () INHERITS (base);"), "child"));
    assert!(unlogged_after(
        &format!("{parents} CREATE UNLOGGED TABLE child () INHERITS (plain);"),
        "child"
    ));
    assert!(!unlogged_after(&format!("{parents} CREATE TABLE copy (LIKE base);"), "copy"));
}

#[test]
fn an_unlogged_table_cannot_be_temporary() {
    for sql in [
        "CREATE TEMP UNLOGGED TABLE t (id INT);",
        "CREATE UNLOGGED TABLE pg_temp.t (id INT);",
        // Refused before `IF NOT EXISTS` is consulted.
        "CREATE TEMP TABLE t (id INT); CREATE UNLOGGED TABLE IF NOT EXISTS pg_temp.t (id INT);",
    ] {
        let error = refusal(sql);
        assert!(
            matches!(&error, Error::UnloggedTemporaryTable { table_name } if table_name == "t"),
            "{sql} reported {error:?}"
        );
    }
}

#[test]
fn a_partitioned_table_cannot_be_created_unlogged() {
    let error = refusal("CREATE UNLOGGED TABLE p (id INT) PARTITION BY RANGE (id);");
    assert!(
        matches!(&error, Error::UnloggedPartitionedTable { table_name } if table_name == "p"),
        "got {error:?}"
    );
    // `IF NOT EXISTS` skips the statement before the refusal is reached.
    assert!(!unlogged_after(
        "CREATE TABLE p (id INT); CREATE UNLOGGED TABLE IF NOT EXISTS p (id INT) PARTITION BY RANGE (id);",
        "p"
    ));
}

#[test]
fn a_temporary_or_partitioned_table_keeps_its_persistence() {
    for (create, reason) in [
        ("CREATE TEMP TABLE t (id INT);", FixedPersistence::Temporary),
        ("CREATE TABLE t (id INT) PARTITION BY RANGE (id);", FixedPersistence::Partitioned),
    ] {
        // Refused even where the table already holds the named state.
        for change in ["SET UNLOGGED", "SET LOGGED"] {
            let sql = format!("{create} ALTER TABLE t {change};");
            let error = refusal(&sql);
            assert!(
                matches!(
                    &error,
                    Error::PersistenceNotChangeable { table_name, reason: found }
                        if table_name == "t" && *found == reason
                ),
                "{sql} reported {error:?}"
            );
        }
    }
}

#[test]
fn one_statement_changes_persistence_at_most_once() {
    for sql in [
        "CREATE TABLE t (id INT); ALTER TABLE t SET UNLOGGED, SET LOGGED;",
        "CREATE TABLE t (id INT); ALTER TABLE t SET UNLOGGED, SET UNLOGGED;",
        "CREATE UNLOGGED TABLE t (id INT); ALTER TABLE t SET LOGGED, SET LOGGED;",
    ] {
        let error = refusal(sql);
        assert!(
            matches!(&error, Error::PersistenceChangedTwice { table_name } if table_name == "t"),
            "{sql} reported {error:?}"
        );
    }
    // A clause naming the state the table already holds changes nothing, so
    // the clause after it is the first change.
    assert!(unlogged_after(
        "CREATE TABLE t (id INT); ALTER TABLE t SET LOGGED, SET UNLOGGED;",
        "t"
    ));
    assert!(!unlogged_after(
        "CREATE UNLOGGED TABLE t (id INT); ALTER TABLE t SET UNLOGGED, SET LOGGED;",
        "t"
    ));
    assert!(unlogged_after(
        "CREATE UNLOGGED TABLE t (id INT); ALTER TABLE t SET UNLOGGED, SET UNLOGGED;",
        "t"
    ));
}

#[test]
fn an_absent_table_is_reported_unless_the_statement_excuses_it() {
    assert!(matches!(
        refusal("ALTER TABLE missing SET UNLOGGED;"),
        Error::AlterTableNotFound { table_name } if table_name == "missing"
    ));
    db("ALTER TABLE IF EXISTS missing SET UNLOGGED, SET LOGGED;");
}

#[test]
fn a_view_has_no_persistence_to_change() {
    for sql in [
        "CREATE VIEW v AS SELECT 1 AS x; ALTER TABLE v SET UNLOGGED;",
        "CREATE MATERIALIZED VIEW v AS SELECT 1 AS x; ALTER TABLE v SET LOGGED;",
    ] {
        let error = refusal(sql);
        assert!(
            matches!(&error, Error::AlterActionUnsupportedOnRelation { .. }),
            "{sql} reported {error:?}"
        );
    }
}

fn references_unlogged(error: &Error, host: &str, referenced: &str) -> bool {
    matches!(
        error,
        Error::ForeignKeyReferencesUnloggedTable { host_table, referenced_table }
            if host_table == host && referenced_table == referenced
    )
}

const UNLOGGED_PARENT: &str = "CREATE UNLOGGED TABLE parent (id INT PRIMARY KEY);";
const LOGGED_PARENT: &str = "CREATE TABLE parent (id INT PRIMARY KEY);";

#[test]
fn a_permanent_table_cannot_reference_an_unlogged_one() {
    for declaration in [
        "CREATE TABLE child (id INT REFERENCES parent);",
        "CREATE TABLE child (id INT, FOREIGN KEY (id) REFERENCES parent (id));",
        "CREATE TABLE child (id INT); ALTER TABLE child ADD FOREIGN KEY (id) REFERENCES parent;",
        "CREATE TABLE child (id INT); ALTER TABLE child ADD COLUMN p INT REFERENCES parent;",
    ] {
        let sql = format!("{UNLOGGED_PARENT} {declaration}");
        let error = refusal(&sql);
        assert!(references_unlogged(&error, "child", "parent"), "{sql} reported {error:?}");
    }
    for (parent, child) in [
        (LOGGED_PARENT, "CREATE UNLOGGED TABLE child (id INT REFERENCES parent);"),
        (UNLOGGED_PARENT, "CREATE UNLOGGED TABLE child (id INT REFERENCES parent);"),
        ("", "CREATE TABLE node (id INT PRIMARY KEY, up INT REFERENCES node);"),
        ("", "CREATE UNLOGGED TABLE node (id INT PRIMARY KEY, up INT REFERENCES node);"),
    ] {
        db(&format!("{parent} {child}"));
    }
}

#[test]
fn set_logged_is_refused_while_the_table_references_an_unlogged_one() {
    let sql = format!(
        "{UNLOGGED_PARENT} CREATE UNLOGGED TABLE child (id INT REFERENCES parent);
         ALTER TABLE child SET LOGGED;"
    );
    assert!(references_unlogged(&refusal(&sql), "child", "parent"));

    // A reference to itself moves with it.
    assert!(!unlogged_after(
        "CREATE UNLOGGED TABLE node (id INT PRIMARY KEY, up INT REFERENCES node);
         ALTER TABLE node SET LOGGED;",
        "node"
    ));
    // A table already permanent changes nothing, so nothing is checked.
    db(&format!(
        "{LOGGED_PARENT} CREATE TABLE child (id INT REFERENCES parent); ALTER TABLE child SET LOGGED;"
    ));
}

#[test]
fn set_unlogged_is_refused_while_a_permanent_table_references_it() {
    let sql = format!(
        "{LOGGED_PARENT} CREATE TABLE child (id INT REFERENCES parent);
         ALTER TABLE parent SET UNLOGGED;"
    );
    assert!(references_unlogged(&refusal(&sql), "child", "parent"));
    // A permanent table referencing some other table does not hold it back.
    assert!(unlogged_after(
        &format!(
            "{LOGGED_PARENT} CREATE TABLE other (id INT PRIMARY KEY);
             CREATE TABLE child (id INT REFERENCES other);
             ALTER TABLE parent SET UNLOGGED;"
        ),
        "parent"
    ));

    assert!(unlogged_after(
        &format!(
            "{LOGGED_PARENT} CREATE UNLOGGED TABLE child (id INT REFERENCES parent);
             ALTER TABLE parent SET UNLOGGED;"
        ),
        "parent"
    ));
    assert!(unlogged_after(
        "CREATE TABLE node (id INT PRIMARY KEY, up INT REFERENCES node);
         ALTER TABLE node SET UNLOGGED;",
        "node"
    ));
    // The referencing table may follow once the referenced one has.
    let database = db(&format!(
        "{LOGGED_PARENT} CREATE TABLE child (id INT REFERENCES parent);
         ALTER TABLE child SET UNLOGGED;
         ALTER TABLE parent SET UNLOGGED;"
    ));
    assert!(table(&database, "child").is_unlogged());
    assert!(table(&database, "parent").is_unlogged());
}

#[test]
fn a_foreign_key_added_beside_a_persistence_change_is_held_to_both_states() {
    // PostgreSQL checks the added key against the persistence the table had
    // when the statement began.
    for clause in ["ADD FOREIGN KEY (id) REFERENCES parent", "ADD COLUMN p INT REFERENCES parent"] {
        let sql = format!(
            "{UNLOGGED_PARENT} CREATE TABLE child (id INT);
             ALTER TABLE child SET UNLOGGED, {clause};"
        );
        assert!(
            references_unlogged(&refusal(&sql), "child", "parent"),
            "{sql} was refused for another reason"
        );
    }
    // A key referencing the table itself shared its persistence then too.
    assert!(unlogged_after(
        "CREATE TABLE node (id INT PRIMARY KEY, up INT);
         ALTER TABLE node SET UNLOGGED, ADD FOREIGN KEY (up) REFERENCES node;",
        "node"
    ));
    // An absent target is reported as absent.
    let error = refusal(
        "CREATE TABLE child (id INT);
         ALTER TABLE child SET UNLOGGED, ADD FOREIGN KEY (id) REFERENCES missing;",
    );
    assert!(
        matches!(
            &error,
            Error::ReferencedTableNotFoundForForeignKey { referenced_table, .. }
                if referenced_table == "missing"
        ),
        "got {error:?}"
    );
    // PostgreSQL accepts this one and leaves a permanent table referencing an
    // unlogged one, which no other spelling reaches and a dump cannot restore.
    let sql = format!(
        "{UNLOGGED_PARENT} CREATE UNLOGGED TABLE child (id INT);
         ALTER TABLE child ADD FOREIGN KEY (id) REFERENCES parent, SET LOGGED;"
    );
    assert!(references_unlogged(&refusal(&sql), "child", "parent"));
}
