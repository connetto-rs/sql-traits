//! Which facts a definer read rests on imply a row-level security policy.

use crate::support::definer_reads::{function_read, schema};

/// Every branch of a read filter must imply the policy.
#[test]
fn boolean_filters_imply_conjunctions_and_disjunctions() {
    for (policy, filter, confined) in [
        ("id = 1 OR id = 2", "id = 1", true),
        ("id = 1 OR id = 2", "id = 2", true),
        ("id = 1 OR id = 2", "id = 3", false),
        ("id = 1 OR id = 2", "id IN (1, 2)", true),
        ("id = 1 OR id = 2", "id IN (1, 3)", false),
        ("id = 1", "(id = 1 AND active) OR (id = 1 AND NOT active)", true),
        ("id = 1", "(id = 1 AND active) OR id = 2", false),
        ("id = 1 AND active", "(id = 1 AND active) AND body <> ''", true),
        ("id = 1 AND active", "id = 1", false),
        ("id = 1", "id = (SELECT 1 AS chosen)", true),
        ("id = (SELECT 1)", "id = 1", true),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE notes (id INT PRIMARY KEY, active BOOLEAN, body TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING ({policy});
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF TEXT
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT body FROM public.notes WHERE {filter}
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "visible".to_owned())),
            "{policy} / {filter}"
        );
    }
}

/// A filter rejecting a null-extended row makes its outer join usable for
/// confinement.
#[test]
fn strict_outer_join_filters_confine_matching_rows() {
    for filter in [
        "task.id IS NOT NULL",
        "NOT ((task.id IS NULL))",
        "NOT (task.id = 0)",
        "(task.id::BIGINT) > 0",
        "-task.id < 0",
        "0 < task.id + 1",
        "task.id = ANY (ARRAY[1, 2])",
        "1 = ANY (task.numbers)",
        "NOT (task.id = ANY (ARRAY[0]))",
        "task.id IN (SELECT 1)",
        "task.id IN (1, 2)",
        "task.id BETWEEN 1 AND 2",
        "task.body LIKE 'a%'",
        "'abc' LIKE task.body",
        "task.body ILIKE 'a%'",
        "task.body SIMILAR TO 'a%'",
        "public.strict_id(task.id) > 0",
        "public.strict_pair(1, task.id) > 0",
    ] {
        let policy = filter.replace("task.", "");
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE projects (id INT PRIMARY KEY);
             CREATE TABLE tasks (id INT PRIMARY KEY, project_id INT, body TEXT, numbers INT[]);
             CREATE FUNCTION strict_id(value INT) RETURNS INT LANGUAGE sql STRICT SECURITY DEFINER
               AS 'SELECT value';
             CREATE FUNCTION strict_pair(first_value INT, second_value INT) RETURNS INT
               LANGUAGE sql STRICT SECURITY DEFINER AS 'SELECT first_value + second_value';
             ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON tasks FOR SELECT TO app USING ({policy});
             GRANT SELECT ON projects, tasks TO app;
             CREATE FUNCTION read_tasks() RETURNS SETOF INT
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT project.id FROM public.projects project
               LEFT JOIN public.tasks task ON task.project_id = project.id
               WHERE {filter}
             $$;"
        ));
        assert_eq!(function_read(&database, None, "read_tasks"), None, "{filter}");
    }
}

/// Null-accepting outer-join filters cannot certify hidden matches.
#[test]
fn nonstrict_outer_join_filters_leave_hidden_matches() {
    for filter in [
        "task.id IS NULL",
        "NOT (task.id = ANY (ARRAY[]::INT[])) AND task.project_id IS NULL",
        "public.nullable_id(task.id) = 1",
        "COALESCE(task.id, 1) = 1",
        "(ARRAY[1] || task.id) = ARRAY[1, NULL]",
        "public.strict_id(1) = 1 AND task.id IS NULL",
        "current_setting('app.mode') = 'on' AND task.id IS NULL",
    ] {
        let policy = filter.replace("task.", "");
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE projects (id INT PRIMARY KEY);
             CREATE TABLE tasks (id INT PRIMARY KEY, project_id INT);
             CREATE FUNCTION nullable_id(value INT) RETURNS INT LANGUAGE sql
               AS 'SELECT COALESCE(value, 1)';
             CREATE FUNCTION strict_id(value INT) RETURNS INT LANGUAGE sql STRICT
               AS 'SELECT value';
             ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON tasks FOR SELECT TO app USING ({policy});
             GRANT SELECT ON projects, tasks TO app;
             CREATE FUNCTION read_tasks() RETURNS SETOF INT
             LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT project.id FROM public.projects project
               LEFT JOIN public.tasks task ON task.project_id = project.id
               WHERE {filter}
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_tasks"),
            Some(("tasks".to_owned(), "visible".to_owned())),
            "{filter}"
        );
    }
}

/// Every enum argument value must yield a subset of a definer helper's set.
#[test]
fn enum_arguments_are_exhaustive_in_definer_set_containment() {
    for (parameter_type, helper_strict, reader_strict, confined) in [
        ("access_level", "STRICT", "", true),
        ("access_level", "", "STRICT", true),
        ("TEXT", "STRICT", "", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TYPE access_level AS ENUM ('reader', 'writer');
             CREATE TABLE grants (id INT, level {parameter_type});
             GRANT SELECT ON grants TO app;
             CREATE FUNCTION allowed(requested {parameter_type}) RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER {helper_strict} SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants WHERE level >= requested
               $$;
             CREATE TABLE notes (id INT PRIMARY KEY, body TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app
               USING (id IN (SELECT public.allowed('reader')));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes(requested {parameter_type}) RETURNS SETOF TEXT
               LANGUAGE sql SECURITY DEFINER {reader_strict} SET search_path = pg_catalog AS $$
                 SELECT body FROM public.notes
                 WHERE id IN (SELECT public.allowed(requested))
               $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "visible".to_owned())),
            "{parameter_type} / {helper_strict} / {reader_strict}"
        );
    }
}

/// An enum containing a weaker level cannot stand for a reader-or-higher
/// filter.
#[test]
fn a_weaker_enum_argument_leaves_hidden_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TYPE access_level AS ENUM ('guest', 'reader', 'writer');
         CREATE TABLE grants (id INT, level access_level);
         GRANT SELECT ON grants TO app;
         CREATE FUNCTION allowed(requested access_level) RETURNS SETOF INT
           LANGUAGE sql STABLE STRICT SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.grants WHERE level >= requested
           $$;
         CREATE TABLE notes (id INT PRIMARY KEY, body TEXT);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app
           USING (id IN (SELECT public.allowed('reader')));
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes(requested access_level) RETURNS SETOF TEXT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT body FROM public.notes WHERE id IN (SELECT public.allowed(requested))
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "visible".to_owned()))
    );
}

/// A helper's row filter remains available alongside checked and generated
/// discriminators.
#[test]
fn helper_rows_preserve_filters_with_held_discriminators() {
    for declaration in [
        "kind TEXT NOT NULL CHECK (kind = 'note')",
        "kind TEXT NOT NULL CHECK (notes.kind = 'note')",
        "kind TEXT NOT NULL CHECK ('note' = (kind))",
        "kind TEXT NOT NULL CHECK (kind IN ('note', 'memo'))",
        "kind TEXT NOT NULL CHECK (kind = ('note'::TEXT))",
        "kind TEXT NOT NULL CHECK (kind = ('note'::VARCHAR))",
        "kind TEXT NOT NULL CHECK (kind = ('note'::CHARACTER VARYING))",
        "kind TEXT NOT NULL CHECK (kind = 'note'::VARCHAR(4))",
        "kind TEXT NOT NULL CHECK (kind = $label$note$label$)",
        "kind TEXT NOT NULL CHECK (kind = E'note')",
        "kind TEXT NOT NULL CHECK (kind = upper('note'))",
        "kind TEXT NOT NULL CHECK (length(kind) > 0)",
        "kind TEXT CHECK (kind = 'note')",
        "kind entry_kind NOT NULL CHECK (kind = ('note'::entry_kind))",
        "kind entry_kind GENERATED ALWAYS AS ('note'::entry_kind) STORED",
        "kind TEXT GENERATED ALWAYS AS (('note'::TEXT)) STORED",
        "kind INT GENERATED ALWAYS AS (1) STORED",
        "kind INT[] GENERATED ALWAYS AS (ARRAY[1]) STORED",
        "kind NUMERIC GENERATED ALWAYS AS (1.0) STORED",
        "kind CHAR(4) GENERATED ALWAYS AS ('note') STORED",
        "kind TEXT NOT NULL",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TYPE entry_kind AS ENUM ('note', 'memo');
             CREATE TABLE notes (id INT PRIMARY KEY, active BOOLEAN, {declaration});
             ALTER TABLE notes ADD CONSTRAINT pending_kind CHECK (kind IS NOT NULL) NOT VALID;
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION visible_ids() RETURNS SETOF INT
               LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE active
               $$;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE id IN (SELECT public.visible_ids())
               $$;"
        ));
        assert_eq!(function_read(&database, None, "read_notes"), None, "{declaration}");
    }
}

/// Set membership can carry a helper filter through equivalent query
/// projections.
#[test]
fn membership_projection_spellings_preserve_helper_filters() {
    for membership in [
        "id IN (SELECT public.visible_ids())",
        "id IN (SELECT * FROM public.visible_ids())",
        "id = ANY (ARRAY(SELECT public.visible_ids()))",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE notes (id INT PRIMARY KEY, active BOOLEAN);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION visible_ids() RETURNS SETOF INT
               LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE active
               $$;
             CREATE FUNCTION visible_mixed() RETURNS TABLE (\"Mixed\" INT)
               LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE active
               $$;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE {membership}
               $$;"
        ));
        assert_eq!(function_read(&database, None, "read_notes"), None, "{membership}");
    }
}

/// Role-dependent filters cannot identify the invoking role inside a definer.
#[test]
fn role_identity_filters_do_not_confine_definer_reads() {
    for role in ["current_user", "current_role", "user"] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE notes (id INT PRIMARY KEY, owner_name TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING (owner_name = {role});
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE owner_name = {role}
               $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "visible".to_owned())),
            "{role}"
        );
    }
}

/// Membership in an unrelated set cannot imply a row's visibility flag.
#[test]
fn unrelated_membership_shapes_leave_hidden_rows() {
    for membership in [
        "id IN (SELECT chosen.id FROM public.all_ids() chosen)",
        "id IN (SELECT all_ids.id FROM public.all_ids())",
        "id IN (SELECT renamed.id FROM public.all_ids() renamed(id))",
        "id IN (SELECT chosen.id FROM public.all_ids(1) chosen)",
        "id IN (SELECT chosen.id FROM public.all_ids(id) chosen)",
        "id IN (SELECT chosen.id FROM public.all_ids() chosen WHERE chosen.id > 0)",
        "id IN (SELECT chosen.id FROM public.all_ids() chosen LIMIT 1)",
        "id IN (SELECT chosen.id FROM public.all_ids() WITH ORDINALITY chosen)",
        "id IN (SELECT first_set.id FROM public.all_ids() first_set, public.all_ids() second_set)",
        "id IN (SELECT first_set.id FROM public.all_ids() first_set JOIN public.all_ids() second_set ON first_set.id = second_set.id)",
        "id IN (SELECT chosen.id FROM LATERAL public.all_ids() chosen)",
        "id + 1 IN (SELECT chosen.id FROM public.all_ids() chosen)",
        "(id + 1, kind) IN (SELECT id, kind FROM public.grants)",
        "id = ANY (ARRAY[1, 2])",
        "id = ANY (ARRAY[id])",
        "id = (SELECT 1 WHERE true)",
        "id IN (SELECT public.grants.id FROM public.grants)",
        "id IN (SELECT grants.id FROM public.grants)",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE grants (id INT, kind TEXT);
             GRANT SELECT ON grants TO app;
             CREATE FUNCTION all_ids() RETURNS TABLE (id INT)
               LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants
               $$;
             CREATE FUNCTION all_ids(requested INT) RETURNS TABLE (id INT)
               LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants WHERE id = requested
               $$;
             CREATE TABLE notes (id INT PRIMARY KEY, kind TEXT, active BOOLEAN);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE {membership}
               $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "visible".to_owned())),
            "{membership}"
        );
    }
}

/// A strict operator's implementation rejects null-extended rows.
#[test]
fn declared_strict_operator_confines_outer_join_matches() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE projects (id INT PRIMARY KEY);
         CREATE TABLE tasks (id INT PRIMARY KEY, project_id INT);
         CREATE FUNCTION equal_ids(left_id INT, right_id INT) RETURNS BOOLEAN
           LANGUAGE sql STRICT AS 'SELECT left_id = right_id';
         CREATE OPERATOR public.<<< (
           FUNCTION = public.equal_ids, LEFTARG = INT, RIGHTARG = INT
         );
         ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON tasks FOR SELECT TO app
           USING (id OPERATOR(public.<<<) 1);
         GRANT SELECT ON projects, tasks TO app;
         CREATE FUNCTION read_tasks() RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT project.id FROM public.projects project
             LEFT JOIN public.tasks task ON task.project_id = project.id
             WHERE task.id OPERATOR(public.<<<) 1
           $$;",
    );
    assert_eq!(function_read(&database, None, "read_tasks"), None);
}

/// A non-SQL helper cannot supply a row-visibility proof.
#[test]
fn procedural_helper_membership_leaves_hidden_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE notes (id INT PRIMARY KEY, active BOOLEAN);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION all_ids() RETURNS SETOF INT
           LANGUAGE plpgsql STABLE SET search_path = pg_catalog AS $$
           BEGIN RETURN QUERY SELECT id FROM public.notes; END
           $$;
         CREATE FUNCTION read_notes() RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE id IN (SELECT public.all_ids())
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "visible".to_owned()))
    );
}

/// A nullable enum argument cannot narrow a helper that admits extra rows on
/// null input.
#[test]
fn nullable_enum_arguments_cannot_use_nonstrict_helpers() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TYPE access_level AS ENUM ('reader', 'writer');
         CREATE TABLE grants (id INT, level access_level);
         GRANT SELECT ON grants TO app;
         CREATE FUNCTION allowed(requested access_level) RETURNS SETOF INT
           LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
             SELECT id FROM public.grants WHERE level >= requested OR requested IS NULL
           $$;
         CREATE TABLE notes (id INT PRIMARY KEY);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app
           USING (id IN (SELECT public.allowed('reader')));
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes(requested access_level) RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE id IN (SELECT public.allowed(requested))
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "visible".to_owned()))
    );
}

/// Typed date arguments preserve the filters of a helper's returned rows.
#[test]
fn typed_literal_arguments_preserve_helper_filters() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE notes (id INT PRIMARY KEY, active BOOLEAN, created DATE);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION visible_ids(requested DATE) RETURNS SETOF INT
           LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE active AND created = requested
           $$;
         CREATE FUNCTION read_notes() RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes
             WHERE id IN (SELECT public.visible_ids(DATE '2020-01-01'))
           $$;",
    );
    assert_eq!(function_read(&database, None, "read_notes"), None);
}

/// A returned scalar cannot certify a table's independent visibility flag.
#[test]
fn scalar_return_helpers_leave_hidden_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE notes (id INT PRIMARY KEY, active BOOLEAN);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION chosen_id() RETURNS INT LANGUAGE sql STABLE RETURN 1;
         CREATE FUNCTION read_notes() RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE id = public.chosen_id()
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "visible".to_owned()))
    );
}

/// An unparseable definer body is reported as unfollowed.
#[test]
fn unreadable_definer_bodies_are_reported() {
    let database = schema(
        "CREATE ROLE app;
         CREATE FUNCTION read_notes() RETURNS INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS 'SELECT FROM';",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A generated null discriminator cannot invent a visibility condition.
#[test]
fn null_generated_discriminators_preserve_helper_filters() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE notes (
           id INT PRIMARY KEY, active BOOLEAN, kind TEXT GENERATED ALWAYS AS (NULL) STORED
         );
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app USING (active);
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION visible_ids() RETURNS SETOF INT
           LANGUAGE sql STABLE SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE active
           $$;
         CREATE FUNCTION read_notes() RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE id IN (SELECT public.visible_ids())
           $$;",
    );
    assert_eq!(function_read(&database, None, "read_notes"), None);
}

/// Tuple membership carries equalities beside a call to a definer set.
#[test]
fn tuple_membership_in_definer_sets_uses_pinned_values() {
    for (policy, filter) in [
        (
            "(id, kind) IN (SELECT * FROM public.allowed())",
            "kind = 'note' AND (id, 'note') IN (SELECT * FROM public.allowed())",
        ),
        (
            "(id, 'note') IN (SELECT * FROM public.allowed())",
            "'note' = kind AND (id, kind) IN (SELECT * FROM public.allowed())",
        ),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE grants (id INT, kind TEXT);
             GRANT SELECT ON grants TO app;
             CREATE FUNCTION allowed() RETURNS TABLE (id INT, kind TEXT)
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id, kind FROM public.grants
               $$;
             CREATE TABLE notes (id INT PRIMARY KEY, kind TEXT);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING ({policy});
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE {filter}
               $$;"
        ));
        assert_eq!(function_read(&database, None, "read_notes"), None, "{policy} / {filter}");
    }
}

/// Membership policies do not admit rows without a matching membership fact.
#[test]
fn projected_set_policies_reject_unrestricted_reads() {
    for policy in [
        "id IN (SELECT chosen.id FROM public.all_ids() chosen)",
        "id IN (SELECT chosen.\"Mixed\" FROM public.mixed_ids() chosen)",
        "id IN (SELECT all_ids.id FROM public.all_ids())",
        "id IN (SELECT renamed.id FROM public.all_ids() renamed(id))",
        "id IN (SELECT chosen.id FROM public.filtered_ids(notes.id) chosen)",
        "id IN (SELECT first_set.id FROM public.all_ids() first_set, public.all_ids() second_set)",
        "id IN (SELECT first_set.id FROM public.all_ids() first_set JOIN public.all_ids() second_set ON first_set.id = second_set.id)",
        "id IN (SELECT chosen.id FROM LATERAL public.all_ids() chosen)",
        "id IN (SELECT chosen.id FROM public.all_ids() WITH ORDINALITY chosen)",
        "CAST(id AS BIGINT) IN (SELECT * FROM public.all_ids())",
        "id + 1 IN (SELECT * FROM public.all_ids())",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE grants (id INT);
             GRANT SELECT ON grants TO app;
             CREATE FUNCTION all_ids() RETURNS TABLE (id INT)
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants
               $$;
             CREATE FUNCTION mixed_ids() RETURNS TABLE (\"Mixed\" INT)
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants
               $$;
             CREATE FUNCTION filtered_ids(requested INT) RETURNS TABLE (id INT)
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants WHERE id = requested
               $$;
             CREATE TABLE notes (id INT PRIMARY KEY);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app USING ({policy});
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE id = 0
               $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "visible".to_owned())),
            "{policy}"
        );
    }
}

/// A nullable enum parameter may select a value outside the policy's admitted
/// label.
#[test]
fn direct_enum_parameter_equality_leaves_hidden_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TYPE entry_kind AS ENUM ('note', 'secret');
         CREATE TABLE notes (id INT PRIMARY KEY, kind entry_kind);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app USING (kind = 'note'::entry_kind);
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes(requested entry_kind) RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE kind = requested
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "visible".to_owned()))
    );
}

/// An atomic set helper's policy requires a membership witness.
#[test]
fn atomic_set_policy_requires_membership() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE grants (id INT);
         GRANT SELECT ON grants TO app;
         CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
           SET search_path = pg_catalog
           BEGIN ATOMIC SELECT id FROM public.grants; END;
         CREATE TABLE notes (id INT PRIMARY KEY);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON notes FOR SELECT TO app
           USING (id IN (SELECT public.allowed()));
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes() RETURNS SETOF INT
           LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT id FROM public.notes WHERE id = 1
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "visible".to_owned()))
    );
}

/// A broad helper set cannot imply membership in a filtered set.
#[test]
fn qualified_helper_projections_leave_hidden_rows() {
    for projection in ["public.grants.id", "grants.id"] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE grants (id INT, active BOOLEAN);
             GRANT SELECT ON grants TO app;
             CREATE FUNCTION all_ids() RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT {projection} FROM public.grants
               $$;
             CREATE FUNCTION visible_ids() RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.grants WHERE active
               $$;
             CREATE TABLE notes (id INT PRIMARY KEY);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY visible ON notes FOR SELECT TO app
               USING (id IN (SELECT public.visible_ids()));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS $$
                 SELECT id FROM public.notes WHERE id IN (SELECT public.all_ids())
               $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "visible".to_owned())),
            "{projection}"
        );
    }
}

/// A polymorphic call without a unique overload cannot certify an outer join.
#[test]
fn unresolved_polymorphic_filters_leave_hidden_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE projects (id INT PRIMARY KEY);
         CREATE TABLE tasks (id INT PRIMARY KEY, project_id INT);
         CREATE FUNCTION chosen(requested INT, row_id INT) RETURNS INT
           LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         CREATE FUNCTION chosen(requested TEXT, row_id INT) RETURNS INT
           LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
         CREATE POLICY visible ON tasks FOR SELECT TO app USING (id IS NULL);
         GRANT SELECT ON projects, tasks TO app;
         CREATE FUNCTION read_tasks(requested ANYELEMENT) RETURNS SETOF INT
           LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
           BEGIN
             RETURN QUERY SELECT project.id FROM public.projects project
             LEFT JOIN public.tasks task ON task.project_id = project.id
             WHERE public.chosen(requested, task.id) = 1;
           END
           $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_tasks"),
        Some(("tasks".to_owned(), "visible".to_owned()))
    );
}
