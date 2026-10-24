// Whether a column of a table accepts nothing. Included textually by the
// tests that ask the same question.

/// Answers whether `column_name` of `table_name` accepts nothing.
fn accepts_nothing(database: &ParserDB, table_name: &str, column_name: &str) -> bool {
    !database
        .table_by_target(TargetName::new(table_name, false), IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("table exists")
        .column(column_name, database, IdentifierCase::AsWritten)
        .expect("lookup succeeds")
        .expect("column exists")
        .is_nullable(database)
        .expect("table is in this database")
}
