//! Tests that a statement naming an absent column, schema or role is reported.
//!
//! The accept-in-silence split was never specific to tables. A uniqueness rule
//! naming a column the table does not declare was refused while an index naming
//! the same column was accepted, a table could be placed in a schema nothing
//! created, and a policy could apply to a role nothing created while a grant to
//! the same role was refused.
//!
//! Two exemptions are deliberate. The default schema is never declared, since
//! no dump emits a statement creating it. A role follows the setting that
//! already governs grants, because a dump of a schema omits role creation
//! either way.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn open_world() -> ParseOptions {
    ParseOptions::default().with_access_resolution(AccessResolution::OpenWorld)
}

/// The sharpest pair: a uniqueness rule already refused this, and an index over
/// the same column did not.
#[test]
fn an_index_naming_an_absent_column_is_refused() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE INDEX i ON t (missing);",
    )
    .expect_err("missing is not declared");

    let rule = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         ALTER TABLE t ADD CONSTRAINT u UNIQUE (missing);",
    )
    .expect_err("missing is not declared");

    assert_eq!(
        error.to_string(),
        rule.to_string(),
        "the index and the uniqueness rule report the same mistake the same way"
    );
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound {
            table_name, column_name }) if table_name == "t" && column_name == "missing"),
        "got {error:?}"
    );
}

/// An `INCLUDE` column is carried alongside the indexed ones and has to exist
/// just the same.
#[test]
fn an_index_including_an_absent_column_is_refused() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE INDEX i ON t (a) INCLUDE (missing);",
    )
    .expect_err("missing is not declared");
    assert!(
        matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound { .. })),
        "got {error:?}"
    );
}

/// A key constraint is backed by an index carrying its `INCLUDE` list, so an
/// absent column there is the same mistake as on `CREATE INDEX`.
#[test]
fn a_key_constraint_including_an_absent_column_is_refused() {
    for constraint in ["UNIQUE (a) INCLUDE (missing)", "PRIMARY KEY (id) INCLUDE (missing)"] {
        let error = parse(&format!("CREATE TABLE t (id INT, a INT, CONSTRAINT c {constraint});"))
            .expect_err("missing is not declared");
        assert!(
            matches!(&error, Error::IdentifierLookupError(LookupError::ColumnNotFound {
                table_name, column_name }) if table_name == "t" && column_name == "missing"),
            "{constraint}: got {error:?}"
        );
    }
}

/// An entry that is an expression rather than a plain column names no single
/// column, so it is left alone, matching how an index-shaped constraint is
/// checked.
#[test]
fn an_expression_index_is_left_alone() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT);
         CREATE INDEX i ON t (lower(a));",
    )
    .expect("an expression names no single column");
    assert_eq!(database.indexes().count(), 1);
}

#[test]
fn a_table_in_an_absent_schema_is_refused() {
    let error = parse("CREATE TABLE missing_schema.u (id INT);")
        .expect_err("missing_schema is not created");
    assert!(
        matches!(&error, Error::SchemaNotFoundForRelation { schema_name, relation_name, object_kind }
            if schema_name == "missing_schema"
                && relation_name == "u"
                && *object_kind == ObjectKind::Table),
        "got {error:?}"
    );

    parse(
        "CREATE SCHEMA declared;
         CREATE TABLE declared.u (id INT);",
    )
    .expect("the schema is created first");
}

/// The default schema is never declared, and a Postgres dump qualifies every
/// table with it, so requiring a declaration would turn away ordinary input.
#[test]
fn the_default_schema_needs_no_declaration() {
    let qualified =
        parse("CREATE TABLE public.u (id INT);").expect("the default schema is assumed");
    let table = qualified.tables().next().expect("one table");
    assert_eq!(table.table_schema(), Some("public"));

    let unqualified = parse("CREATE TABLE u (id INT);").expect("an unqualified name has no schema");
    assert_eq!(unqualified.tables().next().expect("one table").table_schema(), None);
}

/// A rename that moves a table into a schema needs that schema, since the table
/// is placed there just as a fresh declaration would be.
#[test]
fn renaming_a_table_into_an_absent_schema_is_refused() {
    let error = ParserDB::parse::<sqlparser::dialect::MySqlDialect>(
        "CREATE SCHEMA a;
         CREATE TABLE a.t (id INT);
         RENAME TABLE a.t TO b.t;",
    )
    .expect_err("b is not created");
    assert!(matches!(&error, Error::SchemaNotFoundForRelation { .. }), "got {error:?}");
}

#[test]
fn a_policy_applying_to_an_absent_role_is_refused_by_default() {
    let error = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         CREATE POLICY p ON t TO missing_role USING (true);",
    )
    .expect_err("missing_role is not created");
    assert!(
        matches!(&error, Error::RoleNotFoundForPolicy { role_name, policy_name }
            if role_name == "missing_role" && policy_name == "p"),
        "got {error:?}"
    );

    parse(
        "CREATE ROLE declared;
         CREATE TABLE t (id INT PRIMARY KEY);
         CREATE POLICY p ON t TO declared USING (true);",
    )
    .expect("the role is created first");
}

/// A policy follows the setting that already governs grants, because a dump of
/// a schema omits role creation for both.
#[test]
fn the_open_world_accepts_a_policy_role_it_does_not_create() {
    let database = open_world()
        .parse::<PostgreSqlDialect>(
            "CREATE TABLE t (id INT PRIMARY KEY);
             CREATE POLICY p ON t TO missing_role USING (true);",
        )
        .expect("the open world records the policy as written");

    assert_eq!(database.policies().count(), 1);
}

/// `PUBLIC` means every role rather than one called `public`, so it never
/// demanded a declaration, exactly as for a grant.
#[test]
fn the_public_pseudo_role_needs_no_declaration() {
    let database = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         CREATE POLICY p ON t TO PUBLIC USING (true);",
    )
    .expect("PUBLIC needs no CREATE ROLE");
    assert_eq!(database.policies().count(), 1);

    let current = parse(
        "CREATE TABLE t (id INT PRIMARY KEY);
         CREATE POLICY p ON t TO CURRENT_USER USING (true);",
    )
    .expect("a pseudo-role keyword names no role of its own");
    assert_eq!(current.policies().count(), 1);
}

/// A column an exclusion constraint carries in its `INCLUDE` list is plain
/// identifiers rather than an expression, so it needs its own handling on both
/// the drop and the rename paths.
#[test]
fn an_exclusion_constraint_include_list_follows_its_column() {
    let renamed = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT,
             CONSTRAINT x EXCLUDE USING gist (a WITH =) INCLUDE (b));
         ALTER TABLE t RENAME COLUMN b TO renamed;",
    )
    .expect("b is declared");
    let table = renamed
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(
        table
            .columns(&renamed)
            .expect("t is in this database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<String>>(),
        ["id", "a", "renamed"]
    );

    let dropped = parse(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT,
             CONSTRAINT x EXCLUDE USING gist (a WITH =) INCLUDE (b));
         ALTER TABLE t DROP COLUMN b;",
    )
    .expect("the constraint goes with the column it includes");
    let table = dropped
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    assert_eq!(
        table
            .columns(&dropped)
            .expect("t is in this database")
            .map(|column| column.column_name().to_owned())
            .collect::<Vec<String>>(),
        ["id", "a"]
    );
}
