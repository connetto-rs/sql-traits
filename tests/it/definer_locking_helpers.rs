//! Locking reads of a trigger's definer chain, made inline or in a
//! `SECURITY DEFINER` helper the trigger's function passes a field of `NEW`.

use sql_traits::prelude::*;

use crate::support::definer_reads::{schema, trigger_read};

/// A guard on `bindings` whose function runs `first`, then reads the run its
/// row references, with `lock_run` locking the run it is passed under `lock`
/// and `helpers` declaring further functions.
fn guarded(lock: &str, first: &str, helpers: &str) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE ROLE guard_owner;
         CREATE TABLE runs (id INT PRIMARY KEY, team_id INT NOT NULL, status TEXT NOT NULL,
             UNIQUE (id, team_id));
         ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
         CREATE POLICY runs_select ON runs FOR SELECT TO app
             USING (team_id = current_setting('app.team_id')::INT);
         CREATE TABLE bindings (id INT PRIMARY KEY, team_id INT NOT NULL,
             run_id INT NOT NULL, FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id));
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY bindings_insert ON bindings FOR INSERT TO app
             WITH CHECK (team_id = current_setting('app.team_id')::INT);
         GRANT SELECT ON runs TO app;
         GRANT INSERT ON bindings TO app;
         CREATE FUNCTION lock_run(locked_run_id INT) RETURNS VOID
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           PERFORM run.id FROM public.runs run WHERE run.id = locked_run_id{lock};
         END;
         $$;
         ALTER FUNCTION lock_run(INT) OWNER TO guard_owner;
         {helpers}
         CREATE FUNCTION guard_binding() RETURNS trigger
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           {first}
           IF EXISTS (SELECT 1 FROM public.runs
                      WHERE runs.id = NEW.run_id AND runs.status <> 'open') THEN
             RAISE EXCEPTION 'run % is closed', NEW.run_id;
           END IF;
           RETURN NEW;
         END
         $$;
         ALTER FUNCTION guard_binding() OWNER TO guard_owner;
         CREATE TRIGGER bindings_guard AFTER INSERT ON bindings
             FOR EACH ROW EXECUTE FUNCTION guard_binding();"
    ))
}

/// The read `bindings_guard` leaves unconfined when `app` inserts a binding.
fn guard_read(database: &ParserDB) -> Option<(String, String)> {
    trigger_read(database, "bindings_guard", DefinerReading::Outcome)
}

/// The refusal of a read of `runs` that `runs_select` may not admit.
fn refused_run() -> (String, String) {
    ("runs".into(), "runs_select".into())
}

/// A lock leaves the rows a read reaches as they are without it.
#[test]
fn locking_read_of_referenced_row_is_confined() {
    for lock in [
        "FOR UPDATE",
        "FOR NO KEY UPDATE",
        "FOR SHARE",
        "FOR KEY SHARE",
        "FOR UPDATE OF run NOWAIT",
        "FOR SHARE SKIP LOCKED",
    ] {
        let database = guarded(
            "",
            &format!("PERFORM run.id FROM public.runs run WHERE run.id = NEW.run_id {lock};"),
            "",
        );
        assert_eq!(guard_read(&database), None, "{lock}");
    }
}

/// A locking read that reaches a run of another team stays refused.
#[test]
fn locking_read_of_any_row_is_refused() {
    for read in [
        "PERFORM run.id FROM public.runs run FOR UPDATE;",
        "PERFORM run.id FROM public.runs run WHERE run.id IN \
         (SELECT other.id FROM public.runs other FOR SHARE);",
    ] {
        let database = guarded("", read, "");
        assert_eq!(guard_read(&database), Some(refused_run()), "{read}");
    }
}

/// A helper passed a field of `NEW` reads as the same read inline does.
#[test]
fn helper_passed_new_field_is_confined() {
    for lock in ["", " FOR NO KEY UPDATE", " FOR UPDATE", " FOR SHARE"] {
        let database = guarded(lock, "PERFORM public.lock_run(NEW.run_id);", "");
        assert_eq!(guard_read(&database), None, "{lock}");
    }
}

/// A helper called in an expression or as a relation knows the field too.
#[test]
fn helper_called_in_expression_is_confined() {
    for first in [
        "IF (SELECT public.lock_run(NEW.run_id)) IS NULL THEN NULL; END IF;",
        "PERFORM 1 FROM public.lock_run(NEW.run_id);",
        "PERFORM public.lock_run(NEW.run_id), public.lock_run(NEW.run_id);",
        "PERFORM length('run'), public.lock_run(NEW.run_id);",
    ] {
        let database = guarded(" FOR NO KEY UPDATE", first, "");
        assert_eq!(guard_read(&database), None, "{first}");
    }
}

/// A helper passing its parameter on to another helper passes the field.
#[test]
fn nested_helper_is_confined() {
    let database = guarded(
        " FOR NO KEY UPDATE",
        "PERFORM public.lock_twice(NEW.run_id);",
        "CREATE FUNCTION lock_twice(twice_run_id INT) RETURNS VOID
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           PERFORM public.lock_run(twice_run_id);
         END;
         $$;
         ALTER FUNCTION lock_twice(INT) OWNER TO guard_owner;",
    );
    assert_eq!(guard_read(&database), None);
}

/// A helper running as the definer that calls it knows the field too.
#[test]
fn invoker_helper_is_confined() {
    let database = guarded(
        "",
        "PERFORM public.lock_invoked(NEW.run_id);",
        "CREATE FUNCTION lock_invoked(invoked_run_id INT) RETURNS VOID
         LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           PERFORM run.id FROM public.runs run WHERE run.id = invoked_run_id FOR SHARE;
         END;
         $$;",
    );
    assert_eq!(guard_read(&database), None);
}

/// A helper passed anything other than the field `NEW` entered with knows
/// nothing of the row.
#[test]
fn helper_passed_other_value_is_refused() {
    for first in [
        "PERFORM public.lock_run(7);",
        "PERFORM public.lock_run(NEW.run_id + 1);",
        "PERFORM public.lock_run(NEW.id);",
        "NEW.run_id := 7; PERFORM public.lock_run(NEW.run_id);",
        "PERFORM public.lock_run(NEW.run_id); PERFORM public.lock_run(NEW.id);",
    ] {
        let database = guarded(" FOR NO KEY UPDATE", first, "");
        assert_eq!(guard_read(&database), Some(refused_run()), "{first}");
    }
}

/// A helper that changes its parameter before reading knows nothing of the
/// row.
#[test]
fn helper_reassigning_parameter_is_refused() {
    let database = guarded(
        "",
        "PERFORM public.lock_moved(NEW.run_id);",
        "CREATE FUNCTION lock_moved(moved_run_id INT) RETURNS VOID
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           moved_run_id := moved_run_id + 1;
           PERFORM run.id FROM public.runs run WHERE run.id = moved_run_id FOR UPDATE;
         END;
         $$;
         ALTER FUNCTION lock_moved(INT) OWNER TO guard_owner;",
    );
    assert_eq!(guard_read(&database), Some(refused_run()));
}

/// A helper's proof with the facts of one trigger holds for no other caller.
#[test]
fn helper_proof_is_not_reused_without_facts() {
    let database = guarded(
        "",
        "PERFORM public.lock_run(NEW.run_id); PERFORM public.unbound_lock(NEW.run_id);",
        "CREATE FUNCTION unbound_lock(any_run_id INT) RETURNS VOID
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           PERFORM public.lock_run(any_run_id + 0);
         END;
         $$;
         ALTER FUNCTION unbound_lock(INT) OWNER TO guard_owner;",
    );
    assert_eq!(guard_read(&database), Some(refused_run()));
}

/// A helper reached again through its own call proves nothing.
#[test]
fn recursive_helper_is_unfollowed() {
    let database = guarded(
        "",
        "PERFORM public.lock_again(NEW.run_id);",
        "CREATE FUNCTION lock_again(again_run_id INT) RETURNS VOID
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
         BEGIN
           PERFORM run.id FROM public.runs run WHERE run.id = again_run_id FOR UPDATE;
           PERFORM public.lock_again(again_run_id);
         END;
         $$;
         ALTER FUNCTION lock_again(INT) OWNER TO guard_owner;",
    );
    assert_eq!(guard_read(&database), Some((String::new(), "unfollowed".into())));
}

/// A `LANGUAGE sql` helper learns no field, so neither does the helper it
/// passes its parameter on to.
#[test]
fn sql_helper_passes_no_field() {
    let database = guarded(
        " FOR UPDATE",
        "PERFORM public.lock_in_sql(NEW.run_id);",
        "CREATE FUNCTION lock_in_sql(sql_run_id INT) RETURNS VOID
         LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
           SELECT public.lock_run(sql_run_id)
         $$;
         ALTER FUNCTION lock_in_sql(INT) OWNER TO guard_owner;",
    );
    assert_eq!(guard_read(&database), Some(refused_run()));
}

/// A parameter whose type `=` compares otherwise than the field's column,
/// as `CHAR` ignores trailing spaces, or a field whose column has its own
/// collation, may match a row the field does not.
#[test]
fn helper_parameter_comparing_otherwise_is_refused() {
    for (collation, parameter, confined) in [
        ("", "TEXT", true),
        ("", "VARCHAR(8)", true),
        ("", "CHAR(8)", false),
        (" COLLATE \"C\"", "TEXT", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE ROLE guard_owner;
             CREATE TABLE runs (code TEXT PRIMARY KEY, team_id INT NOT NULL,
                 UNIQUE (code, team_id));
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
                 USING (team_id = current_setting('app.team_id')::INT);
             CREATE TABLE bindings (id INT PRIMARY KEY, team_id INT NOT NULL,
                 run_code TEXT{collation} NOT NULL,
                 FOREIGN KEY (run_code, team_id) REFERENCES runs (code, team_id));
             ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
             CREATE POLICY bindings_insert ON bindings FOR INSERT TO app
                 WITH CHECK (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON runs TO app;
             GRANT INSERT ON bindings TO app;
             CREATE FUNCTION lock_run(locked_code {parameter}) RETURNS VOID
             LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               PERFORM run.code FROM public.runs run WHERE run.code = locked_code FOR UPDATE;
             END;
             $$;
             ALTER FUNCTION lock_run({parameter}) OWNER TO guard_owner;
             CREATE FUNCTION guard_binding() RETURNS trigger
             LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
             BEGIN
               PERFORM public.lock_run(NEW.run_code);
               RETURN NEW;
             END
             $$;
             ALTER FUNCTION guard_binding() OWNER TO guard_owner;
             CREATE TRIGGER bindings_guard AFTER INSERT ON bindings
                 FOR EACH ROW EXECUTE FUNCTION guard_binding();"
        ));
        let expected = (!confined).then(refused_run);
        assert_eq!(guard_read(&database), expected, "{collation} {parameter}");
    }
}
