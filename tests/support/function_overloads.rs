//! Finds one overload of a function by its argument types.
#![expect(clippy::expect_used, reason = "a test helper fails loudly on a broken premise")]

use sql_traits::prelude::*;

/// The overload of `name` in `schema` whose arguments are `argument_types`.
pub(crate) fn overload<'db>(
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
