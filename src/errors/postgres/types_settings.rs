use alloc::string::ToString;

use sqlparser::{
    dialect::PostgreSqlDialect,
    tokenizer::{Token, Tokenizer},
};

use super::{PostgresError, pg, quoted};
use crate::errors::{KeyConstraint, ObjectKind, RelationKindContext, SessionRoleRefusal};

pub(super) fn role_in_use(refusal: SessionRoleRefusal) -> PostgresError {
    let (state, message) = match refusal {
        SessionRoleRefusal::DropCurrent => (pg::OBJECT_IN_USE, "current user cannot be dropped"),
        SessionRoleRefusal::DropSession => (pg::OBJECT_IN_USE, "session user cannot be dropped"),
        SessionRoleRefusal::RenameCurrent => {
            (pg::FEATURE_NOT_SUPPORTED, "current user cannot be renamed")
        }
        SessionRoleRefusal::RenameSession => {
            (pg::FEATURE_NOT_SUPPORTED, "session user cannot be renamed")
        }
    };
    PostgresError::new(state, message)
}

pub(super) fn default_privilege(privilege: &str, object_type: &str) -> PostgresError {
    let kind = match object_type {
        "TABLES" => "relation",
        "SEQUENCES" => "sequence",
        "FUNCTIONS" | "ROUTINES" => "function",
        "TYPES" => "type",
        "SCHEMAS" => "schema",
        "LARGE OBJECTS" => "large object",
        _ => object_type,
    };
    PostgresError::new(
        pg::INVALID_GRANT_OPERATION,
        format!("invalid privilege type {privilege} for {kind}"),
    )
}

pub(super) fn name_taken(type_collision: bool, name: &str) -> PostgresError {
    let (state, kind) = if type_collision {
        (pg::DUPLICATE_OBJECT, "type")
    } else {
        (pg::DUPLICATE_TABLE, "relation")
    };
    PostgresError::new(state, format!("{kind} {} already exists", quoted(name)))
}

pub(super) fn kind(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::ForeignTable => "foreign table",
        ObjectKind::VirtualTable => "virtual table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Column => "column",
        ObjectKind::ColumnDefault => "column default",
        ObjectKind::Index => "index",
        ObjectKind::UniqueIndex => "unique index",
        ObjectKind::CheckConstraint => "check constraint",
        ObjectKind::Policy => "policy",
        ObjectKind::Function => "function",
        ObjectKind::Trigger => "trigger",
        ObjectKind::Rule => "rule",
        ObjectKind::Extension => "extension",
        ObjectKind::Role => "role",
        ObjectKind::Schema => "schema",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Domain => "domain",
        ObjectKind::Type => "type",
        ObjectKind::Operator => "operator",
        ObjectKind::Cast => "cast",
    }
}

pub(super) fn constraint(kind: KeyConstraint) -> &'static str {
    match kind {
        KeyConstraint::PrimaryKey => "primary key",
        KeyConstraint::Unique => "unique",
        KeyConstraint::ForeignKey => "foreign key",
        KeyConstraint::Exclusion => "exclusion",
    }
}

pub(super) fn kind_mismatch(
    name: &str,
    expected: ObjectKind,
    context: RelationKindContext,
) -> PostgresError {
    let name = quoted(name);
    let message = match context {
        RelationKindContext::Statement => {
            let article = if expected == ObjectKind::Index { "an" } else { "a" };
            format!("{name} is not {article} {}", kind(expected))
        }
        RelationKindContext::ForeignKey => format!("referenced relation {name} is not a table"),
        RelationKindContext::Index => format!("cannot create index on relation {name}"),
        RelationKindContext::Trigger => format!("relation {name} cannot have triggers"),
    };
    PostgresError::new(pg::WRONG_OBJECT_TYPE, message)
}

pub(super) fn relation_missing(
    name: &str,
    expected: ObjectKind,
    drop_statement: bool,
) -> PostgresError {
    let (state, kind) = if drop_statement {
        let state = if expected == ObjectKind::ForeignTable {
            pg::UNDEFINED_OBJECT
        } else {
            pg::UNDEFINED_TABLE
        };
        (state, kind(expected))
    } else {
        (pg::UNDEFINED_TABLE, "relation")
    };
    PostgresError::new(state, format!("{kind} {} does not exist", quoted(name)))
}

pub(super) fn timezone(value: &str, reason: sql_values::postgres::TimeZoneError) -> PostgresError {
    use sql_values::postgres::{IntervalError, TimeZoneError};
    match reason {
        TimeZoneError::LeapSeconds => {
            PostgresError::new(
                pg::INVALID_PARAMETER_VALUE,
                format!("time zone {} appears to use leap seconds", quoted(value)),
            )
        }
        TimeZoneError::Interval(reason) => {
            let interval = value
                .get(8..)
                .and_then(|body| {
                    body.trim().strip_prefix('\'').and_then(|body| body.strip_suffix('\''))
                })
                .unwrap_or(value);
            let (state, message) = match reason {
                IntervalError::InvalidSyntax => {
                    (
                        pg::INVALID_DATETIME_FORMAT,
                        format!("invalid input syntax for type interval: {}", quoted(interval)),
                    )
                }
                IntervalError::FieldValueOutOfRange => {
                    (
                        pg::INTERVAL_FIELD_OVERFLOW,
                        format!("interval field value out of range: {}", quoted(interval)),
                    )
                }
                IntervalError::OutOfRange => {
                    (pg::INTERVAL_FIELD_OVERFLOW, "interval out of range".into())
                }
                IntervalError::DivisionByZero => (pg::DIVISION_BY_ZERO, "division by zero".into()),
            };
            PostgresError::new(state, message)
        }
        TimeZoneError::InvalidValue
        | TimeZoneError::OffsetOutOfRange
        | TimeZoneError::MonthsInInterval
        | TimeZoneError::DaysInInterval => {
            PostgresError::new(
                pg::INVALID_PARAMETER_VALUE,
                format!("invalid value for parameter \"TimeZone\": {}", quoted(value)),
            )
        }
    }
}

/// The syntax error PostgreSQL raises for a storage option that is not a
/// name, at most once qualified, with an optional value.
///
/// PostgreSQL's grammar takes `name`, `name = value`, `namespace.name` and
/// `namespace.name = value`, so the error names the first token of `option`
/// that breaks that shape.
pub(super) fn storage_option_not_named(option: &str) -> PostgresError {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, option).tokenize().unwrap_or_default();
    let mut names = 0;
    let mut after_period = false;
    let mut assigned = false;
    let offending =
        tokens.into_iter().filter(|token| !matches!(token, Token::Whitespace(_))).find(|token| {
            match token {
                _ if assigned => false,
                Token::Word(_) if names == 0 || after_period => {
                    names += 1;
                    after_period = false;
                    false
                }
                Token::Period if names == 1 && !after_period => {
                    after_period = true;
                    false
                }
                Token::Eq if names > 0 && !after_period => {
                    assigned = true;
                    false
                }
                _ => true,
            }
        });
    let token = offending.map_or_else(|| option.to_string(), |token| token.to_string());
    PostgresError::new(pg::SYNTAX_ERROR, format!("syntax error at or near {}", quoted(&token)))
}
