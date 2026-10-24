// The names a table declares for its columns. Included textually by the tests
// that look them up.

/// The names of the columns `table_name` declares, in declaration order.
fn column_names(database: &ParserDB, table_name: &str) -> Vec<String> {
    database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .columns(database)
        .expect("table is in this database")
        .map(|column| column.column_name().to_owned())
        .collect()
}
