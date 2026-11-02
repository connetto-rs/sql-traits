//! Tests that `DROP POLICY` removes the one policy the statement names.
//!
//! A policy name is unique per table rather than per database, and PostgreSQL
//! folds an unquoted identifier, so both halves of `DROP POLICY p ON t` decide
//! which policy goes. Dropping every policy that happens to share a name is a
//! silent loss of a row level security rule, which reads as an over-grant on
//! the table that kept none.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// Two tables carrying a policy of the same name, which is legal and is what
/// the name-only lookup cannot tell apart.
const SHARED_NAME: &str = "CREATE TABLE a (id INT PRIMARY KEY);
     CREATE TABLE b (id INT PRIMARY KEY);
     CREATE POLICY p ON a USING (id > 0);
     CREATE POLICY p ON b USING (id > 1);
";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn policy_tables(database: &ParserDB) -> Vec<String> {
    database
        .policies()
        .map(|policy| {
            policy.table(database).expect("the policy target resolves").table_name().to_string()
        })
        .collect()
}

#[test]
fn dropping_a_policy_spares_the_same_name_on_another_table() {
    let database = parse(&format!("{SHARED_NAME}DROP POLICY p ON a;")).expect("schema builds");

    assert_eq!(policy_tables(&database), ["b"]);
}

#[test]
fn a_statement_naming_a_table_the_policy_is_not_on_is_refused() {
    let error = parse(
        "CREATE TABLE a (id INT PRIMARY KEY);
         CREATE TABLE b (id INT PRIMARY KEY);
         CREATE POLICY p ON a USING (id > 0);
         DROP POLICY p ON b;",
    )
    .expect_err("b carries no policy called p");

    assert!(
        matches!(&error, Error::DropPolicyNotFound { policy_name } if policy_name == "p"),
        "got {error:?}"
    );
}

/// `IF EXISTS` forgives the absent policy, and forgiving it must not reach for
/// the one on the other table instead.
#[test]
fn if_exists_spares_the_same_name_on_another_table() {
    let database = parse(
        "CREATE TABLE a (id INT PRIMARY KEY);
         CREATE TABLE b (id INT PRIMARY KEY);
         CREATE POLICY p ON a USING (id > 0);
         DROP POLICY IF EXISTS p ON b;",
    )
    .expect("IF EXISTS forgives the absent policy");

    assert_eq!(policy_tables(&database), ["a"]);
}

/// The statement is matched on identifiers rather than on bytes, so PostgreSQL
/// folding decides: an unquoted name is case-insensitive and a quoted one is
/// exact.
#[test]
fn the_lookup_folds_the_names_the_two_statements_spell() {
    let created = "CREATE TABLE docs (id INT PRIMARY KEY);
         CREATE POLICY mypol ON docs USING (true);";

    let folded = parse(&format!("{created} DROP POLICY MYPOL ON DOCS;"))
        .expect("an unquoted name folds to the created one");
    assert_eq!(folded.policies().count(), 0);

    for tail in ["DROP POLICY \"MYPOL\" ON docs;", "DROP POLICY mypol ON \"DOCS\";"] {
        let error = parse(&format!("{created} {tail}"))
            .expect_err("a quoted name is exact, and neither spells mypol on docs");
        assert!(matches!(&error, Error::DropPolicyNotFound { .. }), "got {error:?} for {tail}");
    }
}

/// A `CREATE POLICY` and the `DROP POLICY` that follows it need not spell the
/// table the same way, since an unqualified name means schema `public`.
#[test]
fn the_two_statements_need_not_spell_the_table_the_same_way() {
    let spellings =
        [("public.docs", "docs"), ("docs", "public.docs"), ("public.docs", "public.docs")];

    for (created, dropped) in spellings {
        let database = parse(&format!(
            "CREATE TABLE public.docs (id INT PRIMARY KEY);
             CREATE POLICY docs_sel ON {created} USING (true);
             DROP POLICY docs_sel ON {dropped};"
        ))
        .expect("schema builds");

        assert_eq!(database.policies().count(), 0, "{created} / {dropped}");
    }
}

/// A name of more than two parts denotes no table this model can hold, so it
/// matches nothing rather than falling back to the policy name alone.
#[test]
fn a_table_name_no_lookup_can_denote_matches_no_policy() {
    let error = parse(
        "CREATE TABLE docs (id INT PRIMARY KEY);
         CREATE POLICY docs_sel ON docs USING (true);
         DROP POLICY docs_sel ON catalog.public.docs;",
    )
    .expect_err("a three-part name denotes no table");

    assert!(
        matches!(&error, Error::DropPolicyNotFound { policy_name } if policy_name == "docs_sel"),
        "got {error:?}"
    );
}

/// A `DROP POLICY` is resolved by the names the two statements spell rather
/// than by comparing resolved tables, and an unqualified name means schema
/// `public`, so the two spellings below denote the same policy.
#[test]
fn a_policy_dropped_through_another_spelling_of_its_table_still_drops() {
    let database = parse(
        "CREATE TABLE public.docs (id uuid PRIMARY KEY);
         CREATE POLICY docs_sel ON public.docs USING (true);
         DROP POLICY docs_sel ON docs;",
    )
    .expect("both statements denote the same table");

    assert_eq!(database.policies().count(), 0);
}
