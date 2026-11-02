//! Integration tests for ingestion that honours transaction control.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sql_traits::{
    errors::Error,
    structs::{ParserDB, ParserDBIngestor},
    traits::{ColumnLike, DatabaseLike, TableLike},
};
use sqlparser::{
    ast::Statement,
    dialect::{
        BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect, HiveDialect,
        MsSqlDialect, MySqlDialect, OracleDialect, PostgreSqlDialect, RedshiftSqlDialect,
        SQLiteDialect, SnowflakeDialect, SparkSqlDialect, TeradataDialect,
    },
    parser::Parser,
};

fn statements<D: Dialect + Default>(sql: &str) -> Vec<Statement> {
    Parser::parse_sql(&D::default(), sql).expect("SQL parses")
}

fn describe(db: &ParserDB) -> Vec<String> {
    db.tables()
        .map(|table| {
            let columns: Vec<&str> =
                table.columns(db).expect("columns resolve").map(ColumnLike::column_name).collect();
            format!("{}({})", table.table_name(), columns.join(","))
        })
        .collect()
}

fn tables_in<D: Dialect + Default + 'static>(sql: &str) -> Result<Vec<String>, Error> {
    let db = statements::<D>(sql)
        .into_iter()
        .try_fold(
            ParserDBIngestor::new::<D>("probe".to_owned()),
            ParserDBIngestor::apply_statement,
        )?
        .finish();
    Ok(describe(&db))
}

fn postgres(sql: &str) -> Result<Vec<String>, Error> {
    tables_in::<PostgreSqlDialect>(sql)
}

fn listed(tables: &[&str]) -> Vec<String> {
    tables.iter().map(ToString::to_string).collect()
}

/// The name of the error variant ingestion stopped on, or `ok` with the tables.
fn outcome<D: Dialect + Default + 'static>(sql: &str) -> String {
    match tables_in::<D>(sql) {
        Ok(tables) => format!("ok {tables:?}"),
        Err(Error::UnsupportedTransactionControl { .. }) => "unsupported".to_owned(),
        Err(Error::NoOpenTransaction { .. }) => "no open transaction".to_owned(),
        Err(Error::TransactionAlreadyOpen { .. }) => "already open".to_owned(),
        Err(Error::SavepointNotFound { .. }) => "savepoint not found".to_owned(),
        Err(Error::DefinitionInTransaction { .. }) => "definition in transaction".to_owned(),
        Err(Error::StatementAfterDefinition { .. }) => "after definition".to_owned(),
        Err(other) => panic!("unexpected error for {sql}: {other}"),
    }
}

#[test]
fn rolled_back_ddl_is_discarded() {
    assert_eq!(
        postgres("CREATE TABLE t (id INT); BEGIN; DROP TABLE t; ROLLBACK;").unwrap(),
        listed(&["t(id)"])
    );
    assert_eq!(
        postgres("BEGIN; CREATE TABLE t (id INT); ROLLBACK; CREATE TABLE t (id INT);").unwrap(),
        listed(&["t(id)"])
    );
    assert_eq!(
        postgres(
            "CREATE TABLE t (a INT); START TRANSACTION; ALTER TABLE t RENAME COLUMN a TO b; ABORT;"
        )
        .unwrap(),
        listed(&["t(a)"])
    );
    assert_eq!(
        postgres(
            "CREATE TABLE t (id INT); BEGIN; SAVEPOINT s; DROP TABLE t; ROLLBACK TO SAVEPOINT s; COMMIT;"
        )
        .unwrap(),
        listed(&["t(id)"])
    );
}

#[test]
fn one_shot_construction_honours_rollback() {
    let db = ParserDB::from_statements(
        statements::<PostgreSqlDialect>("CREATE TABLE t (id INT); BEGIN; DROP TABLE t; ROLLBACK;"),
        "probe".to_owned(),
    )
    .expect("statements apply");
    assert_eq!(describe(&db), listed(&["t(id)"]));
    let db = ParserDB::parse::<PostgreSqlDialect>("BEGIN; CREATE TABLE t (id INT); ROLLBACK;")
        .expect("statements apply");
    assert_eq!(describe(&db), listed(&[]));
}

#[test]
fn committed_ddl_is_kept() {
    assert_eq!(
        postgres("BEGIN; CREATE TABLE t (id INT); COMMIT; ROLLBACK;").unwrap(),
        listed(&["t(id)"])
    );
    assert_eq!(postgres("BEGIN; CREATE TABLE t (id INT); END;").unwrap(), listed(&["t(id)"]));
}

#[test]
fn rollback_restores_collations_and_search_path() {
    assert!(
        postgres(
            "BEGIN; CREATE COLLATION ci FROM \"C\"; ROLLBACK; CREATE COLLATION ci FROM \"C\";"
        )
        .is_ok()
    );
    assert_eq!(
        postgres(
            "CREATE SCHEMA s; BEGIN; SET search_path TO s; ROLLBACK; CREATE TABLE t (id INT); \
             CREATE TABLE public.t (id INT);"
        )
        .err()
        .map(|error| error.to_string().contains("already")),
        Some(true),
        "the rolled back search path no longer sends the first table to `s`"
    );
}

#[test]
fn released_savepoint_merges_into_its_parent() {
    assert_eq!(
        postgres("BEGIN; SAVEPOINT a; CREATE TABLE x (id INT); RELEASE SAVEPOINT a; ROLLBACK;")
            .unwrap(),
        listed(&[])
    );
    assert_eq!(
        postgres("BEGIN; SAVEPOINT a; CREATE TABLE x (id INT); RELEASE a; COMMIT;").unwrap(),
        listed(&["x(id)"])
    );
    assert_eq!(
        postgres(
            "BEGIN; SAVEPOINT a; SAVEPOINT b; CREATE TABLE x (id INT); RELEASE a; \
             ROLLBACK TO b;"
        )
        .err()
        .map(|error| matches!(error, Error::SavepointNotFound { .. })),
        Some(true),
        "releasing a savepoint releases every later one"
    );
}

#[test]
fn rollback_to_savepoint_keeps_the_savepoint() {
    assert_eq!(
        postgres(
            "BEGIN; SAVEPOINT s; CREATE TABLE a (id INT); ROLLBACK TO s; CREATE TABLE b (id INT); \
             ROLLBACK TO s; CREATE TABLE c (id INT); COMMIT;"
        )
        .unwrap(),
        listed(&["c(id)"])
    );
}

#[test]
fn repeated_savepoint_name_shadows_the_earlier_one() {
    let sql = "BEGIN; SAVEPOINT s; CREATE TABLE a (id INT); SAVEPOINT s; CREATE TABLE b (id INT); \
               ROLLBACK TO s;";
    assert_eq!(postgres(&format!("{sql} COMMIT;")).unwrap(), listed(&["a(id)"]));
    assert_eq!(postgres(&format!("{sql} RELEASE s; ROLLBACK TO s; COMMIT;")).unwrap(), listed(&[]));
}

#[test]
fn chained_completion_opens_the_next_transaction() {
    assert_eq!(
        postgres(
            "BEGIN; CREATE TABLE a (id INT); COMMIT AND CHAIN; CREATE TABLE b (id INT); ROLLBACK;"
        )
        .unwrap(),
        listed(&["a(id)"])
    );
    assert_eq!(
        postgres(
            "BEGIN; CREATE TABLE a (id INT); ROLLBACK AND CHAIN; CREATE TABLE b (id INT); \
             SAVEPOINT s; ROLLBACK;"
        )
        .unwrap(),
        listed(&[])
    );
}

#[test]
fn snapshot_inside_a_transaction_shows_pending_changes() {
    let mut input = ParserDBIngestor::new::<PostgreSqlDialect>("probe".to_owned());
    for statement in
        statements::<PostgreSqlDialect>("CREATE TABLE t (id INT); BEGIN; DROP TABLE t;")
    {
        input = input.apply_statement(statement).expect("statement applies");
    }
    assert_eq!(describe(&input.snapshot()), listed(&[]));
    for statement in statements::<PostgreSqlDialect>("CREATE TABLE u (id INT);") {
        input = input.apply_statement(statement).expect("statement applies");
    }
    assert_eq!(describe(&input.snapshot()), listed(&["u(id)"]));
    for statement in statements::<PostgreSqlDialect>("ROLLBACK;") {
        input = input.apply_statement(statement).expect("statement applies");
    }
    assert_eq!(describe(&input.finish()), listed(&["t(id)"]));
}

#[test]
fn finishing_discards_an_open_transaction() {
    assert_eq!(
        postgres("CREATE TABLE t (id INT); BEGIN; DROP TABLE t;").unwrap(),
        listed(&["t(id)"])
    );
    assert_eq!(
        postgres(
            "BEGIN; CREATE TABLE a (id INT); COMMIT; BEGIN; SAVEPOINT s; CREATE TABLE b (id INT);"
        )
        .unwrap(),
        listed(&["a(id)"])
    );
}

#[test]
fn redundant_transaction_boundaries_are_ignored() {
    assert_eq!(
        postgres("BEGIN; CREATE TABLE a (id INT); BEGIN; CREATE TABLE b (id INT); ROLLBACK;")
            .unwrap(),
        listed(&[])
    );
    assert_eq!(
        postgres("COMMIT; ROLLBACK; CREATE TABLE a (id INT); ROLLBACK;").unwrap(),
        listed(&["a(id)"])
    );
}

#[test]
fn savepoint_control_outside_a_transaction_is_refused() {
    for sql in [
        "SAVEPOINT s;",
        "RELEASE SAVEPOINT s;",
        "ROLLBACK TO SAVEPOINT s;",
        "COMMIT AND CHAIN;",
        "ROLLBACK AND CHAIN;",
        "BEGIN; COMMIT; SAVEPOINT s;",
    ] {
        assert!(
            matches!(postgres(sql), Err(Error::NoOpenTransaction { .. })),
            "{sql} must be refused outside a transaction"
        );
    }
}

#[test]
fn unknown_savepoints_are_refused() {
    for sql in [
        "BEGIN; RELEASE SAVEPOINT s;",
        "BEGIN; ROLLBACK TO SAVEPOINT s;",
        "BEGIN; SAVEPOINT s; ROLLBACK TO SAVEPOINT s; RELEASE s; RELEASE s;",
    ] {
        assert!(
            matches!(postgres(sql), Err(Error::SavepointNotFound { .. })),
            "{sql} names a savepoint that does not exist"
        );
    }
}

#[test]
fn savepoint_names_compare_as_identifiers() {
    assert_eq!(
        postgres("BEGIN; SAVEPOINT Mark; CREATE TABLE t (id INT); ROLLBACK TO mark; COMMIT;")
            .unwrap(),
        listed(&[])
    );
    assert!(matches!(
        postgres("BEGIN; SAVEPOINT \"Mark\"; ROLLBACK TO mark;"),
        Err(Error::SavepointNotFound { .. })
    ));
}

#[test]
fn sqlite_savepoints_open_and_commit_transactions() {
    assert_eq!(outcome::<SQLiteDialect>("BEGIN; CREATE TABLE t (id INT); ROLLBACK;"), "ok []");
    assert_eq!(
        outcome::<SQLiteDialect>("SAVEPOINT s; CREATE TABLE t (id INT); RELEASE s; ROLLBACK;"),
        "no open transaction",
        "releasing the savepoint that opened the transaction commits it"
    );
    assert_eq!(
        outcome::<SQLiteDialect>("SAVEPOINT s; CREATE TABLE t (id INT); RELEASE s;"),
        "ok [\"t(id)\"]"
    );
    assert_eq!(
        outcome::<SQLiteDialect>("SAVEPOINT s; CREATE TABLE t (id INT); ROLLBACK;"),
        "ok []"
    );
    assert_eq!(
        outcome::<SQLiteDialect>(
            "BEGIN; SAVEPOINT s; CREATE TABLE t (id INT); RELEASE s; ROLLBACK;"
        ),
        "ok []"
    );
    assert_eq!(
        outcome::<SQLiteDialect>(
            "BEGIN; SAVEPOINT \"CaseX\"; CREATE TABLE t (id INT); ROLLBACK TO casex; COMMIT;"
        ),
        "ok []"
    );
}

#[test]
fn sqlite_and_duckdb_refuse_misplaced_boundaries() {
    for sql in ["BEGIN; BEGIN;", "SAVEPOINT s; BEGIN;"] {
        assert_eq!(outcome::<SQLiteDialect>(sql), "already open", "{sql}");
    }
    for sql in ["COMMIT;", "ROLLBACK;", "END;"] {
        assert_eq!(outcome::<SQLiteDialect>(sql), "no open transaction", "{sql}");
        assert_eq!(outcome::<DuckDbDialect>(sql), "no open transaction", "{sql}");
    }
    assert_eq!(outcome::<SQLiteDialect>("BEGIN; COMMIT AND CHAIN;"), "unsupported");
    assert_eq!(outcome::<DuckDbDialect>("BEGIN; BEGIN TRANSACTION;"), "already open");
    assert_eq!(outcome::<DuckDbDialect>("BEGIN; CREATE TABLE t (id INT); ABORT;"), "ok []");
    assert_eq!(outcome::<DuckDbDialect>("BEGIN; SAVEPOINT s;"), "unsupported");
}

#[test]
fn mysql_definitions_commit_the_open_transaction() {
    assert_eq!(
        outcome::<MySqlDialect>("BEGIN; CREATE TABLE t (id INT); ROLLBACK;"),
        "ok [\"t(id)\"]"
    );
    assert_eq!(
        outcome::<MySqlDialect>(
            "CREATE TABLE t (id INT); START TRANSACTION; DROP TABLE t; ROLLBACK;"
        ),
        "ok []"
    );
    assert_eq!(
        outcome::<MySqlDialect>(
            "BEGIN; SAVEPOINT s; CREATE TABLE t (id INT); RELEASE SAVEPOINT s;"
        ),
        "savepoint not found"
    );
    assert_eq!(
        outcome::<MySqlDialect>("BEGIN; SAVEPOINT s; BEGIN; RELEASE SAVEPOINT s;"),
        "savepoint not found"
    );
    assert_eq!(
        outcome::<MySqlDialect>(
            "CREATE TABLE t (id INT); BEGIN; SAVEPOINT s; CREATE INDEX i ON t (id); \
             RELEASE SAVEPOINT s;"
        ),
        "savepoint not found",
        "an index definition commits like a table definition"
    );
    assert_eq!(
        outcome::<MySqlDialect>("BEGIN; SAVEPOINT s; SELECT 1; RELEASE SAVEPOINT s; COMMIT;"),
        "ok []",
        "a query defines nothing, so the transaction stays open"
    );
}

#[test]
fn mysql_temporary_tables_neither_commit_nor_roll_back() {
    assert_eq!(
        outcome::<MySqlDialect>(
            "BEGIN; SAVEPOINT s; CREATE TEMPORARY TABLE t (id INT); ROLLBACK TO SAVEPOINT s; RELEASE SAVEPOINT s; ROLLBACK;"
        ),
        "ok [\"t(id)\"]"
    );
    assert_eq!(
        outcome::<MySqlDialect>(
            "CREATE TEMPORARY TABLE t (id INT); BEGIN; DROP TEMPORARY TABLE t; ROLLBACK;"
        ),
        "ok []"
    );
}

#[test]
fn mysql_savepoints_replace_and_vanish_outside_transactions() {
    assert_eq!(outcome::<MySqlDialect>("SAVEPOINT s; RELEASE SAVEPOINT s;"), "savepoint not found");
    assert_eq!(
        outcome::<MySqlDialect>(
            "BEGIN; SAVEPOINT d; SAVEPOINT d; RELEASE SAVEPOINT d; RELEASE SAVEPOINT d;"
        ),
        "savepoint not found"
    );
    assert_eq!(
        outcome::<MySqlDialect>("BEGIN; SAVEPOINT `CaseX`; RELEASE SAVEPOINT casex; COMMIT;"),
        "ok []"
    );
    assert_eq!(
        outcome::<MySqlDialect>("COMMIT; ROLLBACK; COMMIT AND CHAIN; ROLLBACK AND CHAIN;"),
        "ok []"
    );
}

#[test]
fn mssql_nested_transactions_end_at_the_outermost_commit() {
    assert_eq!(
        outcome::<MsSqlDialect>(
            "BEGIN TRANSACTION; BEGIN TRANSACTION; CREATE TABLE t (id INT); COMMIT; ROLLBACK;"
        ),
        "ok []"
    );
    assert_eq!(
        outcome::<MsSqlDialect>(
            "BEGIN TRANSACTION; BEGIN TRANSACTION; CREATE TABLE t (id INT); COMMIT; COMMIT;"
        ),
        "ok [\"t(id)\"]"
    );
    for sql in ["COMMIT;", "ROLLBACK;", "SAVEPOINT s;"] {
        assert_eq!(outcome::<MsSqlDialect>(sql), "no open transaction", "{sql}");
    }
    assert_eq!(
        outcome::<MsSqlDialect>(
            "BEGIN TRANSACTION; SAVEPOINT s; ROLLBACK TO SAVEPOINT s; ROLLBACK TO SAVEPOINT s;"
        ),
        "savepoint not found",
        "rolling back to a savepoint consumes it"
    );
    assert_eq!(
        outcome::<MsSqlDialect>("BEGIN TRANSACTION; SAVEPOINT s; RELEASE SAVEPOINT s;"),
        "unsupported"
    );
}

#[test]
fn oracle_definitions_erase_savepoints() {
    assert_eq!(outcome::<OracleDialect>("CREATE TABLE t (id INT); ROLLBACK;"), "ok [\"t(id)\"]");
    assert_eq!(
        outcome::<OracleDialect>("SAVEPOINT s; CREATE TABLE t (id INT); ROLLBACK TO SAVEPOINT s;"),
        "savepoint not found"
    );
    assert_eq!(
        outcome::<OracleDialect>(
            "SAVEPOINT s; ROLLBACK TO SAVEPOINT s; ROLLBACK TO SAVEPOINT s; COMMIT; COMMIT;"
        ),
        "ok []"
    );
    assert_eq!(
        outcome::<OracleDialect>("SAVEPOINT \"Other\"; ROLLBACK TO SAVEPOINT Other;"),
        "savepoint not found"
    );
    assert_eq!(outcome::<OracleDialect>("BEGIN;"), "unsupported", "Oracle has no `BEGIN`");
}

#[test]
fn snowflake_definitions_commit_and_nested_begin_is_ignored() {
    assert_eq!(
        outcome::<SnowflakeDialect>("BEGIN TRANSACTION; CREATE TABLE t (id INT); ROLLBACK;"),
        "ok [\"t(id)\"]"
    );
    assert_eq!(
        outcome::<SnowflakeDialect>(
            "CREATE TABLE t (id INT); BEGIN TRANSACTION; BEGIN TRANSACTION; COMMIT; COMMIT;"
        ),
        "ok [\"t(id)\"]"
    );
    assert_eq!(outcome::<SnowflakeDialect>("BEGIN TRANSACTION; SAVEPOINT s;"), "unsupported");
}

#[test]
fn bigquery_and_databricks_refuse_definitions_inside_transactions() {
    assert_eq!(
        outcome::<BigQueryDialect>("BEGIN TRANSACTION; CREATE TABLE t (id INT64);"),
        "definition in transaction"
    );
    assert_eq!(
        outcome::<BigQueryDialect>(
            "BEGIN TRANSACTION; CREATE TEMP TABLE t (id INT64); ROLLBACK TRANSACTION;"
        ),
        "ok []"
    );
    assert_eq!(outcome::<BigQueryDialect>("COMMIT TRANSACTION;"), "no open transaction");
    assert_eq!(outcome::<BigQueryDialect>("ROLLBACK TRANSACTION;"), "no open transaction");
    assert_eq!(
        outcome::<DatabricksDialect>("BEGIN TRANSACTION; CREATE TABLE t (id INT);"),
        "definition in transaction"
    );
    assert_eq!(outcome::<DatabricksDialect>("COMMIT;"), "no open transaction");
    assert_eq!(outcome::<DatabricksDialect>("ROLLBACK;"), "ok []");
}

#[test]
fn teradata_definitions_end_the_transaction() {
    assert_eq!(
        outcome::<TeradataDialect>("BEGIN TRANSACTION; CREATE TABLE t (id INT); END TRANSACTION;"),
        "ok [\"t(id)\"]"
    );
    assert_eq!(
        outcome::<TeradataDialect>("BEGIN TRANSACTION; CREATE TABLE t (id INT); ROLLBACK;"),
        "ok []"
    );
    assert_eq!(
        outcome::<TeradataDialect>(
            "BEGIN TRANSACTION; CREATE TABLE t (id INT); CREATE TABLE u (id INT);"
        ),
        "after definition"
    );
    assert_eq!(
        outcome::<TeradataDialect>(
            "BEGIN TRANSACTION; BEGIN TRANSACTION; END TRANSACTION; CREATE TABLE t (id INT); ROLLBACK;"
        ),
        "ok []"
    );
    assert_eq!(outcome::<TeradataDialect>("END TRANSACTION;"), "no open transaction");
    assert_eq!(outcome::<TeradataDialect>("BEGIN TRANSACTION; COMMIT;"), "unsupported");
}

#[test]
fn redshift_has_no_savepoints() {
    assert_eq!(outcome::<RedshiftSqlDialect>("BEGIN; CREATE TABLE t (id INT); ROLLBACK;"), "ok []");
    assert_eq!(outcome::<RedshiftSqlDialect>("BEGIN; SAVEPOINT s;"), "unsupported");
}

#[test]
fn engines_without_transactions_refuse_transaction_control() {
    assert_eq!(outcome::<ClickHouseDialect>("BEGIN TRANSACTION;"), "unsupported");
    assert_eq!(outcome::<ClickHouseDialect>("COMMIT;"), "unsupported");
    assert_eq!(outcome::<HiveDialect>("ROLLBACK;"), "unsupported");
    assert_eq!(outcome::<SparkSqlDialect>("COMMIT;"), "unsupported");
}
