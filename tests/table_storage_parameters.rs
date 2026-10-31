//! Tests that a table's storage parameters follow every statement that sets
//! them, as `pg_class.reloptions` reports them.
#![allow(clippy::expect_used, clippy::panic)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::{
    ast::CreateTable,
    dialect::{GenericDialect, MySqlDialect, PostgreSqlDialect},
};

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

fn db(sql: &str) -> ParserDB {
    parse(sql).unwrap_or_else(|error| panic!("{sql} was refused: {error:?}"))
}

fn table<'db>(db: &'db ParserDB, name: &str) -> &'db CreateTable {
    db.resolve_target_table(TargetName::new(name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the name resolves")
}

fn parameters_of(database: &ParserDB, name: &str) -> Vec<(String, String)> {
    table(database, name)
        .storage_parameters()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

fn parameters_after(sql: &str, name: &str) -> Vec<(String, String)> {
    parameters_of(&db(sql), name)
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected.iter().map(|&(name, value)| (name.to_owned(), value.to_owned())).collect()
}

#[test]
fn create_table_writes_parameters_under_their_stored_names() {
    assert_eq!(
        parameters_after(
            "CREATE TABLE staging (id INT) WITH (
                AUTOVACUUM_ENABLED = FALSE,
                FillFactor = 70,
                \"vacuum_truncate\" = 'OFF',
                vacuum_index_cleanup = AUTO,
                autovacuum_vacuum_scale_factor = .5,
                log_autovacuum_min_duration = -1
            );",
            "staging"
        ),
        pairs(&[
            ("autovacuum_enabled", "false"),
            ("fillfactor", "70"),
            ("vacuum_truncate", "OFF"),
            ("vacuum_index_cleanup", "auto"),
            ("autovacuum_vacuum_scale_factor", ".5"),
            ("log_autovacuum_min_duration", "-1"),
        ])
    );
    assert_eq!(
        parameters_after("CREATE TABLE t (id INT) WITH (vacuum_index_cleanup = \"Auto\");", "t"),
        pairs(&[("vacuum_index_cleanup", "Auto")])
    );
    assert_eq!(
        parameters_after("CREATE TABLE t (id INT) WITH (fillfactor = $$70$$);", "t"),
        pairs(&[("fillfactor", "70")])
    );
    assert_eq!(
        parameters_after(
            "CREATE TABLE t (id INT) WITH (vacuum_index_cleanup = E'\\x61uto', vacuum_truncate = U&'o\\0066f');",
            "t"
        ),
        pairs(&[("vacuum_index_cleanup", "auto"), ("vacuum_truncate", "off")])
    );
}

#[test]
fn a_table_without_a_with_clause_has_no_parameters() {
    assert_eq!(parameters_after("CREATE TABLE t (id INT);", "t"), pairs(&[]));
    let mysql = ParserDB::parse::<MySqlDialect>("CREATE TABLE t (id INT) ENGINE = InnoDB;")
        .expect("MySQL table options are accepted");
    assert_eq!(parameters_of(&mysql, "t"), pairs(&[]));
}

#[test]
fn an_option_named_without_a_value_is_true() {
    // Measured: PostgreSQL stores `autovacuum_enabled=true` for either form.
    assert_eq!(
        parameters_after("CREATE TABLE t (id INT) WITH (autovacuum_enabled);", "t"),
        pairs(&[("autovacuum_enabled", "true")])
    );
    assert_eq!(
        parameters_after(
            "CREATE TABLE t (id INT) WITH (autovacuum_enabled = false, fillfactor = 70);
             ALTER TABLE t SET (Autovacuum_Enabled);",
            "t"
        ),
        pairs(&[("fillfactor", "70"), ("autovacuum_enabled", "true")])
    );
    // The generic dialect reads MSSQL's `HEAP` into the same bare option.
    let database = ParserDB::parse::<GenericDialect>(
        "CREATE TABLE t (id INT) WITH (HEAP, fillfactor = 70);
         CREATE TABLE s (id INT);
         ALTER TABLE s SET (HEAP, fillfactor = 70);",
    )
    .expect("the generic dialect reads HEAP as a bare option");
    assert_eq!(parameters_of(&database, "t"), pairs(&[("heap", "true"), ("fillfactor", "70")]));
    assert_eq!(parameters_of(&database, "s"), pairs(&[("heap", "true"), ("fillfactor", "70")]));
}

#[test]
fn alter_table_reset_removes_the_parameters_it_names() {
    let schema = "CREATE TABLE t (id INT) WITH (fillfactor = 70, autovacuum_enabled = false);";
    let after = |reset: &str| parameters_after(&format!("{schema} {reset}"), "t");
    assert_eq!(
        after("ALTER TABLE t RESET (FillFactor);"),
        pairs(&[("autovacuum_enabled", "false")])
    );
    assert_eq!(after("ALTER TABLE t RESET (fillfactor, autovacuum_enabled);"), pairs(&[]));
    // Measured: each of these is accepted and resets nothing.
    for untouched in [
        "ALTER TABLE t RESET (\"FillFactor\");",
        "ALTER TABLE t RESET (toast.fillfactor);",
        "ALTER TABLE t RESET (vacuum_truncate);",
        "ALTER TABLE t RESET (no_such_parameter);",
    ] {
        assert_eq!(
            after(untouched),
            pairs(&[("fillfactor", "70"), ("autovacuum_enabled", "false")]),
            "{untouched}"
        );
    }
    assert_eq!(
        parameters_after(
            "CREATE TABLE t (id INT) WITH (autovacuum_enabled);
             ALTER TABLE t RESET (autovacuum_enabled);",
            "t"
        ),
        pairs(&[])
    );
}

#[test]
fn set_and_reset_in_one_statement_apply_in_order() {
    assert_eq!(
        parameters_after(
            "CREATE TABLE t (id INT); ALTER TABLE t SET (fillfactor = 70), RESET (fillfactor);",
            "t"
        ),
        pairs(&[])
    );
    assert_eq!(
        parameters_after(
            "CREATE TABLE t (id INT) WITH (fillfactor = 70, autovacuum_enabled = false);
             ALTER TABLE t RESET (fillfactor), SET (fillfactor = 50);",
            "t"
        ),
        pairs(&[("autovacuum_enabled", "false"), ("fillfactor", "50")])
    );
}

#[test]
fn alter_table_set_is_reported_on_a_table_holding_another_dialects_options() {
    let error = ParserDB::parse::<MySqlDialect>(
        "CREATE TABLE t (id INT) ENGINE = InnoDB;
         ALTER TABLE t SET (fillfactor = 70);",
    )
    .expect_err("the parameters have nowhere to go");
    assert!(
        matches!(&error, Error::UnsupportedAlterTableOperation { table_name, operation }
            if table_name == "t" && operation == "SET (fillfactor = 70)"),
        "got {error:?}"
    );
}

#[test]
fn create_table_as_writes_parameters() {
    assert_eq!(
        parameters_after("CREATE TABLE r WITH (fillfactor = 70) AS SELECT 1 AS id;", "r"),
        pairs(&[("fillfactor", "70")])
    );
}

#[test]
fn alter_table_set_replaces_named_parameters_and_appends_the_rest() {
    assert_eq!(
        parameters_after(
            "CREATE TABLE e (id INT) WITH (autovacuum_enabled = false, fillfactor = 70);
             ALTER TABLE e SET (AUTOVACUUM_ENABLED = true, toast_tuple_target = 256);",
            "e"
        ),
        pairs(&[
            ("fillfactor", "70"),
            ("autovacuum_enabled", "true"),
            ("toast_tuple_target", "256")
        ])
    );
    assert_eq!(
        parameters_after(
            "CREATE TABLE s (id INT);
             ALTER TABLE s SET (fillfactor = 70), SET (autovacuum_enabled = false);
             ALTER TABLE ONLY s SET (fillfactor = 80);",
            "s"
        ),
        pairs(&[("autovacuum_enabled", "false"), ("fillfactor", "80")])
    );
}

#[test]
fn a_parameter_is_looked_up_by_its_stored_name() {
    let database = db("CREATE TABLE staging (id INT) WITH (fillfactor = 70);
         ALTER TABLE staging SET (Autovacuum_Enabled = false);");
    let staging = table(&database, "staging");
    assert_eq!(staging.storage_parameter("autovacuum_enabled").as_deref(), Some("false"));
    assert_eq!(staging.storage_parameter("fillfactor").as_deref(), Some("70"));
    assert_eq!(staging.storage_parameter("toast_tuple_target"), None);
    assert_eq!(
        <&CreateTable as TableLike>::storage_parameter(&staging, "fillfactor").as_deref(),
        Some("70")
    );
}

#[test]
fn a_reference_to_a_table_reports_its_parameters() {
    let database = db("CREATE TABLE staging (id INT) WITH (fillfactor = 70);");
    let staging = table(&database, "staging");
    let through_reference: Vec<_> =
        <&CreateTable as TableLike>::storage_parameters(&staging).collect();
    assert_eq!(through_reference, [("fillfactor".into(), "70".into())]);
}

#[test]
fn parameters_survive_the_statements_that_rebuild_a_table() {
    let database = db("CREATE TABLE staging (id INT) WITH (fillfactor = 70);
         ALTER TABLE staging SET (autovacuum_enabled = false);
         ALTER TABLE staging ADD COLUMN note TEXT;
         ALTER TABLE staging ADD CONSTRAINT positive CHECK (id > 0);
         ALTER TABLE staging SET UNLOGGED;
         ALTER TABLE staging RENAME TO landing;");
    assert_eq!(
        parameters_of(&database, "landing"),
        pairs(&[("fillfactor", "70"), ("autovacuum_enabled", "false")])
    );
}

#[test]
fn a_table_takes_no_parameters_from_the_tables_it_derives_from() {
    let database = db("CREATE TABLE base (id INT) WITH (fillfactor = 70);
         CREATE TABLE copy (LIKE base);
         CREATE TABLE child () INHERITS (base);
         ALTER TABLE base SET (autovacuum_enabled = false);
         CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         CREATE TABLE part PARTITION OF root FOR VALUES FROM (0) TO (10) WITH (fillfactor = 60);");
    assert_eq!(
        parameters_of(&database, "base"),
        pairs(&[("fillfactor", "70"), ("autovacuum_enabled", "false")])
    );
    assert_eq!(parameters_of(&database, "copy"), pairs(&[]));
    assert_eq!(parameters_of(&database, "child"), pairs(&[]));
    assert_eq!(parameters_of(&database, "root"), pairs(&[]));
    assert_eq!(parameters_of(&database, "part"), pairs(&[("fillfactor", "60")]));
}

#[test]
fn alter_table_set_names_a_table_that_exists() {
    let error = parse("ALTER TABLE missing SET (fillfactor = 70);")
        .expect_err("an absent table is refused");
    assert!(
        matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "missing"),
        "got {error:?}"
    );
    parse("ALTER TABLE IF EXISTS missing SET (fillfactor = 70);")
        .expect("IF EXISTS skips an absent table");
}

#[test]
fn alter_table_reset_names_a_table_that_exists() {
    let error =
        parse("ALTER TABLE missing RESET (fillfactor);").expect_err("an absent table is refused");
    assert!(
        matches!(&error, Error::AlterTableNotFound { table_name } if table_name == "missing"),
        "got {error:?}"
    );
    parse("ALTER TABLE IF EXISTS missing RESET (fillfactor);")
        .expect("IF EXISTS skips an absent table");
}

#[test]
fn a_partitioned_table_takes_no_parameters() {
    let error = parse(
        "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
         ALTER TABLE root SET (fillfactor = 70);",
    )
    .expect_err("PostgreSQL refuses storage parameters on a partitioned table");
    assert!(
        matches!(&error, Error::StorageParametersOnPartitionedTable { table_name } if table_name == "root"),
        "got {error:?}"
    );
}

#[test]
fn a_partitioned_table_takes_a_reset() {
    // Measured: PostgreSQL refuses `SET` on a partitioned table and accepts
    // `RESET`, which has nothing to remove.
    assert_eq!(
        parameters_after(
            "CREATE TABLE root (id INT) PARTITION BY RANGE (id);
             ALTER TABLE root RESET (fillfactor);",
            "root"
        ),
        pairs(&[])
    );
}
