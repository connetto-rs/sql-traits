use super::{PostgresError, pg, quoted};

/// The refusal for a routine reference that resolves to no routine.
pub(super) fn missing_routine(
    name: &str,
    signature: Option<&str>,
    procedure: bool,
) -> PostgresError {
    let kind = if procedure { "procedure" } else { "function" };
    let message = signature.map_or_else(
        || format!("could not find a {kind} named {}", quoted(name)),
        |signature| format!("{kind} {signature} does not exist"),
    );
    PostgresError::new(pg::UNDEFINED_FUNCTION, message)
}

/// The object kind named by a routine keyword.
pub(super) const fn routine_kind(keyword: super::super::RoutineKeyword) -> &'static str {
    match keyword {
        super::super::RoutineKeyword::Function => "function",
        super::super::RoutineKeyword::Procedure => "procedure",
        super::super::RoutineKeyword::Routine => "routine",
    }
}

/// The refusal for a privilege unavailable on a routine.
pub(super) fn invalid_privilege(
    privilege: &str,
    routine: super::super::RoutineKeyword,
    columns: bool,
) -> PostgresError {
    if columns {
        return PostgresError::new(
            pg::INVALID_GRANT_OPERATION,
            "column privileges are only valid for relations",
        );
    }
    if matches!(
        privilege,
        "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "TRUNCATE"
            | "REFERENCES"
            | "TRIGGER"
            | "USAGE"
            | "CREATE"
            | "CONNECT"
            | "TEMPORARY"
            | "TEMP"
            | "MAINTAIN"
            | "SET"
            | "ALTER SYSTEM"
    ) {
        PostgresError::new(
            pg::INVALID_GRANT_OPERATION,
            format!("invalid privilege type {privilege} for {}", routine_kind(routine)),
        )
    } else {
        PostgresError::new(
            pg::SYNTAX_ERROR,
            format!("unrecognized privilege type {}", quoted(&privilege.to_ascii_lowercase())),
        )
    }
}
