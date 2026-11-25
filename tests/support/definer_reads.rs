//! Probes of the reads a `SECURITY DEFINER` function chain leaves unconfined
//! when the role `app` runs it.
#![expect(clippy::expect_used, reason = "a test fails loudly on a broken premise")]

use sql_traits::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;

/// The deployment whose acting role is `app`.
pub(crate) fn deployment() -> Deployment {
    Deployment::new("app", ["app"], []).expect("a valid deployment")
}

/// The schema of `sql`, parsed as PostgreSQL.
pub(crate) fn schema(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(sql).expect("the schema parses")
}

/// The name of the table and the reason of the read `trigger` leaves
/// unconfined when `app` writes, under `reading`, or `None`.
pub(crate) fn trigger_read(
    db: &ParserDB,
    trigger: &str,
    reading: DefinerReading,
) -> Option<(String, String)> {
    let deployment = deployment();
    let app = ActingRole::Declared(db.role("app").expect("the role exists"));
    let trigger =
        db.triggers().find(|candidate| candidate.name() == trigger).expect("the trigger exists");
    trigger
        .unconfined_definer_read(db, &deployment, app, reading)
        .expect("the schema reads")
        .map(|read| described(&read))
}

/// The name of the table and the reason of the read `function`, in `schema`
/// or the default one, leaves unconfined when `app` executes it, or `None`.
pub(crate) fn function_read(
    db: &ParserDB,
    schema: Option<&str>,
    function: &str,
) -> Option<(String, String)> {
    let deployment = deployment();
    let app = ActingRole::Declared(db.role("app").expect("the role exists"));
    let mut name = TargetName::new(function, false);
    if let Some(schema) = schema {
        name = name.with_schema(schema, false);
    }
    db.function_by_target(name, IdentifierCase::AsWritten)
        .expect("the name resolves")
        .expect("the function exists")
        .unconfined_definer_read(db, &deployment, &[app])
        .expect("the schema reads")
        .map(|read| described(&read))
}

/// The table `read` reads and why it may reach a hidden row, by name.
pub(crate) fn described(read: &UnconfinedRead<'_, ParserDB>) -> (String, String) {
    let table = read.table().map_or_else(String::new, |table| table.table_name().to_owned());
    let reason = match read.reason() {
        Unconfinement::Restrictive(policy) => policy.name().to_owned(),
        Unconfinement::Permissive(policies) => {
            policies.iter().map(PolicyLike::name).collect::<Vec<_>>().join(", ")
        }
        Unconfinement::HiddenColumn(column) => column.column_name().to_owned(),
        Unconfinement::Unfollowed => "unfollowed".to_owned(),
    };
    (table, reason)
}
