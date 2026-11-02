//! Tests that `ALTER FUNCTION ... OWNER TO` is recorded and reachable.
//!
//! A `SECURITY DEFINER` body reads its tables as the function's owner, so the
//! owner is what decides whose row policies filter that read. Without it a
//! caller can see that a body runs as its definer and still not say who the
//! definer is, which is the difference between translating a definer-wrapped
//! policy correctly and granting more than the schema does. The operation used
//! to parse and be discarded, which left the question unanswerable.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{Error, LookupError},
    prelude::*,
};
use sqlparser::{
    ast::{CreateFunction, FunctionSecurity},
    dialect::PostgreSqlDialect,
};

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

/// A definer-rights function handed to another role, reduced to the parts these
/// tests turn on.
const DUMP: &str = "CREATE FUNCTION f() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
    ALTER FUNCTION f() OWNER TO app_reader;";

#[test]
fn an_owner_to_statement_names_the_owning_role() {
    let database = db(DUMP);
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.owner(&database), Ok(Some("app_reader")));
    assert_eq!(
        function.security_mode(),
        FunctionSecurity::Definer,
        "the mode says the body runs as its definer, and the owner says who that is"
    );
}

/// Ownership is a cluster-level fact a schema need not state, so its absence is
/// an answer rather than a failure.
#[test]
fn a_function_nobody_reassigned_names_no_owner() {
    let database = db("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.owner(&database), Ok(None));
}

#[test]
fn ownership_moves_to_the_role_named_last() {
    let database = db("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO first;
         ALTER FUNCTION f() OWNER TO second;");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.owner(&database), Ok(Some("second")));
}

/// These three name whoever runs the statement, so the owner did change but to
/// a role the input never spells. Reporting the previous one would state
/// something the schema does not say.
#[test]
fn a_session_dependent_owner_leaves_no_role_named() {
    for spelling in ["CURRENT_ROLE", "CURRENT_USER", "SESSION_USER"] {
        let database = db(&format!(
            "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
             ALTER FUNCTION f() OWNER TO app_reader;
             ALTER FUNCTION f() OWNER TO {spelling};"
        ));
        let function = database
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("f exists");

        assert_eq!(function.owner(&database), Ok(None), "{spelling} left a stale owner behind");
    }
}

/// Unquoted owners fold while quoted owners retain case.
#[test]
fn owner_preserves_role_identity() {
    for (spelling, expected) in [("App_Reader", "app_reader"), ("\"App_Reader\"", "App_Reader")] {
        let database = db(&format!(
            "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
             ALTER FUNCTION f() OWNER TO {spelling};"
        ));
        let function = database
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("f exists");

        assert_eq!(function.owner(&database), Ok(Some(expected)));
    }

    let database = parse(
        "CREATE ROLE actor;
         CREATE ROLE \"ACTOR\" BYPASSRLS;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO ACTOR;",
    )
    .expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");
    let owner = function.owner(&database).expect("f is in this database").expect("owner exists");
    let role = database.role(owner).expect("owner resolves");

    assert_eq!(owner, "actor");
    assert_eq!(role.name(), "actor");
    assert!(!role.can_bypass_rls());

    let database = parse(
        "CREATE ROLE App_Reader BYPASSRLS;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO App_Reader;",
    )
    .expect("schema builds");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");
    let owner = function.owner(&database).expect("f is in this database").expect("owner exists");
    let role = database.role(owner).expect("owner resolves");

    assert_eq!(owner, "app_reader");
    assert_eq!(role.name(), "App_Reader");
    assert!(role.can_bypass_rls());
}

/// A statement naming no argument list reaches the only function carrying the
/// name, the way the security clause does.
#[test]
fn a_bare_name_reaches_the_only_function_carrying_it() {
    let database = db("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f OWNER TO app_reader;");

    assert_eq!(
        database
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("f exists")
            .owner(&database),
        Ok(Some("app_reader"))
    );
}

/// Overloads are separate functions, so an owner recorded against one signature
/// never answers for another.
#[test]
fn each_overload_answers_for_itself() {
    let database = db("CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 2';
         ALTER FUNCTION f(INT) OWNER TO app_reader;");

    let owners: Vec<_> = database
        .functions()
        .filter(|function| function.name() == "f")
        .map(|function| {
            (
                function.normalized_argument_type_names(&database),
                function.owner(&database).expect("f is in this database"),
            )
        })
        .collect();

    assert_eq!(owners, vec![(vec!["INT".into()], Some("app_reader")), (vec!["TEXT".into()], None)]);
}

/// PostgreSQL keeps the same `pg_proc` entry across a replacement, verified on
/// 18.4: the role set before `CREATE OR REPLACE` still owns the new definition.
/// The model rebuilds the stored node there, so the owner has to be carried
/// across explicitly or it is silently lost.
#[test]
fn the_owner_survives_a_replacement() {
    let database = db("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO app_reader;
         CREATE OR REPLACE FUNCTION f() RETURNS INT AS 'SELECT 2';");
    let function = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists");

    assert_eq!(function.body(), Some("SELECT 2"), "the replacement is the stored definition");
    assert_eq!(function.owner(&database), Ok(Some("app_reader")));
}

/// A security clause rewrites the stored node, so the owner recorded against it
/// has to survive that rewrite, in either order.
#[test]
fn the_owner_and_the_security_clause_do_not_displace_each_other() {
    let owner_first = db("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() OWNER TO app_reader;
         ALTER FUNCTION f() SECURITY DEFINER;");
    let security_first = db("CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION f() SECURITY DEFINER;
         ALTER FUNCTION f() OWNER TO app_reader;");

    for database in [owner_first, security_first] {
        let function = database
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("f exists");
        assert_eq!(function.owner(&database), Ok(Some("app_reader")));
        assert_eq!(function.security_mode(), FunctionSecurity::Definer);
    }
}

/// Policies cache the function nodes their expressions call, resolved when the
/// policy was created, so the owner has to be reachable through that cache and
/// not only through the canonical store. This is the path a caller translating
/// a definer-wrapped policy actually walks.
#[test]
fn the_owner_is_visible_through_a_policy_cached_node() {
    let database = db("CREATE TABLE t (id INT);
         CREATE FUNCTION guard(x INT) RETURNS BOOLEAN LANGUAGE sql SECURITY DEFINER AS 'SELECT true';
         CREATE POLICY p ON t USING (guard(id));
         ALTER FUNCTION guard(INT) OWNER TO app_reader;");

    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let policy = table.policies(&database).expect("policies").next().expect("p exists");
    let via_policy = policy
        .using_functions(&database)
        .expect("using functions")
        .next()
        .expect("guard is resolved");

    assert_eq!(via_policy.owner(&database), Ok(Some("app_reader")));
    assert_eq!(via_policy.security_mode(), FunctionSecurity::Definer);
}

/// Check constraints cache resolved function nodes the same way policies do.
#[test]
fn the_owner_is_visible_through_a_check_cached_node() {
    let database = db("CREATE FUNCTION guard(x INT) RETURNS BOOLEAN AS 'SELECT true';
         CREATE TABLE t (id INT, CHECK (guard(id)));
         ALTER FUNCTION guard(INT) OWNER TO app_reader;");

    let table = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("t exists");
    let check =
        table.check_constraints(&database).expect("check constraints").next().expect("check");
    let via_check =
        check.functions(&database).expect("check functions").next().expect("guard is resolved");

    assert_eq!(via_check.owner(&database), Ok(Some("app_reader")));
}

/// A dropped function takes its metadata with it, so a node kept across the
/// drop reports that it is no longer in the database rather than an owner the
/// database no longer holds.
#[test]
fn a_dropped_function_reports_that_it_is_gone() {
    let database = db(DUMP);
    let node = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists")
        .clone();
    let dropped = db(&format!("{DUMP} DROP FUNCTION f();"));

    assert!(
        dropped
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .is_none(),
        "the drop is applied"
    );
    assert!(
        matches!(node.owner(&dropped), Err(LookupError::ObjectNotInDatabase { .. })),
        "got {:?}",
        node.owner(&dropped)
    );
}

/// An owner is recorded against a function that exists, so naming an absent one
/// is the same mistake as a security clause naming one, which this statement
/// already refuses.
#[test]
fn an_owner_change_to_an_absent_function_is_reported() {
    let error = parse(
        "CREATE ROLE app_reader;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION absent() OWNER TO app_reader;",
    )
    .expect_err("absent is never created");

    assert!(
        matches!(&error, Error::AlterFunctionNotFound { function_name } if function_name == "absent"),
        "got {error:?}"
    );
}

/// A bare name covering more than one overload cannot say which one to hand
/// over, so it is refused rather than resolved to the first.
#[test]
fn an_ambiguous_owner_change_is_reported() {
    let error = parse(
        "CREATE ROLE app_reader;
         CREATE FUNCTION f(x INT) RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION f(x TEXT) RETURNS INT AS 'SELECT 2';
         ALTER FUNCTION f OWNER TO app_reader;",
    )
    .expect_err("the name covers two functions");

    assert!(
        matches!(&error, Error::AmbiguousAlterFunction { function_name } if function_name == "f"),
        "got {error:?}"
    );
}

/// A bare name reaches the function in the schema it names, `public` when it
/// writes none, whatever other schemas hold under that name.
#[test]
fn an_owner_change_by_bare_name_stays_in_its_schema() {
    let database = parse(
        "CREATE ROLE app_reader;
         CREATE SCHEMA app;
         CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         CREATE FUNCTION app.f() RETURNS INT AS 'SELECT 2';
         ALTER FUNCTION app.f OWNER TO app_reader;",
    )
    .expect("app.f is the only function app.f names");

    let owner_in = |schema: Option<&str>| {
        database
            .functions()
            .find(|function| function.name() == "f" && function.target_name().schema() == schema)
            .expect("the function exists")
            .owner(&database)
    };
    assert_eq!(owner_in(Some("app")), Ok(Some("app_reader")));
    assert_eq!(owner_in(None), Ok(None));
}

/// Ownership follows the same setting as a grant, because it names a role for
/// the same reason and a dump omits the role for the same reason. A real
/// PostgreSQL 18.4 refuses `ALTER FUNCTION f() OWNER TO no_such_role`, which is
/// why the default does.
#[test]
fn an_owner_is_resolved_against_the_roles_the_input_creates() {
    assert_eq!(ParseOptions::default().access_resolution(), AccessResolution::ClosedWorld);

    let refused = parse(DUMP);
    assert!(
        matches!(&refused, Err(Error::RoleNotFoundForOwner { role_name, object_name })
            if role_name == "app_reader" && object_name == "f"),
        "got {refused:?}"
    );

    let created = parse(&format!("CREATE ROLE app_reader; {DUMP}")).expect("the role exists");
    assert_eq!(
        created
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("f exists")
            .owner(&created),
        Ok(Some("app_reader"))
    );

    let dumped = db(DUMP);
    assert!(dumped.role("app_reader").is_none(), "nothing created the role");
    assert_eq!(
        dumped
            .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("f exists")
            .owner(&dumped),
        Ok(Some("app_reader"))
    );
}

/// The keyword owners name whoever runs the statement rather than a role, so
/// there is no role to look for and nothing to refuse.
#[test]
fn a_session_dependent_owner_needs_no_role() {
    for keyword in ["CURRENT_USER", "CURRENT_ROLE", "SESSION_USER"] {
        let parsed = parse(&format!(
            "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
             ALTER FUNCTION f() OWNER TO {keyword};"
        ));
        assert!(parsed.is_ok(), "{keyword} names no role, got {:?}", parsed.err());
    }
}

/// An absent function is reported before the role, matching the order the
/// database reports them in.
#[test]
fn the_function_is_checked_before_the_role() {
    let error = parse(
        "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
         ALTER FUNCTION absent() OWNER TO ghost;",
    )
    .expect_err("absent is never created");

    assert!(
        matches!(&error, Error::AlterFunctionNotFound { function_name } if function_name == "absent"),
        "got {error:?}"
    );
}

/// An aggregate arrives as the very same statement as a function, and no
/// aggregate can be in this model because `CREATE AGGREGATE` is rejected by the
/// parser. A real PostgreSQL 18.4 refuses to reach a function through
/// `ALTER AGGREGATE` (`function agg_sfunc(integer, integer) is not an
/// aggregate`), so a same-named function must not answer for one, and reading
/// the statement and dropping it would hide an ownership change.
#[test]
fn an_owner_change_written_against_an_aggregate_is_refused() {
    let error = parse(
        "CREATE ROLE app_reader;
         CREATE FUNCTION my_agg(x INT) RETURNS INT AS 'SELECT 1';
         ALTER AGGREGATE my_agg(INT) OWNER TO app_reader;",
    )
    .expect_err("this model holds no aggregates");

    assert!(
        matches!(&error, Error::AggregateOwnerUnsupported { aggregate_name }
            if aggregate_name == "my_agg"),
        "got {error:?}"
    );
}

/// The name lives in the database rather than in the function node, so a caller
/// may hand back a borrow of it that outlives the node it asked through. This
/// compiles only while the signature keeps the two lifetimes apart.
fn owner_of<'db>(database: &'db ParserDB, function: &CreateFunction) -> Option<&'db str> {
    function.owner(database).ok().flatten()
}

#[test]
fn the_name_outlives_the_node_it_was_asked_through() {
    let database = db(DUMP);
    let node = database
        .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("f exists")
        .clone();

    assert_eq!(owner_of(&database, &node), Some("app_reader"));
}

/// The motivating input: a table guarded by a policy, and a definer-rights
/// function handed to a role that is not the table's owner, which is the one
/// case where the definer body is filtered like anybody else. Probed on
/// PostgreSQL 18.4: a `USING (false)` policy filters nothing for the table's
/// owner and yields zero rows through a function owned by a third role.
#[test]
fn a_definer_function_reassigned_away_from_the_table_owner_is_visible() {
    let database = db("CREATE TABLE docs (id uuid PRIMARY KEY, body text);
         ALTER TABLE docs OWNER TO app_owner;
         ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY docs_none ON docs USING (false);
         CREATE FUNCTION read_docs() RETURNS SETOF uuid LANGUAGE sql SECURITY DEFINER
             AS 'SELECT id FROM docs';
         ALTER FUNCTION read_docs() OWNER TO app_reader;");

    let docs = database
        .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("docs exists");
    let reader = database
        .function_by_target(TargetName::new("read_docs", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("read_docs exists");

    assert_eq!(docs.owner(&database), Ok(Some("app_owner")));
    assert_eq!(reader.security_mode(), FunctionSecurity::Definer);
    assert_eq!(
        reader.owner(&database),
        Ok(Some("app_reader")),
        "the reassignment is what breaks the inference that the definer owns the table"
    );
    assert_ne!(
        reader.owner(&database).expect("read_docs is in this database"),
        docs.owner(&database).expect("docs is in this database"),
        "the definer is filtered by the policy, because it does not own the table"
    );
}
