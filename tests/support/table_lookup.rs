//! Looks up a table by name and optional schema, or as the one a trigger is
//! defined on.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a test helper fails loudly on a broken premise"
)]

use sql_traits::prelude::*;

/// The table `name` in `schema`, or on the search path when `schema` is `None`.
pub(crate) fn table<'db>(
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

/// The table or foreign table `trigger` is defined on.
pub(crate) fn trigger_table<'db>(
    trigger: &'db <ParserDB as DatabaseLike>::Trigger,
    database: &'db ParserDB,
) -> &'db <ParserDB as DatabaseLike>::Table {
    match trigger.relation(database) {
        Ok(Relation::Table(table) | Relation::ForeignTable(table)) => table,
        other => panic!("the trigger is on a table, got {other:?}"),
    }
}
