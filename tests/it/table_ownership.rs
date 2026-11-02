//! Tests that `ALTER TABLE ... OWNER TO` is recorded and reachable.
//!
//! `PostgreSQL` exempts a table's owner from every policy on it unless the
//! table also has forced Row Level Security, so a caller reporting that
//! exemption has to be able to name the exempt role rather than call it "the
//! table's owner". The operation used to parse and be discarded, which left the
//! question unanswerable.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{ast::CreateTable, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// A dump names the owning role while creating no role at all, which is the
/// case the permissive setting exists for, so these tests read under it.
fn db(sql: &str) -> ParserDB {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
        .expect("schema builds")
}

/// A table with row level security and an owner, reduced to the parts these
/// tests turn on. [`a_pg_dump_of_a_guarded_table_is_read_whole`] carries the
/// full shape a dump emits.
const DUMP: &str = "CREATE TABLE docs (id uuid PRIMARY KEY);
    ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
    ALTER TABLE docs OWNER TO app_owner;";

#[test]
fn an_owner_to_statement_names_the_owning_role() {
    let database = db(DUMP);
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");

    assert_eq!(docs.owner(&database), Ok(Some("app_owner")));
    assert_eq!(docs.has_row_level_security(&database), Ok(true));
    assert_eq!(
        docs.has_forced_row_level_security(&database),
        Ok(false),
        "the owner is exempt from every policy, which is why naming it matters"
    );
}

/// Ownership is a cluster-level fact a schema need not state, so its absence is
/// an answer rather than a failure.
#[test]
fn a_table_nobody_altered_names_no_owner() {
    let database = db("CREATE TABLE docs (id uuid PRIMARY KEY);");
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");

    assert_eq!(docs.owner(&database), Ok(None));
}

#[test]
fn ownership_moves_to_the_role_named_last() {
    let database = db("CREATE TABLE docs (id uuid PRIMARY KEY);
         ALTER TABLE docs OWNER TO first;
         ALTER TABLE docs OWNER TO second;");
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");

    assert_eq!(docs.owner(&database), Ok(Some("second")));
}

/// These three name whoever runs the statement, so the owner did change but to
/// a role the input never spells. Reporting the previous one would state
/// something the schema does not say.
#[test]
fn a_session_dependent_owner_leaves_no_role_named() {
    for spelling in ["CURRENT_ROLE", "CURRENT_USER", "SESSION_USER"] {
        let database = db(&format!(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             ALTER TABLE docs OWNER TO app_owner;
             ALTER TABLE docs OWNER TO {spelling};"
        ));
        let docs = database
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("docs exists");

        assert_eq!(docs.owner(&database), Ok(None), "{spelling} left a stale owner behind");
    }
}

/// Unquoted owners fold while quoted owners retain case.
#[test]
fn owner_preserves_role_identity() {
    for (spelling, expected) in [("App_Owner", "app_owner"), ("\"App_Owner\"", "App_Owner")] {
        let database = db(&format!(
            "CREATE TABLE docs (id uuid PRIMARY KEY);
             ALTER TABLE docs OWNER TO {spelling};"
        ));
        let docs = database
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("docs exists");

        assert_eq!(docs.owner(&database), Ok(Some(expected)));
    }

    let database = parse(
        "CREATE ROLE actor;
         CREATE ROLE \"ACTOR\" BYPASSRLS;
         CREATE TABLE docs (id uuid PRIMARY KEY);
         ALTER TABLE docs OWNER TO ACTOR;",
    )
    .expect("schema builds");
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");
    let owner = docs.owner(&database).expect("docs is in this database").expect("owner exists");
    let role = database.role(owner).expect("owner resolves");

    assert_eq!(owner, "actor");
    assert_eq!(role.name(), "actor");
    assert!(!role.can_bypass_rls());

    let database = parse(
        "CREATE ROLE App_Owner BYPASSRLS;
         CREATE TABLE docs (id uuid PRIMARY KEY);
         ALTER TABLE docs OWNER TO App_Owner;",
    )
    .expect("schema builds");
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");
    let owner = docs.owner(&database).expect("docs is in this database").expect("owner exists");
    let role = database.role(owner).expect("owner resolves");

    assert_eq!(owner, "app_owner");
    assert_eq!(role.name(), "App_Owner");
    assert!(role.can_bypass_rls());
}

/// A rename rebuilds the stored node, so anything hung off the old one has to
/// be carried across explicitly or it is silently lost.
#[test]
fn the_owner_follows_a_renamed_table() {
    let database = db("CREATE TABLE docs (id uuid PRIMARY KEY);
         ALTER TABLE docs OWNER TO app_owner;
         ALTER TABLE docs RENAME TO papers;");
    let papers = database
        .table_by_target(TargetName::new("papers", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the rename is applied");

    assert_eq!(papers.owner(&database), Ok(Some("app_owner")));
}

/// A column change rebuilds the stored node the same way a rename does.
#[test]
fn the_owner_survives_a_column_change() {
    let database = db("CREATE TABLE docs (id uuid PRIMARY KEY, size INT);
         ALTER TABLE docs OWNER TO app_owner;
         ALTER TABLE docs ALTER COLUMN size TYPE BIGINT;
         ALTER TABLE docs ADD COLUMN title TEXT;");
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");

    assert_eq!(docs.columns(&database).expect("docs is in this database").count(), 3);
    assert_eq!(docs.owner(&database), Ok(Some("app_owner")));
}

/// Ownership is recorded per table, so one table's owner never answers for
/// another's.
#[test]
fn each_table_answers_for_itself() {
    let database = db("CREATE TABLE docs (id uuid PRIMARY KEY);
         CREATE TABLE notes (id uuid PRIMARY KEY);
         ALTER TABLE docs OWNER TO app_owner;");

    assert_eq!(
        database
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("docs exists")
            .owner(&database),
        Ok(Some("app_owner"))
    );
    assert_eq!(
        database
            .table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("notes exists")
            .owner(&database),
        Ok(None)
    );
}

/// An owner is recorded against a table that exists, so naming an absent one is
/// the same mistake as any other `ALTER TABLE` against an absent table.
#[test]
fn an_owner_change_to_an_absent_table_is_reported() {
    let error = parse(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         ALTER TABLE absent OWNER TO app_owner;",
    )
    .expect_err("absent is never created");

    assert!(
        matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "absent"),
        "got {error:?}"
    );

    parse(
        "CREATE TABLE docs (id uuid PRIMARY KEY);
         ALTER TABLE IF EXISTS absent OWNER TO app_owner;",
    )
    .expect("IF EXISTS excuses the absent table");
}

/// Ownership follows the same setting as a grant, because it names a role for
/// the same reason and a dump omits the role for the same reason. The database
/// refuses an owner it cannot find, so the default does too, and the permissive
/// setting reads the dump.
#[test]
fn an_owner_is_resolved_against_the_roles_the_input_creates() {
    assert_eq!(ParseOptions::default().access_resolution(), AccessResolution::ClosedWorld);

    let refused = parse(DUMP);
    assert!(
        matches!(&refused, Err(Error::RoleNotFoundForOwner { role_name, object_name })
            if role_name == "app_owner" && object_name == "docs"),
        "got {refused:?}"
    );

    let created = parse(&format!("CREATE ROLE app_owner; {DUMP}")).expect("the role exists");
    assert_eq!(
        created
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("docs exists")
            .owner(&created),
        Ok(Some("app_owner"))
    );

    let dumped = db(DUMP);
    assert!(dumped.role("app_owner").is_none(), "nothing created the role");
    assert_eq!(
        dumped
            .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("docs exists")
            .owner(&dumped),
        Ok(Some("app_owner"))
    );
}

/// All three statements that name an owner answer alike: `ALTER TABLE`,
/// `ALTER SCHEMA` and `CREATE SCHEMA ... AUTHORIZATION`. A real PostgreSQL 16
/// refuses each of them for an absent role, which is why this crate does.
#[test]
fn every_ownership_statement_checks_its_role() {
    let cases = [
        ("CREATE TABLE docs (id INT); ALTER TABLE docs OWNER TO ghost;", "docs"),
        ("CREATE SCHEMA app; ALTER SCHEMA app OWNER TO ghost;", "app"),
        ("CREATE SCHEMA app AUTHORIZATION ghost;", "app"),
    ];

    for (sql, owned) in cases {
        let refused = parse(sql);
        assert!(
            matches!(&refused, Err(Error::RoleNotFoundForOwner { role_name, object_name })
                if role_name == "ghost" && object_name == owned),
            "{sql} reported {refused:?}"
        );

        parse(&format!("CREATE ROLE ghost; {sql}")).expect("the role exists");

        ParseOptions::default()
            .with_access_resolution(AccessResolution::OpenWorld)
            .parse::<PostgreSqlDialect>(sql)
            .expect("a dump names owners it never creates");
    }
}

/// The keyword owners name whoever runs the statement rather than a role, so
/// there is no role to look for and nothing to refuse.
#[test]
fn a_session_dependent_owner_needs_no_role() {
    for keyword in ["CURRENT_USER", "CURRENT_ROLE", "SESSION_USER"] {
        let parsed =
            parse(&format!("CREATE TABLE docs (id INT); ALTER TABLE docs OWNER TO {keyword};"));
        assert!(parsed.is_ok(), "{keyword} names no role, got {:?}", parsed.err());
    }
}

/// An absent table is reported before the role, matching the order the
/// database reports them in, and `IF EXISTS` skips the statement whole so the
/// role is never looked for either.
#[test]
fn the_table_is_checked_before_the_role() {
    let error = parse("CREATE TABLE docs (id INT); ALTER TABLE absent OWNER TO ghost;")
        .expect_err("absent is never created");
    assert!(
        matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "absent"),
        "got {error:?}"
    );

    parse("CREATE TABLE docs (id INT); ALTER TABLE IF EXISTS absent OWNER TO ghost;")
        .expect("IF EXISTS skips the statement, so no role is looked for");
}

/// The name lives in the database rather than in the table node, so a caller
/// may hand back a borrow of it that outlives the node it asked through. This
/// compiles only while the signature keeps the two lifetimes apart.
fn owner_of<'db>(database: &'db ParserDB, table: &CreateTable) -> Option<&'db str> {
    table.owner(database).ok().flatten()
}

#[test]
fn the_name_outlives_the_node_it_was_asked_through() {
    let database = db(DUMP);
    let node = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists")
        .clone();

    assert_eq!(owner_of(&database, &node), Some("app_owner"));
}

/// The motivating input, written the way `pg_dump` writes it: the table is
/// schema-qualified, the primary key arrives as a later `ALTER TABLE ONLY`, and
/// no `CREATE ROLE` ever appears because roles live outside the schema.
#[test]
fn a_pg_dump_of_a_guarded_table_is_read_whole() {
    let database = db("CREATE SCHEMA public;
         CREATE TABLE public.docs (
             id uuid NOT NULL,
             body text
         );
         ALTER TABLE public.docs OWNER TO app_owner;
         ALTER TABLE ONLY public.docs
             ADD CONSTRAINT docs_pkey PRIMARY KEY (id);
         ALTER TABLE public.docs ENABLE ROW LEVEL SECURITY;");
    let docs = database
        .table_by_target(
            TargetName::new("docs", false).with_schema("public", false),
            IdentifierCase::AsWritten,
        )
        .expect("unambiguous lookup")
        .expect("public.docs exists");

    assert_eq!(docs.owner(&database), Ok(Some("app_owner")));
    assert_eq!(docs.has_row_level_security(&database), Ok(true));
    assert_eq!(docs.has_forced_row_level_security(&database), Ok(false));
    assert_eq!(
        docs.primary_key_columns(&database)
            .expect("docs is in this database")
            .map(ColumnLike::column_name)
            .collect::<Vec<_>>(),
        vec!["id"],
        "the later ALTER TABLE ONLY did not displace the owner recorded before it"
    );
}

/// Generic code may hold a reference to a reference, which reaches the owner
/// through the blanket implementation for references rather than the table's
/// own. That forward is free to answer without ever consulting the table, so
/// it is pinned against the table's own answer.
#[test]
fn a_reference_to_a_table_answers_the_same() {
    let database = db(DUMP);
    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");

    assert_eq!(
        <&CreateTable as TableLike>::owner(&docs, &database),
        docs.owner(&database),
        "the blanket implementation for references disagrees with the table itself"
    );
    assert_eq!(<&CreateTable as TableLike>::owner(&docs, &database), Ok(Some("app_owner")));
}
