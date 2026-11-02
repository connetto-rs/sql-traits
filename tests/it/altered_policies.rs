//! Tests that `ALTER POLICY` applies every clause it carries.
//!
//! A policy is created once and then tuned, so the expression a schema ends on
//! lives in an `ALTER POLICY` rather than in the `CREATE POLICY` that a dump of
//! the same database would show. Keeping the superseded expression is worse
//! than refusing the statement, because a policy decides who may read a row:
//! `ALTER POLICY p ON t USING (false)` leaves a database that admits nobody
//! and, when the clause is dropped, a model that still reports the rule it
//! replaced.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

/// A guarded table carrying one policy, so each clause below has both a
/// present and an absent case to land on: it starts with
/// `USING (owner_id = current_user)`, no `WITH CHECK` clause and the `PUBLIC`
/// pseudo-role.
const GUARDED: &str = "CREATE ROLE app_user;
     CREATE TABLE docs (id uuid PRIMARY KEY, owner_id TEXT);
     ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
     CREATE POLICY docs_sel ON docs FOR SELECT TO PUBLIC USING (owner_id = current_user);
";

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn altered(tail: &str) -> ParserDB {
    parse(&format!("{GUARDED}{tail}")).expect("schema builds")
}

fn policy<'db>(database: &'db ParserDB, name: &str) -> &'db sqlparser::ast::CreatePolicy {
    database.policies().find(|policy| policy.name() == name).expect("the policy exists")
}

fn using(database: &ParserDB, name: &str) -> Option<String> {
    policy(database, name).using_expression(database).map(ToString::to_string)
}

fn check(database: &ParserDB, name: &str) -> Option<String> {
    policy(database, name).check_expression(database).map(ToString::to_string)
}

fn roles(database: &ParserDB, name: &str) -> Vec<String> {
    policy(database, name).roles(database).map(ToString::to_string).collect()
}

#[test]
fn a_using_clause_replaces_the_expression_the_policy_was_created_with() {
    assert_eq!(
        using(&altered("ALTER POLICY docs_sel ON docs USING (false);"), "docs_sel").as_deref(),
        Some("false")
    );
    assert_eq!(
        using(&altered("ALTER POLICY docs_sel ON docs USING (true);"), "docs_sel").as_deref(),
        Some("true")
    );
}

#[test]
fn a_with_check_clause_adds_the_one_the_create_policy_never_had() {
    let database = altered("ALTER POLICY docs_sel ON docs WITH CHECK (false);");

    assert_eq!(check(&database, "docs_sel").as_deref(), Some("false"));
}

/// The mirror of the case above: a clause the policy never carried is added
/// rather than merged into an existing one.
#[test]
fn a_using_clause_adds_the_one_the_create_policy_never_had() {
    let database = parse(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE POLICY docs_ins ON docs FOR INSERT WITH CHECK (id IS NOT NULL);
         ALTER POLICY docs_ins ON docs USING (false);",
    )
    .expect("schema builds");

    assert_eq!(using(&database, "docs_ins").as_deref(), Some("false"));
    assert_eq!(check(&database, "docs_ins").as_deref(), Some("id IS NOT NULL"));
}

#[test]
fn a_to_clause_rewrites_the_roles() {
    let database = altered("ALTER POLICY docs_sel ON docs TO app_user;");

    assert_eq!(roles(&database, "docs_sel"), ["app_user"]);
}

/// `PostgreSQL` applies each clause on its own, so an omitted one is left as it
/// was rather than cleared.
#[test]
fn a_clause_the_statement_omits_survives_untouched() {
    let database = altered("ALTER POLICY docs_sel ON docs WITH CHECK (false);");

    assert_eq!(using(&database, "docs_sel").as_deref(), Some("owner_id = current_user"));
    assert_eq!(roles(&database, "docs_sel"), ["PUBLIC"]);
}

#[test]
fn one_statement_applies_every_clause_it_carries() {
    let database =
        altered("ALTER POLICY docs_sel ON docs TO app_user USING (false) WITH CHECK (true);");

    assert_eq!(using(&database, "docs_sel").as_deref(), Some("false"));
    assert_eq!(check(&database, "docs_sel").as_deref(), Some("true"));
    assert_eq!(roles(&database, "docs_sel"), ["app_user"]);
}

#[test]
fn the_statement_named_last_is_the_one_that_stands() {
    let database = altered(
        "ALTER POLICY docs_sel ON docs USING (false);
         ALTER POLICY docs_sel ON docs USING (id IS NOT NULL);",
    );

    assert_eq!(using(&database, "docs_sel").as_deref(), Some("id IS NOT NULL"));
}

/// The sibling operation in the same statement family, kept here because a
/// shared lookup now resolves both.
#[test]
fn a_rename_still_lands_and_the_renamed_policy_still_alters() {
    let database = altered(
        "ALTER POLICY docs_sel ON docs RENAME TO docs_renamed;
         ALTER POLICY docs_renamed ON docs USING (false);",
    );

    assert_eq!(database.policies().count(), 1);
    assert_eq!(using(&database, "docs_renamed").as_deref(), Some("false"));
}

/// A policy name is unique per table rather than per database, so the table the
/// statement names is what picks the policy out.
#[test]
fn the_policy_altered_is_the_one_on_the_table_the_statement_names() {
    let database = parse(
        "CREATE TABLE a (id INT PRIMARY KEY);
         CREATE TABLE b (id INT PRIMARY KEY);
         CREATE POLICY p ON a USING (id > 0);
         CREATE POLICY p ON b USING (id > 0);
         ALTER POLICY p ON b USING (false);",
    )
    .expect("schema builds");

    let expression = |table: &str| {
        database
            .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the table exists")
            .policies(&database)
            .expect("the table is in this database")
            .map(|policy| {
                policy.using_expression(&database).map(ToString::to_string).expect("a USING clause")
            })
            .collect::<Vec<_>>()
    };

    assert_eq!(expression("a"), ["id > 0"]);
    assert_eq!(expression("b"), ["false"]);
}

/// The two statements are matched on identifiers rather than on bytes, so
/// `PostgreSQL` folding decides: an unquoted name is case-insensitive and a
/// quoted one is exact.
#[test]
fn the_lookup_folds_the_names_the_two_statements_spell() {
    let folded = altered("ALTER POLICY DOCS_SEL ON DOCS USING (false);");
    assert_eq!(using(&folded, "docs_sel").as_deref(), Some("false"));

    for tail in [
        "ALTER POLICY \"DOCS_SEL\" ON docs USING (false);",
        "ALTER POLICY docs_sel ON \"DOCS\" USING (false);",
    ] {
        let error = parse(&format!("{GUARDED}{tail}"))
            .expect_err("a quoted name is exact, and neither spells docs_sel on docs");
        assert!(matches!(&error, Error::AlterPolicyNotFound { .. }), "got {error:?} for {tail}");
    }
}

#[test]
fn a_statement_naming_a_table_the_policy_is_not_on_is_refused() {
    let error = parse(
        "CREATE TABLE a (id INT PRIMARY KEY);
         CREATE TABLE b (id INT PRIMARY KEY);
         CREATE POLICY p ON a USING (id > 0);
         ALTER POLICY p ON b USING (false);",
    )
    .expect_err("b carries no policy called p");

    assert!(
        matches!(&error, Error::AlterPolicyNotFound { policy_name } if policy_name == "p"),
        "got {error:?}"
    );
}

const CALLS_A_FUNCTION: &str = "CREATE TABLE docs (id uuid PRIMARY KEY);
     CREATE FUNCTION was_created() RETURNS BOOLEAN AS 'SELECT true';
     CREATE FUNCTION was_altered() RETURNS BOOLEAN AS 'SELECT true';
     CREATE POLICY docs_sel ON docs USING (was_created());
     ALTER POLICY docs_sel ON docs USING (was_altered()) WITH CHECK (was_created());
";

/// The functions a policy reads are recorded beside it rather than read back
/// off the expression, so a replaced clause has to bring its own.
#[test]
fn the_functions_a_replaced_expression_reads_replace_the_recorded_ones() {
    let database = parse(CALLS_A_FUNCTION).expect("schema builds");
    let docs_sel = policy(&database, "docs_sel");

    let names = |functions: &mut dyn Iterator<Item = &sqlparser::ast::CreateFunction>| {
        functions.map(|function| function.name().to_string()).collect::<Vec<_>>()
    };

    assert_eq!(
        names(&mut docs_sel.using_functions(&database).expect("docs_sel is in this database")),
        ["was_altered"]
    );
    assert_eq!(
        names(&mut docs_sel.check_functions(&database).expect("docs_sel is in this database")),
        ["was_created"]
    );
}

/// The recorded functions are what refuses a `DROP FUNCTION`, so a stale record
/// both guards a function nothing reads and drops one a policy calls.
#[test]
fn dropping_a_function_follows_the_replaced_expression() {
    let error = parse(&format!("{CALLS_A_FUNCTION} DROP FUNCTION was_altered();"))
        .expect_err("the altered policy calls was_altered");
    assert!(
        matches!(&error, Error::FunctionReferenced { function_name } if function_name == "was_altered"),
        "got {error:?}"
    );

    let dropped = parse(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE FUNCTION was_created() RETURNS BOOLEAN AS 'SELECT true';
         CREATE FUNCTION was_altered() RETURNS BOOLEAN AS 'SELECT true';
         CREATE POLICY docs_sel ON docs USING (was_created());
         ALTER POLICY docs_sel ON docs USING (was_altered());
         DROP FUNCTION was_created();",
    )
    .expect("nothing reads was_created once the policy stops calling it");
    assert_eq!(dropped.functions().filter(|f| f.name() == "was_created").count(), 0);
}

/// A column a policy reads is a dependency `DROP COLUMN` refuses without
/// `CASCADE`, which only answers correctly if the stored expression is the
/// current one.
#[test]
fn a_column_dependency_follows_the_replaced_expression() {
    let dropped = altered(
        "ALTER POLICY docs_sel ON docs USING (id IS NOT NULL);
         ALTER TABLE docs DROP COLUMN owner_id;",
    );
    let docs = dropped
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");
    assert_eq!(
        docs.columns(&dropped)
            .expect("docs is in this database")
            .map(|column| column.column_name().to_string())
            .collect::<Vec<_>>(),
        ["id"]
    );
    assert_eq!(dropped.policies().count(), 1);

    let error = parse(&format!(
        "{GUARDED}ALTER POLICY docs_sel ON docs USING (owner_id IS NOT NULL);
         ALTER TABLE docs DROP COLUMN owner_id;"
    ))
    .expect_err("the altered policy still reads owner_id");
    assert!(matches!(&error, Error::ColumnReferenced { .. }), "got {error:?}");
}

/// A `TO` clause names roles exactly as `CREATE POLICY` does, so the setting
/// that governs one governs the other.
#[test]
fn a_to_clause_naming_an_absent_role_is_refused_by_default() {
    let error = parse(&format!("{GUARDED}ALTER POLICY docs_sel ON docs TO missing_role;"))
        .expect_err("missing_role is not created");

    assert!(
        matches!(&error, Error::RoleNotFoundForPolicy { role_name, policy_name }
            if role_name == "missing_role" && policy_name == "docs_sel"),
        "got {error:?}"
    );
}

#[test]
fn the_open_world_accepts_a_to_clause_naming_a_role_it_does_not_create() {
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(&format!(
            "{GUARDED}ALTER POLICY docs_sel ON docs TO missing_role;"
        ))
        .expect("the open world records the policy as written");

    assert_eq!(roles(&database, "docs_sel"), ["missing_role"]);
    assert!(matches!(
        database
            .unresolved_access_references()
            .expect("targets are well formed")
            .collect::<Vec<_>>()
            .as_slice(),
        [UnresolvedAccessReference::PolicyRole { policy, role }]
            if policy.value == "docs_sel" && role.value == "missing_role"
    ));
}

/// A `CREATE POLICY` and the `ALTER POLICY` that follows it need not spell the
/// table the same way, since an unqualified name means schema `public`.
#[test]
fn the_two_statements_need_not_spell_the_table_the_same_way() {
    let spellings =
        [("public.docs", "docs"), ("docs", "public.docs"), ("public.docs", "public.docs")];

    for (created, altered) in spellings {
        let database = parse(&format!(
            "CREATE TABLE public.docs (id uuid PRIMARY KEY);
             CREATE POLICY docs_sel ON {created} USING (true);
             ALTER POLICY docs_sel ON {altered} USING (false);"
        ))
        .expect("schema builds");

        assert_eq!(using(&database, "docs_sel").as_deref(), Some("false"), "{created} / {altered}");
    }
}

/// A name of more than two parts denotes no table this model can hold, so it
/// matches nothing rather than falling back to the policy name alone.
#[test]
fn a_table_name_no_lookup_can_denote_matches_no_policy() {
    let error = parse(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE POLICY docs_sel ON docs USING (true);
         ALTER POLICY docs_sel ON catalog.public.docs USING (false);",
    )
    .expect_err("a three-part name denotes no table");

    assert!(
        matches!(&error, Error::AlterPolicyNotFound { policy_name } if policy_name == "docs_sel"),
        "got {error:?}"
    );
}

/// An `ALTER POLICY` is resolved by the names the two statements spell rather
/// than by comparing resolved tables, and an unqualified name means schema
/// `public`, so the two spellings below denote the same policy.
#[test]
fn a_policy_altered_through_another_spelling_of_its_table_still_alters() {
    let database = parse(
        "CREATE TABLE public.docs (id uuid PRIMARY KEY);
         CREATE POLICY docs_sel ON public.docs USING (true);
         ALTER POLICY docs_sel ON docs USING (false);",
    )
    .expect("both statements denote the same table");

    assert_eq!(using(&database, "docs_sel").as_deref(), Some("false"));
}
