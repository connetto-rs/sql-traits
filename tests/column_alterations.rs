//! Tests that `ALTER TABLE` column operations change the model, and that a
//! column carries its dependents with it.
//!
//! Adding, dropping, renaming and altering a column used to fall through a
//! catch-all arm and do nothing at all, so a schema built from a migration
//! history answered as though every one of those statements were absent, with
//! no error and no warning.
//!
//! Dropping follows PostgreSQL: the indexes and constraints that live on the
//! table go along with the column, while anything outside the table calls for
//! `CASCADE`.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/column_names.rs"));

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// The grants each store records, rendered back to SQL.
///
/// A grant is recorded in both stores, so the two lists have to agree: a stale
/// copy in one of them names a column the table no longer declares.
fn rendered_grants(database: &ParserDB) -> (Vec<String>, Vec<String>) {
    (
        database.table_grants().map(ToString::to_string).collect(),
        database.column_grants().map(ToString::to_string).collect(),
    )
}

/// The condition of the single policy the database holds, rendered back to SQL.
fn policy_condition(database: &ParserDB) -> String {
    let policy = database.policies().next().expect("the policy survives");
    policy.using.as_ref().expect("the policy has a condition").to_string()
}

#[test]
fn adding_a_column_declares_it() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ADD COLUMN x TEXT NOT NULL;",
    )
    .expect("t exists");

    assert_eq!(column_names(&database, "t"), ["id", "a", "x"]);
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let added = table
        .column("x", &database, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("x is declared");
    assert!(!added.is_nullable(&database).expect("x is in this database"));
    assert_eq!(added.data_type(&database).to_string(), "TEXT");
}

#[test]
fn adding_a_column_that_exists_is_refused_unless_tolerated() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ADD COLUMN a INT;",
    )
    .expect_err("a is already declared");
    assert!(
        matches!(&error, Error::ColumnAlreadyExists { table_name, column_name }
            if table_name == "t" && column_name == "a"),
        "got {error:?}"
    );

    let tolerated = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ADD COLUMN IF NOT EXISTS a TEXT;",
    )
    .expect("IF NOT EXISTS asks for the statement to be tolerated");
    assert_eq!(column_names(&tolerated, "t"), ["id", "a"]);
    let table = tolerated
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let untouched = table
        .column("a", &tolerated, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert_eq!(
        untouched.data_type(&tolerated).to_string(),
        "INT",
        "the tolerated statement changed nothing rather than redeclaring the column"
    );
}

/// MySQL places a new column relative to an existing one, and the model keeps
/// columns in declaration order, so the placement has to be honoured.
#[test]
fn a_placed_column_lands_where_the_statement_says() {
    let first = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ADD COLUMN x INT FIRST;",
    )
    .expect("t exists");
    assert_eq!(column_names(&first, "t"), ["x", "id", "a"]);

    let after = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ADD COLUMN x INT AFTER id;",
    )
    .expect("t exists");
    assert_eq!(column_names(&after, "t"), ["id", "x", "a"]);
}

#[test]
fn dropping_a_column_removes_it() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT);
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("a is declared");

    assert_eq!(column_names(&database, "t"), ["id", "b"]);
}

#[test]
fn dropping_several_columns_in_one_statement_removes_each() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, c INT);
         ALTER TABLE t DROP COLUMN a, DROP COLUMN c;",
    )
    .expect("both columns are declared");

    assert_eq!(column_names(&database, "t"), ["id", "b"]);
}

#[test]
fn dropping_a_column_that_is_absent_is_refused_unless_tolerated() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t DROP COLUMN nope;",
    )
    .expect_err("nope is not declared");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound {
            table_name, column_name }) if table_name == "t" && column_name == "nope"),
        "got {error:?}"
    );

    parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t DROP COLUMN IF EXISTS nope;",
    )
    .expect("IF EXISTS asks for the statement to be tolerated");
}

/// PostgreSQL drops the indexes and table constraints involving the column
/// along with it, so none of these needs `CASCADE`.
#[test]
fn things_on_the_table_go_with_the_column() {
    let indexed = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE INDEX i ON t (a);
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("an index on the table goes with the column");
    assert_eq!(indexed.indexes().count(), 0);

    let declared = "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, CONSTRAINT u UNIQUE (a, b));";
    let before = parse(declared).expect("t exists");
    let after = parse(&format!("{declared} ALTER TABLE t DROP COLUMN a;"))
        .expect("a uniqueness rule on the table goes with the column");

    let unique_rules = |database: &ParserDB| {
        database
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("t exists")
            .unique_indices(database)
            .expect("t is in this database")
            .count()
    };
    assert_eq!(
        (unique_rules(&before), unique_rules(&after)),
        (2, 1),
        "the whole rule goes even though it also named a surviving column, \
         leaving only the primary key"
    );

    let checked = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, CONSTRAINT c CHECK (a > 0));
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("a check rule on the table goes with the column");
    let table = checked
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(table.check_constraints(&checked).expect("t is in this database").count(), 0);

    let self_referential = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, parent INT REFERENCES t (id));
         ALTER TABLE t DROP COLUMN parent;",
    )
    .expect("the table's own foreign key goes with the column");
    let table = self_referential
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(table.foreign_keys(&self_referential).expect("t is in this database").count(), 0);

    // A constraint reaches a table inline on a sibling column as well as
    // through the constraint list, and only the constraint list is validated
    // when the node is rebuilt, so an inline one left behind would survive
    // pointing at a column that no longer exists.
    let inline = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT UNIQUE, b INT REFERENCES t (a));
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("an inline foreign key on a sibling column goes with the column");
    let table = inline
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(table.foreign_keys(&inline).expect("t is in this database").count(), 0);
    // A foreign key left naming the dropped column would have been refused as
    // the alteration was read, so surviving the parse is the proof.

    let inline_check = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT CHECK (b > a));
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("an inline check rule mentioning the column goes with it");
    let table = inline_check
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(table.check_constraints(&inline_check).expect("t is in this database").count(), 0);
}

/// A foreign key from another table, a policy that reads the column and a
/// trigger that fires on it all live outside the table, which is what
/// PostgreSQL asks `CASCADE` for.
#[test]
fn things_outside_the_table_refuse_without_cascade() {
    let refusals = [
        "CREATE TABLE parent (id INT PRIMARY KEY, key INT UNIQUE);
         CREATE TABLE child (id INT PRIMARY KEY, pk INT REFERENCES parent (key));
         ALTER TABLE parent DROP COLUMN key;",
        "CREATE TABLE t (id INT PRIMARY KEY, owner TEXT);
         CREATE POLICY p ON t USING (owner = 'me');
         ALTER TABLE t DROP COLUMN owner;",
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER g AFTER UPDATE OF a ON t EXECUTE FUNCTION f();
         ALTER TABLE t DROP COLUMN a;",
    ];

    for sql in refusals {
        let error = parse(sql).expect_err("something outside the table depends on the column");
        assert!(matches!(&error, Error::ColumnReferenced { .. }), "got {error:?} for {sql}");
    }
}

#[test]
fn cascade_takes_the_things_outside_the_table() {
    let referencing = parse(
        "CREATE TABLE parent (id INT PRIMARY KEY, key INT UNIQUE);
         CREATE TABLE child (id INT PRIMARY KEY, pk INT REFERENCES parent (key));
         ALTER TABLE parent DROP COLUMN key CASCADE;",
    )
    .expect("CASCADE takes the child's foreign key");
    let child = referencing
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("child exists");
    assert_eq!(child.foreign_keys(&referencing).expect("child is in this database").count(), 0);
    assert_eq!(column_names(&referencing, "parent"), ["id"]);

    let policied = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, owner TEXT);
         CREATE POLICY p ON t USING (owner = 'me');
         ALTER TABLE t DROP COLUMN owner CASCADE;",
    )
    .expect("CASCADE takes the policy");
    assert_eq!(policied.policies().count(), 0);

    let triggered = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER g AFTER UPDATE OF a ON t EXECUTE FUNCTION f();
         ALTER TABLE t DROP COLUMN a CASCADE;",
    )
    .expect("CASCADE takes the trigger");
    assert_eq!(triggered.triggers().count(), 0);
}

/// A permission granted on the column itself belongs to the column and goes
/// with it, while a permission that also names a surviving column keeps that
/// one.
///
/// The assertions read the rendered statement rather than counting entries,
/// because a grant that kept a dropped column in a second action still counts
/// as one grant.
#[test]
fn a_permission_on_the_column_goes_with_it() {
    let only_column = parse(
        "CREATE ROLE app;
         CREATE TABLE t (id INT PRIMARY KEY, a INT);
         GRANT SELECT (a) ON t TO app;
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("the permission goes with the column");
    assert_eq!(rendered_grants(&only_column), (Vec::new(), Vec::new()));

    let shared = parse(
        "CREATE ROLE app;
         CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT);
         GRANT SELECT (a, b) ON t TO app;
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("the permission keeps the surviving column");
    let kept = vec!["GRANT SELECT (b) ON t TO app".to_owned()];
    assert_eq!(rendered_grants(&shared), (kept.clone(), kept));

    let table_wide = parse(
        "CREATE ROLE app;
         CREATE TABLE t (id INT PRIMARY KEY, a INT);
         GRANT SELECT ON t TO app;
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("a permission over the whole table names no column to lose");
    let whole = vec!["GRANT SELECT ON t TO app".to_owned()];
    assert_eq!(rendered_grants(&table_wide), (whole.clone(), whole));
}

/// A grant may name columns under more than one action, so stripping has to
/// visit every action rather than stopping at the first one that still names a
/// column. An action left naming nothing has nothing to grant.
#[test]
fn a_permission_naming_columns_under_several_actions_loses_only_the_dropped_one() {
    let declared = "CREATE ROLE app;
         CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT);
         GRANT SELECT (a), UPDATE (b) ON t TO app;";

    let dropped_second =
        parse(&format!("{declared} ALTER TABLE t DROP COLUMN b;")).expect("b is declared");
    let expected = vec!["GRANT SELECT (a) ON t TO app".to_owned()];
    assert_eq!(rendered_grants(&dropped_second), (expected.clone(), expected));

    let dropped_first =
        parse(&format!("{declared} ALTER TABLE t DROP COLUMN a;")).expect("a is declared");
    let expected = vec!["GRANT UPDATE (b) ON t TO app".to_owned()];
    assert_eq!(rendered_grants(&dropped_first), (expected.clone(), expected));

    let renamed = parse(&format!("{declared} ALTER TABLE t RENAME COLUMN b TO renamed;"))
        .expect("b is declared");
    let expected = vec!["GRANT SELECT (a), UPDATE (renamed) ON t TO app".to_owned()];
    assert_eq!(rendered_grants(&renamed), (expected.clone(), expected));
}

#[test]
fn renaming_a_column_renames_it() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t RENAME COLUMN a TO b;",
    )
    .expect("a is declared");

    assert_eq!(column_names(&database, "t"), ["id", "b"]);
}

#[test]
fn renaming_a_column_is_refused_when_the_name_is_wrong() {
    let absent = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t RENAME COLUMN nope TO b;",
    )
    .expect_err("nope is not declared");
    assert!(
        matches!(&absent, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. })),
        "got {absent:?}"
    );

    let taken = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT);
         ALTER TABLE t RENAME COLUMN a TO b;",
    )
    .expect_err("b is already declared");
    assert!(matches!(&taken, Error::ColumnAlreadyExists { .. }), "got {taken:?}");
}

/// The rename has to reach inside expressions, which is where a check rule, a
/// policy condition and an index's column list keep their mentions.
#[test]
fn renaming_a_column_rewrites_every_mention() {
    let database = parse(
        "CREATE ROLE app;
         CREATE TABLE t (
             id INT PRIMARY KEY,
             a INT,
             b INT,
             CONSTRAINT c CHECK (a > 0),
             CONSTRAINT u UNIQUE (a, b)
         );
         CREATE INDEX i ON t (a);
         CREATE POLICY p ON t USING (a > 1);
         CREATE FUNCTION f() RETURNS TRIGGER AS $$ BEGIN END; $$ LANGUAGE plpgsql;
         CREATE TRIGGER g AFTER UPDATE OF a ON t EXECUTE FUNCTION f();
         GRANT SELECT (a) ON t TO app;
         ALTER TABLE t RENAME COLUMN a TO renamed;",
    )
    .expect("a is declared");

    assert_eq!(column_names(&database, "t"), ["id", "renamed", "b"]);
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");

    let check = table
        .check_constraints(&database)
        .expect("t is in this database")
        .next()
        .expect("the check rule survives");
    assert_eq!(
        check
            .columns(&database)
            .expect("t is in this database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<String>>(),
        ["renamed"],
        "the check rule resolves against the new name"
    );

    let index = database.indexes().next().expect("the index survives");
    let expression =
        IndexLike::expression(index, &database).expect("the index host is in this database");
    assert!(
        expression.to_string().contains("renamed"),
        "the index expression follows the rename: {expression}"
    );

    let policy = database.policies().next().expect("the policy survives");
    assert!(
        policy.using.as_ref().is_some_and(|using| using.to_string().contains("renamed")),
        "the policy condition follows the rename"
    );

    let grant = database.column_grants().next().expect("the permission survives");
    let granted: Vec<String> = grant
        .columns(table, &database)
        .expect("t is in this database")
        .map(|column| column.column_name().to_owned())
        .collect();
    assert_eq!(granted, ["renamed"], "the permission follows the rename");
}

#[test]
fn renaming_a_referenced_column_follows_into_the_referencing_table() {
    let database = parse(
        "CREATE TABLE parent (id INT PRIMARY KEY, key INT UNIQUE);
         CREATE TABLE child (id INT PRIMARY KEY, pk INT REFERENCES parent (key));
         ALTER TABLE parent RENAME COLUMN key TO renamed;",
    )
    .expect("key is declared");

    assert_eq!(column_names(&database, "parent"), ["id", "renamed"]);

    let child = database
        .table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("child exists");
    let foreign_key = child
        .foreign_keys(&database)
        .expect("child is in this database")
        .next()
        .expect("the foreign key survives");
    assert_eq!(
        foreign_key
            .referenced_columns(&database)
            .expect("the target resolves")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<String>>(),
        ["renamed"]
    );
}

#[test]
fn a_self_referential_foreign_key_follows_a_column_rename() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, parent INT REFERENCES t (id));
         ALTER TABLE t RENAME COLUMN id TO key;",
    )
    .expect("id is declared");

    assert_eq!(column_names(&database, "t"), ["key", "parent"]);
    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let foreign_key =
        table.foreign_keys(&database).expect("t is in this database").next().expect("one key");
    assert_eq!(
        foreign_key
            .referenced_columns(&database)
            .expect("the self reference resolves")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<String>>(),
        ["key"]
    );
}

/// A policy condition may reach another table through a subquery, and that
/// table may declare a column of the same name. A qualified mention names this
/// table's column only when the qualifier is this table, so the other table's
/// column has to survive untouched.
#[test]
fn a_rename_leaves_another_tables_column_of_the_same_name_alone() {
    let database = parse(
        "CREATE TABLE other (id INT PRIMARY KEY, a INT);
         CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE POLICY p ON t USING (EXISTS (SELECT 1 FROM other WHERE other.a = t.a));
         ALTER TABLE t RENAME COLUMN a TO renamed;",
    )
    .expect("a is declared on t");

    assert_eq!(column_names(&database, "t"), ["id", "renamed"]);
    assert_eq!(column_names(&database, "other"), ["id", "a"]);

    let policy = database.policies().next().expect("the policy survives");
    let using = policy.using.as_ref().expect("the policy has a condition").to_string();
    assert!(using.contains("other.a"), "the other table keeps its own column: {using}");
    assert!(using.contains("t.renamed"), "this table's column follows the rename: {using}");
}

/// The same rule keeps a drop from being refused by a mention that belongs to
/// another table.
#[test]
fn a_drop_is_not_refused_by_another_tables_column_of_the_same_name() {
    let database = parse(
        "CREATE TABLE other (id INT PRIMARY KEY, a INT);
         CREATE TABLE t (id INT PRIMARY KEY, a INT, keep INT);
         CREATE POLICY p ON t USING (EXISTS (SELECT 1 FROM other WHERE other.a = 1));
         ALTER TABLE t DROP COLUMN a;",
    )
    .expect("the policy reads other.a, not t.a");

    assert_eq!(column_names(&database, "t"), ["id", "keep"]);
    assert_eq!(database.policies().count(), 1, "the policy depends on nothing that was dropped");
}

/// A mention written without a table prefix inside a nested query belongs to a
/// table that query reads, when one of them declares the name. The rewriter
/// asks the model which table declares it rather than assuming the outer one.
#[test]
fn a_bare_mention_inside_a_nested_query_belongs_to_the_table_that_declares_it() {
    let declared = "CREATE TABLE members (id INT PRIMARY KEY, a INT);
         CREATE TABLE t (id INT PRIMARY KEY, a INT, keep INT);
         CREATE POLICY p ON t USING (EXISTS (SELECT 1 FROM members WHERE a = 1));";

    let renamed = parse(&format!("{declared} ALTER TABLE t RENAME COLUMN a TO renamed;"))
        .expect("a is declared on t");
    let using = policy_condition(&renamed);
    assert!(
        using.contains("WHERE a = 1"),
        "members declares a, so the bare mention is theirs and stays: {using}"
    );
    assert_eq!(column_names(&renamed, "members"), ["id", "a"]);

    let dropped = parse(&format!("{declared} ALTER TABLE t DROP COLUMN a;"))
        .expect("the policy reads members.a, not t.a");
    assert_eq!(column_names(&dropped, "t"), ["id", "keep"]);
    assert_eq!(dropped.policies().count(), 1, "the policy depends on nothing that was dropped");

    let cascaded = parse(&format!("{declared} ALTER TABLE t DROP COLUMN a CASCADE;"))
        .expect("CASCADE has nothing outside the table to take");
    assert_eq!(
        cascaded.policies().count(),
        1,
        "CASCADE must not take a policy that reads another table's column"
    );
}

/// When no table the nested query reads declares the name, the bare mention can
/// only be the altered table's, so it follows.
#[test]
fn a_bare_mention_no_nested_table_declares_belongs_to_the_altered_table() {
    let declared = "CREATE TABLE members (id INT PRIMARY KEY, other INT);
         CREATE TABLE t (id INT PRIMARY KEY, a INT, keep INT);
         CREATE POLICY p ON t USING (EXISTS (SELECT 1 FROM members WHERE members.other = a));";

    let renamed = parse(&format!("{declared} ALTER TABLE t RENAME COLUMN a TO renamed;"))
        .expect("a is declared on t");
    let using = policy_condition(&renamed);
    assert!(using.contains("members.other"), "the nested table keeps its own column: {using}");
    assert!(using.contains("= renamed"), "the outer mention follows the rename: {using}");

    let refused = parse(&format!("{declared} ALTER TABLE t DROP COLUMN a;"))
        .expect_err("the policy genuinely reads t.a");
    assert!(matches!(&refused, Error::ColumnReferenced { .. }), "got {refused:?}");
}

#[test]
fn altering_a_column_changes_what_it_declares() {
    let not_null = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ALTER COLUMN a SET NOT NULL;",
    )
    .expect("a is declared");
    let table = not_null
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("a", &not_null, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert!(!column.is_nullable(&not_null).expect("a is in this database"));

    let nullable = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT NOT NULL);
         ALTER TABLE t ALTER COLUMN a DROP NOT NULL;",
    )
    .expect("a is declared");
    let table = nullable
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("a", &nullable, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert!(column.is_nullable(&nullable).expect("a is in this database"));

    let defaulted = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ALTER COLUMN a SET DEFAULT 7;",
    )
    .expect("a is declared");
    let table = defaulted
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("a", &defaulted, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert_eq!(column.default_value().as_deref(), Some("7"));

    let undefaulted = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT DEFAULT 7);
         ALTER TABLE t ALTER COLUMN a DROP DEFAULT;",
    )
    .expect("a is declared");
    let table = undefaulted
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("a", &undefaulted, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert_eq!(column.default_value(), None);

    let retyped = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ALTER COLUMN a SET DATA TYPE TEXT;",
    )
    .expect("a is declared");
    let table = retyped
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("a", &retyped, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert_eq!(column.data_type(&retyped).to_string(), "TEXT");
}

#[test]
fn altering_a_column_that_is_absent_is_refused() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         ALTER TABLE t ALTER COLUMN nope SET NOT NULL;",
    )
    .expect_err("nope is not declared");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. })),
        "got {error:?}"
    );
}

/// MySQL's `CHANGE COLUMN` carries the whole declaration, so it renames and
/// restates the type in one clause, while `MODIFY COLUMN` only restates it.
#[test]
fn the_mysql_spellings_restate_the_declaration() {
    let changed = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t CHANGE COLUMN a b TEXT NOT NULL;",
    )
    .expect("a is declared");
    assert_eq!(column_names(&changed, "t"), ["id", "b"]);
    let table = changed
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("b", &changed, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("b is declared");
    assert_eq!(column.data_type(&changed).to_string(), "TEXT");
    assert!(!column.is_nullable(&changed).expect("b is in this database"));

    let modified = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT NOT NULL);
         ALTER TABLE t MODIFY COLUMN a TEXT;",
    )
    .expect("a is declared");
    assert_eq!(column_names(&modified, "t"), ["id", "a"]);
    let table = modified
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let column = table
        .column("a", &modified, IdentifierCase::AsWritten)
        .expect("t is in this database")
        .expect("a is declared");
    assert_eq!(column.data_type(&modified).to_string(), "TEXT");
    assert!(
        column.is_nullable(&modified).expect("a is in this database"),
        "the clause left NOT NULL out, so the column loses it"
    );
}

/// A column operation against a table the input never created is refused, the
/// same as every other `ALTER TABLE`, unless the statement said `IF EXISTS`.
#[test]
fn a_column_operation_on_an_absent_table_is_refused() {
    for tail in
        ["ADD COLUMN x INT", "DROP COLUMN x", "RENAME COLUMN a TO b", "ALTER COLUMN a SET NOT NULL"]
    {
        let sql = format!(
            "CREATE TABLE present (id INT PRIMARY KEY);
             ALTER TABLE absent {tail};"
        );
        let error = parse(&sql).expect_err("absent is not created");
        assert!(
            matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "absent"),
            "{tail} reported {error:?}"
        );

        let tolerated = format!(
            "CREATE TABLE present (id INT PRIMARY KEY);
             ALTER TABLE IF EXISTS absent {tail};"
        );
        parse(&tolerated).expect("IF EXISTS asks for the statement to be tolerated");
    }
}
