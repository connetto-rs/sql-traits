// Parses one query out of a single statement. Included textually, so it names
// every sqlparser path in full and leaves the including file's imports alone.

fn query_with(dialect: &dyn sqlparser::dialect::Dialect, sql: &str) -> sqlparser::ast::Query {
    let mut statements = sqlparser::parser::Parser::parse_sql(dialect, sql)
        .unwrap_or_else(|error| panic!("statement `{sql}` parses: {error}"));
    match statements.pop().expect("one statement") {
        sqlparser::ast::Statement::Query(query) => *query,
        other => panic!("expected a query, got {other:?}"),
    }
}
