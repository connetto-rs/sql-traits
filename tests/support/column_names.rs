//! The names a table declares for its columns.
#![expect(clippy::expect_used, reason = "a test helper fails loudly on a broken premise")]

use sql_traits::prelude::*;

/// The names of the columns `table_name` declares, in declaration order.
pub(crate) fn column_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| column.column_name().to_owned())
        .collect()
}
