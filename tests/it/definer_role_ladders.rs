//! Which role ladders and keys let a definer read inherit a write's
//! confinement.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;

use crate::support::definer_reads::{deployment, described, function_read, schema, trigger_read};

/// Builds a reader whose policy and filter call the same enum ladder.
fn ladder_schema(body: &str, narrower: &str, wider: &str, extra: &str) -> ParserDB {
    schema(&format!(
        "CREATE ROLE app;
         CREATE SCHEMA authz;
         CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
         CREATE TABLE memberships (id INT NOT NULL, role authz.role NOT NULL);
         GRANT SELECT ON memberships TO app;
         {extra}
         CREATE FUNCTION authz.ids_with(min_role authz.role) RETURNS SETOF INT
           LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
             {body} $$;
         CREATE TABLE notes (id INT PRIMARY KEY, body TEXT);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY notes_select ON notes FOR SELECT TO app
           USING (id = ANY (ARRAY(SELECT authz.ids_with({wider}))));
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes() RETURNS SETOF TEXT LANGUAGE sql STABLE
           SECURITY DEFINER SET search_path = pg_catalog AS $$
             SELECT body FROM public.notes
             WHERE id = ANY (ARRAY(SELECT authz.ids_with({narrower}))) $$;"
    ))
}

/// Enum bounds remain descending through positive relational operations.
#[test]
fn positive_relational_operations_preserve_role_descent() {
    for body in [
        "SELECT id FROM public.memberships WHERE role >= $1",
        "SELECT id FROM public.memberships WHERE (min_role) <= role",
        "SELECT id FROM public.memberships WHERE min_role < role",
        "SELECT id FROM public.memberships WHERE (role >= (min_role))",
        "SELECT id FROM public.memberships WHERE role > min_role OR role >= min_role",
        "WITH allowed AS (SELECT id FROM public.memberships WHERE role >= min_role)
         SELECT id FROM allowed",
        "WITH allowed AS (SELECT id FROM public.memberships WHERE role >= min_role),
              copied AS (SELECT id FROM allowed)
         SELECT id FROM copied",
        "WITH unused AS (SELECT id FROM public.memberships WHERE role >= min_role)
         SELECT id FROM public.memberships",
        "SELECT id FROM public.memberships WHERE role >= min_role
         UNION SELECT id FROM public.memberships",
        "SELECT id FROM public.memberships WHERE role >= min_role
         INTERSECT SELECT id FROM public.memberships WHERE role > min_role",
        "SELECT id FROM public.memberships WHERE role >= min_role
         EXCEPT SELECT id FROM public.memberships WHERE id = 0",
        "(SELECT id FROM public.memberships WHERE role >= min_role)",
        "SELECT selected.id FROM
           (SELECT id FROM public.memberships WHERE role >= min_role) selected",
        "SELECT selected.* FROM
           (SELECT id FROM public.memberships WHERE role >= min_role) selected",
        "SELECT * FROM
           (SELECT id FROM public.memberships WHERE role >= min_role) selected",
        "SELECT selected.id FROM public.memberships selected
           JOIN public.memberships other ON other.role >= min_role
           WHERE selected.id = other.id",
        "SELECT selected.id FROM public.memberships selected
           CROSS JOIN (SELECT id FROM public.memberships WHERE role >= min_role) other
           WHERE selected.id = other.id",
        "SELECT id FROM public.memberships WHERE id IN
           (SELECT id FROM public.memberships WHERE role >= min_role)",
        "SELECT id FROM public.memberships WHERE EXISTS
           (SELECT id FROM public.memberships WHERE role >= min_role)",
        "SELECT id FROM public.memberships WHERE id = ANY
           (ARRAY(SELECT id FROM public.memberships WHERE role >= min_role))",
    ] {
        let database = ladder_schema(body, "'writer'", "'reader'", "");
        assert_eq!(function_read(&database, None, "read_notes"), None, "{body}");
    }
}

/// Operations that can replace rows cannot establish enum set containment.
#[test]
fn non_descending_relational_operations_do_not_authorize_reads() {
    for body in [
        "SELECT id FROM public.memberships WHERE role = min_role",
        "SELECT id FROM public.memberships WHERE role < min_role",
        "SELECT id FROM public.memberships WHERE role >= min_role LIMIT 1",
        "SELECT id FROM public.memberships WHERE role >= min_role FETCH FIRST 1 ROW ONLY",
        "SELECT DISTINCT ON (role) id FROM public.memberships WHERE role >= min_role",
        "SELECT count(*) FROM public.memberships WHERE role >= min_role",
        "SELECT CASE WHEN min_role = 'reader' THEN id ELSE id + 1 END
         FROM public.memberships WHERE role >= min_role",
        "SELECT id FROM public.memberships
         EXCEPT SELECT id FROM public.memberships WHERE role >= min_role",
        "WITH allowed AS (SELECT id FROM public.memberships WHERE role = min_role)
         SELECT id FROM allowed",
        "SELECT selected.id FROM public.memberships selected
         LEFT JOIN public.memberships other ON other.role >= min_role
         WHERE other.id IS NULL",
        "SELECT selected.id FROM public.memberships selected
         JOIN public.memberships other ON other.role = min_role",
        "SELECT id FROM public.memberships WHERE role >= min_role
         AND role = ANY (ARRAY[min_role])",
        "SELECT id FROM public.memberships
         WHERE role >= CASE WHEN min_role = 'writer' THEN 'reader'::authz.role
                            ELSE 'admin'::authz.role END",
        "SELECT id FROM public.memberships WHERE min_role >= 'writer'::authz.role",
    ] {
        let database = ladder_schema(body, "'writer'", "'reader'", "");
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "notes_select".to_owned())),
            "{body}"
        );
    }
}

/// Literal enum labels must move in the direction that narrows the set.
#[test]
fn ladder_arguments_respect_enum_order_and_literal_identity() {
    for (narrower, wider, confined) in [
        ("'writer'", "'reader'", true),
        ("'reader'", "'writer'", false),
        ("'missing'", "'reader'", false),
        ("1", "'reader'", false),
        ("current_setting('app.role')::authz.role", "'reader'", false),
        ("'writer'::authz.role", "'writer'", true),
    ] {
        let database = ladder_schema(
            "SELECT id FROM public.memberships WHERE role >= min_role",
            narrower,
            wider,
            "",
        );
        assert_eq!(
            function_read(&database, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{narrower}, {wider}"
        );
    }
}

/// Passing an unchanged enum parameter preserves a nested ladder's descent.
#[test]
fn nested_ladders_preserve_parameter_positions() {
    let helper = "
        CREATE FUNCTION authz.inner_ids(ignored INT, min_role authz.role) RETURNS SETOF INT
          LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
          SELECT id FROM public.memberships WHERE role >= min_role $$;";
    for body in [
        "SELECT authz.inner_ids(1, min_role)",
        "SELECT authz.inner_ids(1, ((min_role)))",
        "SELECT * FROM authz.inner_ids(1, min_role)",
        "SELECT selected.* FROM authz.inner_ids(1, min_role) selected",
        "SELECT authz.inner_ids(1, min_role) UNION SELECT authz.inner_ids(2, min_role)",
        "SELECT selected.id FROM
         (public.memberships selected JOIN public.memberships other
          ON other.role >= min_role) WHERE selected.id = other.id",
    ] {
        let database = ladder_schema(body, "'writer'", "'reader'", helper);
        assert_eq!(function_read(&database, None, "read_notes"), None, "{body}");
    }
}

/// Altered parameters cannot establish nested set containment.
#[test]
fn nested_ladders_reject_parameter_transformations() {
    let helper = "
        CREATE FUNCTION authz.inner_ids(ignored INT, min_role authz.role) RETURNS SETOF INT
          LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
          SELECT id FROM public.memberships WHERE role >= min_role $$;";
    for body in [
        "SELECT authz.inner_ids(1, CASE WHEN min_role = 'writer' THEN 'reader'::authz.role
                                     ELSE 'admin'::authz.role END)",
        "SELECT random() FROM public.memberships WHERE role >= min_role",
        "SELECT unknown_ladder(min_role)",
        "SELECT * FROM unknown_ladder(min_role)",
        "SELECT * FROM unnest(ARRAY[min_role])",
        "SELECT authz.ids_with(min_role)",
    ] {
        let database = ladder_schema(body, "'writer'", "'reader'", helper);
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "notes_select".to_owned())),
            "{body}"
        );
    }
}

/// Only stable set-returning SQL bodies establish enum descent.
#[test]
fn ladder_function_contracts_are_required() {
    for (declaration, confined) in [
        ("RETURNS SETOF INT LANGUAGE sql STABLE", true),
        ("RETURNS SETOF INT LANGUAGE sql VOLATILE", false),
        ("RETURNS INT LANGUAGE sql STABLE", false),
        ("RETURNS SETOF INT LANGUAGE sql STABLE SET enable_seqscan = off", false),
        ("RETURNS SETOF INT LANGUAGE plpgsql STABLE", false),
    ] {
        let body = if declaration.contains("plpgsql") {
            "BEGIN RETURN QUERY SELECT id FROM public.memberships WHERE role >= min_role; END"
        } else {
            "SELECT id FROM public.memberships WHERE role >= min_role"
        };
        let sql = format!(
            "CREATE ROLE app;
             CREATE SCHEMA authz;
             CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
             CREATE TABLE memberships (id INT NOT NULL, role authz.role NOT NULL);
             GRANT SELECT ON memberships TO app;
             CREATE FUNCTION authz.ids_with(min_role authz.role) {declaration}
               SECURITY DEFINER SET search_path = pg_catalog AS $$ {body} $$;
             CREATE TABLE notes (id INT PRIMARY KEY);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_select ON notes FOR SELECT TO app
               USING (id = ANY (ARRAY(SELECT authz.ids_with('reader'))));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT id FROM public.notes
               WHERE id = ANY (ARRAY(SELECT authz.ids_with('writer'))) $$;"
        );
        assert_eq!(
            function_read(&schema(&sql), None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{declaration}"
        );
    }
}

/// Equal leading arguments and output parameters preserve the enum input
/// position.
#[test]
fn argument_modes_preserve_ladder_input_positions() {
    for (parameters, narrower, wider, confined) in [
        ("tenant INT, min_role authz.role", "1, 'writer'", "1, 'reader'", true),
        ("tenant INT, min_role authz.role, OUT result INT", "1, 'writer'", "1, 'reader'", true),
        ("tenant INT, min_role authz.role", "2, 'writer'", "1, 'reader'", false),
        ("min_role authz.role, VARIADIC tenants INT[]", "'writer', 1", "'reader', 1", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE SCHEMA authz;
             CREATE TYPE authz.role AS ENUM ('reader', 'writer', 'admin');
             CREATE TABLE memberships (id INT NOT NULL, role authz.role NOT NULL);
             GRANT SELECT ON memberships TO app;
             CREATE FUNCTION authz.ids_with({parameters}) RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT id FROM public.memberships WHERE role >= min_role $$;
             CREATE TABLE notes (id INT PRIMARY KEY);
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_select ON notes FOR SELECT TO app
               USING (id = ANY (ARRAY(SELECT authz.ids_with({wider}))));
             GRANT SELECT ON notes TO app;
             CREATE FUNCTION read_notes() RETURNS SETOF INT LANGUAGE sql STABLE
               SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT id FROM public.notes
               WHERE id = ANY (ARRAY(SELECT authz.ids_with({narrower}))) $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{parameters}, {narrower}, {wider}"
        );
    }
}

/// Only unconditional plain unique indexes identify the row admitted by a
/// composite foreign key.
#[test]
fn only_plain_unconditional_unique_indexes_pin_read_rows() {
    for (index, confined) in [
        ("CREATE UNIQUE INDEX run_id ON runs (id);", true),
        ("CREATE UNIQUE INDEX run_id ON runs (id) WHERE team_id = 1;", false),
        ("CREATE UNIQUE INDEX run_id ON runs ((id + 1));", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE runs (id INT NOT NULL, team_id INT NOT NULL, UNIQUE (id, team_id));
             {index}
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             CREATE TABLE bindings (
               run_id INT NOT NULL, team_id INT NOT NULL,
               FOREIGN KEY (run_id, team_id) REFERENCES runs (id, team_id));
             ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
             CREATE POLICY bindings_write ON bindings FOR ALL TO app
               USING (team_id = current_setting('app.team_id')::INT)
               WITH CHECK (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON runs TO app;
             GRANT SELECT, INSERT ON bindings TO app;
             CREATE FUNCTION guard_binding() RETURNS trigger
               LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
               BEGIN
                 IF EXISTS (SELECT 1 FROM public.runs WHERE runs.id = NEW.run_id) THEN
                   RAISE EXCEPTION 'found';
                 END IF;
                 RETURN NEW;
               END $$;
             CREATE TRIGGER binding_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();"
        ));
        assert_eq!(
            trigger_read(&database, "binding_guard", DefinerReading::Outcome),
            (!confined).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{index}"
        );
    }
}

/// Nullable companion columns require `MATCH FULL`, or a check setting them
/// on every row, to bind every nonnull foreign key.
#[test]
fn nullable_foreign_key_companions_require_full_matching() {
    for (matching, enforcement, check, confined) in [
        ("MATCH SIMPLE", "", "", false),
        ("MATCH SIMPLE", "", "CHECK (partition_id IS NOT NULL OR team_id IS NULL),", true),
        ("MATCH SIMPLE", "", "CHECK (partition_id IS NOT NULL OR team_id > 0),", false),
        ("MATCH FULL", "", "", true),
        ("MATCH FULL", "NOT ENFORCED", "", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE runs (
               id INT PRIMARY KEY, team_id INT NOT NULL, partition_id INT NOT NULL,
               UNIQUE (id, team_id, partition_id));
             ALTER TABLE runs ENABLE ROW LEVEL SECURITY;
             CREATE POLICY runs_select ON runs FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             CREATE TABLE bindings (
               run_id INT NOT NULL, team_id INT NOT NULL, partition_id INT, {check}
               FOREIGN KEY (run_id, team_id, partition_id)
                 REFERENCES runs (id, team_id, partition_id) {matching} {enforcement});
             ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
             CREATE POLICY bindings_write ON bindings FOR ALL TO app
               USING (team_id = current_setting('app.team_id')::INT)
               WITH CHECK (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON runs TO app;
             GRANT SELECT, INSERT ON bindings TO app;
             CREATE FUNCTION guard_binding() RETURNS trigger
               LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog AS $$
               BEGIN
                 IF EXISTS (SELECT 1 FROM public.runs WHERE runs.id = NEW.run_id) THEN
                   RAISE EXCEPTION 'found';
                 END IF;
                 RETURN NEW;
               END $$;
             CREATE TRIGGER binding_guard AFTER INSERT ON bindings
               FOR EACH ROW EXECUTE FUNCTION guard_binding();"
        ));
        assert_eq!(
            trigger_read(&database, "binding_guard", DefinerReading::Outcome),
            (!confined).then(|| ("runs".to_owned(), "runs_select".to_owned())),
            "{matching}, {enforcement}, {check}"
        );
    }
}

/// A declared set containment authorizes an opaque helper without inventing
/// enum descent.
#[test]
fn declared_containment_authorizes_an_opaque_role_set() {
    let database = ladder_schema(
        "SELECT id FROM public.memberships WHERE role = min_role",
        "'writer'",
        "'reader'",
        "",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
    let deployment = deployment()
        .with_set_within("authz.ids_with('writer')", "authz.ids_with('reader')")
        .expect("valid set expressions");
    let role = ActingRole::Declared(database.role("app").expect("the role exists"));
    let function = database
        .function_by_target(TargetName::new("read_notes", false), IdentifierCase::AsWritten)
        .expect("the name resolves")
        .expect("the function exists");
    assert_eq!(
        function
            .unconfined_definer_read(&database, &deployment, &[role])
            .expect("the schema reads")
            .map(|read| described(&read)),
        None
    );
}

/// An unresolved enum body cannot justify the visibility of a protected table.
#[test]
fn ambiguous_body_columns_do_not_establish_role_descent() {
    let database = ladder_schema(
        "SELECT selected.id FROM public.memberships selected
         JOIN public.memberships other ON selected.id = other.id
         WHERE role >= min_role",
        "'writer'",
        "'reader'",
        "",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// A parameter-dependent `VALUES` body must not authorize a different row.
#[test]
fn parameter_dependent_values_do_not_establish_role_descent() {
    let database = ladder_schema(
        "VALUES (CASE WHEN min_role = 'reader' THEN 1 ELSE 2 END)",
        "'writer'",
        "'reader'",
        "",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// Positional parameter reads in `VALUES` must not establish enum descent.
#[test]
fn positional_parameter_values_do_not_establish_role_descent() {
    let database = ladder_schema(
        "VALUES (CASE WHEN $1 = 'reader' THEN 1 ELSE 2 END)",
        "'writer'",
        "'reader'",
        "",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// Named calls to disjoint role sets cannot authorize another role's rows.
#[test]
fn named_arguments_do_not_confine_disjoint_role_sets() {
    let helper = "
        CREATE FUNCTION authz.inner_ids(ignored INT, min_role authz.role) RETURNS SETOF INT
          LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
          SELECT id FROM public.memberships WHERE role = min_role $$;";
    for body in [
        "SELECT authz.inner_ids(ignored => 1, min_role => min_role)",
        "SELECT * FROM authz.inner_ids(ignored => 1, min_role => min_role)",
    ] {
        let database = ladder_schema(body, "'writer'", "'reader'", helper);
        assert_eq!(
            function_read(&database, None, "read_notes"),
            Some(("notes".to_owned(), "notes_select".to_owned())),
            "{body}"
        );
    }
    let database = ladder_schema(
        "SELECT id FROM public.memberships WHERE role = min_role",
        "min_role => 'writer'",
        "'reader'",
        "",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// A surviving value can acquire another ordinal when preceding rows disappear.
#[test]
fn filtered_ordinals_do_not_preserve_role_sets() {
    let helper = "
        CREATE FUNCTION authz.inner_ids(ignored INT, min_role authz.role) RETURNS SETOF INT
          LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
          SELECT id FROM public.memberships WHERE role >= min_role ORDER BY id $$;";
    let database = ladder_schema(
        "SELECT selected.ordinal FROM authz.inner_ids(1, min_role)
         WITH ORDINALITY AS selected(id, ordinal) WHERE selected.id = 2",
        "'writer'",
        "'reader'",
        helper,
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// An independent common table expression does not change a descending role
/// filter.
#[test]
fn independent_common_tables_preserve_role_descent() {
    let database = ladder_schema(
        "WITH unused AS (SELECT 1)
         SELECT id FROM public.memberships WHERE role >= min_role",
        "'writer'",
        "'reader'",
        "",
    );
    assert_eq!(function_read(&database, None, "read_notes"), None);
}

/// A null comparison does not make a disjoint role equality descending.
#[test]
fn nullable_derived_bounds_do_not_authorize_disjoint_role_sets() {
    let database = ladder_schema(
        "SELECT member.id FROM public.memberships member
         CROSS JOIN (SELECT NULL AS unknown_role) selected
         WHERE selected.unknown_role >= min_role OR member.role = min_role",
        "'writer'",
        "'reader'",
        "",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// Disjoint built-in set calls do not authorize each other's result
/// identifiers.
#[test]
fn builtin_sets_do_not_establish_containment_without_a_body() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE notes (id INT PRIMARY KEY);
         ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
         CREATE POLICY notes_select ON notes FOR SELECT TO app
           USING (id = ANY (ARRAY(SELECT unnest(ARRAY[1]))));
         GRANT SELECT ON notes TO app;
         CREATE FUNCTION read_notes() RETURNS SETOF INT LANGUAGE sql STABLE
           SECURITY DEFINER SET search_path = pg_catalog AS $$
           SELECT id FROM public.notes
           WHERE id = ANY (ARRAY(SELECT unnest(ARRAY[2]))) $$;",
    );
    assert_eq!(
        function_read(&database, None, "read_notes"),
        Some(("notes".to_owned(), "notes_select".to_owned()))
    );
}

/// Only enforced foreign keys that check the selected row carry helper filters
/// to its parent.
#[test]
fn foreign_key_matching_controls_carried_helper_filters() {
    for (matching, enforcement, confined) in [
        ("MATCH SIMPLE", "", false),
        ("MATCH FULL", "", true),
        ("MATCH FULL", "NOT ENFORCED", false),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE notes (
               id INT PRIMARY KEY, team_id INT NOT NULL, partition_id INT NOT NULL,
               UNIQUE (id, team_id, partition_id));
             ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY notes_select ON notes FOR SELECT TO app
               USING (team_id = current_setting('app.team_id')::INT);
             GRANT SELECT ON notes TO app;
             CREATE TABLE bindings (
               note_id INT NOT NULL, team_id INT NOT NULL, partition_id INT,
               FOREIGN KEY (note_id, team_id, partition_id)
                 REFERENCES notes (id, team_id, partition_id) {matching} {enforcement});
             GRANT SELECT ON bindings TO app;
             CREATE FUNCTION allowed_ids() RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT note_id FROM public.bindings
               WHERE team_id = current_setting('app.team_id')::INT $$;
             CREATE FUNCTION read_notes() RETURNS SETOF INT
               LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog AS $$
               SELECT id FROM public.notes
               WHERE id IN (SELECT public.allowed_ids()) $$;"
        ));
        assert_eq!(
            function_read(&database, None, "read_notes"),
            (!confined).then(|| ("notes".to_owned(), "notes_select".to_owned())),
            "{matching}, {enforcement}"
        );
    }
}
