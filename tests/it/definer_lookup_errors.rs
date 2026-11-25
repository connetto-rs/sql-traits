//! How lookup failures and unresolvable references inside a definer chain reach
//! the consumer.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::{errors::LookupError, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::definer_reads::{deployment, function_read};

/// Open-world parsing preserves references whose lookup the consumer must
/// report.
fn open_schema(sql: &str) -> ParserDB {
    ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(sql)
        .expect("the open-world schema parses")
}

/// A function's invalid reference reaches the public read consumer.
fn assert_function_error(database: &ParserDB, name: &str) {
    let function =
        database.functions().find(|function| function.name() == name).expect("the function exists");
    let role = ActingRole::Declared(database.role("app").expect("app exists"));
    assert!(matches!(
        function.unconfined_definer_read(database, &deployment(), &[role]),
        Err(LookupError::InvalidObjectName { .. })
    ));
}

/// An invoker propagates a reached definer's relation lookup error.
#[test]
fn invoker_definer_relation_lookup_error() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE FUNCTION helper() RETURNS INT LANGUAGE sql SECURITY DEFINER
             AS $$ SELECT id FROM cluster.database.schema.secrets $$;
         REVOKE EXECUTE ON FUNCTION helper() FROM PUBLIC;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE sql AS $$ SELECT helper() $$;",
    );
    assert_function_error(&database, "reader");
}

/// A definer's unresolved relation propagates its lookup error.
#[test]
fn definer_relation_lookup_error() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE sql SECURITY DEFINER
             AS $$ SELECT id FROM cluster.database.schema.secrets $$;",
    );
    assert_function_error(&database, "reader");
}

/// An invalid mutation target leaves the definer body unfollowed.
#[test]
fn definer_write_lookup_is_unfollowed() {
    for language in ["sql", "plpgsql"] {
        let body = if language == "sql" {
            "INSERT INTO cluster.database.schema.secrets VALUES (1);"
        } else {
            "BEGIN INSERT INTO cluster.database.schema.secrets VALUES (1); END"
        };
        let database = open_schema(&format!(
            "CREATE ROLE app;
             CREATE FUNCTION reader() RETURNS VOID LANGUAGE {language} SECURITY DEFINER
                 AS $$ {body} $$;"
        ));
        assert_eq!(
            function_read(&database, None, "reader"),
            Some((String::new(), "unfollowed".into()))
        );
    }
}

/// Policy lookup errors survive nested read and write traversals.
#[test]
fn nested_policy_lookup_errors() {
    for body in [
        "SELECT id FROM secrets",
        "SELECT (SELECT id FROM secrets)",
        "INSERT INTO sink SELECT id FROM secrets",
        "UPDATE sink SET id = (SELECT id FROM secrets)",
        "DELETE FROM sink WHERE EXISTS (SELECT id FROM secrets)",
    ] {
        let database = open_schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             CREATE TABLE sink (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app
                 USING (EXISTS (SELECT 1 FROM cluster.database.schema.allowed));
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION reader() RETURNS VOID LANGUAGE sql SECURITY DEFINER
                 SET search_path = public, pg_temp AS $$ {body} $$;"
        ));
        assert_function_error(&database, "reader");
    }
}

/// An unresolved procedural alias leaves the body unfollowed.
#[test]
fn unresolved_procedural_alias_is_unfollowed() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE plpgsql SECURITY DEFINER AS $$
         DECLARE missing ALIAS FOR absent;
         BEGIN RETURN 1; END $$;",
    );
    assert_eq!(
        function_read(&database, None, "reader"),
        Some((String::new(), "unfollowed".into()))
    );
}

/// An unenforced key cannot carry a helper's tenant filter onto a protected
/// row.
#[test]
fn unenforced_helper_key_carries_no_filter() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY, tenant INT, UNIQUE (id, tenant));
         CREATE TABLE allowed (id INT PRIMARY KEY, tenant INT,
             FOREIGN KEY (id, tenant) REFERENCES secrets (id, tenant) NOT ENFORCED);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets, allowed TO app;
         CREATE FUNCTION listed() RETURNS SETOF INT LANGUAGE sql STABLE
             SET search_path = public, pg_temp AS $$
             SELECT id FROM allowed WHERE tenant = 7 $$;
         CREATE FUNCTION reader() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
             SELECT id FROM secrets WHERE id IN (SELECT listed()) $$;",
    );
    assert_eq!(
        function_read(&database, None, "reader"),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Lookup errors in a fired trigger propagate through the writing function.
#[test]
fn written_trigger_lookup_error() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE TABLE incoming (id INT);
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE sql SECURITY DEFINER AS $$
             SELECT id FROM cluster.database.schema.secrets $$;
         CREATE TRIGGER guard AFTER INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();
         CREATE FUNCTION reader() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
             INSERT INTO incoming VALUES (1) $$;",
    );
    assert_function_error(&database, "reader");
}

/// Uncertain definer write targets prevent a trigger from trusting insertion
/// policies.
#[test]
fn uncertain_write_targets_invalidate_trigger_facts() {
    for statement in [
        "UPDATE (SELECT id FROM incoming) target SET id = 1",
        "INSERT INTO FUNCTION sink() VALUES (1)",
    ] {
        let database = ParseOptions::default()
            .with_access_resolution(AccessResolution::OpenWorld)
            .parse::<sqlparser::dialect::GenericDialect>(&format!(
                "CREATE ROLE app;
                 CREATE TABLE incoming (id INT);
                 ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (id = 7);
                 CREATE POLICY visible ON incoming FOR SELECT TO app USING (id = 7);
                 GRANT SELECT, INSERT ON incoming TO app;
                 CREATE FUNCTION writer() RETURNS VOID LANGUAGE sql SECURITY DEFINER
                     AS $$ {statement} $$;
                 CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
                     SET search_path = public, pg_temp AS $$
                     BEGIN
                         IF EXISTS (SELECT 1 FROM incoming WHERE id = NEW.id) THEN
                             RAISE EXCEPTION 'present';
                         END IF;
                         RETURN NEW;
                     END $$;
                 CREATE TRIGGER guard AFTER INSERT ON incoming
                     FOR EACH ROW EXECUTE FUNCTION guard();"
            ))
            .expect("the schema parses");
        assert_eq!(
            crate::support::definer_reads::trigger_read(
                &database,
                "guard",
                DefinerReading::Outcome
            ),
            Some(("incoming".into(), "visible".into())),
        );
    }
}

/// Invalid runtime paths propagate from invoker and definer body walkers.
#[test]
fn invalid_runtime_search_paths_are_errors() {
    for (language, security, body) in [
        ("sql", "", "SET search_path = a.b; SELECT 1"),
        ("sql", "SECURITY DEFINER", "SET search_path = a.b; SELECT 1"),
        ("plpgsql", "SECURITY DEFINER", "BEGIN SET search_path = a.b; RETURN 1; END"),
    ] {
        let database = open_schema(&format!(
            "CREATE ROLE app;
             CREATE FUNCTION reader() RETURNS INT LANGUAGE {language} {security}
                 AS $$ {body} $$;"
        ));
        let function = database
            .functions()
            .find(|function| function.name() == "reader")
            .expect("reader exists");
        let role = ActingRole::Declared(database.role("app").expect("app exists"));
        assert!(matches!(
            function.unconfined_definer_read(&database, &deployment(), &[role]),
            Err(LookupError::InvalidSetting { .. })
        ));
    }
}

/// An opaque statement leaves an SQL definer body unfollowed.
#[test]
fn opaque_sql_statement_is_unfollowed() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE FUNCTION reader() RETURNS VOID LANGUAGE sql SECURITY DEFINER
             AS $$ CALL external_procedure() $$;",
    );
    assert_eq!(
        function_read(&database, None, "reader"),
        Some((String::new(), "unfollowed".into()))
    );
}

/// A refused stable helper still propagates errors while checking policy
/// release.
#[test]
fn stable_helper_release_lookup_error() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE FUNCTION helper() RETURNS INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
             INSERT INTO absent VALUES (1);
             SET search_path = a.b;
             SELECT 1 $$;
         REVOKE EXECUTE ON FUNCTION helper() FROM PUBLIC;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE sql SECURITY DEFINER
             AS $$ SELECT helper() $$;",
    );
    let function =
        database.functions().find(|function| function.name() == "reader").expect("reader exists");
    let role = ActingRole::Declared(database.role("app").expect("app exists"));
    assert!(matches!(
        function.unconfined_definer_read(&database, &deployment(), &[role]),
        Err(LookupError::InvalidSetting { .. })
    ));
}

/// An unresolved parameterized source cannot prove containment between enum
/// arguments.
#[test]
fn unresolved_ladder_body_proves_no_containment() {
    let database = open_schema(
        "CREATE ROLE app;
         CREATE TYPE access_level AS ENUM ('reader', 'writer');
         CREATE FUNCTION listed(minimum access_level) RETURNS SETOF INT
             LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public, pg_temp AS $$
             SELECT id FROM absent(minimum) $$;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app
             USING (id IN (SELECT listed('reader')));
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION reader() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
             SELECT id FROM secrets WHERE id IN (SELECT listed('writer')) $$;",
    );
    assert_eq!(
        function_read(&database, None, "reader"),
        Some(("secrets".into(), "visible".into()))
    );
}
