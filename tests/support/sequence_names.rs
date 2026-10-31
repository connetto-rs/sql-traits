// The sequences a database holds, by name. Included textually by the tests
// that list them.

/// Every sequence the database holds, as `schema.name` with `public` for one
/// stored without a schema, sorted.
fn sequence_names(database: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> = database
        .sequences()
        .map(|sequence| {
            format!("{}.{}", sequence.sequence_schema().unwrap_or("public"), sequence.sequence_name())
        })
        .collect();
    names.sort();
    names
}
