use alloc::{
    boxed::Box,
    string::{String, ToString},
};

use super::{PostgresError, pg, quoted};
use crate::errors::{Misapplied, SequenceParameterError, TriggerFiringRefusal, Unresolved};

pub(super) fn trigger_firing(
    relation_name: &str,
    reason: TriggerFiringRefusal,
) -> Option<PostgresError> {
    let (state, message) = match reason {
        TriggerFiringRefusal::InsteadOfOnTable => {
            (pg::WRONG_OBJECT_TYPE, format!("{} is a table", quoted(relation_name)))
        }
        TriggerFiringRefusal::RowLevelBeforeOrAfterOnView
        | TriggerFiringRefusal::TruncateOnView
        | TriggerFiringRefusal::TransitionTablesOnView => {
            (pg::WRONG_OBJECT_TYPE, format!("{} is a view", quoted(relation_name)))
        }
        TriggerFiringRefusal::InsteadOfPerStatement => {
            (pg::FEATURE_NOT_SUPPORTED, "INSTEAD OF triggers must be FOR EACH ROW".into())
        }
        TriggerFiringRefusal::InsteadOfColumnList => {
            (pg::FEATURE_NOT_SUPPORTED, "INSTEAD OF triggers cannot have column lists".into())
        }
        TriggerFiringRefusal::InsteadOfCondition => {
            (pg::FEATURE_NOT_SUPPORTED, "INSTEAD OF triggers cannot have WHEN conditions".into())
        }
        TriggerFiringRefusal::BeforeOrAfterOnView => return None,
    };
    Some(PostgresError::new(state, message))
}

pub(super) fn call_rule(function: &str, rule: Misapplied) -> PostgresError {
    let state = match rule {
        Misapplied::OverOnOrderedSetAggregate
        | Misapplied::NamedArgumentOnAggregate
        | Misapplied::DistinctOverWindow
        | Misapplied::OrderByOverWindow
        | Misapplied::FilterOnWindowFunction => pg::FEATURE_NOT_SUPPORTED,
        Misapplied::OrderByWithinGroup | Misapplied::DistinctWithinGroup => pg::SYNTAX_ERROR,
        _ => pg::WRONG_OBJECT_TYPE,
    };
    PostgresError::new(state, rule.message(function))
}

pub(super) fn unresolved(
    name: &str,
    types: &[Box<str>],
    reason: Unresolved,
    operator: bool,
) -> PostgresError {
    if reason == Unresolved::PolymorphicUnknown {
        return PostgresError::new(
            pg::DATATYPE_MISMATCH,
            "could not determine polymorphic type because input has type unknown",
        );
    }
    let state = if reason == Unresolved::NotUnique {
        pg::AMBIGUOUS_FUNCTION
    } else {
        pg::UNDEFINED_FUNCTION
    };
    let description =
        if reason == Unresolved::NotUnique { "is not unique" } else { "does not exist" };
    let message = if operator {
        let signature = match types {
            [operand] => format!("{name} {operand}"),
            [left, right] => format!("{left} {name} {right}"),
            _ => format!("{name} {}", types.join(", ")),
        };
        format!("operator {description}: {signature}")
    } else {
        format!("function {name}({}) {description}", types.join(", "))
    };
    PostgresError::new(state, message)
}

pub(super) fn sequence_parameters(
    reason: &SequenceParameterError,
    identity: bool,
) -> PostgresError {
    match reason {
        SequenceParameterError::UnsupportedType { .. } => {
            let object = if identity { "identity column" } else { "sequence" };
            PostgresError::new(
                pg::INVALID_PARAMETER_VALUE,
                format!("{object} type must be smallint, integer, or bigint"),
            )
        }
        SequenceParameterError::NotABigint { value, refusal, .. } => {
            match refusal {
                crate::errors::SequenceIntegerRefusal::OutOfRange => {
                    PostgresError::new(
                        pg::NUMERIC_VALUE_OUT_OF_RANGE,
                        format!("value {} is out of range for type bigint", quoted(value)),
                    )
                }
                crate::errors::SequenceIntegerRefusal::InvalidInput => {
                    PostgresError::new(
                        pg::INVALID_TEXT_REPRESENTATION,
                        format!("invalid input syntax for type bigint: {}", quoted(value)),
                    )
                }
                crate::errors::SequenceIntegerRefusal::UnexpectedToken { written_token } => {
                    PostgresError::new(
                        pg::SYNTAX_ERROR,
                        format!("syntax error at or near {}", quoted(written_token)),
                    )
                }
            }
        }
        _ => PostgresError::new(pg::INVALID_PARAMETER_VALUE, reason.to_string()),
    }
}

pub(super) fn collation_option(
    name: &str,
    value: &str,
    qualified: bool,
    nondeterministic_provider: bool,
) -> PostgresError {
    if qualified {
        return PostgresError::new(pg::SYNTAX_ERROR, "syntax error at or near \".\"");
    }
    if nondeterministic_provider {
        return PostgresError::new(
            pg::FEATURE_NOT_SUPPORTED,
            "nondeterministic collations not supported with this provider",
        );
    }
    match name {
        "provider" => {
            PostgresError::new(
                pg::INVALID_OBJECT_DEFINITION,
                format!("unrecognized collation provider: {value}"),
            )
        }
        "deterministic" => {
            PostgresError::new(pg::SYNTAX_ERROR, "deterministic requires a Boolean value")
        }
        _ => {
            PostgresError::new(
                pg::SYNTAX_ERROR,
                format!("collation attribute {} not recognized", quoted(name)),
            )
        }
    }
}

pub(super) fn relation_name(parts: &[String]) -> PostgresError {
    match parts {
        [] => PostgresError::new(pg::INVALID_NAME, "invalid name syntax"),
        [name] if name.is_empty() => {
            PostgresError::new(pg::UNDEFINED_TABLE, "relation \"\" does not exist")
        }
        [_, _, _] => {
            PostgresError::new(
                pg::FEATURE_NOT_SUPPORTED,
                format!(
                    "cross-database references are not implemented: {}",
                    quoted(&parts.join("."))
                ),
            )
        }
        _ => {
            PostgresError::new(
                pg::SYNTAX_ERROR,
                format!("improper relation name (too many dotted names): {}", parts.join(".")),
            )
        }
    }
}
