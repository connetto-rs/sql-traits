//! The declared column types and the schema fingerprint of a one-table
//! database built from `ddl`.
#![expect(clippy::expect_used, reason = "a test helper fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

/// Builds a one-table database from `ddl` and returns the declared type of
/// every one of its columns, in declaration order.
pub(crate) fn column_types(ddl: &str) -> Vec<String> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, ddl).expect("SQL parses");
    let database =
        ParserDB::from_statements(statements, "test".to_string()).expect("schema builds");
    let table = database.tables().next().expect("input declares a table");

    table
        .columns(&database)
        .expect("table belongs to the database")
        .map(|column| column.data_type(&database).name(&database).into_owned())
        .collect()
}

/// Builds a one-table database from `ddl` and answers its schema fingerprint.
pub(crate) fn fingerprint(ddl: &str) -> SchemaFingerprint {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, ddl).expect("parses");
    let database =
        ParserDB::from_statements(statements, "test".to_string()).expect("schema builds");
    let table = database.tables().next().expect("input declares a table");
    table.schema_fingerprint(&database).expect("fingerprint computes")
}
