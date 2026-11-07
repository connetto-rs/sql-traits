//! Tests that "applies to every role" is one question with one answer.
//!
//! SQL spells "everyone" as `PUBLIC`, and the grammar hands that back as an
//! ordinary name, so it is indistinguishable from a role somebody created with
//! that name unless the reader also checks the quoting. PostgreSQL folds the
//! name before it compares it with `public`, so a quoted `"public"` means
//! everyone too, while a quoted `"PUBLIC"` is a role of that exact name. A
//! policy has a second way to say it, by writing no `TO` clause at all, and a
//! grant has a second spelling, because Microsoft SQL Server reserves the word
//! and the parser then falls back to reading it as a name. These tests pin
//! every route to the same answer.
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
     CREATE POLICY lowercase_quoted ON docs TO \"public\" USING (true);
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
    // PostgreSQL 18.6 stores `polroles = {0}` for `TO "public"`, as for `TO
    // public`.
    assert!(policy_applies_to_public("lowercase_quoted"), "a quoted lowercase public is PUBLIC");
}

#[test]
fn a_policy_naming_a_role_does_not_say_everyone() {
    assert!(!policy_applies_to_public("named"));
    assert!(
        !policy_applies_to_public("quoted"),
        "a quoted name is a role of that exact name, not the pseudo-role"
    );
}

/// A policy that applies to every role names none, as `pg_policy.polroles`
/// stores only the public pseudo-role for it. PostgreSQL keeps that alone even
/// when the `TO` list names other roles beside `PUBLIC`.
#[test]
fn a_policy_applying_to_everyone_names_no_role() {
    let db = ParserDB::parse::<PostgreSqlDialect>(POLICIES).expect("schema builds");
    let roles = |name: &str| {
        db.policies()
            .find(|policy| policy.name() == name)
            .expect("the policy exists")
            .roles(&db)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };

    let none: Vec<String> = Vec::new();
    assert_eq!(roles("spelled"), none);
    assert_eq!(roles("implied"), none);
    assert_eq!(roles("lowercase_quoted"), none);
    assert_eq!(roles("mixed"), none, "PostgreSQL drops the roles listed beside PUBLIC");
    assert_eq!(roles("named"), ["reader"]);
    assert_eq!(roles("quoted"), ["\"PUBLIC\""]);
}

/// A role's policies are the ones naming it, so a policy whose named roles
/// PostgreSQL dropped for `PUBLIC` is not among them.
#[test]
fn a_role_listed_beside_public_is_not_named_by_the_policy() {
    let db = ParserDB::parse::<PostgreSqlDialect>(POLICIES).expect("schema builds");
    let reader = db.role("reader").expect("the role exists");

    let names: Vec<_> = reader.policies(&db).map(PolicyLike::name).collect();
    assert_eq!(names, ["named"]);
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
            table
                .privileges(reader, db)
                .map(|held| held.select.on_table())
                .expect("the table is known"),
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

/// The grantee spellings PostgreSQL 18.6 reads as `PUBLIC`, and the one it
/// reads as a role. `GRANT INSERT ON docs TO "public"` stores `=a/postgres`,
/// and `TO "PUBLIC"` stores `PUBLIC=d/postgres`.
const GRANTEE_SPELLINGS: [(&str, bool); 4] =
    [("public", true), ("\"public\"", true), ("GROUP public", true), ("\"PUBLIC\"", false)];

#[test]
fn a_grant_reads_each_spelling_as_postgresql_does() {
    for (spelling, everyone) in GRANTEE_SPELLINGS {
        let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE ROLE \"PUBLIC\";
             CREATE ROLE reader;
             CREATE TABLE docs (id INT);
             GRANT SELECT ON docs TO {spelling};"
        ))
        .expect("schema builds");
        let grant = db.table_grants().next().expect("the grant exists");
        let reader = db.role("reader").expect("the role exists");

        assert_eq!(grant.applies_to_public(), everyone, "{spelling}");
        assert_eq!(grant.applies_to_role(reader), everyone, "{spelling}");
    }
}

#[test]
fn a_revoke_reads_each_spelling_as_postgresql_does() {
    for (spelling, everyone) in GRANTEE_SPELLINGS {
        let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE ROLE \"PUBLIC\";
             CREATE TABLE docs (id INT);
             GRANT SELECT ON docs TO PUBLIC;
             REVOKE SELECT ON docs FROM {spelling};"
        ))
        .expect("schema builds");

        assert_eq!(db.table_grants().next().is_none(), everyone, "{spelling}");
    }
}

#[test]
fn a_revoke_from_public_takes_a_grant_to_any_spelling_of_it() {
    for (spelling, everyone) in GRANTEE_SPELLINGS {
        let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE ROLE \"PUBLIC\";
             CREATE TABLE docs (id INT);
             GRANT SELECT ON docs TO {spelling};
             REVOKE SELECT ON docs FROM PUBLIC;"
        ))
        .expect("schema builds");

        assert_eq!(db.table_grants().next().is_none(), everyone, "{spelling}");
    }
}

#[test]
fn a_function_revoke_reads_each_spelling_as_postgresql_does() {
    for (spelling, everyone) in GRANTEE_SPELLINGS {
        let db = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE ROLE \"PUBLIC\";
             CREATE FUNCTION f() RETURNS INT LANGUAGE sql AS 'SELECT 1';
             REVOKE EXECUTE ON FUNCTION f() FROM {spelling};"
        ))
        .expect("schema builds");
        let f = db
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the function exists");

        assert_eq!(f.is_executable_by_public(&db), Ok(!everyone), "{spelling}");
    }
}

#[test]
fn an_index_name_reads_back_with_its_quoting() {
    let db = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE TABLE users (id INT, name TEXT);
         CREATE INDEX \"IdxName\" ON users (name);
         CREATE INDEX idx_id ON users (id);",
    )
    .expect("schema builds");
    let mut names: Vec<_> =
        db.indexes().map(|index| (index.name(), index.name_is_quoted())).collect();
    names.sort_unstable();

    assert_eq!(names, [(Some("IdxName"), true), (Some("idx_id"), false)]);
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
