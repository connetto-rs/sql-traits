//! Which facts a trigger's firing holds for the reads of its definer chain.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;

use crate::support::definer_reads::{function_read, schema, trigger_read};

/// The inserted tenant must pass the same policy as the row the guard reads.
fn tenant_guard(body: &str, trigger: &str, additions: &str) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE ROLE owner;
         CREATE TABLE secrets (tenant INT NOT NULL);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (tenant INT NOT NULL);
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (tenant = 7);
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         BEGIN
             {body}
             IF EXISTS (SELECT 1 FROM public.secrets WHERE tenant = NEW.tenant) THEN
                 RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
         END $$;
         ALTER FUNCTION guard() OWNER TO owner;
         CREATE TRIGGER guard {trigger} EXECUTE FUNCTION guard();
         {additions}"
    ))
}

/// A row trigger's `WHEN` condition confines a read even without a write
/// policy.
#[test]
fn trigger_condition_holds_for_disclosure() {
    let database =
        tenant_guard("", "BEFORE INSERT ON incoming FOR EACH ROW WHEN (NEW.tenant = 7)", "");
    assert_eq!(trigger_read(&database, "guard", DefinerReading::Disclosure), None);
}

/// A statement trigger has no policy-checked `NEW` row.
#[test]
fn statement_trigger_has_no_row_facts() {
    let database = tenant_guard("", "AFTER INSERT ON incoming FOR EACH STATEMENT", "");
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// An `INSTEAD OF` view trigger derives no facts from table write policies.
#[test]
fn view_trigger_has_no_table_write_facts() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE VIEW incoming AS SELECT tenant FROM secrets;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         BEGIN
             IF EXISTS (SELECT 1 FROM public.secrets WHERE tenant = NEW.tenant) THEN
                 RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER guard INSTEAD OF INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// A later `BEFORE` trigger can replace the row whose write policy would
/// justify the read.
#[test]
fn later_before_trigger_invalidates_outcome_facts() {
    let database = tenant_guard(
        "",
        "BEFORE INSERT ON incoming FOR EACH ROW",
        "CREATE FUNCTION change_row() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN NEW.tenant := 7; RETURN NEW; END $$;
         CREATE TRIGGER z_change BEFORE INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION change_row();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Every permissive insertion alternative must confine the read.
#[test]
fn permissive_write_alternatives_do_not_imply_one_tenant() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE POLICY another ON incoming FOR INSERT TO app WITH CHECK (tenant = 8);",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Control flow that leaves `NEW` unchanged preserves its outcome policy facts.
#[test]
fn control_flow_preserves_new() {
    for body in [
        "CASE WHEN true THEN NULL; ELSE NULL; END CASE;",
        "CASE WHEN true THEN NULL; END CASE;",
        "ASSERT true;",
        "FOR counter IN 1..2 LOOP NULL; END LOOP;",
        "LOOP EXIT; END LOOP;",
        "WHILE false LOOP NULL; END LOOP;",
        "ASSERT true, (SELECT count(*) FROM public.secrets);",
        "BEGIN NULL; END;",
    ] {
        let database = tenant_guard(body, "BEFORE INSERT ON incoming FOR EACH ROW", "");
        assert_eq!(trigger_read(&database, "guard", DefinerReading::Outcome), None, "{body}");
    }
}

/// Assignments hidden in control flow invalidate the original `NEW` row.
#[test]
fn control_flow_writes_invalidate_new() {
    for body in [
        "CASE WHEN true THEN NEW.tenant := 7; ELSE NULL; END CASE;",
        "FOR new IN 1..2 LOOP NULL; END LOOP;",
        "EXECUTE 'SELECT tenant FROM public.secrets' INTO NEW.tenant;",
        "EXECUTE 'SELECT tenant FROM public.secrets';",
        "GET DIAGNOSTICS NEW.tenant = ROW_COUNT;",
    ] {
        let database = tenant_guard(body, "BEFORE INSERT ON incoming FOR EACH ROW", "");
        assert_eq!(
            trigger_read(&database, "guard", DefinerReading::Outcome),
            if body.starts_with("EXECUTE") {
                Some((String::new(), "unfollowed".into()))
            } else {
                Some(("secrets".into(), "visible".into()))
            },
            "{body}"
        );
    }
}

/// Diagnostic reads make exception-message queries observable to the outcome.
#[test]
fn diagnostics_observe_exception_expression() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (tenant INT);
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         DECLARE affected BIGINT;
         BEGIN
             GET DIAGNOSTICS affected = ROW_COUNT;
             RAISE EXCEPTION '%', (SELECT count(*) FROM public.secrets);
         END $$;
         CREATE TRIGGER guard AFTER INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Owner-run writes prevent a trigger from relying on the acting role's
/// insertion policy.
#[test]
fn foreign_definer_writes_bypass_policy_facts() {
    for writer in [
        "INSERT INTO public.incoming VALUES (8)",
        "UPDATE public.incoming SET tenant = 8",
        "DELETE FROM public.incoming",
        "DELETE incoming FROM public.incoming",
    ] {
        let database = tenant_guard(
            "",
            "AFTER INSERT ON incoming FOR EACH ROW",
            &format!(
                "CREATE FUNCTION writer() RETURNS void LANGUAGE sql SECURITY DEFINER
                     SET search_path = public, pg_temp AS $$ {writer} $$;
                 CREATE FUNCTION zz_writer() RETURNS void LANGUAGE sql SECURITY DEFINER
                     SET search_path = public, pg_temp AS $$ SELECT 1 $$;"
            ),
        );
        assert_eq!(
            trigger_read(&database, "guard", DefinerReading::Outcome),
            Some(("secrets".into(), "visible".into())),
            "{writer}"
        );
    }
}

/// A nested writer block restores its local relation search path.
#[test]
fn nested_writer_block_invalidates_policy_facts() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE FUNCTION writer() RETURNS void LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         BEGIN
             BEGIN
                 INSERT INTO public.incoming VALUES (8);
             EXCEPTION WHEN OTHERS THEN NULL;
             END;
         END $$;",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// A view on another relation does not invalidate the written row's policies.
#[test]
fn unrelated_owner_view_preserves_facts() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE TABLE unrelated (tenant INT);
         CREATE VIEW unrelated_view AS SELECT tenant FROM unrelated;",
    );
    assert_eq!(trigger_read(&database, "guard", DefinerReading::Outcome), None);
}

/// An owner-run view with a column update grant permits writes outside
/// insertion policies.
#[test]
fn owner_view_column_grant_invalidates_facts() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE VIEW incoming_view AS SELECT tenant FROM incoming;
         GRANT UPDATE (tenant) ON incoming_view TO app;",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// A SQL trigger body receives no PL/pgSQL row-policy assumptions.
#[test]
fn sql_trigger_body_reads_hidden_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$ SELECT tenant FROM public.secrets $$;
         CREATE TRIGGER guard AFTER INSERT ON secrets
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// An unreadable trigger body is reported as an unfollowed read.
#[test]
fn unsupported_trigger_language_is_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE incoming (tenant INT);
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE python SECURITY DEFINER AS 'pass';
         CREATE TRIGGER guard AFTER INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some((String::new(), "unfollowed".into()))
    );
}

/// A refusal identifies the function chain and SQL that exposed a hidden row.
#[test]
fn refusal_reports_chain_and_statement() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION leaf() RETURNS INT LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$ SELECT tenant FROM public.secrets $$;
         REVOKE EXECUTE ON FUNCTION leaf() FROM PUBLIC;
         CREATE FUNCTION entry() RETURNS INT LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$ SELECT public.leaf() $$;",
    );
    assert_eq!(function_read(&database, None, "entry"), Some(("secrets".into(), "visible".into())));
    let deployment = crate::support::definer_reads::deployment();
    let function =
        database.functions().find(|function| function.name() == "entry").expect("entry exists");
    let role = ActingRole::Declared(database.role("app").expect("app exists"));
    let read = function
        .unconfined_definer_read(&database, &deployment, &[role])
        .expect("the read resolves")
        .expect("the row is hidden");
    assert_eq!(
        read.through().iter().map(|function| function.name()).collect::<Vec<_>>(),
        ["entry", "leaf"]
    );
    assert_eq!(read.statement(), Some("SELECT tenant FROM public.secrets"));
    assert!(
        function
            .unconfined_definer_read(&database, &deployment, &[])
            .expect("the read resolves")
            .is_none()
    );
}

/// A merged join column preserves the tenant filter on both stored relations.
#[test]
fn using_join_resolves_stored_columns() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT NOT NULL);
         CREATE TABLE other (tenant INT NOT NULL);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         ALTER TABLE other ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         CREATE POLICY visible ON other FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets, other TO app;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
             SELECT tenant FROM public.secrets JOIN public.other USING (tenant) WHERE tenant = 7
         $$;",
    );
    assert_eq!(function_read(&database, None, "reader"), None);
}

/// An unenforced foreign key cannot transfer the written tenant to a read row.
#[test]
fn unenforced_foreign_key_proves_nothing() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT, tenant INT NOT NULL, UNIQUE (id, tenant));
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (id INT, tenant INT NOT NULL,
             FOREIGN KEY (id, tenant) REFERENCES secrets (id, tenant) NOT ENFORCED);
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (tenant = 7);
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         BEGIN
             IF EXISTS (SELECT 1 FROM public.secrets WHERE id = NEW.id) THEN
                 RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER guard AFTER INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Cascaded owner-run updates bypass the insertion policy of a referencing
/// table.
#[test]
fn cascaded_writer_invalidates_row_facts() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE TABLE parents (tenant INT PRIMARY KEY);
         ALTER TABLE incoming ADD FOREIGN KEY (tenant) REFERENCES parents (tenant)
             ON UPDATE CASCADE;
         CREATE FUNCTION writer() RETURNS void LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
             UPDATE public.parents SET tenant = 8;
             SELECT public.helper();
         $$;
         CREATE FUNCTION helper() RETURNS INT LANGUAGE sql RETURN 1;",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// An expression-bodied definer that writes nothing preserves insertion facts.
#[test]
fn return_expression_definer_preserves_facts() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE FUNCTION constant_value() RETURNS INT LANGUAGE sql SECURITY DEFINER RETURN 1;",
    );
    assert_eq!(trigger_read(&database, "guard", DefinerReading::Outcome), None);
}

/// View triggers do not imply writes to the guarded table.
#[test]
fn unrelated_view_trigger_preserves_facts() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "CREATE TABLE unrelated (tenant INT);
         CREATE VIEW unrelated_view AS SELECT tenant FROM unrelated;
         CREATE FUNCTION ignore_row() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RETURN NEW; END $$;
         CREATE TRIGGER ignored INSTEAD OF INSERT ON unrelated_view
             FOR EACH ROW EXECUTE FUNCTION ignore_row();",
    );
    assert_eq!(trigger_read(&database, "guard", DefinerReading::Outcome), None);
}

/// A cursor fetch into `NEW` invalidates its original policy facts.
#[test]
fn cursor_fetch_replaces_new() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (tenant INT);
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (tenant = 7);
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         DECLARE rows CURSOR FOR SELECT 8;
         BEGIN
             OPEN rows;
             FETCH rows INTO NEW.tenant;
             CLOSE rows;
             IF EXISTS (SELECT 1 FROM public.secrets WHERE tenant = NEW.tenant) THEN
                 RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER guard BEFORE INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// A non-select return value cannot promise to return the checked trigger row.
#[test]
fn values_return_does_not_preserve_new() {
    let database = tenant_guard(
        "IF false THEN RETURN (VALUES (1)); END IF;",
        "BEFORE INSERT ON incoming FOR EACH ROW",
        "",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Public insertion policies preserve the tenant fact for public callers.
#[test]
fn public_policy_preserves_tenant_fact() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "DROP POLICY accepted ON incoming;
         DROP POLICY visible ON secrets;
         GRANT SELECT ON secrets TO PUBLIC;
         GRANT INSERT ON incoming TO PUBLIC;
         CREATE POLICY public_visible ON secrets FOR SELECT TO PUBLIC USING (tenant = 7);
         CREATE POLICY public_accepted ON incoming FOR INSERT TO PUBLIC WITH CHECK (tenant = 7);",
    );
    let deployment = crate::support::definer_reads::deployment();
    let trigger =
        database.triggers().find(|trigger| trigger.name() == "guard").expect("guard exists");
    let read = trigger
        .unconfined_definer_read(
            &database,
            &deployment,
            ActingRole::Public,
            DefinerReading::Outcome,
        )
        .expect("the read resolves");
    assert!(read.is_none());
}

/// A quoted trigger sorting before foreign-key checks cannot disclose the
/// referenced row.
#[test]
fn quoted_trigger_reference_preserves_key_check_order() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT, tenant INT NOT NULL, UNIQUE (id, tenant));
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (id INT, tenant INT NOT NULL,
             FOREIGN KEY (id, tenant) REFERENCES secrets (id, tenant));
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (tenant = 7);
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         BEGIN
             IF EXISTS (SELECT 1 FROM public.secrets WHERE id = NEW.id) THEN
                 RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER \"A_guard\" AFTER INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    let deployment = crate::support::definer_reads::deployment();
    let trigger = database.triggers().next().expect("the trigger exists");
    let role = ActingRole::Declared(database.role("app").expect("app exists"));
    let read = <&_ as TriggerLike>::unconfined_definer_read(
        &trigger,
        &database,
        &deployment,
        role,
        DefinerReading::Disclosure,
    )
    .expect("the read resolves")
    .expect("the key is unchecked");
    assert_eq!(
        crate::support::definer_reads::described(&read),
        ("secrets".into(), "visible".into())
    );
}

/// Exclusion constraints prevent a `BEFORE INSERT` guard from trusting a
/// skipped row's key.
#[test]
fn exclusion_conflict_prevents_key_fact() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (id INT, tenant INT NOT NULL, UNIQUE (id, tenant));
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets TO app;
         CREATE TABLE incoming (id INT, tenant INT NOT NULL, period INT4RANGE,
             FOREIGN KEY (id, tenant) REFERENCES secrets (id, tenant),
             EXCLUDE USING gist (period WITH &&));
         ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
         CREATE POLICY accepted ON incoming FOR INSERT TO app WITH CHECK (tenant = 7);
         GRANT INSERT ON incoming TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
         BEGIN
             IF EXISTS (SELECT 1 FROM public.secrets WHERE id = NEW.id) THEN
                 RAISE EXCEPTION 'present';
             END IF;
             RETURN NEW;
         END $$;
         CREATE TRIGGER guard BEFORE INSERT ON incoming
             FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    let table =
        database.tables().find(|table| table.table_name() == "incoming").expect("incoming exists");
    assert!(
        <&_ as TableLike>::has_exclusion_constraints(&table, &database)
            .expect("the constraints resolve")
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// An unrelated later trigger cannot replace the guarded table's inserted row.
#[test]
fn unrelated_later_trigger_preserves_facts() {
    let database = tenant_guard(
        "",
        "BEFORE INSERT ON incoming FOR EACH ROW",
        "CREATE TABLE unrelated (tenant INT);
         CREATE FUNCTION unchanged() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RETURN NEW; END $$;
         CREATE TRIGGER z_change BEFORE INSERT ON unrelated
             FOR EACH ROW EXECUTE FUNCTION unchanged();",
    );
    assert_eq!(trigger_read(&database, "guard", DefinerReading::Outcome), None);
}

/// A trigger condition confines the read even when no permissive insertion
/// policy admits rows.
#[test]
fn trigger_condition_holds_without_permissive_write_policy() {
    let database = tenant_guard(
        "",
        "BEFORE INSERT ON incoming FOR EACH ROW WHEN (NEW.tenant = 7)",
        "DROP POLICY accepted ON incoming;
         CREATE POLICY restricted ON incoming AS RESTRICTIVE FOR INSERT TO app
             WITH CHECK (tenant = 7);",
    );
    assert_eq!(trigger_read(&database, "guard", DefinerReading::Disclosure), None);
}

/// A policy reading a view of unknown ownership supplies no tenant guarantee.
#[test]
fn unsettled_policy_view_owner_proves_nothing() {
    let database = tenant_guard(
        "",
        "AFTER INSERT ON incoming FOR EACH ROW",
        "DROP POLICY accepted ON incoming;
         CREATE TABLE allowed (tenant INT);
         CREATE VIEW allowed_view AS SELECT tenant FROM allowed;
         CREATE POLICY accepted ON incoming FOR INSERT TO app
             WITH CHECK (tenant IN (SELECT tenant FROM allowed_view));",
    );
    assert_eq!(
        trigger_read(&database, "guard", DefinerReading::Outcome),
        Some(("secrets".into(), "visible".into()))
    );
}

/// Renaming join columns does not authorize reading hidden tenants.
#[test]
fn renamed_using_join_reads_hidden_tenants() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT NOT NULL);
         CREATE TABLE other (tenant INT NOT NULL);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         ALTER TABLE other ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = 7);
         CREATE POLICY visible ON other FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secrets, other TO app;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
             SELECT renamed FROM public.secrets AS first(renamed)
                 JOIN public.other AS second(renamed) USING (renamed)
         $$;",
    );
    assert_eq!(
        function_read(&database, None, "reader"),
        Some((String::new(), "unfollowed".into()))
    );
}

/// Recursive containment declarations do not authorize a hidden tenant.
#[test]
fn recursive_set_declaration_does_not_authorize_wider_read() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secrets (tenant INT NOT NULL);
         ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON secrets FOR SELECT TO app USING (tenant = ANY(ARRAY[7]));
         GRANT SELECT ON secrets TO app;
         CREATE FUNCTION loop_set() RETURNS INT[] LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = public, pg_temp AS $$ SELECT public.loop_set() $$;
         CREATE FUNCTION reader() RETURNS INT LANGUAGE sql SECURITY DEFINER
             SET search_path = public, pg_temp AS $$
             SELECT tenant FROM public.secrets WHERE tenant = ANY(ARRAY[7, 8])
         $$;",
    );
    let deployment = crate::support::definer_reads::deployment()
        .with_set_within("public.loop_set()", "ARRAY[7]")
        .expect("the declaration parses");
    let function =
        database.functions().find(|function| function.name() == "reader").expect("reader exists");
    let role = ActingRole::Declared(database.role("app").expect("app exists"));
    let read = function
        .unconfined_definer_read(&database, &deployment, &[role])
        .expect("the read resolves")
        .expect("tenant eight is hidden");
    assert_eq!(
        crate::support::definer_reads::described(&read),
        ("secrets".into(), "visible".into())
    );
}

/// An unresolved policy relation is returned to the caller as a lookup error.
#[test]
fn open_world_policy_lookup_error_is_propagated() {
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<sqlparser::dialect::PostgreSqlDialect>(
            "CREATE ROLE app;
             CREATE TABLE incoming (tenant INT);
             ALTER TABLE incoming ENABLE ROW LEVEL SECURITY;
             CREATE POLICY accepted ON incoming FOR INSERT TO app
                 WITH CHECK (EXISTS (SELECT 1 FROM cluster.database.schema.allowed));
             GRANT INSERT ON incoming TO app;
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
                 SET search_path = public, pg_temp AS $$ BEGIN RETURN NEW; END $$;
             CREATE TRIGGER guard AFTER INSERT ON incoming
                 FOR EACH ROW EXECUTE FUNCTION guard();",
        )
        .expect("the open-world schema parses");
    let deployment = crate::support::definer_reads::deployment();
    let trigger = database.triggers().next().expect("guard exists");
    let role = ActingRole::Declared(database.role("app").expect("app exists"));
    assert!(matches!(
        trigger.unconfined_definer_read(&database, &deployment, role, DefinerReading::Outcome),
        Err(sql_traits::errors::LookupError::InvalidObjectName { .. })
    ));
}
