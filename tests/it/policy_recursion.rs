//! When expanding a statement's policies loops back into a table whose
//! policies are still being expanded, PostgreSQL refuses the statement with
//! `infinite recursion detected in policy for relation` (`42P17`).
//!
//! Every expectation was measured on PostgreSQL 18.6 as a role that does not
//! own the tables.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{LookupError, RowSecurityError},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

const TABLES: &str = "
    CREATE ROLE reader NOLOGIN;
    CREATE ROLE other NOLOGIN;
    CREATE ROLE admin NOLOGIN SUPERUSER;
    CREATE TABLE t (id int PRIMARY KEY, v int NOT NULL);
    CREATE TABLE u (id int PRIMARY KEY, v int NOT NULL);
    CREATE TABLE w (id int PRIMARY KEY, v int NOT NULL);
    ALTER TABLE t ENABLE ROW LEVEL SECURITY;
    ALTER TABLE u ENABLE ROW LEVEL SECURITY;
    ALTER TABLE w ENABLE ROW LEVEL SECURITY;
";

const OPEN_T_WRITES: &str = "
    CREATE POLICY t_ins ON t FOR INSERT WITH CHECK (true);
    CREATE POLICY t_upd ON t FOR UPDATE USING (true);
    CREATE POLICY t_del ON t FOR DELETE USING (true);
";

/// What the statement meets as `role` on `table`: the relation PostgreSQL
/// names in its recursion error, or `None` where the statement runs.
fn recursion(sql: &str, table: &str, role: &str, statement: RowStatement) -> Option<String> {
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!("{TABLES}{sql}")).expect("builds");
    let role = database.role(role).expect("the role exists");
    let table = database
        .table_by_target(TargetName::new(table, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let security = table.row_security(statement, role, &database).expect("settled");
    security.infinite_recursion().map(str::to_string)
}

const SELECT: RowStatement = RowStatement::Select;
const UPDATE_READING: RowStatement = RowStatement::Update { reads: true };
const UPDATE_BLIND: RowStatement = RowStatement::Update { reads: false };

#[test]
fn a_read_policy_reading_its_own_table_refuses_every_statement_applying_it() {
    let sql = format!(
        "{OPEN_T_WRITES}
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id));"
    );
    for statement in [
        SELECT,
        RowStatement::Insert { returning: true },
        UPDATE_READING,
        RowStatement::Delete { reads: true },
    ] {
        assert_eq!(
            recursion(&sql, "t", "reader", statement).as_deref(),
            Some("t"),
            "{statement:?}"
        );
    }
    for statement in [
        RowStatement::Insert { returning: false },
        UPDATE_BLIND,
        RowStatement::Delete { reads: false },
    ] {
        assert_eq!(recursion(&sql, "t", "reader", statement), None, "{statement:?}");
    }
}

#[test]
fn a_write_policy_reading_its_own_table_reads_it_through_the_read_policies() {
    let open_read = "CREATE POLICY t_sel ON t FOR SELECT USING (true);
         CREATE POLICY t_clauseless ON t FOR SELECT;
         CREATE POLICY t_ins ON t FOR INSERT WITH CHECK (EXISTS (SELECT 1 FROM t t2));
         CREATE POLICY t_upd ON t FOR UPDATE USING (EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id));
         CREATE POLICY t_del ON t FOR DELETE USING (EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id));";
    for statement in [RowStatement::Insert { returning: true }, UPDATE_READING, UPDATE_BLIND] {
        assert_eq!(recursion(open_read, "t", "reader", statement), None, "{statement:?}");
    }

    // Once the read policy reads a table of its own, re-reading `t` expands it
    // while `t`'s policies are still being expanded.
    let reading_read = "CREATE POLICY u_all ON u USING (true);
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM u WHERE u.id = t.id));
         CREATE POLICY t_upd ON t FOR UPDATE USING (EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id));";
    assert_eq!(recursion(reading_read, "t", "reader", UPDATE_BLIND).as_deref(), Some("t"));
    assert_eq!(recursion(reading_read, "t", "reader", SELECT), None);
}

#[test]
fn two_read_policies_reading_each_other_name_the_table_read_first() {
    let sql = format!(
        "{OPEN_T_WRITES}
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM u WHERE u.id = t.id));
         CREATE POLICY u_sel ON u FOR SELECT USING (EXISTS (SELECT 1 FROM t WHERE t.id = u.id));"
    );
    assert_eq!(recursion(&sql, "t", "reader", SELECT).as_deref(), Some("t"));
    assert_eq!(recursion(&sql, "u", "reader", SELECT).as_deref(), Some("u"));
    assert_eq!(recursion(&sql, "t", "reader", UPDATE_BLIND), None);
}

#[test]
fn a_loop_past_the_table_read_is_still_refused() {
    let sql = format!(
        "{OPEN_T_WRITES}
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM u WHERE u.id = t.id));
         CREATE POLICY u_sel ON u FOR SELECT USING (EXISTS (SELECT 1 FROM w WHERE w.id = u.id));
         CREATE POLICY w_sel ON w FOR SELECT USING (EXISTS (SELECT 1 FROM u WHERE u.id = w.id));"
    );
    assert_eq!(recursion(&sql, "t", "reader", SELECT).as_deref(), Some("u"));
}

#[test]
fn a_literal_whose_value_is_unknown_hides_no_loop() {
    let d = "
        CREATE TABLE d (id int PRIMARY KEY, team int, expires timestamptz);
        ALTER TABLE d ENABLE ROW LEVEL SECURITY;
        CREATE POLICY d_sel ON d FOR SELECT
            USING (expires > '2026-01-01 09:00' AND team IN (SELECT id FROM u));";
    let looping =
        format!("{d} CREATE POLICY u_sel ON u FOR SELECT USING (v IN (SELECT id FROM d));");
    assert_eq!(recursion(&looping, "d", "reader", SELECT).as_deref(), Some("d"));
    assert_eq!(recursion(&looping, "u", "reader", SELECT).as_deref(), Some("u"));

    // Without a loop the gate on `d` is answered, its term marked unread.
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{TABLES}{d} CREATE POLICY u_sel ON u FOR SELECT USING (true);"
    ))
    .expect("builds");
    let reader = database.role("reader").expect("the role exists");
    let table = database
        .table_by_target(TargetName::new("d", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    let security = table.row_security(SELECT, reader, &database).expect("settled");
    let gate = &security.gates().expect("the role is not exempt")[0];
    assert!(matches!(
        gate.permissive()[0].expression(),
        PolicyExpression::Written { unreadable: LookupError::PolicyLiteralUnreadable { .. }, .. }
    ));
}

#[test]
fn a_loop_closes_only_through_policies_and_tables_that_bind_the_role() {
    let loop_back =
        "CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM u WHERE u.id = t.id));";
    let scoped = format!(
        "{loop_back}
         CREATE POLICY u_sel ON u FOR SELECT TO other USING (EXISTS (SELECT 1 FROM t WHERE t.id = u.id));
         CREATE POLICY u_open ON u FOR SELECT TO reader USING (true);"
    );
    assert_eq!(recursion(&scoped, "t", "reader", SELECT), None);
    assert_eq!(recursion(&scoped, "t", "other", SELECT).as_deref(), Some("t"));

    let back =
        "CREATE POLICY u_sel ON u FOR SELECT USING (EXISTS (SELECT 1 FROM t WHERE t.id = u.id));";
    let unprotected = format!("{loop_back}{back} ALTER TABLE u DISABLE ROW LEVEL SECURITY;");
    assert_eq!(recursion(&unprotected, "t", "reader", SELECT), None);
    let owned = format!("{loop_back}{back} ALTER TABLE u OWNER TO reader;");
    assert_eq!(recursion(&owned, "t", "reader", SELECT), None);
}

#[test]
fn an_array_subquery_reads_like_any_other_and_a_materialized_view_reads_nothing() {
    let array = "CREATE POLICY t_sel ON t FOR SELECT
         USING (coalesce(id, 0) = ANY (ARRAY(SELECT id FROM t t2)));";
    assert_eq!(recursion(array, "t", "reader", SELECT).as_deref(), Some("t"));

    let stored = "CREATE MATERIALIZED VIEW mt AS SELECT id FROM t;
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM mt WHERE mt.id = t.id));";
    assert_eq!(recursion(stored, "t", "reader", SELECT), None);
}

#[test]
fn a_parent_read_applies_none_of_its_children_policies() {
    let sql = "CREATE TABLE c (extra int) INHERITS (u);
         ALTER TABLE c ENABLE ROW LEVEL SECURITY;
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM u WHERE u.id = t.id));
         CREATE POLICY u_sel ON u FOR SELECT USING (true);
         CREATE POLICY c_sel ON c FOR SELECT USING (EXISTS (SELECT 1 FROM t WHERE t.id = c.id));";
    assert_eq!(recursion(sql, "t", "reader", SELECT), None);
}

#[test]
fn a_plain_view_reads_as_its_owner_and_an_invoker_view_as_the_caller() {
    let through_view = |view: &str| {
        format!(
            "{view}
             CREATE POLICY t_sel ON t FOR SELECT TO reader
                 USING (EXISTS (SELECT 1 FROM tv WHERE tv.id = t.id));
             CREATE POLICY t_sel_other ON t FOR SELECT TO other USING (true);"
        )
    };
    let owned_by = |owner: &str| {
        through_view(&format!(
            "CREATE VIEW tv AS SELECT id FROM t; ALTER VIEW tv OWNER TO {owner};"
        ))
    };
    assert_eq!(recursion(&owned_by("reader"), "t", "reader", SELECT).as_deref(), Some("t"));
    assert_eq!(recursion(&owned_by("other"), "t", "reader", SELECT), None);
    assert_eq!(recursion(&owned_by("admin"), "t", "reader", SELECT), None);

    let invoker = through_view(
        "CREATE VIEW tv WITH (security_invoker = true) AS SELECT id FROM t;
         ALTER VIEW tv OWNER TO admin;",
    );
    assert_eq!(recursion(&invoker, "t", "reader", SELECT).as_deref(), Some("t"));
}

#[test]
fn a_plain_view_whose_owner_the_schema_never_names_refuses_the_answer() {
    let database = ParserDB::parse::<PostgreSqlDialect>(&format!(
        "{TABLES}
         CREATE VIEW tv AS SELECT id FROM t;
         CREATE POLICY t_sel ON t FOR SELECT USING (EXISTS (SELECT 1 FROM tv WHERE tv.id = t.id));"
    ))
    .expect("builds");
    let reader = database.role("reader").expect("the role exists");
    let t = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    assert_eq!(
        t.row_security(SELECT, reader, &database).map(|_| ()),
        Err(RowSecurityError::UnsettledViewOwner { view: "tv".into() })
    );
}

/// A migration run as a session role owns what it creates, so a plain view
/// it creates without `OWNER TO` reads as that role.
#[test]
fn a_plain_view_created_by_the_session_role_reads_as_it() {
    let recursion = |force: &str| {
        let database = ParseOptions::default()
            .with_session_role("other")
            .parse::<PostgreSqlDialect>(&format!(
                "{TABLES}{force}
                 CREATE VIEW tv AS SELECT id FROM t;
                 CREATE POLICY t_sel ON t FOR SELECT
                     USING (EXISTS (SELECT 1 FROM tv WHERE tv.id = t.id));"
            ))
            .expect("builds");
        let reader = database.role("reader").expect("the role exists");
        let t = database
            .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("the table exists");
        t.row_security(SELECT, reader, &database)
            .expect("settled")
            .infinite_recursion()
            .map(str::to_string)
    };
    // The view reads `t` as `other`, which created it and so is exempt.
    assert_eq!(recursion(""), None);
    // Under FORCE the owner meets `t`'s policies too, and the loop closes.
    assert_eq!(recursion("ALTER TABLE t FORCE ROW LEVEL SECURITY;").as_deref(), Some("t"));
}

/// Skipping a name it cannot read would hide a loop through it.
#[test]
fn a_subquery_naming_a_malformed_relation_refuses_the_answer() {
    let database = ParseOptions::default()
        .with_access_resolution(AccessResolution::OpenWorld)
        .parse::<PostgreSqlDialect>(&format!(
            "{TABLES}
             CREATE POLICY t_sel ON t FOR SELECT
                 USING (EXISTS (SELECT 1 FROM cluster.db.schema.x WHERE x.id = t.id));"
        ))
        .expect("builds");
    let reader = database.role("reader").expect("the role exists");
    let t = database
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists");
    assert!(matches!(
        t.row_security(SELECT, reader, &database),
        Err(RowSecurityError::Lookup(LookupError::InvalidObjectName { .. }))
    ));
}
