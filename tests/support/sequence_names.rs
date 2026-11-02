//! The sequences a database holds, by name.

use sql_traits::prelude::*;

/// Every sequence the database holds, as `schema.name` with `public` for one
/// stored without a schema, sorted.
pub(crate) fn sequence_names(database: &ParserDB) -> Vec<String> {
    let mut names: Vec<String> = database
        .sequences()
        .map(|sequence| {
            format!(
                "{}.{}",
                sequence.sequence_schema().unwrap_or("public"),
                sequence.sequence_name()
            )
        })
        .collect();
    names.sort();
    names
}
