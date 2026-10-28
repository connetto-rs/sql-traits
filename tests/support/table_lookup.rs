// Looks up a table by name and optional schema. Included textually by the
// tests that read one.

/// The table `name` in `schema`, or on the search path when `schema` is `None`.
fn table<'db>(
    database: &'db ParserDB,
    schema: Option<&str>,
    name: &str,
) -> &'db <ParserDB as DatabaseLike>::Table {
    let target = match schema {
        Some(schema) => TargetName::new(name, false).with_schema(schema, false),
        None => TargetName::new(name, false),
    };
    database
        .table_by_target(target, IdentifierCase::AsWritten)
        .expect("unambiguous lookup")
        .expect("the table exists")
}
