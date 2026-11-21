//! Tests that a foreign key under SQLite names its target without binding to
//! it. SQLite stores the `REFERENCES` clause as written, looks the target up
//! by name in the schema of the host table whenever the key is used, and
//! checks nothing about the target when a table is created, altered or
//! dropped.
//!
//! Every expectation below was read from sqlite3 3.51.1, with its defaults of
//! `foreign_keys = 0` and `legacy_alter_table = 0`.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<SQLiteDialect>(sql)
}

/// The only foreign key the table `target` names declares.
fn key<'db>(
    database: &'db ParserDB,
    target: TargetName<'_>,
) -> &'db <ParserDB as DatabaseLike>::ForeignKey {
    let table = database
        .table_by_target(target, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the host table exists");
    let mut keys = table.foreign_keys(database).expect("the host table is in this database");
    let key = keys.next().expect("the host table declares a key");
    assert!(keys.next().is_none(), "the host table declares one key");
    key
}

/// The only foreign key of the table `host` in the main schema.
fn key_of<'db>(database: &'db ParserDB, host: &str) -> &'db <ParserDB as DatabaseLike>::ForeignKey {
    key(database, TargetName::new(host, false))
}

/// The name of the table the only key of `host` reaches.
fn target_of(database: &ParserDB, host: &str) -> Result<String, LookupError> {
    key_of(database, host).referenced_table(database).map(|table| table.table_name().to_owned())
}

/// The names of the columns the only key of `host` references.
fn referenced_of(database: &ParserDB, host: &str) -> Result<Vec<String>, LookupError> {
    Ok(key_of(database, host)
        .referenced_columns(database)?
        .map(|column| column.column_name().to_owned())
        .collect())
}

fn table_not_found(object_name: &str) -> LookupError {
    LookupError::TableNotFound { object_name: object_name.to_owned() }
}

/// A key may reach a table created after it, and resolves to that table once
/// it exists, the primary key included for a key naming no columns.
#[test]
fn a_target_created_later_is_reached_once_it_exists() {
    let database = parse(
        "CREATE TABLE child (pid REFERENCES parent (id), pk, pa, FOREIGN KEY (pk, pa) REFERENCES composite);
         CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE composite (a, b, PRIMARY KEY (b, a));",
    )
    .expect("SQLite stores a key whose target comes later");
    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("child exists");
    let mut resolved: Vec<(String, Vec<String>)> = child
        .foreign_keys(&database)
        .expect("child is in this database")
        .map(|key| {
            (
                key.referenced_table(&database).expect("the target exists").table_name().to_owned(),
                key.referenced_columns(&database)
                    .expect("the target holds the key")
                    .map(|column| column.column_name().to_owned())
                    .collect(),
            )
        })
        .collect();
    resolved.sort();
    assert_eq!(
        resolved,
        [
            ("composite".to_owned(), vec!["b".to_owned(), "a".to_owned()]),
            ("parent".to_owned(), vec!["id".to_owned()]),
        ]
    );
}

/// Two tables may reference each other, whichever is created first.
#[test]
fn a_cyclic_pair_is_stored_and_each_key_resolves() {
    let database = parse(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, b_id REFERENCES b (id));
         CREATE TABLE b (id INTEGER PRIMARY KEY, a_id REFERENCES a (id));",
    )
    .expect("SQLite stores a cyclic pair of keys");
    assert_eq!(target_of(&database, "a"), Ok("b".to_owned()));
    assert_eq!(target_of(&database, "b"), Ok("a".to_owned()));
}

/// A target that never exists is stored as written and answers a lookup
/// error when it is read.
#[test]
fn a_target_that_never_exists_is_kept_as_written() {
    let database = parse("CREATE TABLE child (pid REFERENCES parent (id));")
        .expect("SQLite stores a key whose target never exists");
    let key = key_of(&database, "child");
    assert_eq!(key.referenced_table_name().to_string(), "parent");
    assert_eq!(target_of(&database, "child"), Err(table_not_found("parent")));
    assert_eq!(referenced_of(&database, "child"), Err(table_not_found("parent")));
}

/// The referenced columns a key names read back in the order written, each
/// with whether it was quoted, with no target to resolve them against. A key
/// naming none reads back empty, which is how SQLite spells the primary key.
#[test]
fn referenced_column_names_read_without_a_target() {
    let database = parse(
        "CREATE TABLE child (a, b, FOREIGN KEY (a, b) REFERENCES parent (\"Code\", id));
         CREATE TABLE orphan (pid REFERENCES absent);",
    )
    .expect("SQLite stores keys whose targets never exist");
    let written: Vec<(&str, bool)> = key_of(&database, "child").referenced_column_names().collect();
    assert_eq!(written, [("Code", true), ("id", false)]);
    assert_eq!(key_of(&database, "orphan").referenced_column_names().next(), None);
}

/// A view holding the name is no table to reference, whether it comes before
/// or after the key.
#[test]
fn a_view_holding_the_name_is_not_a_target() {
    for sql in [
        "CREATE VIEW parent AS SELECT 1 AS id; CREATE TABLE child (pid REFERENCES parent (id));",
        "CREATE TABLE child (pid REFERENCES parent (id)); CREATE VIEW parent AS SELECT 1 AS id;",
    ] {
        let database = parse(sql).expect("SQLite stores a key whose name a view holds");
        assert_eq!(
            target_of(&database, "child"),
            Err(LookupError::TableNotFound { object_name: "parent".to_owned() }),
            "{sql}"
        );
    }
}

/// A referenced column the target does not declare is reported when the
/// columns are read.
#[test]
fn a_referenced_column_the_target_lacks_is_reported_on_read() {
    let database = parse(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (pid REFERENCES parent (nope));",
    )
    .expect("SQLite stores a key naming a column its target lacks");
    assert_eq!(target_of(&database, "child"), Ok("parent".to_owned()));
    assert_eq!(
        referenced_of(&database, "child"),
        Err(LookupError::ColumnNotFound {
            table_name: "parent".to_owned(),
            column_name: "nope".to_owned(),
        })
    );
}

/// Referenced columns need the primary key, a unique constraint, or a unique
/// index over every row, in any column order, which SQLite asks only when the
/// key is used.
#[test]
fn referenced_columns_without_a_unique_key_are_reported_on_read() {
    let not_unique = |referenced_table: &str, referenced_columns: &str| {
        Err(LookupError::ReferencedColumnsNotUnique {
            host_table: "child".to_owned(),
            referenced_table: referenced_table.to_owned(),
            referenced_columns: referenced_columns.to_owned(),
        })
    };
    for (sql, expected) in [
        (
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, tag);
             CREATE TABLE child (t REFERENCES parent (tag));",
            not_unique("parent", "tag"),
        ),
        ("CREATE TABLE child (t REFERENCES child (t));", not_unique("child", "t")),
        (
            "CREATE TABLE parent (tag);
             CREATE UNIQUE INDEX positive ON parent (tag) WHERE tag > 0;
             CREATE TABLE child (t REFERENCES parent (tag));",
            not_unique("parent", "tag"),
        ),
        (
            "CREATE TABLE parent (tag);
             CREATE UNIQUE INDEX every ON parent (tag);
             CREATE TABLE child (t REFERENCES parent (tag));",
            Ok(vec!["tag".to_owned()]),
        ),
        (
            "CREATE TABLE parent (a, b, UNIQUE (b, a));
             CREATE TABLE child (x, y, FOREIGN KEY (x, y) REFERENCES parent (a, b));",
            Ok(vec!["a".to_owned(), "b".to_owned()]),
        ),
    ] {
        let database = parse(sql).expect("SQLite stores the key");
        assert_eq!(referenced_of(&database, "child"), expected, "{sql}");
    }
}

/// A key naming no columns needs a primary key on its target as wide as the
/// host column list, which SQLite asks only when the key is used.
#[test]
fn an_implied_primary_key_mismatch_is_reported_on_read() {
    let database = parse(
        "CREATE TABLE parent (id);
         CREATE TABLE child (pid REFERENCES parent);",
    )
    .expect("SQLite stores a key to a table without a primary key");
    assert_eq!(
        referenced_of(&database, "child"),
        Err(LookupError::ReferencedPrimaryKeyNotFound {
            host_table: "child".to_owned(),
            referenced_table: "parent".to_owned(),
        })
    );

    let database = parse(
        "CREATE TABLE parent (a, b, PRIMARY KEY (a, b));
         CREATE TABLE child (pid REFERENCES parent);",
    )
    .expect("SQLite stores a key narrower than the primary key");
    assert_eq!(
        referenced_of(&database, "child"),
        Err(LookupError::ForeignKeyColumnCountMismatch {
            host_table: "child".to_owned(),
            host_columns: 1,
            referenced_table: "parent".to_owned(),
            referenced_columns: 2,
        })
    );
}

/// The one check SQLite makes at creation compares the two written column
/// lists, so it refuses whether or not the target exists.
#[test]
fn written_column_lists_of_different_lengths_are_refused() {
    for schema in ["", "CREATE TABLE parent (id INTEGER PRIMARY KEY, other);"] {
        let error = parse(&format!(
            "{schema} CREATE TABLE child (x, y, FOREIGN KEY (x, y) REFERENCES parent (id));"
        ))
        .expect_err("SQLite refuses lists of different lengths");
        assert!(
            matches!(
                &error,
                Error::ForeignKeyColumnCountMismatch {
                    host_table,
                    host_columns: 2,
                    referenced_table,
                    referenced_columns: 1,
                } if host_table == "child" && referenced_table == "parent"
            ),
            "{schema}: got {error:?}"
        );
    }

    let error = parse("CREATE TABLE child (x, FOREIGN KEY (y) REFERENCES parent (id));")
        .expect_err("SQLite refuses a key naming a column its table lacks");
    assert!(
        matches!(&error, Error::HostColumnNotFoundForForeignKey { host_column, host_table }
            if host_column == "y" && host_table == "child"),
        "got {error:?}"
    );
}

/// SQLite looks a target up in the schema of the host table, so a temporary
/// table reaches only a temporary target and a main table only a main one.
#[test]
fn the_target_is_looked_up_in_the_schema_of_the_host() {
    let temporary_child = || TargetName::new("child", false).with_schema("temp", false);

    let database = parse(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE child (pid REFERENCES parent (id));",
    )
    .expect("SQLite stores a temporary key to a main table");
    assert_eq!(
        key(&database, temporary_child()).referenced_table(&database),
        Err(table_not_found("temp.parent"))
    );

    let database = parse(
        "CREATE TEMP TABLE child (pid REFERENCES parent (id));
         CREATE TEMP TABLE parent (id INTEGER PRIMARY KEY);",
    )
    .expect("SQLite stores a temporary key to a later temporary table");
    let target = key(&database, temporary_child())
        .referenced_table(&database)
        .expect("the temporary parent exists");
    assert_eq!((target.table_schema(), target.table_name()), (Some("temp"), "parent"));

    let database = parse(
        "CREATE TEMP TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (pid REFERENCES parent (id));",
    )
    .expect("SQLite stores a main key to a temporary table");
    assert_eq!(target_of(&database, "child"), Err(table_not_found("parent")));

    let database = parse(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE child (x);
         ALTER TABLE child ADD COLUMN pid REFERENCES parent (id);",
    )
    .expect("SQLite adds a temporary key to a main table");
    assert_eq!(
        key(&database, temporary_child()).referenced_table(&database),
        Err(table_not_found("temp.parent"))
    );
}

/// SQLite's foreign key clause takes a bare table name, since the target is
/// always looked up in the schema of the host, and its parser refuses one
/// qualified with a schema in every spelling.
#[test]
fn a_target_qualified_with_a_schema_is_refused() {
    for sql in [
        "CREATE TABLE child (pid REFERENCES main.parent (id));",
        "CREATE TABLE child (pid, FOREIGN KEY (pid) REFERENCES temp.parent (id));",
        "CREATE TABLE child (x); ALTER TABLE child ADD COLUMN pid REFERENCES main.parent (id);",
    ] {
        let error = parse(&format!("CREATE TABLE parent (id INTEGER PRIMARY KEY); {sql}"))
            .expect_err("SQLite refuses a qualified target");
        assert!(
            matches!(&error, Error::QualifiedForeignKeyTarget { host_table, referenced_table }
                if host_table == "child" && referenced_table.ends_with(".parent")),
            "{sql}: got {error:?}"
        );
    }
}

/// A column added with `REFERENCES` declares its key the way `CREATE TABLE`
/// does.
#[test]
fn an_added_column_may_reference_a_later_table() {
    let database = parse(
        "CREATE TABLE child (y);
         ALTER TABLE child ADD COLUMN x REFERENCES parent (id);
         CREATE TABLE parent (id INTEGER PRIMARY KEY);",
    )
    .expect("SQLite stores an added key whose target comes later");
    assert_eq!(target_of(&database, "child"), Ok("parent".to_owned()));
}

/// Dropping a referenced table keeps the keys naming it, and a table created
/// under the name afterwards is the one they reach.
#[test]
fn dropping_a_referenced_table_keeps_the_keys_naming_it() {
    let schema = "CREATE TABLE parent (id INTEGER PRIMARY KEY);
                  CREATE TABLE child (pid REFERENCES parent (id));
                  DROP TABLE parent;";
    let database = parse(schema).expect("SQLite drops a referenced table");
    assert_eq!(target_of(&database, "child"), Err(table_not_found("parent")));

    let database = parse(&format!("{schema} CREATE TABLE parent (id INTEGER PRIMARY KEY);"))
        .expect("SQLite creates the table again");
    assert_eq!(target_of(&database, "child"), Ok("parent".to_owned()));
}

/// Renaming a referenced table rewrites the keys naming it, except a
/// temporary table's, which never reached it. A table renamed to a name a
/// key holds is the one that key reaches.
#[test]
fn renames_move_the_keys_that_reached_the_table() {
    let database = parse(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY);
         CREATE TABLE child (pid REFERENCES parent (id));
         CREATE TEMP TABLE scratch (pid REFERENCES parent (id));
         ALTER TABLE parent RENAME TO renamed;",
    )
    .expect("SQLite renames a referenced table");
    assert_eq!(key_of(&database, "child").referenced_table_name().to_string(), "renamed");
    assert_eq!(target_of(&database, "child"), Ok("renamed".to_owned()));
    let scratch = TargetName::new("scratch", false).with_schema("temp", false);
    assert_eq!(key(&database, scratch).referenced_table_name().to_string(), "temp.parent");

    let database = parse(
        "CREATE TABLE child (pid REFERENCES parent (id));
         CREATE TABLE other (id INTEGER PRIMARY KEY);
         ALTER TABLE other RENAME TO parent;",
    )
    .expect("SQLite renames a table to a name a key holds");
    assert_eq!(target_of(&database, "child"), Ok("parent".to_owned()));

    let database = parse(
        "CREATE TABLE tree (id INTEGER PRIMARY KEY, up REFERENCES tree (id));
         ALTER TABLE tree RENAME TO forest;",
    )
    .expect("SQLite renames a table referencing itself");
    assert_eq!(target_of(&database, "forest"), Ok("forest".to_owned()));
}

/// Renaming a column rewrites the keys naming it on either side.
#[test]
fn column_renames_reach_both_sides_of_a_key() {
    let database = parse(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, code UNIQUE);
         CREATE TABLE child (pc REFERENCES parent (code));
         ALTER TABLE parent RENAME COLUMN code TO label;
         ALTER TABLE child RENAME COLUMN pc TO parent_label;",
    )
    .expect("SQLite renames the columns of a key");
    let key = key_of(&database, "child");
    let host: Vec<&str> =
        key.host_columns(&database).expect("host columns").map(ColumnLike::column_name).collect();
    assert_eq!(host, ["parent_label"]);
    assert_eq!(referenced_of(&database, "child"), Ok(vec!["label".to_owned()]));
}

/// Dropping a referenced column keeps the keys naming it, from another table
/// or from its own, and they report the column missing when read.
#[test]
fn dropping_a_referenced_column_keeps_the_keys_naming_it() {
    let database = parse(
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, code);
         CREATE TABLE child (pc REFERENCES parent (code));
         ALTER TABLE parent DROP COLUMN code;",
    )
    .expect("SQLite drops a column another table's key references");
    assert_eq!(
        referenced_of(&database, "child"),
        Err(LookupError::ColumnNotFound {
            table_name: "parent".to_owned(),
            column_name: "code".to_owned(),
        })
    );

    let database = parse(
        "CREATE TABLE tree (up, code, FOREIGN KEY (up) REFERENCES tree (code));
         ALTER TABLE tree DROP COLUMN code;",
    )
    .expect("SQLite drops a column its own table's key references");
    assert_eq!(
        referenced_of(&database, "tree"),
        Err(LookupError::ColumnNotFound {
            table_name: "tree".to_owned(),
            column_name: "code".to_owned(),
        })
    );
}

/// A column its own table's key is declared on cannot be dropped, since the
/// key would name a column the table no longer has.
#[test]
fn a_column_a_key_is_declared_on_cannot_be_dropped() {
    let error = parse(
        "CREATE TABLE child (x, w, FOREIGN KEY (x) REFERENCES parent (id));
         ALTER TABLE child DROP COLUMN x;",
    )
    .expect_err("SQLite refuses to drop a key's own column");
    assert!(
        matches!(&error, Error::HostColumnNotFoundForForeignKey { host_column, host_table }
            if host_column == "x" && host_table == "child"),
        "got {error:?}"
    );
}

/// Under SQLite's identifier comparison a quoted name reaches the table it
/// spells in any case.
#[test]
fn a_folded_catalog_reaches_the_target_in_any_case() {
    let database = ParseOptions::default()
        .with_identifier_case(IdentifierCase::Folded)
        .parse::<SQLiteDialect>(
            "CREATE TABLE child (pid REFERENCES \"Parent\" (id));
             CREATE TABLE parent (id INTEGER PRIMARY KEY);",
        )
        .expect("SQLite stores the key");
    assert_eq!(target_of(&database, "child"), Ok("parent".to_owned()));
}

/// PostgreSQL resolves the target when the key is declared, so it keeps
/// refusing a target that does not exist yet.
#[test]
fn postgresql_still_refuses_a_later_target() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE child (pid INT REFERENCES parent (id));
         CREATE TABLE parent (id INT PRIMARY KEY);",
    )
    .expect_err("PostgreSQL resolves the target at creation");
    assert!(
        matches!(&error, Error::ReferencedTableNotFoundForForeignKey { referenced_table, .. }
            if referenced_table == "parent"),
        "got {error:?}"
    );
}
