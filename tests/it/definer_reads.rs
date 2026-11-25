//! Whether the reads a `SECURITY DEFINER` function makes as its owner reach
//! only rows the acting role reads, for functions a caller executes and for
//! the function chains of triggers.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;

use crate::support::definer_reads::{deployment, described, function_read, schema, trigger_read};

/// The schema of the request's reproduction, the guard reading `runs` by
/// `keyed`, a field of `NEW`, and `bindings` declaring `keys`.
fn guarded_bindings(keyed: &str, keys: &str, timing: &str, trigger: &str) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE ROLE guard_owner;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL, status TEXT NOT NULL,
             UNIQUE (id, team_id));
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         CREATE TABLE bindings (id INT PRIMARY KEY, team_id INT NOT NULL,
             run_id INT NOT NULL, other_run_id INT NOT NULL{keys});
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY bindings_select ON bindings FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         CREATE POLICY bindings_insert ON bindings FOR INSERT TO app
             WITH CHECK (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON runs TO app, guard_owner;
         GRANT SELECT, INSERT ON bindings TO app;
         CREATE FUNCTION guard_binding() RETURNS trigger
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           IF EXISTS (SELECT 1 FROM public.runs
                      WHERE runs.id = NEW.{keyed} AND runs.status <> 'open') THEN
             RAISE EXCEPTION 'run % is closed', NEW.{keyed} USING ERRCODE = 'check_violation';
           END IF;
           RETURN NEW;
         END
         $$;
         ALTER FUNCTION guard_binding() OWNER TO guard_owner;
         REVOKE EXECUTE ON FUNCTION guard_binding() FROM PUBLIC;
         CREATE TRIGGER {trigger} {timing} INSERT ON bindings
             FOR EACH ROW EXECUTE FUNCTION guard_binding();"
    ))
}

const COMPOSITE_KEY: &str = ",
    FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id),
    FOREIGN KEY (other_run_id) REFERENCES runs (id)";
const RUN_KEY_ONLY: &str = ",
    FOREIGN KEY (run_id) REFERENCES runs (id)";

#[test]
fn a_key_holding_the_read_row_to_the_written_one_confines_the_guard() {
    let db = guarded_bindings("run_id", COMPOSITE_KEY, "BEFORE", "bindings_guard");
    assert_eq!(
        trigger_read(&db, "bindings_guard", DefinerReading::Outcome),
        Some(("runs".to_owned(), "runs_select".to_owned()))
    );
    let db = guarded_bindings("run_id", COMPOSITE_KEY, "AFTER", "bindings_guard");
    assert_eq!(trigger_read(&db, "bindings_guard", DefinerReading::Outcome), None);
}

#[test]
fn a_key_leaving_the_team_apart_confines_nothing() {
    let unconfined = Some(("runs".to_owned(), "runs_select".to_owned()));
    let db = guarded_bindings("other_run_id", COMPOSITE_KEY, "AFTER", "bindings_guard");
    assert_eq!(trigger_read(&db, "bindings_guard", DefinerReading::Outcome), unconfined);
    let db = guarded_bindings("run_id", RUN_KEY_ONLY, "AFTER", "bindings_guard");
    assert_eq!(trigger_read(&db, "bindings_guard", DefinerReading::Outcome), unconfined);
}

#[test]
fn a_disclosure_holds_only_the_checks_postgresql_made_before_the_read() {
    let unconfined = Some(("runs".to_owned(), "runs_select".to_owned()));
    let before = guarded_bindings("run_id", COMPOSITE_KEY, "BEFORE", "bindings_guard");
    assert_eq!(trigger_read(&before, "bindings_guard", DefinerReading::Disclosure), unconfined);
    // An `AFTER` trigger sorting after the key's check triggers reads a row
    // the key and the policy already held.
    let after = guarded_bindings("run_id", COMPOSITE_KEY, "AFTER", "bindings_guard");
    assert_eq!(trigger_read(&after, "bindings_guard", DefinerReading::Disclosure), None);
    // One sorting before them reads it before the key is checked.
    let early = guarded_bindings("run_id", COMPOSITE_KEY, "AFTER", "\"A_guard\"");
    assert_eq!(trigger_read(&early, "A_guard", DefinerReading::Disclosure), unconfined);
    assert_eq!(trigger_read(&early, "A_guard", DefinerReading::Outcome), None);
}

#[test]
fn a_written_row_the_guard_changes_holds_nothing_the_checks_hold() {
    let db = schema(
        "CREATE ROLE app;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL, status TEXT NOT NULL,
             UNIQUE (id, team_id));
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         CREATE TABLE bindings (id INT, team_id INT NOT NULL, run_id INT NOT NULL,
             FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id));
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY bindings_insert ON bindings FOR INSERT TO app
             WITH CHECK (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON runs TO app;
         GRANT INSERT ON bindings TO app;
         CREATE FUNCTION guard_binding() RETURNS trigger
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           IF EXISTS (SELECT 1 FROM public.runs WHERE runs.id = NEW.run_id AND runs.status <> 'open') THEN
             RAISE EXCEPTION 'closed';
           END IF;
           NEW.team_id := 0;
           RETURN NEW;
         END
         $$;
         CREATE TRIGGER bindings_guard BEFORE INSERT ON bindings
             FOR EACH ROW EXECUTE FUNCTION guard_binding();",
    );
    assert_eq!(
        trigger_read(&db, "bindings_guard", DefinerReading::Outcome),
        Some(("runs".to_owned(), "runs_select".to_owned()))
    );
}

#[test]
fn perform_evaluation_counts_as_an_outcome_and_a_disclosure() {
    let db = schema(
        "CREATE ROLE app;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         CREATE TABLE audit (run_count BIGINT);
         CREATE TABLE bindings (id INT PRIMARY KEY);
         GRANT INSERT ON bindings TO app;
         CREATE FUNCTION count_runs() RETURNS trigger
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           INSERT INTO public.audit SELECT count(*) FROM public.runs;
           PERFORM 1 FROM public.runs;
           RETURN NEW;
         END
         $$;
         CREATE TRIGGER bindings_counted AFTER INSERT ON bindings
             FOR EACH ROW EXECUTE FUNCTION count_runs();",
    );
    assert_eq!(
        trigger_read(&db, "bindings_counted", DefinerReading::Outcome),
        Some(("runs".to_owned(), "runs_select".to_owned()))
    );
    assert_eq!(
        trigger_read(&db, "bindings_counted", DefinerReading::Disclosure),
        Some(("runs".to_owned(), "runs_select".to_owned()))
    );
}

/// The procedures and the assets bound to them, gated by a role ladder and a
/// blocklist of organizations, and a guard reading the procedure of an
/// asset binding.
const PROCEDURES: &str = "
    CREATE ROLE app;
    CREATE SCHEMA authz;
    CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
    CREATE TABLE grants (resource_id INT NOT NULL, role authz.role NOT NULL);
    CREATE TABLE blocks (organization_id INT NOT NULL);
    CREATE FUNCTION authz.resources_with(min_role authz.role) RETURNS SETOF INT
      LANGUAGE sql STABLE STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
        SELECT permission.resource_id FROM public.grants permission
        WHERE permission.role >= min_role $$;
    CREATE FUNCTION authz.blocking_organizations() RETURNS SETOF INT
      LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
        SELECT block.organization_id FROM public.blocks block $$;
    CREATE TABLE procedures (id INT PRIMARY KEY, organization_id INT NOT NULL,
        status TEXT NOT NULL, UNIQUE (id, organization_id));
    ALTER TABLE procedures ENABLE ROW LEVEL SECURITY;
    CREATE POLICY procedures_select ON procedures FOR SELECT TO app
        USING (id = ANY (ARRAY(SELECT authz.resources_with('reader'))));
    CREATE POLICY procedures_blocked ON procedures AS RESTRICTIVE FOR ALL TO app
        USING (NOT (organization_id = ANY (ARRAY(SELECT authz.blocking_organizations()))));
    CREATE TABLE procedure_assets (id INT PRIMARY KEY, procedure_id INT NOT NULL,
        organization_id INT NOT NULL,
        FOREIGN KEY (procedure_id, organization_id) REFERENCES procedures (id, organization_id));
    ALTER TABLE procedure_assets ENABLE ROW LEVEL SECURITY;
    CREATE POLICY procedure_assets_insert ON procedure_assets FOR INSERT TO app
        WITH CHECK (procedure_id = ANY (ARRAY(SELECT authz.resources_with('writer'))));
    CREATE POLICY procedure_assets_blocked ON procedure_assets AS RESTRICTIVE FOR ALL TO app
        USING (NOT (organization_id = ANY (ARRAY(SELECT authz.blocking_organizations()))));
    GRANT SELECT ON procedures TO app;
    GRANT SELECT, INSERT ON procedure_assets TO app;
    CREATE FUNCTION procedure_assets_guard() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    BEGIN
      IF (SELECT procedure.status FROM public.procedures procedure
          WHERE procedure.id = NEW.procedure_id) <> 'draft' THEN
        RAISE EXCEPTION 'procedure % is not a draft', NEW.procedure_id;
      END IF;
      RETURN NEW;
    END
    $$;
    CREATE TRIGGER procedure_assets_guard AFTER INSERT ON procedure_assets
        FOR EACH ROW EXECUTE FUNCTION procedure_assets_guard();";

#[test]
fn a_write_ladder_above_the_read_one_and_a_carried_blocklist_confine_the_guard() {
    let db = schema(PROCEDURES);
    assert_eq!(trigger_read(&db, "procedure_assets_guard", DefinerReading::Outcome), None);
}

#[test]
fn a_write_ladder_below_the_read_one_confines_nothing() {
    let db = schema(
        &PROCEDURES.replace("resources_with('writer')", "resources_with('reader')").replace(
            "USING (id = ANY (ARRAY(SELECT authz.resources_with('reader'))))",
            "USING (id = ANY (ARRAY(SELECT authz.resources_with('admin'))))",
        ),
    );
    assert_eq!(
        trigger_read(&db, "procedure_assets_guard", DefinerReading::Outcome),
        Some(("procedures".to_owned(), "procedures_select".to_owned()))
    );
}

#[test]
fn a_function_reading_only_the_rows_its_callers_read_is_confined() {
    let db = schema(
        "CREATE ROLE app;
         CREATE TABLE notes (id INT PRIMARY KEY, team_id INT NOT NULL, body TEXT);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY notes_select ON notes FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION team_note(note INT) RETURNS TEXT LANGUAGE plpgsql STABLE
         SECURITY DEFINER SET search_path = pg_catalog AS $$
         DECLARE found_body TEXT;
         BEGIN
           SELECT body INTO found_body FROM public.notes
           WHERE id = note AND team_id = current_setting('app.team_id')::INT;
           RETURN found_body;
         END $$;
         CREATE FUNCTION any_note(note INT) RETURNS TEXT LANGUAGE plpgsql STABLE
         SECURITY DEFINER SET search_path = pg_catalog AS $$
         DECLARE found_body TEXT;
         BEGIN
           SELECT body INTO found_body FROM public.notes WHERE id = note;
           RETURN found_body;
         END $$;",
    );
    assert_eq!(function_read(&db, None, "team_note"), None);
    assert_eq!(
        function_read(&db, None, "any_note"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

#[test]
fn a_row_drawn_through_invoker_helpers_keeps_their_conditions() {
    let reported = |condition: &str, tested: &str| {
        let db = schema(&format!(
            "CREATE ROLE app NOLOGIN;
             CREATE SCHEMA authz;
             CREATE TABLE resources (id INT PRIMARY KEY, organization_id INT NOT NULL,
               hidden BOOLEAN NOT NULL);
             ALTER TABLE resources ENABLE ROW LEVEL SECURITY;
             GRANT SELECT ON resources TO app;
             ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
             ALTER DEFAULT PRIVILEGES GRANT EXECUTE ON FUNCTIONS TO app;
             CREATE POLICY readable_resources ON resources FOR SELECT TO app USING (NOT hidden);
             CREATE FUNCTION authz.listed() RETURNS SETOF INT
               LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
                 SELECT resource.id FROM public.resources resource WHERE {condition} $$;
             CREATE FUNCTION authz.visible_ids() RETURNS SETOF INT
               LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
                 SELECT listed.id FROM authz.listed() listed (id) $$;
             CREATE FUNCTION authz.organizations_reaching() RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
                 SELECT resource.organization_id FROM public.resources resource
                 WHERE resource.id IN (SELECT authz.{tested}()) $$;"
        ));
        function_read(&db, Some("authz"), "organizations_reaching").is_some()
    };
    for tested in ["listed", "visible_ids"] {
        assert!(!reported("NOT resource.hidden", tested), "{tested}");
        assert!(reported("resource.organization_id > 0", tested), "{tested}");
    }
}

/// Templates of resources a role ladder over `resources` gates, whose helper
/// leaves out the resources of blocked organizations, and a guard reading the
/// template of a step.
const TEMPLATES: &str = "
    CREATE ROLE app;
    CREATE SCHEMA authz;
    CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
    CREATE TABLE blocks (organization_id INT NOT NULL);
    CREATE TABLE resources (id INT PRIMARY KEY, organization_id INT NOT NULL,
        role authz.role NOT NULL, UNIQUE (organization_id, id));
    CREATE FUNCTION authz.blocking_organizations() RETURNS SETOF INT
      LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
        SELECT block.organization_id FROM public.blocks block $$;
    CREATE FUNCTION authz.resources_with(min_role authz.role) RETURNS SETOF INT
      LANGUAGE sql STABLE STRICT SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
        SELECT resource.id FROM public.resources resource
        WHERE resource.role >= min_role
          AND NOT (resource.organization_id = ANY (ARRAY(SELECT authz.blocking_organizations()))) $$;
    CREATE TABLE procedure_templates (id INT PRIMARY KEY, organization_id INT NOT NULL,
        status TEXT NOT NULL,
        FOREIGN KEY (organization_id, id) REFERENCES resources (organization_id, id));
    ALTER TABLE procedure_templates ENABLE ROW LEVEL SECURITY;
    CREATE POLICY procedure_templates_select ON procedure_templates FOR SELECT TO app
        USING (id = ANY (ARRAY(SELECT authz.resources_with('reader'))));
    CREATE POLICY procedure_templates_blocked ON procedure_templates AS RESTRICTIVE FOR ALL TO app
        USING (NOT (organization_id = ANY (ARRAY(SELECT authz.blocking_organizations()))));
    CREATE TABLE template_steps (id INT PRIMARY KEY, template_id INT NOT NULL
        REFERENCES procedure_templates (id));
    ALTER TABLE template_steps ENABLE ROW LEVEL SECURITY;
    CREATE POLICY template_steps_insert ON template_steps FOR INSERT TO app
        WITH CHECK (template_id = ANY (ARRAY(SELECT authz.resources_with('writer'))));
    GRANT SELECT ON procedure_templates TO app;
    GRANT INSERT ON template_steps TO app;
    CREATE FUNCTION template_steps_guard() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    BEGIN
      IF (SELECT template.status FROM public.procedure_templates template
          WHERE template.id = NEW.template_id) <> 'draft' THEN
        RAISE EXCEPTION 'template % is not a draft', NEW.template_id;
      END IF;
      RETURN NEW;
    END
    $$;
    CREATE TRIGGER template_steps_guard AFTER INSERT ON template_steps
        FOR EACH ROW EXECUTE FUNCTION template_steps_guard();";

#[test]
fn a_helper_filter_carries_across_a_key_of_the_tested_table() {
    let db = schema(TEMPLATES);
    assert_eq!(trigger_read(&db, "template_steps_guard", DefinerReading::Outcome), None);
    let unkeyed = schema(&TEMPLATES.replace(
        "FOREIGN KEY (organization_id, id) REFERENCES resources (organization_id, id)",
        "UNIQUE (organization_id, id)",
    ));
    assert_eq!(
        trigger_read(&unkeyed, "template_steps_guard", DefinerReading::Outcome),
        Some(("procedure_templates".to_owned(), "procedure_templates_blocked".to_owned()))
    );
}

#[test]
fn an_outer_join_where_filter_does_not_confine_hidden_matches() {
    for (join, confined) in [("LEFT JOIN", false), ("INNER JOIN", true)] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE projects (id INT PRIMARY KEY);
             CREATE TABLE tasks (id INT PRIMARY KEY, project_id INT, deleted_at TIMESTAMP);
             ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
             CREATE POLICY tasks_select ON tasks FOR SELECT TO app USING (deleted_at IS NULL);
             GRANT SELECT ON projects, tasks TO app;
             CREATE FUNCTION active_projects() RETURNS SETOF INT
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
               SELECT project.id FROM public.projects project
               {join} public.tasks task ON task.project_id = project.id
               WHERE task.deleted_at IS NULL
             $$;"
        ));
        assert_eq!(
            function_read(&db, None, "active_projects"),
            (!confined).then(|| ("tasks".to_owned(), "tasks_select".to_owned())),
            "{join}"
        );
    }
}

#[test]
fn a_nullable_lateral_source_does_not_witness_its_filter() {
    for (join, confined) in [("LEFT JOIN", false), ("INNER JOIN", true)] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secrets (id INT PRIMARY KEY, team_id INT NOT NULL, secret TEXT);
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY;
             CREATE POLICY secrets_select ON secrets FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON secrets TO app;
             CREATE FUNCTION listed_secrets() RETURNS SETOF TEXT
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
               SELECT secret.secret FROM public.secrets secret
               {join} LATERAL (
                 SELECT 1 AS one WHERE secret.team_id = current_setting('app.team_id')::INT
               ) witnessed ON true
             $$;"
        ));
        assert_eq!(
            function_read(&db, None, "listed_secrets"),
            (!confined).then(|| ("secrets".to_owned(), "secrets_select".to_owned())),
            "{join}"
        );
    }
}

#[test]
fn a_composite_record_field_does_not_inherit_the_top_level_field_policy() {
    for (field, confined) in
        [("NEW.addr.team_id", false), ("NEW.team_id", true), ("guard_binding.new.team_id", true)]
    {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TYPE address AS (team_id INT);
             CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             CREATE TABLE bindings (id INT PRIMARY KEY, team_id INT NOT NULL, addr address);
             ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
             CREATE POLICY bindings_insert ON bindings FOR INSERT TO app
               WITH CHECK (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON runs TO app;
             GRANT INSERT ON bindings TO app;
             CREATE FUNCTION guard_binding() RETURNS trigger
             LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               IF EXISTS (SELECT 1 FROM public.runs WHERE team_id = {field}) THEN
                 RAISE EXCEPTION 'matching run';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER bindings_guard BEFORE INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();"
        ));
        assert_eq!(
            trigger_read(&db, "bindings_guard", DefinerReading::Outcome),
            (!confined).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{field}"
        );
    }
}

#[test]
fn carried_helper_filters_use_stored_column_names() {
    let mixed = TEMPLATES.replace("organization_id", "OrganizationId");
    let db = schema(&mixed);
    assert_eq!(trigger_read(&db, "template_steps_guard", DefinerReading::Outcome), None);
    let unkeyed = schema(&mixed.replace(
        "FOREIGN KEY (OrganizationId, id) REFERENCES resources (OrganizationId, id)",
        "UNIQUE (OrganizationId, id)",
    ));
    assert_eq!(
        trigger_read(&unkeyed, "template_steps_guard", DefinerReading::Outcome),
        Some(("procedure_templates".to_owned(), "procedure_templates_blocked".to_owned()))
    );
}

#[test]
fn writes_follow_invoker_triggers_as_the_definer_owner() {
    for (statement, event) in [
        ("INSERT INTO public.audit VALUES (1)", "INSERT"),
        ("COPY public.audit FROM '/srv/audit.csv'", "INSERT"),
        ("UPDATE public.audit SET id = 2", "UPDATE"),
        ("DELETE FROM public.audit", "DELETE"),
        (
            "MERGE INTO public.audit USING (SELECT 1 AS id) source ON audit.id = source.id
             WHEN NOT MATCHED THEN INSERT (id) VALUES (source.id)",
            "INSERT",
        ),
    ] {
        for level in ["ROW", "STATEMENT"] {
            for condition in ["true", "team_id = current_setting('app.team_id')::INT"] {
                let db = schema(&format!(
                    "CREATE ROLE app;
                     CREATE ROLE guard_owner;
                     CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
                     ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
                     CREATE POLICY runs_select ON runs FOR SELECT TO app
                       USING (team_id = current_setting('app.team_id')::INT);
                     CREATE TABLE audit (id INT PRIMARY KEY);
                     CREATE TABLE bindings (id INT PRIMARY KEY);
                     GRANT SELECT ON runs TO app, guard_owner;
                     GRANT INSERT ON bindings TO app;
                     GRANT SELECT, INSERT, UPDATE, DELETE ON audit TO guard_owner;
                     CREATE FUNCTION inspect_audit() RETURNS trigger
                     LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
                     BEGIN
                       IF EXISTS (SELECT 1 FROM public.runs WHERE {condition}) THEN
                         RAISE EXCEPTION 'matching run';
                       END IF;
                       RETURN NEW;
                     END $$;
                     CREATE TRIGGER audit_guard BEFORE {event} ON audit
                       FOR EACH {level} EXECUTE FUNCTION inspect_audit();
                     CREATE FUNCTION write_audit() RETURNS trigger
                     LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
                     BEGIN
                       {statement};
                       RETURN NEW;
                     END $$;
                     ALTER FUNCTION write_audit() OWNER TO guard_owner;
                     CREATE TRIGGER bindings_guard BEFORE INSERT ON bindings
                       FOR EACH ROW EXECUTE FUNCTION write_audit();"
                ));
                assert_eq!(
                    trigger_read(&db, "bindings_guard", DefinerReading::Outcome),
                    (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned())),
                    "{statement}, {level}, {condition}"
                );
            }
        }
    }
}

/// A lossy value cast does not imply equality with the original value.
#[test]
fn a_truncated_setting_does_not_confine_a_read() {
    for (filter, confined) in
        [("current_setting('app.code')::varchar(3)", false), ("current_setting('app.code')", true)]
    {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE notes (id INT PRIMARY KEY, code TEXT NOT NULL, body TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_select ON notes FOR SELECT TO app
               USING (code = current_setting('app.code'));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF TEXT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT body FROM public.notes WHERE code = {filter} $$;"
        ));
        assert_eq!(
            function_read(&db, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{filter}"
        );
    }
}

/// Ladder containment preserves casts on returned values and accepts cast
/// labels.
#[test]
fn a_truncated_ladder_result_does_not_confine_a_read() {
    for (filter, confined) in [
        ("authz.codes_with('writer')::varchar(3)", false),
        ("authz.codes_with('writer'::authz.role)", true),
    ] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE SCHEMA authz;
             CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
             CREATE TABLE memberships (code TEXT NOT NULL, role authz.role NOT NULL);
             GRANT SELECT ON memberships TO app;
             CREATE FUNCTION authz.codes_with(min_role authz.role) RETURNS SETOF TEXT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT code FROM public.memberships WHERE role >= min_role $$;
             CREATE TABLE notes (id INT PRIMARY KEY, code TEXT NOT NULL, body TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_select ON notes FOR SELECT TO app
               USING (code = ANY (ARRAY(SELECT authz.codes_with('reader'))));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF TEXT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT body FROM public.notes WHERE code = ANY (ARRAY(SELECT {filter})) $$;"
        ));
        assert_eq!(
            function_read(&db, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{filter}"
        );
    }
}

/// Declared containment follows a helper that projects the declared set.
#[test]
fn a_set_declaration_unfolds_a_projecting_helper() {
    let sql = "CREATE ROLE app;
         CREATE SCHEMA authz;
         CREATE SCHEMA app;
         CREATE TABLE memberships (id INT NOT NULL, role TEXT NOT NULL);
         GRANT SELECT ON memberships TO app;
         CREATE FUNCTION authz.capped(requested_role TEXT) RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.memberships WHERE role = requested_role $$;
         CREATE FUNCTION app.writer_set() RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT authz.capped('writer') $$;
         CREATE TABLE notes (id INT PRIMARY KEY, body TEXT);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY notes_select ON notes FOR SELECT TO app
           USING (id = ANY (ARRAY(SELECT authz.capped('reader'))));
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes() RETURNS SETOF TEXT LANGUAGE sql STABLE
           SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT body FROM public.notes
             WHERE id = ANY (ARRAY(SELECT app.writer_set())) $$;";
    let db = schema(sql);
    assert_eq!(
        function_read(&db, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
    let deployment = deployment()
        .with_set_within("authz.capped('writer')", "authz.capped('reader')")
        .expect("valid set expressions");
    let app = ActingRole::Declared(db.role("app").expect("the role exists"));
    let function = db
        .function_by_target(TargetName::new("read_notes", false), IdentifierCase::AsWritten)
        .expect("the name resolves")
        .expect("the function exists");
    assert_eq!(
        function
            .unconfined_definer_read(&db, &deployment, &[app])
            .expect("the schema reads")
            .map(|read| described(&read)),
        None
    );
    for (guard, sql) in [
        (
            "invoker source",
            sql.replace(
                "LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$\n             SELECT id",
                "LANGUAGE sql STABLE SECURITY INVOKER SET search_path = pg_catalog AS $$\n             SELECT id",
            ),
        ),
        (
            "volatile wrapper",
            sql.replace(
                "app.writer_set() RETURNS SETOF INT\n           LANGUAGE sql STABLE",
                "app.writer_set() RETURNS SETOF INT\n           LANGUAGE sql VOLATILE",
            ),
        ),
        (
            "parameterized wrapper",
            sql.replace(
                "CREATE FUNCTION app.writer_set()",
                "CREATE FUNCTION app.writer_set(ignored INT)",
            )
            .replace("SELECT app.writer_set()", "SELECT app.writer_set(1)"),
        ),
        (
            "unqualified projection",
            sql.replace(
                "SET search_path = pg_catalog AS $$\n             SELECT authz.capped('writer')",
                "SET search_path = authz, pg_catalog AS $$\n             SELECT capped('writer')",
            ),
        ),
        (
            "return coercion",
            sql.replace(
                "app.writer_set() RETURNS SETOF INT",
                "app.writer_set() RETURNS SETOF BIGINT",
            ),
        ),
    ] {
        let db = schema(&sql);
        let app = ActingRole::Declared(db.role("app").expect("the role exists"));
        let function = db
            .function_by_target(TargetName::new("read_notes", false), IdentifierCase::AsWritten)
            .expect("the name resolves")
            .expect("the function exists");
        assert_eq!(
            function
                .unconfined_definer_read(&db, &deployment, &[app])
                .expect("the schema reads")
                .map(|read| described(&read)),
            Some(("notes".to_owned(), "notes_select".to_owned())),
            "{guard}"
        );
    }
}

/// Builds a binding guard with configurable trigger facts.
fn trigger_fact_schema(
    keys: &str,
    trigger: &str,
    declarations: &str,
    body: &str,
    extra: &str,
) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL, UNIQUE (id, team_id));
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         CREATE TABLE bindings (id INT, team_id INT NOT NULL, run_id INT NOT NULL {keys});
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY bindings_write ON bindings FOR ALL TO app
             USING (team_id = current_setting('app.team_id')::INT)
             WITH CHECK (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON runs TO app;
         GRANT SELECT, INSERT ON bindings TO app;
         CREATE FUNCTION guard_binding() RETURNS trigger
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         {declarations}
         BEGIN {body} END $$;
         {trigger}
         {extra}"
    ))
}

const FACT_READ: &str = "
    IF EXISTS (SELECT 1 FROM public.runs WHERE runs.id = NEW.run_id) THEN
        RAISE EXCEPTION 'found';
    END IF;";
const FACT_INSERT: &str = "
    CREATE TRIGGER a_guard BEFORE INSERT ON bindings
        FOR EACH ROW EXECUTE FUNCTION guard_binding();";
const FACT_KEY: &str = ", FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id)";

#[test]
fn deferrable_keys_do_not_hold_the_new_row_under_outcome() {
    for deferral in
        ["DEFERRABLE INITIALLY DEFERRED", "DEFERRABLE INITIALLY IMMEDIATE", "NOT DEFERRABLE"]
    {
        let database =
            guarded_bindings("run_id", &format!("{FACT_KEY} {deferral}"), "AFTER", "a_guard");
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            (deferral != "NOT DEFERRABLE").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{deferral}"
        );
    }
}

#[test]
fn an_unvalidated_key_does_not_hold_new_on_update() {
    for validation in ["NOT VALID", ""] {
        let database = trigger_fact_schema(
            "",
            "CREATE TRIGGER z_guard AFTER UPDATE ON bindings FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            &format!("{FACT_READ} RETURN NEW;"),
            &format!(
                "ALTER TABLE bindings ADD CONSTRAINT binding_run
                FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id) {validation};"
            ),
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "z_guard", reading),
                (!validation.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{validation} {reading:?}"
            );
        }
    }
}

#[test]
fn trigger_order_folds_unquoted_names() {
    for (name, unconfined) in [("B_retarget", true), ("\"B_retarget\"", false)] {
        let database = trigger_fact_schema(
            FACT_KEY,
            FACT_INSERT,
            "",
            &format!("{FACT_READ} RETURN NEW;"),
            &format!(
                "CREATE FUNCTION retarget() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN NEW.run_id := 1; RETURN NEW; END $$;
                 CREATE TRIGGER {name} BEFORE INSERT ON bindings
                     FOR EACH ROW EXECUTE FUNCTION retarget();"
            ),
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{name}"
        );
    }
    let database = guarded_bindings("run_id", FACT_KEY, "AFTER", "A_guard");
    assert_eq!(trigger_read(&database, "A_guard", DefinerReading::Disclosure), None);
}

#[test]
fn initially_deferred_constraint_triggers_can_precede_key_checks() {
    for (name, unconfined) in [("\"Guard\"", true), ("z_guard", false)] {
        let database = trigger_fact_schema(
            FACT_KEY,
            &format!(
                "CREATE CONSTRAINT TRIGGER {name} AFTER INSERT ON bindings
                DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION guard_binding();"
            ),
            "",
            &format!("{FACT_READ} RETURN NEW;"),
            "",
        );
        assert_eq!(
            trigger_read(&database, name.trim_matches('"'), DefinerReading::Disclosure),
            unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{name}"
        );
    }
}

#[test]
fn indirect_new_writes_invalidate_written_row_facts() {
    for (declarations, assignment) in [
        ("DECLARE replacement ALIAS FOR new;", "replacement.run_id := 1;"),
        ("", "\"new\".run_id := 1;"),
        ("", "guard_binding.new.run_id := 1;"),
        ("", "FOR NEW.run_id IN SELECT 1 LOOP NULL; END LOOP;"),
        ("", "FOREACH NEW.run_id IN ARRAY ARRAY[1] LOOP NULL; END LOOP;"),
        ("", "DECLARE new RECORD; BEGIN NULL; END;"),
    ] {
        let database = trigger_fact_schema(
            FACT_KEY,
            FACT_INSERT,
            declarations,
            &format!("{FACT_READ} {assignment} RETURN NEW;"),
            "",
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            Some(("runs".to_owned(), "runs_select".to_owned())),
            "{declarations} {assignment}"
        );
    }
    let database = trigger_fact_schema(
        FACT_KEY,
        FACT_INSERT,
        "",
        &format!("{FACT_READ} CALL public.retarget(NEW.run_id); RETURN NEW;"),
        "CREATE PROCEDURE retarget(INOUT run_id INT) LANGUAGE plpgsql AS $$
         BEGIN run_id := 1; END $$;",
    );
    assert_eq!(
        trigger_read(&database, "a_guard", DefinerReading::Outcome),
        Some((String::new(), "unfollowed".to_owned()))
    );
    let database = trigger_fact_schema(
        FACT_KEY,
        FACT_INSERT,
        "DECLARE replacement INT;",
        &format!("{FACT_READ} replacement := 1; RETURN NEW;"),
        "",
    );
    assert_eq!(trigger_read(&database, "a_guard", DefinerReading::Outcome), None);
}

#[test]
fn referential_actions_do_not_supply_acting_role_write_policy_facts() {
    for (action, event, row) in [
        ("ON DELETE CASCADE", "DELETE", "OLD"),
        ("ON DELETE SET NULL", "UPDATE", "OLD"),
        ("ON DELETE SET DEFAULT", "UPDATE", "NEW"),
        ("ON UPDATE CASCADE", "UPDATE", "OLD"),
        ("ON UPDATE SET NULL", "UPDATE", "NEW"),
        ("ON UPDATE SET DEFAULT", "UPDATE", "NEW"),
    ] {
        for (key_action, unconfined) in [(action, true), ("", false)] {
            let database = trigger_fact_schema(
                &format!(", FOREIGN KEY (run_id) REFERENCES runs (id) {key_action}"),
                &format!(
                    "CREATE TRIGGER z_guard AFTER {event} ON bindings
                    FOR EACH ROW EXECUTE FUNCTION guard_binding();"
                ),
                "",
                &format!(
                    "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = {row}.team_id) THEN
                       RAISE EXCEPTION 'found';
                     END IF;
                     RETURN {row};"
                ),
                "",
            );
            assert_eq!(
                trigger_read(&database, "z_guard", DefinerReading::Outcome),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{key_action} {event} {row}"
            );
        }
    }
}

#[test]
fn perform_and_nonexception_raise_evaluations_decide_outcomes() {
    for statement in [
        "PERFORM 1 / count(*) FROM public.runs;",
        "RAISE NOTICE '%', (SELECT id FROM public.runs);",
        "RAISE NOTICE 'notice' USING DETAIL = (SELECT id::TEXT FROM public.runs);",
    ] {
        let database =
            trigger_fact_schema("", FACT_INSERT, "", &format!("{statement} RETURN NEW;"), "");
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            Some(("runs".to_owned(), "runs_select".to_owned())),
            "{statement}"
        );
    }
}

#[test]
fn exception_handlers_observe_message_evaluation_failures() {
    for statement in [
        "RAISE EXCEPTION '%', (SELECT id FROM public.runs);",
        "RAISE EXCEPTION 'failed' USING DETAIL = (SELECT id::TEXT FROM public.runs);",
        "ASSERT false, (SELECT id::TEXT FROM public.runs);",
    ] {
        for (handler, unconfined) in
            [("EXCEPTION WHEN cardinality_violation THEN RETURN NEW;", true), ("", false)]
        {
            let database = trigger_fact_schema(
                "",
                FACT_INSERT,
                "",
                &format!("{statement} RETURN NEW; {handler}"),
                "",
            );
            assert_eq!(
                trigger_read(&database, "a_guard", DefinerReading::Outcome),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{statement} {handler}"
            );
        }
    }
}

#[test]
fn keys_do_not_hold_new_when_the_role_can_rewrite_the_table() {
    for grant in [
        "GRANT UPDATE ON bindings TO app;",
        "GRANT DELETE ON bindings TO PUBLIC;",
        "CREATE ROLE writer; GRANT writer TO app; GRANT UPDATE ON bindings TO writer;",
        "",
    ] {
        let database = trigger_fact_schema(
            FACT_KEY,
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            &format!("{FACT_READ} RETURN NEW;"),
            grant,
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "z_guard", reading),
                (!grant.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{grant} {reading:?}"
            );
        }
    }
}

#[test]
fn definer_writers_invalidate_keys_and_write_policy_facts() {
    for write in [
        "INSERT INTO public.bindings VALUES (1, 2, 3);",
        "UPDATE public.bindings SET run_id = 1;",
        "DELETE FROM public.bindings;",
        "EXECUTE 'DELETE FROM public.bindings';",
        "PERFORM public.write_binding();",
    ] {
        for read in [
            FACT_READ,
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF;",
        ] {
            let database = trigger_fact_schema(
                FACT_KEY, FACT_INSERT, "", &format!("{read} RETURN NEW;"),
                &format!(
                    "CREATE ROLE writer;
                     CREATE FUNCTION write_binding() RETURNS void LANGUAGE sql AS $$
                       DELETE FROM public.bindings $$;
                     CREATE FUNCTION rewrite_binding() RETURNS void LANGUAGE plpgsql SECURITY DEFINER
                       SET search_path = pg_catalog, pg_temp AS $$ BEGIN {write} END $$;
                     ALTER FUNCTION rewrite_binding() OWNER TO writer;"
                ),
            );
            assert_eq!(
                trigger_read(&database, "a_guard", DefinerReading::Outcome),
                Some(("runs".to_owned(), "runs_select".to_owned())),
                "{write} {read}"
            );
        }
    }
}

#[test]
fn rules_and_referential_actions_prevent_new_key_facts() {
    for (key_action, extra) in [
        ("ON UPDATE CASCADE", ""),
        (
            "",
            "CREATE TABLE requests (id INT);
              CREATE RULE rewrite_binding AS ON INSERT TO requests DO ALSO
                UPDATE bindings SET run_id = NEW.id;",
        ),
    ] {
        let database = trigger_fact_schema(
            &format!("{FACT_KEY} {key_action}"),
            FACT_INSERT,
            "",
            &format!("{FACT_READ} RETURN NEW;"),
            extra,
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            Some(("runs".to_owned(), "runs_select".to_owned()))
        );
    }
}

#[test]
fn definer_write_chains_follow_triggers_on_written_tables() {
    let database = trigger_fact_schema(
        FACT_KEY,
        FACT_INSERT,
        "",
        &format!("{FACT_READ} RETURN NEW;"),
        "CREATE ROLE writer;
         CREATE TABLE requests (id INT);
         CREATE FUNCTION retarget() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN UPDATE public.bindings SET run_id = NEW.id; RETURN NEW; END $$;
         CREATE TRIGGER retarget AFTER INSERT ON requests
           FOR EACH ROW EXECUTE FUNCTION retarget();
         CREATE FUNCTION submit() RETURNS void LANGUAGE sql SECURITY DEFINER
           SET search_path = pg_catalog, pg_temp AS $$ INSERT INTO public.requests VALUES (1) $$;
         ALTER FUNCTION submit() OWNER TO writer;",
    );
    assert_eq!(
        trigger_read(&database, "a_guard", DefinerReading::Outcome),
        Some(("runs".to_owned(), "runs_select".to_owned()))
    );
}

#[test]
fn foreign_definer_writers_drop_policy_facts_for_every_event() {
    for (event, row) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("UPDATE", "OLD"), ("DELETE", "OLD")]
    {
        for (owner, unconfined) in [("writer", true), ("app", false)] {
            let database = trigger_fact_schema(
                "",
                &format!(
                    "CREATE TRIGGER z_guard AFTER {event} ON bindings
                  FOR EACH ROW EXECUTE FUNCTION guard_binding();"
                ),
                "",
                &format!(
                    "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = {row}.team_id) THEN
                    RAISE EXCEPTION 'found'; END IF; RETURN {row};"
                ),
                &format!(
                    "CREATE ROLE writer;
                     CREATE FUNCTION rewrite_binding() RETURNS void LANGUAGE sql SECURITY DEFINER
                       SET search_path = pg_catalog, pg_temp AS $$ DELETE FROM public.bindings $$;
                     ALTER FUNCTION rewrite_binding() OWNER TO {owner};"
                ),
            );
            assert_eq!(
                trigger_read(&database, "z_guard", DefinerReading::Outcome),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{event} {row} {owner}"
            );
        }
    }
}

#[test]
fn definer_writes_follow_referential_action_trigger_chains() {
    for (statement, action, event) in [
        ("DELETE FROM public.parents", "ON DELETE CASCADE", "DELETE"),
        ("DELETE FROM public.parents", "ON DELETE SET NULL", "UPDATE"),
        ("DELETE FROM public.parents", "ON DELETE SET DEFAULT", "UPDATE"),
        ("UPDATE public.parents SET id = 2", "ON UPDATE CASCADE", "UPDATE"),
        ("UPDATE public.parents SET id = 2", "ON UPDATE SET NULL", "UPDATE"),
        ("UPDATE public.parents SET id = 2", "ON UPDATE SET DEFAULT", "UPDATE"),
    ] {
        for condition in ["true", "team_id = current_setting('app.team_id')::INT"] {
            let db = schema(&format!(
                "CREATE ROLE app;
                 CREATE ROLE guard_owner;
                 CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
                 ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY runs_select ON runs FOR SELECT TO app
                   USING (team_id = current_setting('app.team_id')::INT);
                 CREATE TABLE parents (id INT PRIMARY KEY);
                 CREATE TABLE children (id INT PRIMARY KEY, parent_id INT REFERENCES parents(id) {action});
                 CREATE TABLE bindings (id INT PRIMARY KEY);
                 GRANT SELECT ON runs TO app, guard_owner;
                 GRANT INSERT ON bindings TO app;
                 GRANT SELECT, UPDATE, DELETE ON parents TO guard_owner;
                 CREATE FUNCTION inspect_child() RETURNS trigger
                 LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
                 BEGIN
                   IF EXISTS (SELECT 1 FROM public.runs WHERE {condition}) THEN
                     RAISE EXCEPTION 'matching run';
                   END IF;
                   RETURN NEW;
                 END $$;
                 CREATE TRIGGER child_guard AFTER {event} ON children
                   FOR EACH ROW EXECUTE FUNCTION inspect_child();
                 CREATE FUNCTION write_parent() RETURNS trigger
                 LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
                 BEGIN
                   {statement};
                   RETURN NEW;
                 END $$;
                 ALTER FUNCTION write_parent() OWNER TO guard_owner;
                 CREATE TRIGGER bindings_guard BEFORE INSERT ON bindings
                   FOR EACH ROW EXECUTE FUNCTION write_parent();"
            ));
            assert_eq!(
                trigger_read(&db, "bindings_guard", DefinerReading::Outcome),
                (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{statement}, {action}, {condition}"
            );
        }
    }
}

#[test]
fn definer_writes_refuse_rules_and_unknown_targets() {
    for (target, rules, unconfined) in [
        ("audit", "", None),
        (
            "audit",
            "CREATE RULE audit_rule AS ON INSERT TO audit DO ALSO NOTIFY audited;",
            Some(("audit".to_owned(), "unfollowed".to_owned())),
        ),
        ("missing", "", Some((String::new(), "unfollowed".to_owned()))),
    ] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE audit (id INT PRIMARY KEY);
             CREATE TABLE bindings (id INT PRIMARY KEY);
             GRANT INSERT ON bindings TO app;
             {rules}
             CREATE FUNCTION write_audit() RETURNS trigger
             LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               INSERT INTO public.{target} VALUES (1);
               RETURN NEW;
             END $$;
             CREATE TRIGGER bindings_guard BEFORE INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION write_audit();"
        ));
        assert_eq!(trigger_read(&db, "bindings_guard", DefinerReading::Outcome), unconfined);
    }
}

#[test]
fn sql_definer_writes_follow_statement_triggers() {
    for condition in ["true", "team_id = current_setting('app.team_id')::INT"] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON runs TO app;
             CREATE TABLE audit (id INT PRIMARY KEY);
             CREATE FUNCTION inspect_audit() RETURNS trigger
             LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               IF EXISTS (SELECT 1 FROM public.runs WHERE {condition}) THEN
                 RAISE EXCEPTION 'matching run';
               END IF;
               RETURN NULL;
             END $$;
             CREATE TRIGGER audit_guard AFTER INSERT ON audit
               FOR EACH STATEMENT EXECUTE FUNCTION inspect_audit();
             CREATE FUNCTION write_audit() RETURNS VOID
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
               INSERT INTO public.audit VALUES (1)
             $$;"
        ));
        assert_eq!(
            function_read(&db, None, "write_audit"),
            (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned()))
        );
    }
}

/// Tuple membership preserves casts in pinned values on both sides.
#[test]
fn a_truncated_tuple_pin_does_not_confine_a_read() {
    for (policy_value, pinned_value, confined) in [
        ("'writer'", "'writer'::varchar(3)", false),
        ("'writer'::varchar(3)", "'writer'", false),
        ("'writer'", "('writer')", true),
    ] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE SCHEMA authz;
             CREATE TABLE memberships (team_id INT NOT NULL, role TEXT NOT NULL);
             GRANT SELECT ON memberships TO app;
             CREATE FUNCTION authz.grants() RETURNS TABLE (team_id INT, role TEXT)
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT team_id, role FROM public.memberships $$;
             CREATE TABLE notes (id INT PRIMARY KEY, team_id INT NOT NULL,
               role TEXT NOT NULL, body TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_select ON notes FOR SELECT TO app
               USING ((team_id, {policy_value}) IN (SELECT * FROM authz.grants()));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF TEXT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT body FROM public.notes
                 WHERE (team_id, role) IN (SELECT * FROM authz.grants())
                   AND role = {pinned_value} $$;"
        ));
        assert_eq!(
            function_read(&db, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{policy_value}, {pinned_value}"
        );
    }
}

#[test]
fn conflict_skipped_insert_rows_do_not_supply_foreign_key_facts() {
    let database = trigger_fact_schema(
        FACT_KEY,
        FACT_INSERT,
        "",
        &format!("{FACT_READ} RETURN NEW;"),
        "ALTER TABLE bindings ADD PRIMARY KEY (id);",
    );
    assert_eq!(
        trigger_read(&database, "a_guard", DefinerReading::Outcome),
        Some(("runs".to_owned(), "runs_select".to_owned()))
    );
}

#[test]
fn rule_writes_bypass_acting_role_write_policy_facts() {
    for action in ["INSERT INTO bindings VALUES (1, 2, 3)", "NOTHING"] {
        let database = trigger_fact_schema(
            "",
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE TABLE requests (id INT);
                 GRANT INSERT ON requests TO app;
                 CREATE RULE insert_binding AS ON INSERT TO requests DO ALSO {action};"
            ),
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "z_guard", reading),
                (action != "NOTHING").then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{action} {reading:?}"
            );
        }
    }
}

#[test]
fn foreign_view_owners_bypass_acting_role_write_policy_facts() {
    for (options, owner, unconfined) in [
        ("", "writer", true),
        ("WITH (security_invoker = true)", "writer", false),
        ("", "app", false),
    ] {
        let database = trigger_fact_schema(
            "",
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE ROLE writer;
                 CREATE VIEW binding_entry {options} AS SELECT * FROM bindings;
                 ALTER VIEW binding_entry OWNER TO {owner};
                 GRANT INSERT ON binding_entry TO app;"
            ),
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "z_guard", reading),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{options} {owner} {reading:?}"
            );
        }
    }
}

#[test]
fn old_keys_do_not_hold_rows_rewritable_within_the_statement() {
    for grant in ["GRANT UPDATE ON bindings TO app;", ""] {
        let database = trigger_fact_schema(
            FACT_KEY,
            "CREATE TRIGGER z_guard AFTER UPDATE ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.id = OLD.run_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            grant,
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "z_guard", reading),
                (!grant.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{grant} {reading:?}"
            );
        }
    }
}

#[test]
fn writes_alone_can_raise_from_hidden_row_evaluation() {
    for statement in [
        "INSERT INTO public.audit VALUES (1 / (SELECT count(*) FROM public.runs));",
        "UPDATE public.audit SET amount = (SELECT id FROM public.runs);",
    ] {
        let database = trigger_fact_schema(
            "",
            FACT_INSERT,
            "",
            &format!("{statement} RETURN NEW;"),
            "CREATE TABLE audit (amount BIGINT);",
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            Some(("runs".to_owned(), "runs_select".to_owned())),
            "{statement}"
        );
    }
}

#[test]
fn definer_write_chains_follow_cascades_before_child_triggers() {
    for action in ["ON DELETE CASCADE", "ON DELETE SET NULL", "ON UPDATE CASCADE", ""] {
        let database = trigger_fact_schema(
            "",
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE ROLE writer;
                 CREATE TABLE parents (id INT PRIMARY KEY);
                 CREATE TABLE children (id INT PRIMARY KEY, parent_id INT REFERENCES parents (id) {action});
                 ALTER TABLE children OWNER TO writer;
                 CREATE FUNCTION insert_binding() RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN INSERT INTO public.bindings VALUES (1, 2, 3); RETURN OLD; END $$;
                 CREATE TRIGGER child_write BEFORE DELETE OR UPDATE ON children
                   FOR EACH ROW EXECUTE FUNCTION insert_binding();
                 CREATE FUNCTION rewrite_parent() RETURNS void LANGUAGE plpgsql SECURITY DEFINER
                   SET search_path = pg_catalog, pg_temp AS $$
                   BEGIN DELETE FROM public.parents; UPDATE public.parents SET id = id + 1; END $$;
                 ALTER FUNCTION rewrite_parent() OWNER TO app;"
            ),
        );
        assert_eq!(
            trigger_read(&database, "z_guard", DefinerReading::Outcome),
            (!action.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{action}"
        );
    }
}

/// Builds a definer write whose target trigger reads a policy-protected table.
fn indirect_write_schema(
    tables: &str,
    target: &str,
    event: &str,
    orientation: &str,
    statement: &str,
    condition: &str,
) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
           USING (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON runs TO app;
         {tables}
         CREATE FUNCTION inspect_write() RETURNS trigger
         LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           IF EXISTS (SELECT 1 FROM public.runs WHERE {condition}) THEN
             RAISE EXCEPTION 'matching run';
           END IF;
           RETURN NULL;
         END $$;
         CREATE TRIGGER write_guard AFTER {event} ON {target}
           FOR EACH {orientation} EXECUTE FUNCTION inspect_write();
         CREATE FUNCTION run_write() RETURNS VOID
         LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
           {statement}
         $$;"
    ))
}

#[test]
fn definer_truncates_follow_statement_triggers_and_cascaded_tables() {
    for (statement, target) in [
        ("TRUNCATE public.audit CASCADE", "audit"),
        ("TRUNCATE public.audit CASCADE", "child"),
        ("TRUNCATE public.audit CASCADE", "grandchild"),
        ("TRUNCATE public.audit, public.child, public.grandchild", "grandchild"),
    ] {
        for condition in ["true", "team_id = current_setting('app.team_id')::INT"] {
            let db = indirect_write_schema(
                "CREATE TABLE audit (id INT PRIMARY KEY);
                 CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES audit(id));
                 CREATE TABLE grandchild (id INT PRIMARY KEY, parent_id INT REFERENCES child(id));",
                target,
                "TRUNCATE",
                "STATEMENT",
                statement,
                condition,
            );
            assert_eq!(
                function_read(&db, None, "run_write"),
                (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{statement}, {target}, {condition}"
            );
        }
    }
}

#[test]
fn definer_partition_writes_follow_the_tree_and_row_movement_events() {
    for (statement, target, event) in [
        ("INSERT INTO public.events VALUES (1)", "events_low", "INSERT"),
        ("INSERT INTO public.events_low VALUES (1)", "events", "INSERT"),
        ("UPDATE public.events_low SET id = 11", "events_high", "INSERT"),
        ("UPDATE public.events SET id = 11", "events_low", "DELETE"),
        ("DELETE FROM public.events_low", "children", "DELETE"),
    ] {
        for condition in ["true", "team_id = current_setting('app.team_id')::INT"] {
            let db = indirect_write_schema(
                "CREATE TABLE events (id INT PRIMARY KEY) PARTITION BY RANGE (id);
                 CREATE TABLE events_low PARTITION OF events FOR VALUES FROM (0) TO (10);
                 CREATE TABLE events_high PARTITION OF events FOR VALUES FROM (10) TO (20);
                 CREATE TABLE children (id INT PRIMARY KEY,
                   event_id INT REFERENCES events(id) ON DELETE CASCADE);",
                target,
                event,
                "ROW",
                statement,
                condition,
            );
            assert_eq!(
                function_read(&db, None, "run_write"),
                (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{statement}, {target}, {event}, {condition}"
            );
        }
    }
}

#[test]
fn carried_helper_filters_preserve_quoted_column_names_and_collisions() {
    for (name, collision) in [("\"TeamId\"", false), ("\"Team\"", true)] {
        let mut sql = TEMPLATES.replace("organization_id", name);
        if collision {
            sql = sql.replace("\"Team\" INT", "team INT, \"Team\" INT");
        }
        let db = schema(&sql);
        assert_eq!(
            trigger_read(&db, "template_steps_guard", DefinerReading::Outcome),
            None,
            "{name}, {collision}"
        );
        let unkeyed = schema(&sql.replace(
            &format!("FOREIGN KEY ({name}, id) REFERENCES resources ({name}, id)"),
            &format!("UNIQUE ({name}, id)"),
        ));
        assert_eq!(
            trigger_read(&unkeyed, "template_steps_guard", DefinerReading::Outcome),
            Some(("procedure_templates".to_owned(), "procedure_templates_blocked".to_owned())),
            "{name}, {collision}"
        );
    }
}

#[test]
fn view_update_privileges_make_new_keys_rewritable() {
    for privilege in ["UPDATE", "DELETE", "INSERT"] {
        let database = trigger_fact_schema(
            FACT_KEY,
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.id = NEW.run_id
                 AND NEW.team_id = current_setting('app.team_id')::INT) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE ROLE writer;
                 CREATE VIEW binding_entry AS SELECT * FROM bindings;
                 ALTER VIEW binding_entry OWNER TO writer;
                 GRANT {privilege} ON binding_entry TO app;"
            ),
        );
        assert_eq!(
            trigger_read(&database, "z_guard", DefinerReading::Outcome),
            (privilege != "INSERT").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{privilege}"
        );
    }
}

#[test]
fn trigger_record_facts_resolve_quoted_stored_column_names() {
    for name in ["\"TeamId\"", "\"Team\""] {
        for field in [name, "id"] {
            let db = schema(&format!(
                "CREATE ROLE app;
                 CREATE TABLE runs (id INT PRIMARY KEY, {name} INT NOT NULL, team TEXT);
                 ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY runs_select ON runs FOR SELECT TO app
                   USING ({name} = current_setting('app.team_id')::INT);
                 CREATE TABLE bindings (id INT PRIMARY KEY, {name} INT NOT NULL, team TEXT);
                 ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY bindings_insert ON bindings FOR INSERT TO app
                   WITH CHECK ({name} = current_setting('app.team_id')::INT);
                 GRANT SELECT ON runs TO app;
                 GRANT INSERT ON bindings TO app;
                 CREATE FUNCTION inspect_binding() RETURNS trigger
                 LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
                 BEGIN
                   IF EXISTS (SELECT 1 FROM public.runs WHERE {name} = NEW.{field}) THEN
                     RAISE EXCEPTION 'matching run';
                   END IF;
                   RETURN NEW;
                 END $$;
                 CREATE TRIGGER bindings_guard BEFORE INSERT ON bindings
                   FOR EACH ROW EXECUTE FUNCTION inspect_binding();"
            ));
            assert_eq!(
                trigger_read(&db, "bindings_guard", DefinerReading::Outcome),
                (field == "id").then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{name}, {field}"
            );
        }
    }
}

#[test]
fn drawn_helpers_resolve_quoted_stored_column_names() {
    for name in ["\"TeamId\"", "\"Team\""] {
        for condition in ["NOT resource.hidden", "true"] {
            let db = schema(&format!(
                "CREATE ROLE app;
                 CREATE TABLE resources ({name} INT PRIMARY KEY, team TEXT, hidden BOOLEAN NOT NULL);
                 ALTER TABLE resources ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY resources_select ON resources FOR SELECT TO app USING (NOT hidden);
                 GRANT SELECT ON resources TO app;
                 CREATE FUNCTION listed() RETURNS SETOF INT
                 LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
                   SELECT resource.{name} FROM public.resources resource WHERE {condition}
                 $$;
                 CREATE FUNCTION inspect_resources() RETURNS SETOF INT
                 LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
                   SELECT resource.{name} FROM public.resources resource
                   WHERE resource.{name} IN (SELECT public.listed())
                 $$;"
            ));
            assert_eq!(
                function_read(&db, None, "inspect_resources"),
                (condition == "true")
                    .then(|| ("resources".to_owned(), "resources_select".to_owned())),
                "{name}, {condition}"
            );
        }
    }
}

#[test]
fn exclusion_presence_tracks_added_and_dropped_constraints() {
    for (alter, excluded) in [
        ("", false),
        (
            "ALTER TABLE reservations ADD CONSTRAINT overlaps EXCLUDE USING gist (period WITH &&);",
            true,
        ),
        (
            "ALTER TABLE reservations ADD CONSTRAINT overlaps EXCLUDE USING gist (period WITH &&);
          ALTER TABLE reservations DROP CONSTRAINT overlaps;",
            false,
        ),
    ] {
        let database = schema(&format!("CREATE TABLE reservations (period INT4RANGE); {alter}"));
        let table = database.tables().next().expect("the table exists");
        assert_eq!(table.has_exclusion_constraints(&database).expect("the table exists"), excluded);
    }
}

#[test]
fn conflict_arbiters_prevent_before_insert_key_facts() {
    for arbiter in [
        "ALTER TABLE bindings ADD PRIMARY KEY (id);",
        "ALTER TABLE bindings ADD UNIQUE (id);",
        "CREATE UNIQUE INDEX bindings_identity ON bindings (id);",
        "ALTER TABLE bindings ADD EXCLUDE USING gist (period WITH &&);",
        "",
    ] {
        let database = trigger_fact_schema(
            &format!("{FACT_KEY}, period INT4RANGE"),
            FACT_INSERT,
            "",
            &format!("{FACT_READ} RETURN NEW;"),
            arbiter,
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            (!arbiter.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{arbiter}"
        );
    }
}

#[test]
fn definer_bodies_refuse_unmodeled_statement_effects() {
    for (statement, refused) in
        [("SELECT 1", false), ("CREATE TABLE public.created (id INT)", true)]
    {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE FUNCTION run_statement() RETURNS VOID
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
               {statement}
             $$;"
        ));
        assert_eq!(
            function_read(&db, None, "run_statement"),
            refused.then(|| (String::new(), "unfollowed".to_owned())),
            "{statement}"
        );
    }
}

/// Builds a guard on a partitioned table with configurable partition access.
fn partition_fact_schema(read: &str, timing: &str, extra: &str) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE ROLE writer;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL, UNIQUE (id, team_id));
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
           USING (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON runs TO app;
         CREATE TABLE bindings (id INT, team_id INT NOT NULL, run_id INT NOT NULL,
           FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id)) PARTITION BY LIST (team_id);
         CREATE TABLE bindings_1 PARTITION OF bindings FOR VALUES IN (1);
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY bindings_write ON bindings FOR ALL TO app
           USING (team_id = current_setting('app.team_id')::INT)
           WITH CHECK (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT, INSERT ON bindings, bindings_1 TO app;
         CREATE FUNCTION guard_binding() RETURNS trigger
           LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
           BEGIN
             IF EXISTS (SELECT 1 FROM public.runs WHERE {read}) THEN
               RAISE EXCEPTION 'found';
             END IF;
             RETURN NEW;
           END $$;
         CREATE TRIGGER a_guard {timing} INSERT ON bindings
           FOR EACH ROW EXECUTE FUNCTION guard_binding();
         {extra}"
    ))
}

const PARTITION_POLICY: &str = "
    ALTER TABLE bindings_1 ENABLE ROW LEVEL SECURITY;
    CREATE POLICY partition_write ON bindings_1 FOR ALL TO app
      USING (team_id = current_setting('app.team_id')::INT)
      WITH CHECK (team_id = current_setting('app.team_id')::INT);";

#[test]
fn direct_partition_inserts_do_not_inherit_parent_policy_facts() {
    for (extra, unconfined) in [("", true), (PARTITION_POLICY, false)] {
        let database = partition_fact_schema("runs.team_id = NEW.team_id", "AFTER", extra);
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "a_guard", reading),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{extra} {reading:?}"
            );
        }
    }
}

#[test]
fn partition_update_privileges_invalidate_parent_key_facts() {
    for grant in ["GRANT UPDATE ON bindings_1 TO app;", ""] {
        let database = partition_fact_schema(
            "runs.id = NEW.run_id",
            "AFTER",
            &format!("{PARTITION_POLICY} {grant}"),
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            (!grant.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{grant}"
        );
    }
}

#[test]
fn partition_conflict_arbiters_invalidate_parent_before_insert_keys() {
    for index in ["CREATE UNIQUE INDEX child_identity ON bindings_1 (id);", ""] {
        let database = partition_fact_schema(
            "runs.id = NEW.run_id",
            "BEFORE",
            &format!("{PARTITION_POLICY} {index}"),
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            (!index.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{index}"
        );
    }
}

#[test]
fn later_partition_before_triggers_invalidate_parent_row_facts() {
    for (name, unconfined) in [("z_retarget", true), ("\"A_retarget\"", false)] {
        let database = partition_fact_schema(
            "runs.team_id = NEW.team_id",
            "BEFORE",
            &format!(
                "{PARTITION_POLICY}
                 CREATE FUNCTION retarget_partition() RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN NEW.team_id := 1; RETURN NEW; END $$;
                 CREATE TRIGGER {name} BEFORE INSERT ON bindings_1
                   FOR EACH ROW EXECUTE FUNCTION retarget_partition();"
            ),
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{name}"
        );
    }
}

#[test]
fn foreign_definer_partition_writes_bypass_parent_policy_facts() {
    for target in ["bindings_1", "unrelated"] {
        let database = partition_fact_schema(
            "runs.team_id = NEW.team_id", "AFTER",
            &format!(
                "{PARTITION_POLICY}
                 CREATE TABLE unrelated (id INT, team_id INT, run_id INT);
                 CREATE FUNCTION write_partition() RETURNS void LANGUAGE sql SECURITY DEFINER
                   SET search_path = pg_catalog, pg_temp AS $$ INSERT INTO public.{target} VALUES (1, 1, 1) $$;
                 ALTER FUNCTION write_partition() OWNER TO writer;"
            ),
        );
        assert_eq!(
            trigger_read(&database, "a_guard", DefinerReading::Outcome),
            (target == "bindings_1").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{target}"
        );
    }
}

#[test]
fn acting_role_cascades_root_foreign_trigger_write_chains() {
    for action in ["ON DELETE CASCADE", "ON DELETE SET NULL", "ON UPDATE CASCADE", ""] {
        let database = trigger_fact_schema(
            "",
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE ROLE writer;
                 CREATE TABLE parents (id INT PRIMARY KEY);
                 CREATE TABLE children (id INT PRIMARY KEY, parent_id INT REFERENCES parents (id) {action});
                 ALTER TABLE children OWNER TO writer;
                 GRANT UPDATE, DELETE ON parents TO app;
                 CREATE FUNCTION insert_binding() RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN INSERT INTO public.bindings VALUES (1, 2, 3); RETURN OLD; END $$;
                 CREATE TRIGGER child_write BEFORE DELETE OR UPDATE ON children
                   FOR EACH ROW EXECUTE FUNCTION insert_binding();"
            ),
        );
        assert_eq!(
            trigger_read(&database, "z_guard", DefinerReading::Outcome),
            (!action.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{action}"
        );
    }
}

#[test]
fn definer_copy_from_bypasses_acting_role_write_policy_facts() {
    for target in ["bindings", "unrelated"] {
        let database = trigger_fact_schema(
            "",
            "CREATE TRIGGER z_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE ROLE writer;
                 CREATE TABLE unrelated (id INT);
                 CREATE FUNCTION load_rows() RETURNS void LANGUAGE plpgsql SECURITY DEFINER
                   SET search_path = pg_catalog, pg_temp AS $$
                   BEGIN COPY public.{target} FROM '/data/rows.csv'; END $$;
                 ALTER FUNCTION load_rows() OWNER TO writer;"
            ),
        );
        assert_eq!(
            trigger_read(&database, "z_guard", DefinerReading::Outcome),
            (target == "bindings").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{target}"
        );
    }
}

#[test]
fn definer_writes_follow_the_statement_search_path() {
    for path in ["public", "second"] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE SCHEMA second;
             CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON runs TO app;
             CREATE TABLE audit (id INT PRIMARY KEY);
             CREATE TABLE second.audit (id INT PRIMARY KEY);
             CREATE FUNCTION inspect_audit() RETURNS trigger
             LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               IF EXISTS (SELECT 1 FROM public.runs) THEN
                 RAISE EXCEPTION 'matching run';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER audit_guard BEFORE INSERT ON second.audit
               FOR EACH ROW EXECUTE FUNCTION inspect_audit();
             CREATE FUNCTION write_audit() RETURNS VOID
             LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
             BEGIN
               SET LOCAL search_path = {path}, pg_temp;
               INSERT INTO audit VALUES (1);
             END $$;"
        ));
        assert_eq!(
            function_read(&db, None, "write_audit"),
            (path == "second").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{path}"
        );
    }
}

#[test]
fn definer_reads_follow_the_statement_search_path() {
    for (relation, condition) in [
        ("runs", "true"),
        ("runs", "team_id = current_setting('app.team_id')::INT"),
        ("second.runs", "true"),
        ("second.runs", "team_id = current_setting('app.team_id')::INT"),
    ] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE SCHEMA second;
             CREATE TABLE public.runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             CREATE TABLE second.runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             ALTER TABLE second.runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON second.runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON second.runs TO app;
             CREATE FUNCTION inspect_runs() RETURNS VOID
             LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               SET LOCAL search_path = second, pg_temp;
               IF EXISTS (SELECT 1 FROM {relation} WHERE {condition}) THEN
                 RAISE EXCEPTION 'matching run';
               END IF;
             END $$;"
        ));
        assert_eq!(
            function_read(&db, None, "inspect_runs"),
            if relation == "runs" {
                Some((String::new(), "unfollowed".to_owned()))
            } else {
                (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned()))
            },
            "{relation}, {condition}"
        );
    }
}

#[test]
fn definer_copy_to_checks_table_reads() {
    for (target, expected) in [
        ("runs", Some(("runs".to_owned(), "unfollowed".to_owned()))),
        ("open_runs", None),
        ("missing", Some((String::new(), "unfollowed".to_owned()))),
    ] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             CREATE TABLE open_runs (id INT PRIMARY KEY);
             GRANT SELECT ON runs, open_runs TO app;
             CREATE FUNCTION export_runs() RETURNS VOID
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
               COPY public.{target} TO '/srv/runs.csv'
             $$;"
        ));
        assert_eq!(function_read(&db, None, "export_runs"), expected, "{target}");
    }
}

#[test]
fn sql_body_search_path_changes_preserve_confined_reads() {
    for condition in ["team_id = current_setting('app.team_id')::INT", "true"] {
        let db = schema(&format!(
            "CREATE ROLE app;
             CREATE SCHEMA second;
             CREATE TABLE second.runs (id INT PRIMARY KEY, team_id INT NOT NULL);
             ALTER TABLE second.runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON second.runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON second.runs TO app;
             CREATE FUNCTION listed_runs() RETURNS SETOF INT
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
               SET search_path = second, pg_temp;
               SELECT id FROM second.runs WHERE {condition}
             $$;"
        ));
        assert_eq!(
            function_read(&db, None, "listed_runs"),
            (condition == "true").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{condition}"
        );
    }
}

#[test]
fn partition_row_moves_intersect_insert_and_delete_with_update_policies() {
    for (event, row) in [("INSERT", "NEW"), ("DELETE", "OLD")] {
        for (update, unconfined) in
            [("true", true), ("team_id = current_setting('app.team_id')::INT", false)]
        {
            let database = partition_fact_schema(
                &format!("runs.team_id = {row}.team_id"),
                "AFTER",
                &format!(
                    "{PARTITION_POLICY}
                     CREATE TABLE bindings_2 PARTITION OF bindings FOR VALUES IN (2);
                     ALTER TABLE bindings_2 ENABLE ROW LEVEL SECURITY;
                     CREATE POLICY second_write ON bindings_2 FOR ALL TO app
                       USING (team_id = current_setting('app.team_id')::INT)
                       WITH CHECK (team_id = current_setting('app.team_id')::INT);
                     DROP POLICY bindings_write ON bindings;
                     CREATE POLICY binding_insert ON bindings FOR INSERT TO app
                       WITH CHECK (team_id = current_setting('app.team_id')::INT);
                     CREATE POLICY binding_delete ON bindings FOR DELETE TO app
                       USING (team_id = current_setting('app.team_id')::INT);
                     CREATE POLICY binding_update ON bindings FOR UPDATE TO app USING ({update});
                     GRANT UPDATE ON bindings TO app;
                     DROP TRIGGER a_guard ON bindings;
                     CREATE TRIGGER a_guard AFTER {event} ON bindings
                       FOR EACH ROW EXECUTE FUNCTION guard_binding();"
                ),
            );
            assert_eq!(
                trigger_read(&database, "a_guard", DefinerReading::Outcome),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{event} {update}"
            );
        }
    }
}

#[test]
fn cascading_partition_moves_bypass_insert_and_delete_policies() {
    for (event, row) in [("INSERT", "NEW"), ("DELETE", "OLD")] {
        for action in ["ON UPDATE CASCADE", ""] {
            let database = partition_fact_schema(
                &format!("runs.team_id = {row}.team_id"),
                "AFTER",
                &format!(
                    "{PARTITION_POLICY}
                     CREATE TABLE teams (id INT PRIMARY KEY);
                     ALTER TABLE bindings ADD CONSTRAINT binding_team
                       FOREIGN KEY (team_id) REFERENCES teams (id) {action};
                     DROP TRIGGER a_guard ON bindings;
                     CREATE TRIGGER a_guard AFTER {event} ON bindings
                       FOR EACH ROW EXECUTE FUNCTION guard_binding();"
                ),
            );
            assert_eq!(
                trigger_read(&database, "a_guard", DefinerReading::Outcome),
                (!action.is_empty()).then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{event} {action}"
            );
        }
    }
}

#[test]
fn inheritance_parent_policies_limit_child_trigger_facts() {
    for (update, unconfined) in
        [("true", true), ("team_id = current_setting('app.team_id')::INT", false)]
    {
        let database = trigger_fact_schema(
            "",
            "",
            "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "DROP POLICY bindings_write ON bindings;
                 CREATE POLICY parent_update ON bindings FOR UPDATE TO app USING ({update});
                 GRANT UPDATE ON bindings TO app;
                 CREATE TABLE bindings_archive () INHERITS (bindings);
                 ALTER TABLE bindings_archive ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY archive_write ON bindings_archive FOR ALL TO app
                   USING (team_id = current_setting('app.team_id')::INT)
                   WITH CHECK (team_id = current_setting('app.team_id')::INT);
                 CREATE TRIGGER z_guard AFTER UPDATE ON bindings_archive
                   FOR EACH ROW EXECUTE FUNCTION guard_binding();"
            ),
        );
        assert_eq!(
            trigger_read(&database, "z_guard", DefinerReading::Outcome),
            unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{update}"
        );
    }
}

#[test]
fn definer_parent_writes_reach_inheritance_child_triggers() {
    for target in ["bindings", "unrelated"] {
        let database = trigger_fact_schema(
            "", "", "",
            "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = NEW.team_id) THEN
               RAISE EXCEPTION 'found'; END IF; RETURN NEW;",
            &format!(
                "CREATE ROLE writer;
                 CREATE TABLE unrelated (team_id INT);
                 CREATE TABLE bindings_archive () INHERITS (bindings);
                 ALTER TABLE bindings_archive ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY archive_write ON bindings_archive FOR ALL TO app
                   USING (team_id = current_setting('app.team_id')::INT)
                   WITH CHECK (team_id = current_setting('app.team_id')::INT);
                 CREATE TRIGGER z_guard AFTER UPDATE ON bindings_archive
                   FOR EACH ROW EXECUTE FUNCTION guard_binding();
                 CREATE FUNCTION rewrite_parent() RETURNS void LANGUAGE sql SECURITY DEFINER
                   SET search_path = pg_catalog, pg_temp AS $$ UPDATE public.{target} SET team_id = 7 $$;
                 ALTER FUNCTION rewrite_parent() OWNER TO writer;"
            ),
        );
        assert_eq!(
            trigger_read(&database, "z_guard", DefinerReading::Outcome),
            (target == "bindings").then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{target}"
        );
    }
}

#[test]
fn unprotected_reads_need_only_the_statement_search_path() {
    let db = schema(
        "CREATE ROLE app;
         CREATE SCHEMA app_data;
         CREATE TABLE app_data.items (id INT);
         CREATE FUNCTION count_items() RETURNS BIGINT
         LANGUAGE sql SECURITY DEFINER SET search_path = app_data, pg_temp AS $$
           SELECT count(*) FROM items
         $$;",
    );
    assert_eq!(function_read(&db, None, "count_items"), None);
}

#[test]
fn standalone_update_actions_preserve_insert_and_delete_policy_facts() {
    for (action, event, row) in
        [("ON UPDATE CASCADE", "INSERT", "NEW"), ("ON DELETE SET NULL", "DELETE", "OLD")]
    {
        let database = trigger_fact_schema(
            &format!(", FOREIGN KEY (run_id) REFERENCES runs (id) {action}"),
            &format!(
                "CREATE TRIGGER z_guard AFTER {event} ON bindings
                FOR EACH ROW EXECUTE FUNCTION guard_binding();"
            ),
            "",
            &format!(
                "IF EXISTS (SELECT 1 FROM public.runs WHERE runs.team_id = {row}.team_id) THEN
                RAISE EXCEPTION 'found'; END IF; RETURN {row};"
            ),
            "",
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "z_guard", reading),
                None,
                "{action} {event} {reading:?}"
            );
        }
    }
}

#[test]
fn insert_only_partition_roles_keep_insert_policy_facts() {
    for (grant, unconfined) in [
        ("", false),
        (
            "GRANT UPDATE ON bindings TO app;
          CREATE POLICY parent_update ON bindings FOR UPDATE TO app USING (true);",
            true,
        ),
    ] {
        let database = partition_fact_schema(
            "runs.team_id = NEW.team_id",
            "AFTER",
            &format!(
                "ALTER TABLE bindings_1 ENABLE ROW LEVEL SECURITY;
                 DROP POLICY bindings_write ON bindings;
                 CREATE POLICY parent_insert ON bindings FOR INSERT TO app
                   WITH CHECK (team_id = current_setting('app.team_id')::INT);
                 CREATE POLICY child_insert ON bindings_1 FOR INSERT TO app
                   WITH CHECK (team_id = current_setting('app.team_id')::INT);
                 {grant}"
            ),
        );
        for reading in [DefinerReading::Outcome, DefinerReading::Disclosure] {
            assert_eq!(
                trigger_read(&database, "a_guard", reading),
                unconfined.then(|| ("runs".to_owned(), "runs_select".to_owned())),
                "{grant} {reading:?}"
            );
        }
    }
}
