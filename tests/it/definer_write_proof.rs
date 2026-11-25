//! How a definer function's writes, calls and bodies are followed.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;

use crate::support::definer_reads::{deployment, described, function_read, schema, trigger_read};

/// An invoker trigger follows private definer callees under both read
/// contracts.
#[test]
fn invoker_trigger_follows_private_definer_reads() {
    for predicate in ["true", "id > 0"] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE TABLE writes (id INT);
             GRANT INSERT ON writes TO app;
             CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql STABLE SECURITY DEFINER
               SET search_path = pg_catalog AS $$
                 SELECT id FROM public.secrets WHERE {predicate} $$;
             REVOKE EXECUTE ON FUNCTION inspect() FROM PUBLIC;
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY INVOKER AS $$
               BEGIN
                 IF public.inspect() = 1 THEN RAISE EXCEPTION 'blocked'; END IF;
                 RETURN NEW;
               END $$;
             CREATE TRIGGER guarded BEFORE INSERT ON writes
               FOR EACH ROW EXECUTE FUNCTION guard();"
        ));
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "guarded", reading),
                (predicate == "true").then(|| ("secrets".to_owned(), "visible".to_owned())),
                "{predicate}, {reading:?}"
            );
        }
    }
}

/// A directly callable definer adds no disclosure through an invoker trigger.
#[test]
fn invoker_trigger_distinguishes_disclosure_from_outcome() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE writes (id INT);
         GRANT INSERT ON writes TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN
             IF public.inspect() = 1 THEN RAISE EXCEPTION 'blocked'; END IF;
             RETURN NEW;
           END $$;
         CREATE TRIGGER guarded BEFORE INSERT ON writes
           FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(trigger_read(&database, "guarded", DefinerReading::Disclosure), None);
    assert_eq!(
        trigger_read(&database, "guarded", DefinerReading::Outcome),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Recursion through private functions cannot establish a definer read proof.
#[test]
fn private_definer_recursion_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE FUNCTION recursive_read() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
           AS $$ SELECT public.recursive_read() $$;
         REVOKE EXECUTE ON FUNCTION recursive_read() FROM PUBLIC;
         CREATE FUNCTION entry() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
           AS $$ SELECT public.recursive_read() $$;",
    );
    assert_eq!(
        function_read(&database, None, "entry"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// An invoker call cycle without definer reads exposes no owner-only rows.
#[test]
fn invoker_trigger_cycle_has_no_definer_reads() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE writes (id INT);
         GRANT INSERT ON writes TO app;
         CREATE FUNCTION cycle() RETURNS INT LANGUAGE sql AS $$ SELECT public.cycle() $$;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN PERFORM public.cycle(); RETURN NEW; END $$;
         CREATE TRIGGER guarded BEFORE INSERT ON writes
           FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(trigger_read(&database, "guarded", DefinerReading::Outcome), None);
    assert_eq!(trigger_read(&database, "guarded", DefinerReading::Disclosure), None);
}

/// Unreadable trigger and function languages cannot establish confinement.
#[test]
fn unreadable_bodies_are_unfollowed() {
    for language in ["c", "plpgsql", "sql"] {
        let body = match language {
            "c" => "external_symbol",
            "plpgsql" => "BEGIN broken syntax; END",
            _ => "SELECT FROM",
        };
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE writes (id INT);
             GRANT INSERT ON writes TO app;
             CREATE FUNCTION opaque() RETURNS trigger LANGUAGE {language}
               SECURITY DEFINER AS '{body}';
             CREATE TRIGGER guarded BEFORE INSERT ON writes
               FOR EACH ROW EXECUTE FUNCTION opaque();"
        ));
        assert_eq!(
            trigger_read(&database, "guarded", DefinerReading::Outcome),
            Some((String::new(), "unfollowed".to_owned())),
            "{language}"
        );
        assert_eq!(
            function_read(&database, None, "opaque"),
            Some((String::new(), "unfollowed".to_owned())),
            "{language}"
        );
    }
}

/// A definer setting session state cannot reuse the caller's row predicate.
#[test]
fn definer_session_settings_invalidate_shared_predicates() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (id = current_setting('app.id')::INT);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER
           SET app.id = '1' AS $$
             SELECT id FROM public.secrets WHERE id = current_setting('app.id')::INT $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// Every statement in a multi-statement SQL body must confine its reads.
#[test]
fn sql_statement_bodies_check_reads_and_writes() {
    for body in [
        "AS $$ SELECT 1; SELECT id FROM public.secrets $$",
        "BEGIN ATOMIC SELECT 1; SELECT id FROM public.secrets; END",
        "RETURN (SELECT id FROM public.secrets)",
        "AS $$ UPDATE public.secrets SET id = id RETURNING id $$",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER {body};"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            Some(if body.starts_with("RETURN") {
                (String::new(), "unfollowed".to_owned())
            } else {
                ("secrets".to_owned(), "visible".to_owned())
            }),
            "{body}"
        );
    }
}

/// Conflict updates and every modifying merge arm follow their fired triggers.
#[test]
fn conflict_and_merge_follow_update_and_delete_triggers() {
    for (statement, event) in [
        ("INSERT INTO public.writes VALUES (1) ON CONFLICT (id) DO UPDATE SET id = 2", "UPDATE"),
        (
            "MERGE INTO public.writes target USING public.source source ON target.id = source.id
             WHEN MATCHED THEN UPDATE SET id = source.id",
            "UPDATE",
        ),
        (
            "MERGE INTO public.writes target USING public.source source ON target.id = source.id
             WHEN MATCHED THEN DELETE",
            "DELETE",
        ),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE TABLE writes (id INT PRIMARY KEY);
             CREATE TABLE source (id INT);
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
               BEGIN
                 IF EXISTS (SELECT 1 FROM public.secrets) THEN
                   RAISE EXCEPTION 'hidden';
                 END IF;
                 RETURN NEW;
               END $$;
             CREATE TRIGGER guarded AFTER {event} ON writes
               FOR EACH ROW EXECUTE FUNCTION guard();
             CREATE FUNCTION write_rows() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
               {statement} $$;"
        ));
        assert_eq!(
            function_read(&database, None, "write_rows"),
            Some(("secrets".to_owned(), "visible".to_owned())),
            "{statement}"
        );
    }
}

/// Nonmodifying conflict and merge arms never fire update triggers.
#[test]
fn nonmodifying_arms_do_not_fire_update_triggers() {
    for statement in [
        "INSERT INTO public.writes VALUES (1) ON CONFLICT (id) DO NOTHING",
        "MERGE INTO public.writes target USING public.source source ON target.id = source.id
         WHEN MATCHED THEN DO NOTHING",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (false);
             CREATE TABLE writes (id INT PRIMARY KEY);
             CREATE TABLE source (id INT);
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
               BEGIN PERFORM 1 FROM public.secrets; RETURN NEW; END $$;
             CREATE TRIGGER guarded AFTER UPDATE ON writes
               FOR EACH ROW EXECUTE FUNCTION guard();
             CREATE FUNCTION write_rows() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
               {statement} $$;"
        ));
        assert_eq!(function_read(&database, None, "write_rows"), None, "{statement}");
    }
}

/// A write to an inherited table cannot ignore triggers on its descendants.
#[test]
fn inherited_write_targets_are_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE parent (id INT);
         CREATE TABLE child () INHERITS (parent);
         CREATE FUNCTION write_rows() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
           UPDATE public.parent SET id = 1 $$;",
    );
    assert_eq!(
        function_read(&database, None, "write_rows"),
        Some(("parent".to_owned(), "unfollowed".to_owned()))
    );
}

/// A truncate whose target cannot resolve has no confinement proof.
#[test]
fn unresolved_truncate_target_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE FUNCTION write_rows() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
           TRUNCATE public.missing $$;",
    );
    assert_eq!(
        function_read(&database, None, "write_rows"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A read requires privileges on every column of its source row.
#[test]
fn hidden_columns_refuse_an_otherwise_admitted_row() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT, hidden TEXT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (true);
         GRANT SELECT (id) ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "hidden".to_owned()))
    );
}

/// A caller exempt from row security can read every granted row.
#[test]
fn bypass_role_reads_are_confined() {
    let database = schema(
        "CREATE ROLE app BYPASSRLS;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (false);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;",
    );
    assert_eq!(function_read(&database, None, "inspect"), None);
}

/// A read without any applicable policy remains unconfined.
#[test]
fn missing_select_policy_refuses_reads() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), String::new()))
    );
}

/// A stable private reader may release only the set admitted by the exposed
/// function's policy.
#[test]
fn private_reads_release_through_the_exposed_policy_set() {
    for (projection, policy, confined) in [
        ("SELECT public.private_ids()", "id IN (SELECT public.visible_ids())", true),
        ("SELECT public.private_ids()", "id > 0", false),
        ("SELECT public.private_ids() + 1", "id IN (SELECT public.visible_ids())", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION private_ids() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.secrets WHERE pg_catalog.abs(id) >= 0 $$;
             CREATE FUNCTION other_ids() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.secrets WHERE id < 0 $$;
             REVOKE EXECUTE ON FUNCTION private_ids() FROM PUBLIC;
             REVOKE EXECUTE ON FUNCTION other_ids() FROM PUBLIC;
             CREATE FUNCTION visible_ids() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$ {projection} $$;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING ({policy});"
        ));
        assert_eq!(
            function_read(&database, None, "visible_ids"),
            (!confined).then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{projection}, {policy}"
        );
    }
}

/// A stable snapshot preserves confinement when a nested volatile call returns
/// a constant.
#[test]
fn volatile_private_readers_cannot_release_policy_sets() {
    for (volatility, nested) in [("VOLATILE", ""), ("STABLE", "WHERE public.changes() > 0")] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION changes() RETURNS INT LANGUAGE sql VOLATILE SECURITY DEFINER
               AS $$ SELECT 1 $$;
             CREATE FUNCTION private_ids() RETURNS SETOF INT LANGUAGE sql {volatility}
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.secrets {nested} $$;
             REVOKE EXECUTE ON FUNCTION private_ids() FROM PUBLIC;
             CREATE FUNCTION visible_ids() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT public.private_ids() $$;
             CREATE POLICY visible ON secrets FOR SELECT TO app
               USING (id IN (SELECT public.visible_ids()));"
        ));
        assert_eq!(
            function_read(&database, None, "visible_ids"),
            (volatility == "VOLATILE").then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{volatility}, {nested}"
        );
    }
}

/// A scalar wrapper cannot claim that its output is an admitted row set.
#[test]
fn scalar_wrappers_cannot_release_policy_sets() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION private_id() RETURNS INT LANGUAGE sql STABLE SECURITY DEFINER
           AS $$ SELECT id FROM public.secrets $$;
         REVOKE EXECUTE ON FUNCTION private_id() FROM PUBLIC;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql STABLE SECURITY DEFINER
           AS $$ SELECT public.private_id() $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Every enum argument selects a rung admitted by the reader policy.
#[test]
fn enum_parameter_domains_imply_read_policies() {
    for strictness in ["STRICT", "CALLED ON NULL INPUT"] {
        for helper_strictness in ["STRICT", "RETURNS NULL ON NULL INPUT"] {
            let database = schema(&format!(
                "CREATE ROLE app;
                 CREATE TYPE audience AS ENUM ('reader', 'writer');
                 CREATE TABLE memberships (id INT, audience audience);
                 CREATE FUNCTION members(requested audience) RETURNS SETOF INT
                   LANGUAGE sql STABLE {helper_strictness} SECURITY DEFINER
                   SET search_path = pg_catalog AS $$
                     SELECT id FROM public.memberships WHERE audience >= requested $$;
                 CREATE TABLE secrets (id INT);
                 ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY visible ON secrets FOR SELECT TO app
                   USING (id IN (SELECT public.members('reader')));
                 GRANT SELECT ON secrets TO app;
                 CREATE FUNCTION inspect(requested audience) RETURNS SETOF INT
                   LANGUAGE sql STABLE {strictness} SECURITY DEFINER AS $$
                     SELECT id FROM public.secrets
                     WHERE id IN (SELECT public.members(requested)) $$;"
            ));
            assert_eq!(
                function_read(&database, None, "inspect"),
                None,
                "{strictness}, {helper_strictness}"
            );
        }
    }
}

/// Session-dependent set filters cannot establish a caller-independent row
/// restriction.
#[test]
fn session_privilege_filters_do_not_confine_set_reads() {
    for predicate in [
        "has_table_privilege('public.secrets', 'SELECT')",
        "current_schema() = 'public'",
        "pg_has_role('app', 'MEMBER')",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING ({predicate});
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION listed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
               AS $$ SELECT id FROM public.secrets WHERE {predicate} $$;
             CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
               AS $$ SELECT id FROM public.secrets WHERE id IN (SELECT public.listed()) $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            Some(("secrets".to_owned(), "visible".to_owned())),
            "{predicate}"
        );
    }
}

/// An invoker trigger with an opaque body cannot rule out private definer
/// calls.
#[test]
fn opaque_invoker_trigger_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE writes (id INT);
         GRANT INSERT ON writes TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE c AS 'guard_symbol';
         CREATE TRIGGER guarded BEFORE INSERT ON writes
           FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guarded", DefinerReading::Disclosure),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A dynamic procedural write cannot establish which trigger chains execute.
#[test]
fn dynamic_procedural_body_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE FUNCTION write_rows(command TEXT) RETURNS VOID LANGUAGE plpgsql
           SECURITY DEFINER AS $$ BEGIN EXECUTE command; END $$;",
    );
    assert_eq!(
        function_read(&database, None, "write_rows"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A polymorphic unused argument does not weaken a concrete row restriction.
#[test]
fn polymorphic_parameters_preserve_independent_row_restrictions() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect(unused ANYELEMENT) RETURNS SETOF INT LANGUAGE sql
           SECURITY DEFINER AS $$ SELECT id FROM public.secrets WHERE id > 0 $$;",
    );
    assert_eq!(function_read(&database, None, "inspect"), None);
}

/// A subquery-dependent filter cannot supply caller-independent set lineage.
#[test]
fn relational_set_filters_do_not_supply_shared_conditions() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         CREATE TABLE allowed (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (id = (SELECT id FROM public.allowed));
         GRANT SELECT ON secrets, allowed TO app;
         CREATE FUNCTION listed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT id FROM public.secrets WHERE id = (SELECT id FROM public.allowed) $$;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT id FROM public.secrets WHERE id IN (SELECT public.listed()) $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// An invoker trigger may use catalog functions without creating definer reads.
#[test]
fn invoker_trigger_catalog_calls_are_confined() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE writes (id INT);
         GRANT INSERT ON writes TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN
             IF pg_catalog.abs(NEW.id) > 10 THEN RAISE EXCEPTION 'large'; END IF;
             RETURN NEW;
           END $$;
         CREATE TRIGGER guarded BEFORE INSERT ON writes
           FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(trigger_read(&database, "guarded", DefinerReading::Outcome), None);
}

/// Public callers receive no extra disclosure from a publicly executable
/// definer.
#[test]
fn public_execute_privileges_bound_callee_disclosure() {
    for (privileges, confined) in
        [("", true), ("REVOKE EXECUTE ON FUNCTION private_id() FROM PUBLIC;", false)]
    {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO PUBLIC USING (id > 0);
             GRANT SELECT ON secrets TO PUBLIC;
             CREATE FUNCTION private_id() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
               SELECT id FROM public.secrets $$;
             {privileges}
             CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
               SELECT public.private_id() $$;"
        ));
        let function = database
            .function_by_target(TargetName::new("inspect", false), IdentifierCase::AsWritten)
            .expect("the function name resolves")
            .expect("the function exists");
        let deployment = deployment();
        let outcome = function
            .unconfined_definer_read(&database, &deployment, &[ActingRole::Public])
            .expect("the schema reads");
        assert_eq!(
            outcome.as_ref().map(described),
            (!confined).then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{privileges}"
        );
    }
}

/// An arbitrary helper result does not establish membership in the policy's
/// set.
#[test]
fn helpers_without_set_projection_do_not_confine_reads() {
    for body in [
        "LANGUAGE sql STABLE AS $$ SELECT 1 $$",
        "LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN NEXT 1; END $$",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TYPE audience AS ENUM ('reader', 'writer');
             CREATE TABLE memberships (id INT, audience audience);
             CREATE FUNCTION members(requested audience) RETURNS SETOF INT
               LANGUAGE sql STABLE STRICT SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.memberships WHERE audience >= requested $$;
             CREATE FUNCTION arbitrary() RETURNS SETOF INT SECURITY DEFINER
               SET search_path = pg_catalog {body};
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app
               USING (id IN (SELECT public.members('reader')));
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
               SELECT id FROM public.secrets WHERE id IN (SELECT public.arbitrary()) $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            Some(("secrets".to_owned(), "visible".to_owned())),
            "{body}"
        );
    }
}

/// A nullable enum argument to a nonstrict helper can reveal a hidden
/// null-ranked row.
#[test]
fn nonstrict_enum_sets_do_not_confine_null_arguments() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TYPE audience AS ENUM ('reader', 'writer');
         CREATE TABLE memberships (id INT, audience audience);
         CREATE FUNCTION members(requested audience) RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.memberships
             WHERE requested IS NULL OR audience >= requested $$;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (id IN (SELECT public.members('reader')));
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect(requested audience) RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER AS $$
             SELECT id FROM public.secrets WHERE id IN (SELECT public.members(requested)) $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// An output parameter prevents a wrapper from representing an ordinary
/// policy-set call.
#[test]
fn output_parameters_do_not_release_private_reads() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION private_ids() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
           AS $$ SELECT id FROM public.secrets $$;
         REVOKE EXECUTE ON FUNCTION private_ids() FROM PUBLIC;
         CREATE FUNCTION inspect(OUT selected INT) RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER AS $$ SELECT public.private_ids() $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// A stable declaration cannot establish the lineage of an opaque private
/// function.
#[test]
fn opaque_private_sets_are_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE FUNCTION private_ids() RETURNS SETOF INT LANGUAGE c STABLE
           SECURITY DEFINER AS 'private_symbol';
         REVOKE EXECUTE ON FUNCTION private_ids() FROM PUBLIC;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT public.private_ids() $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A recursively expanded policy supplies no admitted-row proof.
#[test]
fn recursive_select_policies_are_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (id IN (SELECT id FROM public.secrets));
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "unfollowed".to_owned()))
    );
}

/// A returned target row must satisfy the caller's read policy.
#[test]
fn returned_update_and_delete_targets_require_select_policies() {
    for (statement, refused) in [
        ("DELETE FROM public.secrets RETURNING id", true),
        ("UPDATE public.secrets SET id = id WHERE id > 0 RETURNING id", false),
        ("DELETE FROM public.secrets WHERE id > 0 RETURNING id", false),
        ("UPDATE public.secrets SET id = id RETURNING id; SELECT 1", true),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
               {statement}
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            refused.then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{statement}"
        );
    }
}

/// A returned row assigned to a procedural variable discloses the target row.
#[test]
fn procedural_returning_into_requires_select_policies() {
    for (condition, refused) in [("true", true), ("id > 0", false)] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS VOID LANGUAGE plpgsql SECURITY DEFINER AS $$
             DECLARE returned_id INT;
             BEGIN
               UPDATE public.secrets SET id = id WHERE {condition} RETURNING id INTO returned_id;
             END $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            refused.then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{condition}"
        );
    }
}

/// Reading `FOUND` exposes whether a protected write selected any row.
#[test]
fn procedural_found_requires_select_policies_for_functions_and_triggers() {
    for (condition, refused) in [("true", true), ("id > 0", false)] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE TABLE requests (id INT PRIMARY KEY);
             GRANT INSERT ON requests TO app;
             CREATE FUNCTION inspect() RETURNS VOID LANGUAGE plpgsql SECURITY DEFINER AS $$
             BEGIN
               UPDATE public.secrets SET id = id WHERE {condition};
               IF FOUND THEN RAISE EXCEPTION 'matched'; END IF;
             END $$;
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER AS $$
             BEGIN
               UPDATE public.secrets SET id = id WHERE {condition};
               IF FOUND THEN RAISE EXCEPTION 'matched'; END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER guarded BEFORE INSERT ON requests
               FOR EACH ROW EXECUTE FUNCTION guard();"
        ));
        let expected = refused.then(|| ("secrets".to_owned(), "visible".to_owned()));
        assert_eq!(function_read(&database, None, "inspect"), expected, "{condition}");
        assert_eq!(
            trigger_read(&database, "guarded", DefinerReading::Outcome),
            expected,
            "{condition}"
        );
    }
}

/// Reading the affected-row count exposes rows selected by a protected write.
#[test]
fn procedural_row_count_requires_select_policies() {
    for (condition, refused) in [("true", true), ("id > 0", false)] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS VOID LANGUAGE plpgsql SECURITY DEFINER AS $$
             DECLARE affected_rows BIGINT;
             BEGIN
               UPDATE public.secrets SET id = id WHERE {condition};
               GET DIAGNOSTICS affected_rows = ROW_COUNT;
               IF affected_rows > 0 THEN RAISE EXCEPTION 'matched'; END IF;
             END $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            refused.then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{condition}"
        );
    }
}

/// A write whose result nobody reads need not establish a target read policy.
#[test]
fn bare_updates_do_not_require_select_policies() {
    for (language, body) in [
        ("sql", "UPDATE public.secrets SET id = id"),
        ("plpgsql", "BEGIN UPDATE public.secrets SET id = id; END"),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS VOID LANGUAGE {language} SECURITY DEFINER AS $$
               {body}
             $$;"
        ));
        assert_eq!(function_read(&database, None, "inspect"), None, "{language}");
    }
}

/// Conflict updates returning existing rows need their own confining condition.
#[test]
fn conflict_update_returning_requires_select_policies() {
    for (condition, refused) in [("", true), ("WHERE secrets.id > 0", false)] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
               INSERT INTO public.secrets VALUES (-1)
               ON CONFLICT (id) DO UPDATE SET id = secrets.id {condition}
               RETURNING id
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            refused.then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{condition}"
        );
    }
}

/// Plain insert returning exposes only the rows the statement inserts.
#[test]
fn insert_returning_does_not_require_select_policies() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
           INSERT INTO public.secrets VALUES (-1) RETURNING id
         $$;",
    );
    assert_eq!(function_read(&database, None, "inspect"), None);
}

/// A returning merge cannot establish a target read restriction.
#[test]
fn merge_returning_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
           MERGE INTO public.secrets target USING (SELECT -1 AS id) source ON target.id = source.id
           WHEN MATCHED THEN UPDATE SET id = source.id
           RETURNING target.id
         $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "unfollowed".to_owned()))
    );
}

/// Observing a merge into an unprotected table preserves confined reads.
#[test]
fn observed_merge_into_unprotected_table_is_confined() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE audit (id INT PRIMARY KEY);
         CREATE FUNCTION inspect() RETURNS BOOLEAN LANGUAGE plpgsql SECURITY DEFINER AS $$
         BEGIN
           PERFORM id FROM public.secrets WHERE id > 0;
           MERGE INTO public.audit a USING (SELECT 1 AS id) s ON a.id = s.id
             WHEN NOT MATCHED THEN INSERT VALUES (s.id);
           RETURN FOUND;
         END $$;",
    );
    assert_eq!(function_read(&database, None, "inspect"), None);
}

/// A private stable membership read discloses only identifiers released by the
/// exposed policy set.
#[test]
fn direct_row_projection_releases_private_membership_sets() {
    for (policy, confined) in [("id IN (SELECT public.visible_ids())", true), ("id > 100", false)] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION private_ids() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER AS $$ SELECT id FROM public.secrets $$;
             REVOKE EXECUTE ON FUNCTION private_ids() FROM PUBLIC;
             CREATE FUNCTION visible_ids() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER AS $$
                 SELECT id FROM public.secrets
                 WHERE id > 0 AND id IN (SELECT public.private_ids()) $$;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING ({policy});"
        ));
        assert_eq!(
            function_read(&database, None, "visible_ids"),
            (!confined).then(|| ("secrets".to_owned(), "visible".to_owned())),
            "{policy}"
        );
    }
}

/// A private reader that writes once per hidden row cannot release only its
/// returned set.
#[test]
fn nested_volatile_writes_prevent_policy_set_release() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE TABLE audit (id INT);
         GRANT SELECT ON secrets, audit TO app;
         CREATE FUNCTION record_read() RETURNS INT LANGUAGE plpgsql VOLATILE
           SECURITY DEFINER AS $$
             BEGIN INSERT INTO public.audit VALUES (1); RETURN 1; END $$;
         CREATE FUNCTION private_ids() RETURNS SETOF INT LANGUAGE sql STABLE
           SECURITY DEFINER AS $$
             SELECT id FROM public.secrets WHERE public.record_read() = 1 $$;
         REVOKE EXECUTE ON FUNCTION private_ids() FROM PUBLIC;
         CREATE FUNCTION visible_ids() RETURNS SETOF INT LANGUAGE sql STABLE
           SECURITY DEFINER AS $$
             SELECT id FROM public.secrets
             WHERE id > 0 AND id IN (SELECT public.private_ids()) $$;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (id IN (SELECT public.visible_ids()));",
    );
    assert_eq!(
        function_read(&database, None, "visible_ids"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// A policy bound to an unknown creation role cannot prove the acting role's
/// visibility.
#[test]
fn current_user_policy_roles_are_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO CURRENT_USER USING (true);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "unfollowed".to_owned()))
    );
}

/// Disjoint catalog-generated sets cannot establish the policy's admitted
/// identifiers.
#[test]
fn catalog_set_helpers_do_not_imply_disjoint_membership() {
    for members in ["pg_catalog.generate_series(4, 6)", "public.generated()"] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             GRANT SELECT ON secrets TO app;
             CREATE POLICY visible ON secrets FOR SELECT TO app
               USING (id IN (SELECT pg_catalog.generate_series(1, 3)));
             CREATE FUNCTION generated() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER AS $$ SELECT pg_catalog.generate_series(4, 6) $$;
             CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
               SELECT id FROM public.secrets WHERE id IN (SELECT {members}) $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            Some(("secrets".to_owned(), "visible".to_owned())),
            "{members}"
        );
    }
}

/// Existence of an owner-only row cannot supply a caller-independent set
/// restriction.
#[test]
fn constant_subquery_projection_preserves_role_dependence() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE allowed (id INT);
         ALTER TABLE allowed ENABLE ROW LEVEL SECURITY;
         CREATE POLICY allowed_visible ON allowed FOR SELECT TO app USING (false);
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (EXISTS (SELECT 1 FROM public.allowed));
         GRANT SELECT ON secrets, allowed TO app;
         CREATE FUNCTION listed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT id FROM public.secrets WHERE EXISTS (SELECT 1 FROM public.allowed) $$;
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT id FROM public.secrets WHERE id IN (SELECT public.listed()) $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Constant expression bodies disclose no protected rows.
#[test]
fn parsed_expression_bodies_without_reads_are_confined() {
    for body in ["AS RETURN (1)", "AS RETURN SELECT 1"] {
        let database = ParserDB::parse::<sqlparser::dialect::MsSqlDialect>(&format!(
            "CREATE ROLE app;
             CREATE FUNCTION inspect() RETURNS INT {body};
             ALTER FUNCTION inspect() SECURITY DEFINER;"
        ))
        .expect("the schema parses");
        assert_eq!(function_read(&database, None, "inspect"), None, "{body}");
    }
}

/// Unsupported write targets cannot establish which triggers execute.
#[test]
fn nonordinary_write_targets_are_unfollowed() {
    for statement in [
        "DELETE writes FROM public.writes",
        "DELETE FROM public.writes, public.other",
        "UPDATE (SELECT id FROM public.writes) target SET id = 1",
        "INSERT INTO FUNCTION sink() VALUES (1)",
    ] {
        let database = ParserDB::parse::<sqlparser::dialect::GenericDialect>(&format!(
            "CREATE ROLE app;
             CREATE TABLE writes (id INT);
             CREATE TABLE other (id INT);
             CREATE FUNCTION inspect() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
               {statement} $$;"
        ))
        .expect("the schema parses");
        assert_eq!(
            function_read(&database, None, "inspect"),
            Some((String::new(), "unfollowed".to_owned())),
            "{statement}"
        );
    }
}

/// Duplicate-key updates follow update triggers before proving reads.
#[test]
fn duplicate_key_updates_follow_update_triggers() {
    let database = ParserDB::parse::<sqlparser::dialect::GenericDialect>(
        "CREATE ROLE app;
         CREATE TABLE writes (id INT PRIMARY KEY);
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN EXECUTE 'SELECT 1'; RETURN NEW; END $$;
         CREATE TRIGGER guarded AFTER UPDATE ON writes
           FOR EACH ROW EXECUTE FUNCTION guard();
         CREATE FUNCTION inspect() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
           INSERT INTO public.writes VALUES (1) ON DUPLICATE KEY UPDATE id = 2 $$;",
    )
    .expect("the schema parses");
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// Open-world parsing still requires each trigger function to exist.
#[test]
fn unresolved_write_trigger_is_rejected() {
    let result = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<sqlparser::dialect::PostgreSqlDialect>(
        "CREATE ROLE app;
             CREATE TABLE writes (id INT);
             CREATE TRIGGER guarded AFTER INSERT ON writes
               FOR EACH ROW EXECUTE FUNCTION absent();
             CREATE FUNCTION inspect() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
               INSERT INTO public.writes VALUES (1) $$;",
    );
    assert!(matches!(result, Err(sql_traits::errors::Error::FunctionNotFoundForTrigger { .. })));
}

/// An absent conflict target cannot establish a returned-row restriction.
#[test]
fn unresolved_conflict_target_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           INSERT INTO public.absent VALUES (1)
           ON CONFLICT (id) DO UPDATE SET id = 2 RETURNING id $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// Conjunctive trigger facts transfer through an enforced composite key.
#[test]
fn composite_trigger_checks_confine_both_read_columns() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY, tenant INT NOT NULL, rank INT NOT NULL,
           UNIQUE (id, tenant, rank));
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7 AND rank = 3);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (id INT, tenant INT NOT NULL, rank INT NOT NULL,
           FOREIGN KEY (id, tenant, rank) REFERENCES secrets (id, tenant, rank));
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app
           WITH CHECK (tenant = 7 AND rank = 3);
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER AS $$
           BEGIN
             IF EXISTS (SELECT 1 FROM public.secrets WHERE id = NEW.id) THEN
               RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
           END $$;
         CREATE TRIGGER guarded AFTER INSERT ON incoming
           FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(trigger_read(&database, "guarded", DefinerReading::Outcome), None);
}

/// A coalesced outer join key alone admits hidden rows.
#[test]
fn full_join_merged_key_requires_read_restrictions() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT NOT NULL);
         CREATE TABLE other (tenant INT NOT NULL);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         ALTER TABLE other ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         CREATE POLICY visible ON other FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets, other TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT tenant FROM public.secrets FULL JOIN public.other USING (tenant) $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Recursive policy-set lineage cannot establish a finite admitted set.
#[test]
fn recursive_policy_set_lineage_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION listed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT * FROM public.listed() $$;
         CREATE POLICY visible ON secrets FOR SELECT TO app
           USING (id IN (SELECT public.listed()));
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Omitted default arguments cannot stand in for a stated set restriction.
#[test]
fn omitted_set_arguments_do_not_transfer_parameter_conditions() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION listed(minimum INT DEFAULT 1) RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER AS $$
             SELECT id FROM public.secrets WHERE id >= minimum $$;
         CREATE FUNCTION wrapped() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
           SELECT * FROM public.listed() $$;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT id FROM public.secrets WHERE id IN (SELECT public.wrapped()) $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Fixed-width character keys cannot transfer exact trigger predicates.
#[test]
fn character_foreign_key_does_not_transfer_trigger_fact() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT PRIMARY KEY, tenant CHAR(4) NOT NULL,
           UNIQUE (id, tenant));
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 'app');
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (id INT, tenant CHAR(4) NOT NULL,
           FOREIGN KEY (id, tenant) REFERENCES secrets (id, tenant));
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (tenant = 'app');
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER AS $$
           BEGIN
             IF EXISTS (SELECT 1 FROM public.secrets WHERE id = NEW.id) THEN
               RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
           END $$;
         CREATE TRIGGER guarded AFTER INSERT ON incoming
           FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guarded", DefinerReading::Outcome),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// A merged join key cannot constrain an unrelated source.
#[test]
fn merged_join_key_leaves_third_source_unconfined() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE first (tenant INT NOT NULL);
         CREATE TABLE second (tenant INT NOT NULL);
         CREATE TABLE secrets (tenant INT NOT NULL);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON first, second, secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT secrets.tenant FROM public.first JOIN public.second USING (tenant)
           CROSS JOIN public.secrets WHERE first.tenant = 7 $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// A copied-row count does not expose preexisting protected rows.
#[test]
fn observed_copy_from_does_not_read_existing_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE plpgsql SECURITY DEFINER AS $$
           DECLARE affected INT;
           BEGIN
             COPY public.secrets FROM '/data/rows';
             GET DIAGNOSTICS affected = ROW_COUNT;
             RETURN affected;
           END $$;",
    );
    assert_eq!(function_read(&database, None, "inspect"), None);
}

/// Composite key lineage cannot transfer fixed-width character conditions.
#[test]
fn membership_key_lineage_preserves_character_uncertainty() {
    for (source, target, identifier_type) in [
        ("memberships", "secrets", "INT"),
        ("secrets", "memberships", "INT"),
        ("memberships", "secrets", "CHAR(4)"),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id {identifier_type} PRIMARY KEY, tag CHAR(4) NOT NULL,
               UNIQUE (id, tag));
             CREATE TABLE memberships (id {identifier_type} PRIMARY KEY, tag CHAR(4) NOT NULL,
               FOREIGN KEY (id, tag) REFERENCES secrets (id, tag));
             ALTER TABLE {target} ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON {target} FOR SELECT TO app USING (tag = 'app');
             GRANT SELECT ON secrets, memberships TO app;
             CREATE FUNCTION listed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER AS $$
               SELECT id FROM public.{source} WHERE tag = 'app' $$;
             CREATE FUNCTION inspect() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER AS $$
               SELECT id FROM public.{target} WHERE id IN (SELECT public.listed()) $$;"
        ));
        assert_eq!(
            function_read(&database, None, "inspect"),
            Some((target.to_owned(), "visible".to_owned())),
            "{source}"
        );
    }
}

/// An unresolved argument type cannot weaken an independent row predicate.
#[test]
fn unresolved_parameter_type_preserves_independent_predicate() {
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<sqlparser::dialect::PostgreSqlDialect>(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION inspect(unused external_type) RETURNS SETOF INT LANGUAGE sql
               SECURITY DEFINER AS $$ SELECT id FROM public.secrets WHERE id > 0 $$;",
        )
        .expect("the schema parses");
    assert_eq!(function_read(&database, None, "inspect"), None);
}

/// A conflict target rebound by the function path has no shared row proof.
#[test]
fn rebound_conflict_target_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE SCHEMA private;
         CREATE TABLE public.secrets (id INT PRIMARY KEY);
         CREATE TABLE private.secrets (id INT PRIMARY KEY);
         ALTER TABLE public.secrets ENABLE ROW LEVEL SECURITY;
         ALTER TABLE private.secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON public.secrets FOR SELECT TO app USING (id > 0);
         CREATE POLICY visible ON private.secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON public.secrets, private.secrets TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER
           SET search_path = private, pg_temp AS $$
             INSERT INTO secrets VALUES (1) ON CONFLICT (id)
             DO UPDATE SET id = secrets.id WHERE secrets.id > 0 RETURNING id $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A view's merged column cannot supply a stored-table identity proof.
#[test]
fn view_join_columns_do_not_establish_stored_identity() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT NOT NULL);
         CREATE TABLE other (tenant INT NOT NULL);
         CREATE VIEW visible_other AS SELECT tenant FROM other;
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets, other, visible_other TO app;
         CREATE FUNCTION inspect() RETURNS INT LANGUAGE sql SECURITY DEFINER AS $$
           SELECT tenant FROM public.secrets JOIN public.visible_other USING (tenant) $$;",
    );
    assert_eq!(
        function_read(&database, None, "inspect"),
        Some(("secrets".to_owned(), "visible".to_owned()))
    );
}

/// Copying supplied rows performs no read of existing protected rows.
#[test]
fn sql_copy_from_does_not_read_existing_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (id > 0);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION inspect() RETURNS VOID LANGUAGE sql SECURITY DEFINER AS $$
           COPY public.secrets FROM '/data/rows' $$;",
    );
    assert_eq!(function_read(&database, None, "inspect"), None);
}
