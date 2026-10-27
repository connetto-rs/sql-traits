/// The overload of `name` in `schema` whose arguments are `argument_types`.
fn overload<'db>(
    database: &'db ParserDB,
    schema: &str,
    name: &str,
    argument_types: &[&str],
) -> &'db <ParserDB as DatabaseLike>::Function {
    database
        .functions()
        .find(|function| {
            function.target_name().schema() == Some(schema)
                && function.name() == name
                && function
                    .argument_type_names(database)
                    .map(|declared| declared.to_ascii_lowercase())
                    .eq(argument_types.iter().copied())
        })
        .expect("the overload is registered")
}
