// The declared column types and the schema fingerprint of a one-table
// database built from `ddl`. Included textually, so it names every sqlparser
// path in full and leaves the including file's imports alone.

/// Builds a one-table database from `ddl` and returns the declared type of
/// every one of its columns, in declaration order.
fn column_types(ddl: &str) -> Vec<String> {
    let statements =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, ddl)
            .expect("SQL parses");
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
fn fingerprint(ddl: &str) -> SchemaFingerprint {
    let statements =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, ddl)
            .expect("parses");
    let database =
        ParserDB::from_statements(statements, "test".to_string()).expect("schema builds");
    let table = database.tables().next().expect("input declares a table");
    table.schema_fingerprint(&database).expect("fingerprint computes")
}
