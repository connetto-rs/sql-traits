//! Which rows the sources, joins and nested queries of a definer read can
//! reach.

use sql_traits::prelude::*;

use crate::support::definer_reads::{function_read, schema, trigger_read};

/// Inner joins carry identical keys through merged and projected columns.
#[test]
fn merged_and_projected_keys_confine_inner_reads() {
    for query in [
        "SELECT secret.id FROM public.secret JOIN public.visible USING (id) WHERE id = 7",
        "SELECT secret.id FROM public.secret JOIN public.visible USING (id) JOIN public.third USING (id) WHERE id = 7",
        "SELECT secret.id FROM public.secret JOIN (SELECT visible.id FROM public.visible WHERE id = 7) allowed USING (id)",
        "SELECT secret.id FROM public.secret JOIN (SELECT visible.id AS key FROM public.visible WHERE id = 7) allowed(id) USING (id)",
        "SELECT secret.id FROM public.secret JOIN (SELECT 1, visible.id FROM public.visible WHERE id = 7) allowed(dummy, id) USING (id)",
        "WITH allowed(key) AS (SELECT visible.id FROM public.visible WHERE id = 7) SELECT secret.id FROM public.secret JOIN allowed AS renamed(id) USING (id)",
        "SELECT secret.id FROM public.secret JOIN (SELECT id FROM public.visible WHERE id = 7) allowed ON secret.id = allowed.id",
        "SELECT secret.id FROM public.secret WHERE secret.id IN (SELECT visible.id FROM public.visible WHERE id = 7)",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             CREATE TABLE third (id INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret, visible, third TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// Null-rejecting filters make outer joins confine their padded side.
#[test]
fn null_rejection_confines_outer_join_reads() {
    for query in [
        "SELECT secret.id FROM public.secret LEFT JOIN public.visible ON secret.id = visible.id WHERE visible.id = 7",
        "SELECT secret.id FROM public.visible RIGHT JOIN public.secret ON secret.id = visible.id WHERE visible.id = 7",
        "SELECT secret.id FROM public.visible LEFT JOIN public.secret ON secret.id = visible.id WHERE secret.id = 7",
        "SELECT secret.id FROM public.secret RIGHT JOIN public.visible ON secret.id = visible.id WHERE secret.id = 7",
        "SELECT secret.id FROM public.secret LEFT JOIN public.visible USING (id) JOIN public.third USING (id) WHERE third.id = 7",
        "SELECT secret.id FROM public.secret RIGHT JOIN public.visible USING (id) JOIN public.third USING (id) WHERE secret.id IS NOT NULL AND third.id = 7",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             CREATE TABLE third (id INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret, visible, third TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// Preserved outer rows cannot borrow a filter from an absent joined row.
#[test]
fn outer_join_padding_does_not_confine_preserved_rows() {
    for query in [
        "SELECT secret.id FROM public.secret LEFT JOIN public.visible ON secret.id = visible.id AND visible.id = 7",
        "SELECT secret.id FROM public.visible RIGHT JOIN public.secret ON secret.id = visible.id AND visible.id = 7",
        "SELECT secret.id FROM public.secret FULL JOIN public.visible ON secret.id = visible.id AND visible.id = 7",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             CREATE TABLE third (id INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret, visible, third TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(
            function_read(&database, None, "probe"),
            Some(("secret".to_owned(), "readable".to_owned())),
            "{query}"
        );
    }
}

/// Whole set-returning bodies admit the rows their own policy exposes.
#[test]
fn whole_set_lineage_follows_cte_names_and_union_arms() {
    for query in [
        "WITH allowed AS (SELECT id FROM public.secret) SELECT id FROM allowed",
        "WITH allowed AS (SELECT secret.id FROM public.secret) SELECT allowed.id FROM allowed",
        "WITH allowed AS (SELECT id AS key FROM public.secret) SELECT key FROM allowed",
        "WITH allowed(key) AS (SELECT id FROM public.secret) SELECT key FROM allowed",
        "WITH allowed(key) AS (SELECT id FROM public.secret) SELECT renamed.id FROM allowed renamed(id)",
        "WITH allowed AS (SELECT id FROM public.secret UNION ALL SELECT id FROM public.secret) SELECT id FROM allowed",
        "WITH allowed AS ((SELECT id FROM public.secret)) SELECT id FROM allowed",
        "WITH first AS (SELECT id FROM public.secret), second AS (SELECT id FROM first) SELECT id FROM second",
        "SELECT id FROM public.secret UNION SELECT id FROM public.secret",
        "(SELECT id FROM public.secret)",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;
             CREATE POLICY readable ON secret FOR SELECT TO app
             USING (id IN (SELECT public.probe()));
             GRANT SELECT ON secret TO app;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// A discarded row cannot use membership in the function result as permission.
#[test]
fn incomplete_set_lineage_does_not_authorize_source_rows() {
    for query in [
        "SELECT id FROM public.secret LIMIT 1",
        "SELECT id FROM public.secret OFFSET 1",
        "SELECT id FROM public.secret FETCH FIRST 1 ROW ONLY",
        "SELECT id FROM public.secret INTERSECT SELECT id FROM public.secret",
        "WITH allowed AS (SELECT id FROM public.secret LIMIT 1) SELECT id FROM allowed",
        "WITH allowed AS (SELECT id + 1 AS value FROM public.secret) SELECT value FROM allowed",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;
             CREATE POLICY readable ON secret FOR SELECT TO app
             USING (id IN (SELECT public.probe()));
             GRANT SELECT ON secret TO app;"
        ));
        assert_eq!(
            function_read(&database, None, "probe"),
            Some(("secret".to_owned(), "readable".to_owned())),
            "{query}"
        );
    }
}

/// Matching a policy helper's exhaustive source establishes membership.
#[test]
fn policy_membership_is_witnessed_by_matching_source_rows() {
    for query in [
        "SELECT secret.id FROM public.secret WHERE enabled = 1",
        "SELECT secret.id FROM public.secret JOIN public.permissions ON secret.id = permissions.id WHERE permissions.enabled = 1",
    ] {
        let same_table = !query.contains("permissions");
        let source = if same_table { "secret" } else { "permissions" };
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, enabled INT);
             CREATE TABLE permissions (id INT PRIMARY KEY, enabled INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE FUNCTION allowed(threshold INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.{source} WHERE enabled = threshold
             $$;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id IN (SELECT public.allowed(1)));
             GRANT SELECT ON secret, permissions TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// A helper row that fails its argument filter does not establish membership.
#[test]
fn policy_membership_requires_the_helpers_filter() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, enabled INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION allowed(threshold INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE enabled = threshold
         $$;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id IN (SELECT public.allowed(1)));
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.secret WHERE enabled = 2 $$;",
    );
    assert_eq!(
        function_read(&database, None, "probe"),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// Each union arm must independently confine its protected reads.
#[test]
fn nested_queries_and_union_arms_keep_their_own_filters() {
    for (query, confined) in [
        (
            "SELECT id FROM public.secret WHERE id = 7 UNION ALL SELECT id FROM public.secret WHERE id = 7",
            true,
        ),
        (
            "SELECT id FROM public.secret WHERE id = 7 UNION ALL SELECT id FROM public.secret WHERE id = 8",
            false,
        ),
        (
            "SELECT id FROM public.visible WHERE id = 7 AND EXISTS (SELECT 1 FROM public.secret WHERE secret.id = visible.id)",
            true,
        ),
        ("(SELECT id FROM public.secret WHERE id = 7) UNION ALL VALUES (7)", true),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret, visible TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        let expected = (!confined).then(|| ("secret".to_owned(), "readable".to_owned()));
        assert_eq!(function_read(&database, None, "probe"), expected, "{query}");
    }
}

/// Renamed protected tables and parenthesized protected joins are unfollowed.
#[test]
fn opaque_relation_shapes_report_unfollowed_reads() {
    for query in [
        "SELECT renamed.key FROM public.secret renamed(key)",
        "SELECT secret.id FROM (public.secret JOIN public.visible ON secret.id = visible.id)",
        "SELECT id FROM public.secret FOR UPDATE",
        "TABLE public.secret",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret, visible TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(
            function_read(&database, None, "probe"),
            Some((String::new(), "unfollowed".to_owned())),
            "{query}"
        );
    }
}

/// Parenthesized joins of unrestricted tables need no row policy.
#[test]
fn open_nested_joins_are_confined() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE visible (id INT);
         CREATE TABLE other (id INT);
         GRANT SELECT ON visible, other TO app;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT visible.id FROM (public.visible JOIN public.other ON visible.id = other.id)
         $$;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// Complete helper unions carry every surviving arm's conditions.
#[test]
fn helper_unions_carry_conditions_through_function_sources() {
    for body in [
        "SELECT id FROM public.secret WHERE enabled = 1 UNION ALL SELECT id FROM public.secret WHERE enabled = 1",
        "WITH selected AS (SELECT id FROM public.secret WHERE enabled = 1) SELECT id FROM selected",
        "SELECT wrapped.id FROM public.allowed(1) wrapped(id)",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, enabled INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (enabled = 1);
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION allowed(threshold INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.secret WHERE enabled = threshold
             $$;
             CREATE FUNCTION gathered() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {body} $$;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.secret WHERE id IN (SELECT public.gathered())
             $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{body}");
    }
}

/// A foreign key carries a helper's tenant constraint onto the referenced row.
#[test]
fn helper_membership_carries_composite_foreign_key_conditions() {
    for (helper_table, read_table, projected) in
        [("bindings", "secret", "run_id"), ("secret", "bindings", "id")]
    {
        let tested = if read_table == "secret" { "id" } else { "run_id" };
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, tenant INT NOT NULL, UNIQUE (id, tenant));
             CREATE TABLE bindings (run_id INT PRIMARY KEY, tenant INT NOT NULL,
               FOREIGN KEY (run_id, tenant) REFERENCES secret (id, tenant));
             ALTER TABLE {read_table} ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON {read_table} FOR SELECT TO app USING (tenant = 7);
             GRANT SELECT ON secret, bindings TO app;
             CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT {projected} FROM public.{helper_table} WHERE tenant = 7
             $$;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT {tested} FROM public.{read_table} WHERE {tested} IN (SELECT public.allowed())
             $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{read_table}");
    }
}

/// Disjoint discriminator values exclude an otherwise unconfined helper arm.
#[test]
fn helper_union_discriminators_exclude_unrelated_rows() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, kind TEXT NOT NULL, tenant INT NOT NULL);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE kind = 'wanted' AND tenant = 7
           UNION ALL SELECT id FROM public.secret WHERE kind = 'other'
         $$;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE kind = 'wanted' AND id IN (SELECT public.allowed())
         $$;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// Neither arbitrary arguments nor reassigned parameters confine a direct
/// helper read.
#[test]
fn procedural_parameters_do_not_confine_unrestricted_helper_calls() {
    for (declaration, assignment) in [
        ("", ""),
        ("", "threshold := 8;"),
        ("DECLARE local_threshold INT := threshold;", "threshold := local_threshold;"),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION helper(threshold INT) RETURNS INT LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               {declaration}
               BEGIN
                 {assignment}
                 RETURN (SELECT id FROM public.secret WHERE id = threshold);
               END
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "helper"),
            Some(("secret".to_owned(), "readable".to_owned())),
            "{assignment}"
        );
    }
}

/// A write's additional protected source is reported separately from its
/// target.
#[test]
fn write_sources_distinguish_open_and_protected_relations() {
    for (statement, expected) in [
        ("DELETE FROM public.audit USING public.visible WHERE audit.id = visible.id", None),
        (
            "UPDATE public.audit SET id = visible.id FROM public.visible WHERE audit.id = visible.id",
            None,
        ),
        ("UPDATE public.audit SET id = (SELECT id FROM public.secret WHERE id = 7)", None),
        (
            "DELETE FROM public.audit USING public.secret WHERE audit.id = secret.id",
            Some(("secret".to_owned(), "unfollowed".to_owned())),
        ),
        (
            "UPDATE public.audit SET id = secret.id FROM public.secret WHERE audit.id = secret.id",
            Some(("secret".to_owned(), "unfollowed".to_owned())),
        ),
        (
            "UPDATE public.audit SET id = exposed.id FROM public.exposed WHERE audit.id = exposed.id",
            Some((String::new(), "unfollowed".to_owned())),
        ),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE audit (id INT);
             CREATE TABLE visible (id INT);
             CREATE TABLE secret (id INT);
             CREATE VIEW exposed AS SELECT id FROM secret;
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT, UPDATE, DELETE ON audit TO app;
             GRANT SELECT ON secret, visible, exposed TO app;
             CREATE FUNCTION probe() RETURNS VOID LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {statement} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), expected, "{statement}");
    }
}

/// An explicit protected-side filter confines joins with opaque output columns.
#[test]
fn explicit_filters_confine_opaque_using_sources() {
    for query in [
        "SELECT secret.id FROM public.secret JOIN (SELECT * FROM public.visible) allowed USING (id) WHERE secret.id = 7",
        "SELECT secret.id FROM public.secret JOIN (SELECT 7 AS id) allowed USING (id) WHERE secret.id = 7",
        "SELECT secret.id FROM public.secret JOIN (SELECT id FROM public.visible UNION ALL SELECT id FROM public.visible) allowed USING (id) WHERE secret.id = 7",
        "WITH allowed AS (SELECT id FROM public.visible) SELECT secret.id FROM public.secret JOIN allowed first USING (id) JOIN allowed second USING (id) WHERE secret.id = 7",
        "SELECT secret.id FROM public.secret JOIN public.visible USING (id) JOIN public.wide USING (id) WHERE secret.id = 7",
        "SELECT secret.id FROM public.secret JOIN (SELECT visible.*, 7 AS extra FROM public.visible) allowed USING (id) WHERE secret.id = 7",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             CREATE TABLE third (id INT);
             CREATE TABLE wide (id BIGINT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
             GRANT SELECT ON secret, visible, third, wide TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// Parameterized helper wrappers preserve literal and named argument
/// constraints.
#[test]
fn parameterized_function_sources_carry_constraints() {
    for expression in ["threshold", "$1", "7"] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, tenant INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION allowed(threshold INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.secret WHERE tenant = threshold
             $$;
             CREATE FUNCTION wrapped(threshold INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT result.id FROM public.allowed({expression}) result(id)
             $$;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.secret WHERE id IN (SELECT public.wrapped(7))
             $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{expression}");
    }
}

/// A policy helper without a usable source cannot authorize an unrelated read.
#[test]
fn incomplete_helpers_do_not_authorize_protected_rows() {
    for body in [
        "VALUES (7)",
        "SELECT 7",
        "SELECT id FROM public.secret LIMIT 1",
        "SELECT result.id FROM public.allowed() WITH ORDINALITY result(id, ordinal)",
        "WITH allowed AS (VALUES (7)) SELECT column1 FROM allowed",
        "WITH allowed AS (SELECT 7 AS id) SELECT id FROM allowed",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, tenant INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.secret $$;
             CREATE FUNCTION helper() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {body} $$;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id IN (SELECT public.helper()));
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.secret $$;"
        ));
        assert_eq!(
            function_read(&database, None, "probe"),
            Some(("secret".to_owned(), "readable".to_owned())),
            "{body}"
        );
    }
}

/// A set-returning output parameter cannot stand for an ordinary input
/// argument.
#[test]
fn output_parameters_do_not_create_self_membership() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION probe(OUT result INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.secret $$;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
         GRANT SELECT ON secret TO app;",
    );
    assert_eq!(
        function_read(&database, None, "probe"),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// A bound SQL body keeps its write target across a local search-path scope.
#[test]
fn bound_sql_write_targets_keep_their_creation_binding() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE audit (id INT);
         GRANT INSERT ON audit TO app;
         CREATE FUNCTION probe() RETURNS VOID LANGUAGE sql SECURITY DEFINER
         BEGIN ATOMIC
           INSERT INTO public.audit VALUES (7);
         END;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// A nullable foreign-key component can leave the referenced row unrestricted.
#[test]
fn nullable_trigger_key_does_not_confine_the_referenced_row() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, tenant INT NOT NULL, UNIQUE (id, tenant));
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         CREATE TABLE bindings (run_id INT NOT NULL, tenant INT,
           FOREIGN KEY (run_id, tenant) REFERENCES secret (id, tenant));
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY writable ON bindings FOR INSERT TO app WITH CHECK (tenant = 7 OR tenant IS NULL);
         GRANT INSERT ON bindings TO app;
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           BEGIN
             PERFORM 1 FROM public.secret WHERE secret.id = NEW.run_id;
             RETURN NEW;
           END
         $$;
         CREATE TRIGGER guarded AFTER INSERT ON bindings FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guarded", DefinerReading::Outcome),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// Mutating a trigger field discards facts about its entry value.
#[test]
fn changed_trigger_fields_cannot_confine_a_read() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
         CREATE TABLE bindings (id INT);
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY writable ON bindings FOR INSERT TO app WITH CHECK (id = 7);
         GRANT INSERT ON bindings TO app;
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           BEGIN
             NEW.id := 8;
             PERFORM 1 FROM public.secret WHERE id = NEW.id;
             RETURN NEW;
           END
         $$;
         CREATE TRIGGER guarded AFTER INSERT ON bindings FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guarded", DefinerReading::Outcome),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// Procedural locals and output arguments do not alter an unrestricted policy.
#[test]
fn procedural_variable_kinds_leave_open_policies_confined() {
    for (arguments, declarations, body) in [
        (
            "input INT",
            "DECLARE local_value INT := input;",
            "PERFORM 1 FROM public.secret WHERE id = local_value; RETURN input;",
        ),
        (
            "OUT result INT, IN input INT",
            "",
            "SELECT id INTO result FROM public.secret WHERE id = input; RETURN;",
        ),
        (
            "OUT result INT",
            "",
            "result := 7; PERFORM 1 FROM public.secret WHERE id = result; RETURN;",
        ),
        (
            "INOUT input INT",
            "",
            "input := 7; PERFORM 1 FROM public.secret WHERE id = input; RETURN;",
        ),
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (TRUE);
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION probe({arguments}) RETURNS INT LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               {declarations}
               BEGIN {body} END
             $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{arguments}");
    }
}

/// Output lineage visits recursive sources without assuming recursive rows are
/// new.
#[test]
fn recursive_cte_lineage_keeps_the_seed_permission() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           WITH RECURSIVE reached(id) AS (
             SELECT id FROM public.secret
             UNION SELECT id FROM reached
           ) SELECT id FROM reached
         $$;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id IN (SELECT public.probe()));
         GRANT SELECT ON secret TO app;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// Helper discriminator facts follow foreign keys with finite cycles.
#[test]
fn helper_discriminators_follow_checked_foreign_keys() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE kinds (id INT PRIMARY KEY REFERENCES kinds(id), kind TEXT NOT NULL CHECK (kind IN ('allowed', 'other')),
           parent INT REFERENCES kinds(id), UNIQUE (id, kind));
         CREATE TABLE secret (id INT PRIMARY KEY REFERENCES kinds(id), kind TEXT NOT NULL,
           tenant INT NOT NULL, FOREIGN KEY (id, kind) REFERENCES kinds (id, kind));
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secret, kinds TO app;
         CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE kind = 'allowed' AND tenant = 7
           UNION ALL SELECT id FROM public.secret WHERE kind = 'other'
         $$;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE kind = 'allowed' AND id IN (SELECT public.allowed())
         $$;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// Projecting an input parameter does not grant membership to a stored column.
#[test]
fn parameter_projection_keeps_the_explicit_row_filter() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION probe(input INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT input FROM public.secret WHERE id = 7 $$;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// Descendant rows cannot borrow a parent's unique-key trigger witness.
#[test]
fn inherited_read_rows_do_not_borrow_parent_uniqueness() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, tenant INT NOT NULL, UNIQUE (id, tenant));
         CREATE TABLE child () INHERITS (secret);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         CREATE TABLE bindings (run_id INT NOT NULL, tenant INT NOT NULL,
           FOREIGN KEY (run_id, tenant) REFERENCES secret (id, tenant));
         ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
         CREATE POLICY writable ON bindings FOR INSERT TO app WITH CHECK (tenant = 7);
         GRANT INSERT ON bindings TO app;
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           BEGIN
             PERFORM 1 FROM public.secret WHERE id = NEW.run_id;
             RETURN NEW;
           END
         $$;
         CREATE TRIGGER guarded AFTER INSERT ON bindings FOR EACH ROW EXECUTE FUNCTION guard();",
    );
    assert_eq!(
        trigger_read(&database, "guarded", DefinerReading::Outcome),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// A helper without row restrictions cannot prove a protected row's tenant.
#[test]
fn unrestricted_helper_rows_do_not_supply_missing_conditions() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, tenant INT);
         CREATE TABLE visible (id INT PRIMARY KEY);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secret, visible TO app;
         CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.visible $$;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE id IN (SELECT public.allowed())
         $$;",
    );
    assert_eq!(
        function_read(&database, None, "probe"),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// Unrestricted row policies admit rows across unusual join shapes.
#[test]
fn unusual_join_shapes_keep_unrestricted_rows_visible() {
    for query in [
        "SELECT secret.id FROM public.secret JOIN (SELECT id FROM public.visible) source USING (id)",
        "SELECT secret.id FROM (SELECT 7 AS id) source JOIN public.secret USING (id)",
        "SELECT secret.id FROM public.padding CROSS JOIN public.secret JOIN public.visible USING (id)",
        "SELECT secret.id FROM public.secret JOIN public.visible USING (id) JOIN public.decimal_ids USING (id)",
        "SELECT secret.id FROM public.secret RIGHT JOIN public.visible ON secret.id = visible.id",
        "SELECT secret.id FROM public.secret FULL JOIN public.visible USING (id) JOIN public.third USING (id)",
        "SELECT secret.id FROM public.secret, (public.visible JOIN public.third ON visible.id = third.id)",
        "SELECT secret.id FROM public.secret JOIN (SELECT * FROM public.visible) source ON source.id = secret.id",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT);
             CREATE TABLE visible (id INT);
             CREATE TABLE third (id INT);
             CREATE TABLE padding (other INT);
             CREATE TABLE decimal_ids (id NUMERIC);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (TRUE);
             CREATE POLICY writable ON secret FOR INSERT TO app WITH CHECK (TRUE);
             GRANT SELECT ON secret, visible, third, padding, decimal_ids TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// Restricted target rows remain confined when an update returns a joined
/// value.
#[test]
fn returned_update_confines_its_joined_source() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY);
         CREATE TABLE visible (id INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id = 7);
         GRANT SELECT, UPDATE ON secret TO app;
         GRANT SELECT ON visible TO app;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           UPDATE public.secret SET id = 7 FROM public.visible
           WHERE secret.id = 7 RETURNING visible.id
         $$;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// Conflict updates retain their target alias and procedural parameter scope.
#[test]
fn conflict_updates_resolve_aliases_and_procedural_parameters() {
    for protected in [false, true] {
        let security = if protected {
            "ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (TRUE);"
        } else {
            ""
        };
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY);
             CREATE TABLE sentinel (id INT);
             ALTER TABLE sentinel ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON sentinel FOR SELECT TO app USING (TRUE);
             {security}
             GRANT SELECT, INSERT, UPDATE ON secret TO app;
             GRANT SELECT ON sentinel TO app;
             CREATE FUNCTION probe(input INT) RETURNS INT LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               DECLARE result INT;
               BEGIN
                 PERFORM id FROM public.sentinel;
                 INSERT INTO public.secret AS target VALUES (7)
                 ON CONFLICT (id) DO UPDATE SET id = 7 WHERE target.id = input
                 RETURNING target.id INTO result;
                 RETURN result;
               END
             $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{protected}");
    }
}

/// Missing joined witnesses cannot establish membership in a policy helper.
#[test]
fn absent_and_derived_witnesses_do_not_establish_helper_membership() {
    for joined in [
        "LEFT JOIN public.permissions ON secret.id = permissions.id",
        "CROSS JOIN (SELECT 1 AS marker) source",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY);
             CREATE TABLE permissions (id INT PRIMARY KEY);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.permissions $$;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (id IN (SELECT public.allowed()));
             GRANT SELECT ON secret, permissions TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT secret.id FROM public.secret {joined}
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "probe"),
            Some(("secret".to_owned(), "readable".to_owned())),
            "{joined}"
        );
    }
}

/// A helper reading a view cannot certify unrestricted protected rows.
#[test]
fn view_helper_sources_do_not_certify_membership() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY);
         CREATE TABLE visible (id INT);
         CREATE VIEW exposed AS SELECT id FROM visible;
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.exposed $$;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (id IN (SELECT public.allowed()));
         GRANT SELECT ON secret, visible, exposed TO app;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.secret $$;",
    );
    assert_eq!(
        function_read(&database, None, "probe"),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// Unrestricted policies admit source rows even when output names cannot carry
/// lineage.
#[test]
fn unusual_cte_outputs_keep_unrestricted_rows_visible() {
    for query in [
        "SELECT id FROM secret",
        "WITH source(first, second) AS (SELECT id, other FROM public.secret) SELECT first FROM source",
        "WITH source AS (SELECT id, other FROM public.secret) SELECT id FROM source",
        "WITH source AS (SELECT * FROM public.secret) SELECT id FROM source",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT, other INT);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (TRUE);
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = public, pg_catalog, pg_temp AS $$ {query} $$;"
        ));
        assert_eq!(function_read(&database, None, "probe"), None, "{query}");
    }
}

/// Insert-policy conjunctions constrain each equated trigger-row field.
#[test]
fn insert_policy_conjunctions_constrain_equated_fields() {
    for check in [
        "tenant = 7 AND marker = 9",
        "(tenant = 7 AND marker = 9) OR (tenant = 7 AND marker = 10)",
        "bindings.tenant = 7 AND bindings.marker = 9",
        "public.bindings.tenant = 7 AND public.bindings.marker = 9",
    ] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, tenant INT NOT NULL);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
             CREATE TABLE bindings (run_id INT, tenant INT NOT NULL, marker INT NOT NULL);
             ALTER TABLE bindings ENABLE ROW LEVEL SECURITY;
             CREATE POLICY writable ON bindings FOR INSERT TO app WITH CHECK ({check});
             GRANT INSERT ON bindings TO app;
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               BEGIN
                 PERFORM 1 FROM public.secret WHERE secret.tenant = NEW.tenant AND secret.id = NEW.run_id;
                 RETURN NEW;
               END
             $$;
             CREATE TRIGGER guarded AFTER INSERT ON bindings FOR EACH ROW EXECUTE FUNCTION guard();"
        ));
        assert_eq!(trigger_read(&database, "guarded", DefinerReading::Outcome), None, "{check}");
    }
}

/// Foreign keys unrelated to a returned identifier cannot supply its policy
/// facts.
#[test]
fn unrelated_helper_keys_do_not_supply_policy_facts() {
    for reversed in [false, true] {
        let (source, target) = if reversed { ("parent", "child") } else { ("child", "parent") };
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE unrelated (id INT PRIMARY KEY);
             CREATE TABLE parent (id INT PRIMARY KEY, other INT UNIQUE, tenant INT,
               unrelated INT REFERENCES unrelated(id), UNIQUE (id, other));
             CREATE TABLE child (id INT PRIMARY KEY, other INT NOT NULL, tenant NUMERIC,
               unrelated INT REFERENCES unrelated(id),
               FOREIGN KEY (id) REFERENCES parent(other),
               FOREIGN KEY (id, other) REFERENCES parent(id, other));
             ALTER TABLE {target} ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON {target} FOR SELECT TO app USING (tenant = 7);
             GRANT SELECT ON parent, child, unrelated TO app;
             CREATE FUNCTION allowed() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.{source} WHERE tenant = 8
             $$;
             CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               SELECT id FROM public.{target} WHERE id IN (SELECT public.allowed())
             $$;"
        ));
        assert_eq!(
            function_read(&database, None, "probe"),
            Some((target.to_owned(), "readable".to_owned())),
            "{reversed}"
        );
    }
}

/// A helper argument drawn from a joined row remains unknown to a constant
/// policy.
#[test]
fn column_arguments_do_not_become_literal_helper_constraints() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, tenant INT);
         CREATE TABLE inputs (tenant INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         GRANT SELECT ON secret, inputs TO app;
         CREATE FUNCTION allowed(tenant INT) RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$ SELECT id FROM public.secret WHERE secret.tenant = $1 $$;
         CREATE FUNCTION wrapped() RETURNS SETOF INT LANGUAGE sql STABLE SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT result.id FROM public.inputs CROSS JOIN LATERAL public.allowed(inputs.tenant) result(id)
         $$;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           SELECT id FROM public.secret WHERE id IN (SELECT public.wrapped())
         $$;",
    );
    assert_eq!(
        function_read(&database, None, "probe"),
        Some(("secret".to_owned(), "readable".to_owned()))
    );
}

/// An unobserved joined write may read unrestricted relations.
#[test]
fn unobserved_joined_writes_allow_open_sources() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE audit (id INT);
         CREATE TABLE visible (id INT);
         CREATE TABLE secret (id INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (TRUE);
         GRANT SELECT, UPDATE ON audit TO app;
         GRANT SELECT ON visible, secret TO app;
         CREATE FUNCTION probe() RETURNS VOID LANGUAGE plpgsql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           BEGIN
             UPDATE public.audit SET id = visible.id FROM public.visible WHERE audit.id = visible.id;
             PERFORM id FROM public.secret;
             RETURN;
           END
         $$;",
    );
    assert_eq!(function_read(&database, None, "probe"), None);
}

/// A writable common-table expression is reported as unfollowed.
#[test]
fn writable_cte_reads_remain_unfollowed() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (TRUE);
         GRANT SELECT, DELETE ON secret TO app;
         CREATE FUNCTION probe() RETURNS SETOF INT LANGUAGE sql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           WITH removed AS (DELETE FROM public.secret RETURNING id)
           SELECT id FROM removed
         $$;",
    );
    assert_eq!(
        function_read(&database, None, "probe"),
        Some((String::new(), "unfollowed".to_owned()))
    );
}

/// A trigger's old-row condition confines a read equated to the old field.
#[test]
fn old_row_trigger_conditions_confine_equated_reads() {
    let database = schema(
        "CREATE ROLE app;
         CREATE TABLE secret (id INT PRIMARY KEY, tenant INT);
         ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
         CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
         CREATE TABLE bindings (run_id INT, tenant INT, marker INT);
         GRANT SELECT, UPDATE ON bindings TO app;
         GRANT SELECT ON secret TO app;
         CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
         SET search_path = pg_catalog, pg_temp AS $$
           BEGIN
             PERFORM 1 FROM public.secret WHERE secret.tenant = OLD.tenant;
             RETURN NEW;
           END
         $$;
         CREATE TRIGGER guarded AFTER UPDATE ON bindings FOR EACH ROW
           WHEN ((OLD.tenant = 7 AND OLD.marker = 9) OR (OLD.tenant = 7 AND OLD.marker = 10))
           EXECUTE FUNCTION guard();",
    );
    assert_eq!(trigger_read(&database, "guarded", DefinerReading::Outcome), None);
}

/// A read keyed by a trigger variable other than the written row, or by a
/// field of the row taken through a parenthesised record, holds no fact of
/// the written row.
#[test]
fn trigger_references_besides_row_fields_hold_no_written_row_fact() {
    for key in ["TG_RELID::INT", "(NEW).tenant"] {
        let database = schema(&format!(
            "CREATE ROLE app;
             CREATE TABLE secret (id INT PRIMARY KEY, tenant INT NOT NULL);
             ALTER TABLE secret ENABLE ROW LEVEL SECURITY;
             CREATE POLICY readable ON secret FOR SELECT TO app USING (tenant = 7);
             CREATE TABLE bindings (run_id INT, tenant INT NOT NULL);
             GRANT SELECT, INSERT ON bindings TO app;
             GRANT SELECT ON secret TO app;
             CREATE FUNCTION guard() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
             SET search_path = pg_catalog, pg_temp AS $$
               BEGIN
                 PERFORM 1 FROM public.secret WHERE secret.tenant = {key};
                 RETURN NEW;
               END
             $$;
             CREATE TRIGGER guarded AFTER INSERT ON bindings FOR EACH ROW
               EXECUTE FUNCTION guard();"
        ));
        assert_eq!(
            trigger_read(&database, "guarded", DefinerReading::Outcome),
            Some(("secret".to_owned(), "readable".to_owned())),
            "{key}"
        );
    }
}
