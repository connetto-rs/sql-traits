//! A grant naming columns reaches those columns only, so it answers the
//! column-level privilege question and never the table-level one, as
//! PostgreSQL's `has_table_privilege` keeps the two apart.
#![allow(clippy::expect_used)]

use sql_traits::prelude::*;
use sqlparser::{ast::Action, dialect::PostgreSqlDialect};

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("schema builds")
}

fn table<'db>(database: &'db ParserDB, name: &str) -> &'db <ParserDB as DatabaseLike>::Table {
    database
        .table_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
}

/// The table-level privileges `role` holds on `a`, in the order SELECT,
/// INSERT, UPDATE.
fn table_privileges(database: &ParserDB, role: &str) -> [bool; 3] {
    let a = table(database, "a");
    let role = database.role(role).expect("the role exists");
    let held = a.privileges(role, database).expect("the table is in the database");
    [held.select.on_table(), held.insert.on_table(), held.update.on_table()]
}

fn rendered(grants: impl Iterator<Item = impl core::fmt::Display>) -> Vec<String> {
    grants.map(|grant| grant.to_string()).collect()
}

#[test]
fn a_column_grant_holds_no_table_privilege() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT (id), INSERT (b), UPDATE (b) ON a TO r1;",
    );

    assert_eq!(table_privileges(&database, "r1"), [false, false, false]);
}

#[test]
fn a_table_grant_beside_a_column_grant_holds_only_its_own_privilege() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b) ON a TO r1;",
    );

    assert_eq!(table_privileges(&database, "r1"), [true, false, false]);
}

#[test]
fn a_column_grant_reaches_no_member_as_a_table_privilege() {
    let database = parse(
        "CREATE ROLE grp;
         CREATE ROLE member IN ROLE grp;
         CREATE TABLE a (id int, b text);
         GRANT UPDATE (b) ON a TO grp;
         GRANT INSERT (id) ON a TO PUBLIC;",
    );

    assert_eq!(table_privileges(&database, "member"), [false, false, false]);
}

#[test]
fn a_mixed_grant_splits_between_the_table_and_column_stores() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b), DELETE ON a TO r1;",
    );

    assert_eq!(rendered(database.table_grants()), ["GRANT SELECT, DELETE ON a TO r1"]);
    assert_eq!(rendered(database.column_grants()), ["GRANT UPDATE (b) ON a TO r1"]);
    assert_eq!(
        rendered(table(&database, "a").grants(&database).expect("held")),
        ["GRANT SELECT, DELETE ON a TO r1"]
    );
}

#[test]
fn a_whole_relation_grant_stays_out_of_the_column_store() {
    let database = parse(
        "CREATE SCHEMA s;
         CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         CREATE TABLE s.c (id int);
         CREATE SEQUENCE q;
         GRANT ALL PRIVILEGES ON a TO r1;
         GRANT USAGE ON SEQUENCE q TO r1;
         GRANT CREATE ON SCHEMA s TO r1;
         GRANT SELECT ON ALL TABLES IN SCHEMA s TO r1;",
    );

    assert_eq!(database.table_grants().count(), 4);
    assert!(!database.has_column_grants());
}

#[test]
fn a_column_grant_stays_out_of_the_table_store() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT REFERENCES (id), SELECT (b) ON a TO r1;",
    );

    assert!(!database.has_table_grants());
    let grant = database.column_grants().next().expect("the column grant");
    let columns: Vec<_> = grant
        .columns(table(&database, "a"), &database)
        .expect("held")
        .map(ColumnLike::column_name)
        .collect();
    assert_eq!(columns, ["id", "b"]);
}

#[test]
fn revoking_one_part_of_a_mixed_grant_leaves_the_other() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b) ON a TO r1;
         REVOKE SELECT ON a FROM r1;",
    );
    assert_eq!(table_privileges(&database, "r1"), [false, false, false]);
    assert_eq!(rendered(database.column_grants()), ["GRANT UPDATE (b) ON a TO r1"]);

    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT, UPDATE (b) ON a TO r1;
         REVOKE UPDATE (b) ON a FROM r1;",
    );
    assert_eq!(table_privileges(&database, "r1"), [true, false, false]);
    assert!(!database.has_column_grants());
}

#[test]
fn a_table_revoke_takes_the_column_grants_of_that_privilege() {
    let database = parse(
        "CREATE ROLE r1;
         CREATE TABLE a (id int, b text);
         GRANT SELECT (id), UPDATE (b) ON a TO r1;
         REVOKE UPDATE ON a FROM r1;",
    );

    let remaining: Vec<_> = database
        .column_grants()
        .flat_map(|grant| grant.privileges(&database))
        .map(|action| matches!(action, Action::Select { .. }))
        .collect();
    assert_eq!(remaining, [true]);
}

fn all_columns_database(statements: &str) -> ParserDB {
    parse(&format!("CREATE ROLE r1; CREATE TABLE a (id int, b int); {statements}"))
}

fn column_actions(database: &ParserDB) -> Vec<String> {
    database
        .column_grants()
        .flat_map(|grant| grant.privileges(database))
        .map(ToString::to_string)
        .collect()
}

#[test]
fn all_columns_grants_only_the_four_column_privileges() {
    for (columns, expected_columns) in
        [("id", vec!["id"]), ("id, b", vec!["id", "b"]), ("id, id", vec!["id"])]
    {
        for keyword in ["ALL", "ALL PRIVILEGES"] {
            let database =
                all_columns_database(&format!("GRANT {keyword} ({columns}) ON a TO r1;"));
            assert_eq!(table_privileges(&database, "r1"), [false; 3]);
            assert!(!database.has_table_grants());
            assert_eq!(
                column_actions(&database),
                ["SELECT", "INSERT", "UPDATE", "REFERENCES"]
                    .map(|action| format!("{action} ({columns})"))
            );
            let grant = database.column_grants().next().expect("column grant");
            assert!(!grant.is_all_privileges());
            let held: Vec<_> = grant
                .columns(table(&database, "a"), &database)
                .expect("columns resolve")
                .map(ColumnLike::column_name)
                .collect();
            assert_eq!(held, expected_columns);
        }
    }
}

#[test]
fn all_columns_with_grant_option_stays_column_scoped() {
    let database = all_columns_database("GRANT ALL (id) ON a TO r1 WITH GRANT OPTION;");
    assert_eq!(table_privileges(&database, "r1"), [false; 3]);
    assert_eq!(
        rendered(database.column_grants()),
        [
            "GRANT SELECT (id), INSERT (id), UPDATE (id), REFERENCES (id) ON a TO r1 WITH GRANT OPTION"
        ]
    );
}

#[test]
fn all_columns_on_all_tables_reaches_each_tables_columns() {
    let database = all_columns_database(
        "CREATE TABLE c (id int, b int);
         GRANT ALL (id) ON ALL TABLES IN SCHEMA public TO r1;",
    );
    assert!(!database.has_table_grants());
    for name in ["a", "c"] {
        let relation = table(&database, name);
        let grant = database
            .column_grants()
            .find(|grant| grant.table(&database) == Some(relation))
            .expect("table has a column grant");
        assert_eq!(grant.privileges(&database).count(), 4);
        let columns: Vec<_> = grant
            .columns(relation, &database)
            .expect("columns resolve")
            .map(ColumnLike::column_name)
            .collect();
        assert_eq!(columns, ["id"]);
    }
    assert_eq!(database.column_grants().count(), 2);
}

#[test]
fn all_columns_on_views_and_foreign_tables_stays_column_scoped() {
    for definition in [
        "CREATE VIEW relation AS SELECT id, b FROM a;",
        "CREATE FOREIGN TABLE relation (id int, b int) SERVER remote;",
    ] {
        let database =
            all_columns_database(&format!("{definition} GRANT ALL (id) ON relation TO r1;"));
        assert!(!database.has_table_grants());
        assert_eq!(
            column_actions(&database),
            ["SELECT (id)", "INSERT (id)", "UPDATE (id)", "REFERENCES (id)"]
        );
    }
}

#[test]
fn all_columns_validates_unknown_columns_for_grants_and_revokes() {
    for statement in [
        "GRANT ALL (missing) ON a TO r1;",
        "REVOKE ALL (missing) ON a FROM r1;",
        "CREATE TABLE c (missing int); GRANT ALL (missing) ON c, a TO r1;",
    ] {
        let error = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE ROLE r1; CREATE TABLE a (id int); {statement}"
        ))
        .expect_err("unknown column");
        let postgres = error.postgres_error().expect("PostgreSQL error");
        assert_eq!(postgres.sqlstate().to_string(), "42703");
        assert_eq!(postgres.message(), "column \"missing\" of relation \"a\" does not exist");
    }
}

#[test]
fn revoking_all_columns_leaves_table_privileges() {
    let database = all_columns_database("GRANT ALL ON a TO r1; REVOKE ALL (id) ON a FROM r1;");
    assert_eq!(table_privileges(&database, "r1"), [true; 3]);
    assert!(!database.has_column_grants());
}

#[test]
fn revoking_all_columns_preserves_other_columns() {
    let database = all_columns_database(
        "GRANT SELECT (id, b), UPDATE (id) ON a TO r1;
         REVOKE ALL (id) ON a FROM r1;",
    );
    assert_eq!(column_actions(&database), ["SELECT (b)"]);
}

#[test]
fn revoking_select_from_all_columns_preserves_other_privileges() {
    let database =
        all_columns_database("GRANT ALL (id) ON a TO r1; REVOKE SELECT (id) ON a FROM r1;");
    assert_eq!(column_actions(&database), ["INSERT (id)", "UPDATE (id)", "REFERENCES (id)"]);
    assert!(!database.has_table_grants());
}

#[test]
fn revoking_all_table_privileges_removes_all_column_privileges() {
    let granted = all_columns_database("GRANT ALL (id) ON a TO r1;");
    assert_eq!(column_actions(&granted).len(), 4);
    let database = all_columns_database("GRANT ALL (id) ON a TO r1; REVOKE ALL ON a FROM r1;");
    assert!(!database.has_column_grants());
    assert!(!database.has_table_grants());
}

#[test]
fn revoking_all_column_grant_options_preserves_privileges() {
    let database = all_columns_database(
        "GRANT ALL (id) ON a TO r1 WITH GRANT OPTION;
         REVOKE GRANT OPTION FOR ALL (id) ON a FROM r1;",
    );
    assert_eq!(
        rendered(database.column_grants()),
        ["GRANT SELECT (id), INSERT (id), UPDATE (id), REFERENCES (id) ON a TO r1"]
    );
    assert!(!database.has_table_grants());
}

#[test]
fn all_columns_is_rejected_for_function_grants_and_revokes() {
    for statement in
        ["GRANT ALL (id) ON FUNCTION f() TO r1;", "REVOKE ALL (id) ON FUNCTION f() FROM r1;"]
    {
        let error = ParserDB::parse::<PostgreSqlDialect>(&format!(
            "CREATE ROLE r1; CREATE FUNCTION f() RETURNS int LANGUAGE SQL AS 'SELECT 1';
             {statement}"
        ))
        .expect_err("function column privileges");
        let postgres = error.postgres_error().expect("PostgreSQL error");
        assert_eq!(postgres.sqlstate().to_string(), "0LP01");
        assert_eq!(postgres.message(), "column privileges are only valid for relations");
    }
}

#[test]
fn all_columns_is_rejected_for_default_privileges() {
    let error = ParserDB::parse::<PostgreSqlDialect>(
        "CREATE ROLE r1; ALTER DEFAULT PRIVILEGES GRANT ALL (id) ON TABLES TO r1;",
    )
    .expect_err("default column privileges");
    assert_eq!(error.postgres_error().expect("PostgreSQL error").sqlstate().to_string(), "0LP01");
}
