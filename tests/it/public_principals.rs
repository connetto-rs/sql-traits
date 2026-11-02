//! Tests that "applies to every role" is one question with one answer.
//!
//! SQL spells "everyone" as `PUBLIC`, and the grammar hands that back as an
//! ordinary unquoted name, so it is indistinguishable from a role somebody
//! created with that name unless the reader also checks the quoting. A policy
//! has a second way to say it, by writing no `TO` clause at all, and a grant
//! has a second spelling, because Microsoft SQL Server reserves the word and
//! the parser then falls back to reading it as a name. These tests pin all
//! four routes to the same answer.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::{MsSqlDialect, MySqlDialect, PostgreSqlDialect};

const POLICIES: &str = "CREATE ROLE \"PUBLIC\";
     CREATE ROLE reader;
     CREATE TABLE docs (id INT);
     CREATE POLICY spelled ON docs TO PUBLIC USING (true);
     CREATE POLICY implied ON docs USING (true);
     CREATE POLICY named ON docs TO reader USING (true);
     CREATE POLICY quoted ON docs TO \"PUBLIC\" USING (true);
     CREATE POLICY mixed ON docs TO reader, PUBLIC USING (true);";

fn policy_applies_to_public(name: &str) -> bool {
    let db = ParserDB::parse::<PostgreSqlDialect>(POLICIES).expect("schema builds");
    db.policies()
        .find(|policy| policy.name() == name)
        .expect("the policy exists")
        .applies_to_public()
}

#[test]
fn a_policy_says_everyone_two_ways() {
    assert!(policy_applies_to_public("spelled"));
    assert!(policy_applies_to_public("implied"), "an absent TO clause defaults to PUBLIC");
    assert!(policy_applies_to_public("mixed"), "one PUBLIC among named roles still means everyone");
}

#[test]
fn a_policy_naming_a_role_does_not_say_everyone() {
    assert!(!policy_applies_to_public("named"));
    assert!(
        !policy_applies_to_public("quoted"),
        "a quoted name is a role of that exact name, not the pseudo-role"
    );
}

/// The distinction the reader exists for: both policies below yield exactly one
/// role, and reading that role alone cannot tell them apart.
#[test]
fn the_role_iterator_cannot_answer_this() {
    let db = ParserDB::parse::<PostgreSqlDialect>(POLICIES).expect("schema builds");
    let roles = |name: &str| {
        db.policies()
            .find(|policy| policy.name() == name)
            .expect("the policy exists")
            .roles(&db)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };

    assert_eq!(roles("spelled"), ["PUBLIC"]);
    assert_eq!(roles("quoted"), ["\"PUBLIC\""]);
    assert!(roles("implied").is_empty(), "an absent TO clause is an empty iterator");
}

#[test]
fn a_grant_says_everyone_in_either_spelling() {
    let sql = "CREATE TABLE docs (id INT); GRANT SELECT ON docs TO PUBLIC;";

    let keyword = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds");
    let grant = keyword.table_grants().next().expect("the grant exists");
    assert!(grant.applies_to_public());

    // Microsoft SQL Server reserves the word, so the parser reads it as a name
    // rather than recording a public grantee.
    let reserved = ParserDB::parse::<MsSqlDialect>(sql).expect("schema builds");
    let grant = reserved.table_grants().next().expect("the grant exists");
    assert!(grant.applies_to_public(), "the reserved-word spelling means everyone too");
}

/// The rule `GrantLike::applies_to_role` states: a grant applying to everyone
/// applies to every role. Both spellings have to honour it, and the table
/// readers built on top of it have to agree.
#[test]
fn a_grant_to_everyone_applies_to_every_role() {
    let sql = "CREATE TABLE docs (id INT);
               CREATE ROLE reader;
               GRANT SELECT ON docs TO PUBLIC;";

    let keyword = ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds");
    let reserved = ParserDB::parse::<MsSqlDialect>(sql).expect("schema builds");

    for (label, db) in [("keyword spelling", &keyword), ("reserved-word spelling", &reserved)] {
        let grant = db.table_grants().next().expect("the grant exists");
        let reader = db.role("reader").expect("the role exists");
        let table = db
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the table exists");

        assert!(grant.applies_to_public(), "{label}");
        assert!(
            grant.applies_to_role(reader),
            "{label}: a grant to everyone applies to every role"
        );
        assert!(
            table.can_select(reader, db).expect("the table is known"),
            "{label}: the readers built on applies_to_role follow it"
        );
    }
}

#[test]
fn a_grant_naming_a_role_does_not_say_everyone() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE docs (id INT);
         CREATE ROLE reader;
         GRANT SELECT ON docs TO reader;",
    )
    .expect("schema builds");
    let grant = db.table_grants().next().expect("the grant exists");

    assert!(!grant.applies_to_public());

    let by_reference = &grant;
    assert!(!by_reference.applies_to_public());
}

#[test]
fn an_index_name_reads_back_with_its_quoting_and_qualifier() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE app.users (id INT, name TEXT);
         CREATE INDEX app.\"IdxName\" ON app.users (name);",
    )
    .expect("schema builds");
    let index = db.indexes().next().expect("the index exists");

    assert_eq!(index.name(), Some("IdxName"));
    assert!(index.name_is_quoted());
    assert_eq!(index.schema(), Some("app"));
    assert!(!index.schema_is_quoted());
}

#[test]
fn an_unqualified_index_name_reports_no_qualifier() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE users (id INT, name TEXT);
         CREATE INDEX idx_name ON users (name);",
    )
    .expect("schema builds");
    let index = db.indexes().next().expect("the index exists");

    assert_eq!(index.name(), Some("idx_name"));
    assert!(!index.name_is_quoted());
    assert_eq!(index.schema(), None);
    assert!(!index.schema_is_quoted());
}

/// A unique constraint reports the name it was declared with. PostgreSQL
/// builds the backing index under that name, so it is the index's name and not
/// a different thing wearing the same label.
#[test]
fn a_named_unique_constraint_reports_its_name() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE users (id INT, name TEXT,
             CONSTRAINT uq_id UNIQUE (id),
             CONSTRAINT \"UqName\" UNIQUE (name));",
    )
    .expect("schema builds");
    let table = db
        .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let names: Vec<_> = table
        .unique_indices(&db)
        .expect("unique indices")
        .map(|unique| (IndexLike::name(unique), IndexLike::name_is_quoted(unique)))
        .collect();

    assert_eq!(names, [(Some("uq_id"), false), (Some("UqName"), true)]);
}

/// MySQL spells the same thing as an index name rather than a constraint name,
/// and the parser keeps the two in separate fields.
#[test]
fn a_mysql_unique_key_reports_its_index_name() {
    let db = ParserDB::parse::<MySqlDialect>("CREATE TABLE users (id INT, UNIQUE KEY uq_id (id));")
        .expect("schema builds");
    let table = db
        .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let unique = table.unique_indices(&db).expect("unique indices").next().expect("one");

    assert_eq!(IndexLike::name(unique), Some("uq_id"));
}

/// A constraint the SQL left unnamed stays anonymous, and the quoting reader
/// has nothing to answer.
#[test]
fn an_unnamed_unique_constraint_is_anonymous() {
    let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE users (id INT, UNIQUE (id));")
        .expect("schema builds");
    let table = db
        .table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");

    for unique in table.unique_indices(&db).expect("unique indices") {
        assert_eq!(IndexLike::name(unique), None);
        assert!(!IndexLike::name_is_quoted(unique));
    }
}

/// A unique constraint name is a bare identifier in every dialect, so it never
/// carries a qualifier.
#[test]
fn a_unique_constraint_never_reports_a_qualifier() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE SCHEMA app;
         CREATE TABLE app.users (id INT, CONSTRAINT uq_id UNIQUE (id));",
    )
    .expect("schema builds");
    let table = db
        .table_by_target(
            TargetName::new("users", false).with_schema("app", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("the table exists");

    for unique in table.unique_indices(&db).expect("unique indices") {
        assert_eq!(IndexLike::schema(unique), None);
        assert!(!IndexLike::schema_is_quoted(unique));
    }
}
