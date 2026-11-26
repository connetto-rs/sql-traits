//! Tests that `ViewLike::privileges` answers what PostgreSQL's
//! `has_table_privilege` and `has_column_privilege` answer on a view, per
//! command and per output column.
//!
//! Every expectation was measured on PostgreSQL 18.7, with role `r`
//! inheriting from `grp`, `noinh` a `WITH INHERIT FALSE` member of `grp`, and
//! `su` a superuser.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

use crate::support::privilege_listing::{EVERYTHING, privilege_names, view_privilege_names};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

const ROLES: &str = "CREATE ROLE grp; CREATE ROLE r; CREATE ROLE noinh; CREATE ROLE own;
     CREATE ROLE own2; CREATE ROLE su SUPERUSER;
     GRANT grp TO r; GRANT grp TO noinh WITH INHERIT FALSE;";

type View = <ParserDB as DatabaseLike>::View;

fn view<'db>(db: &'db ParserDB, name: &str) -> &'db View {
    db.view_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the view exists")
}

/// What `role` holds on view `name`, its output columns spelled `outputs`.
fn held(db: &ParserDB, name: &str, role: &str, outputs: &[&str]) -> Vec<String> {
    let privileges = view(db, name)
        .privileges(db.role(role).expect("the role exists"), db)
        .expect("the view is in the database");
    view_privilege_names(&privileges, outputs)
}

#[test]
fn a_column_grant_on_a_view_holds_on_its_output_columns() {
    let db = parse(
        "CREATE ROLE app; CREATE ROLE mover;
         CREATE TABLE resources (id INT PRIMARY KEY, parent_id INT);
         CREATE VIEW resource_moves AS SELECT id, parent_id FROM resources WHERE parent_id > 0;
         ALTER VIEW resource_moves OWNER TO mover;
         GRANT SELECT, UPDATE (parent_id) ON resource_moves TO app;",
    );
    let outputs = ["id", "parent_id"];

    assert_eq!(held(&db, "resource_moves", "app", &outputs), ["SELECT", "UPDATE (parent_id)"]);
    assert_eq!(held(&db, "resource_moves", "mover", &outputs), EVERYTHING);
}

#[test]
fn a_revoke_naming_the_view_owner_takes_from_its_entry() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         ALTER VIEW v OWNER TO own;
         REVOKE UPDATE, TRUNCATE ON v FROM own;"
    ));

    assert_eq!(
        held(&db, "v", "own", &["id", "x"]),
        ["SELECT", "INSERT", "REFERENCES", "DELETE", "TRIGGER", "MAINTAIN"]
    );
}

#[test]
fn a_column_grant_names_the_columns_the_view_declares() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v (a, b) AS SELECT id, x FROM t;
         GRANT INSERT (b) ON v TO r;"
    ));

    assert_eq!(held(&db, "v", "r", &["a", "b"]), ["INSERT (b)"]);
}

#[test]
fn view_grants_reach_inheriting_members_and_public() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         GRANT DELETE ON v TO grp;
         GRANT SELECT ON v TO PUBLIC;
         GRANT UPDATE (x) ON v TO grp;"
    ));
    let outputs = ["id", "x"];

    assert_eq!(held(&db, "v", "r", &outputs), ["SELECT", "UPDATE (x)", "DELETE"]);
    assert_eq!(held(&db, "v", "noinh", &outputs), ["SELECT"]);
    let public =
        view(&db, "v").privileges(ActingRole::Public, &db).expect("the view is in the database");
    assert_eq!(view_privilege_names(&public, &outputs), ["SELECT"]);
}

#[test]
fn a_schema_sweep_grants_and_revokes_on_views() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         ALTER VIEW v OWNER TO own;
         GRANT UPDATE ON ALL TABLES IN SCHEMA public TO r;
         REVOKE ALL ON ALL TABLES IN SCHEMA public FROM own;"
    ));
    let outputs = ["id", "x"];

    assert_eq!(held(&db, "v", "r", &outputs), ["UPDATE"]);
    assert_eq!(held(&db, "v", "own", &outputs), Vec::<String>::new());
}

#[test]
fn a_column_revoke_on_a_view_leaves_the_other_columns() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v (a, b) AS SELECT id, x FROM t;
         GRANT UPDATE (a, b) ON v TO r;
         REVOKE UPDATE (a) ON v FROM r;"
    ));

    assert_eq!(held(&db, "v", "r", &["a", "b"]), ["UPDATE (b)"]);
}

#[test]
fn a_grant_reaches_only_the_relation_it_names() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         CREATE MATERIALIZED VIEW mv AS SELECT id FROM t;
         GRANT SELECT ON mv TO r;
         GRANT ALL ON t TO r;"
    ));
    let materialized = db
        .materialized_view_by_target(TargetName::new("mv", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the materialized view exists")
        .privileges(db.role("r").expect("the role exists"), &db)
        .expect("the materialized view is in the database");

    assert_eq!(held(&db, "v", "r", &["id", "x"]), Vec::<String>::new());
    assert_eq!(view_privilege_names(&materialized, &["id"]), ["SELECT"]);
}

#[test]
fn a_revoke_naming_the_materialized_view_owner_takes_from_its_entry() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE MATERIALIZED VIEW mv AS SELECT id, x FROM t;
         ALTER MATERIALIZED VIEW mv OWNER TO own;
         REVOKE UPDATE, TRUNCATE ON mv FROM own;"
    ));
    let owned = db
        .materialized_view_by_target(TargetName::new("mv", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the materialized view exists")
        .privileges(db.role("own").expect("the role exists"), &db)
        .expect("the materialized view is in the database");

    assert_eq!(
        view_privilege_names(&owned, &["id", "x"]),
        ["SELECT", "INSERT", "REFERENCES", "DELETE", "TRIGGER", "MAINTAIN"]
    );
}

/// The columns `grant` lists, read through generic code that holds grants by
/// reference.
fn listed<G: ColumnGrantLike>(grant: G) -> Vec<(String, bool)> {
    grant.column_names().map(|(name, quoted)| (name.to_owned(), quoted)).collect()
}

#[test]
fn only_column_privileges_list_column_names() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         GRANT ALL ON v TO r;
         GRANT SELECT (x), DELETE ON v TO grp;"
    ));

    assert!(db.table_grants().map(listed).all(|names| names.is_empty()));
    assert_eq!(db.column_grants().map(listed).collect::<Vec<_>>(), [[("x".to_owned(), false)]]);
}

#[test]
fn a_declared_name_holds_where_the_definition_names_no_output() {
    let db = parse(&format!(
        "{ROLES}
         CREATE VIEW s (n) AS SELECT * FROM generate_series(1, 3);
         GRANT SELECT (n) ON s TO r;"
    ));

    assert_eq!(held(&db, "s", "r", &["n"]), ["SELECT (n)"]);
}

#[test]
fn a_superuser_holds_every_privilege_on_a_view() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;"
    ));

    assert_eq!(held(&db, "v", "su", &["id", "x"]), EVERYTHING);
}

#[test]
fn a_replacement_keeps_the_view_privileges() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id FROM t;
         ALTER VIEW v OWNER TO own;
         REVOKE DELETE ON v FROM own;
         GRANT SELECT ON v TO r;
         CREATE OR REPLACE VIEW v AS SELECT id, x FROM t;"
    ));
    let outputs = ["id", "x"];

    assert_eq!(
        held(&db, "v", "own", &outputs),
        ["SELECT", "INSERT", "UPDATE", "REFERENCES", "TRUNCATE", "TRIGGER", "MAINTAIN"]
    );
    assert_eq!(held(&db, "v", "r", &outputs), ["SELECT"]);
}

#[test]
fn an_owner_change_hands_the_owner_entry_over() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         ALTER VIEW v OWNER TO own;
         REVOKE DELETE ON v FROM own;
         ALTER VIEW v OWNER TO own2;"
    ));
    let outputs = ["id", "x"];

    assert_eq!(held(&db, "v", "own", &outputs), Vec::<String>::new());
    assert_eq!(
        held(&db, "v", "own2", &outputs),
        ["SELECT", "INSERT", "UPDATE", "REFERENCES", "TRUNCATE", "TRIGGER", "MAINTAIN"]
    );
}

#[test]
fn default_privileges_shape_the_owner_entry_of_a_new_view() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         GRANT SELECT ON t TO own;
         GRANT CREATE ON SCHEMA public TO own;
         ALTER DEFAULT PRIVILEGES FOR ROLE own REVOKE TRUNCATE ON TABLES FROM own;
         SET ROLE own;
         CREATE VIEW v AS SELECT id, x FROM t;
         RESET ROLE;"
    ));

    assert_eq!(
        held(&db, "v", "own", &["id", "x"]),
        ["SELECT", "INSERT", "UPDATE", "REFERENCES", "DELETE", "TRIGGER", "MAINTAIN"]
    );
}

#[test]
fn all_privileges_on_a_quoted_output_column() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x AS \"X\" FROM t;
         GRANT ALL (\"X\") ON v TO r;"
    ));

    assert_eq!(
        held(&db, "v", "r", &["id", "X"]),
        ["SELECT (X)", "INSERT (X)", "UPDATE (X)", "REFERENCES (X)"]
    );
}

#[test]
fn a_column_grant_follows_a_view_column_rename() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         GRANT UPDATE (x) ON v TO r;
         GRANT SELECT (x), INSERT (id) ON v TO grp;
         ALTER VIEW v RENAME COLUMN x TO y;"
    ));

    assert_eq!(held(&db, "v", "r", &["id", "y"]), ["SELECT (y)", "INSERT (id)", "UPDATE (y)"]);
}

#[test]
fn a_view_column_rename_leaves_grants_on_other_relations() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE TABLE u (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM u;
         GRANT UPDATE (x) ON t TO r;
         GRANT UPDATE (x) ON v TO r;
         ALTER VIEW v RENAME COLUMN x TO y;"
    ));
    let table = db
        .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
        .expect("the lookup is unambiguous")
        .expect("the table exists")
        .privileges(db.role("r").expect("the role exists"), &db)
        .expect("the table is in the database");

    assert_eq!(privilege_names(&table), ["UPDATE (x)"]);
    assert_eq!(held(&db, "v", "r", &["id", "y"]), ["UPDATE (y)"]);
}

#[test]
fn a_base_column_rename_leaves_the_view_column_grant() {
    let db = parse(&format!(
        "{ROLES}
         CREATE TABLE t (id INT, x INT);
         CREATE VIEW v AS SELECT id, x FROM t;
         GRANT UPDATE (x) ON v TO r;
         ALTER TABLE t RENAME COLUMN x TO z;"
    ));

    assert_eq!(held(&db, "v", "r", &["id", "x"]), ["UPDATE (x)"]);
}
