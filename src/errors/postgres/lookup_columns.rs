use alloc::{format, string::String};

use sqlparser::ast::Ident;

use super::{PostgresError, pg, quoted};
use crate::{
    errors::{ColumnLookupSite, ColumnReference},
    impls::SqlparserDialect,
};

impl ColumnReference {
    /// Records the stored parts and scope of a refused column reference.
    pub(crate) fn unresolved(
        dialect: SqlparserDialect,
        parts: &[Ident],
        catalog: &str,
        relation_found: bool,
        inaccessible: bool,
    ) -> Self {
        let joined = || {
            parts
                .iter()
                .map(|part| dialect.stored_name(part))
                .collect::<alloc::vec::Vec<_>>()
                .join(".")
        };
        if parts.len() > 4 {
            return Self::Improper { reference: joined() };
        }
        if parts.len() == 4 && dialect.stored_name(&parts[0]) != catalog {
            return Self::CrossDatabase { reference: joined() };
        }
        let column_name = parts.last().map(|part| dialect.stored_name(part)).unwrap_or_default();
        let relation_name =
            parts.len().checked_sub(2).map(|position| dialect.stored_name(&parts[position]));
        if !relation_found && let Some(relation_name) = relation_name {
            return Self::Relation { relation_name, inaccessible };
        }
        Self::Column { column_name, relation_name }
    }

    /// The primary refusal for this column reference.
    pub(super) fn postgres_error(&self) -> PostgresError {
        match self {
            Self::Column { column_name, relation_name } => {
                let reference = match relation_name {
                    Some(relation_name) => format!("{relation_name}.{column_name}"),
                    None => quoted(column_name),
                };
                PostgresError::new(
                    pg::UNDEFINED_COLUMN,
                    format!("column {reference} does not exist"),
                )
            }
            Self::Relation { relation_name, inaccessible } => {
                let prefix = if *inaccessible { "invalid reference to" } else { "missing" };
                PostgresError::new(
                    pg::UNDEFINED_TABLE,
                    format!("{prefix} FROM-clause entry for table {}", quoted(relation_name)),
                )
            }
            Self::CrossDatabase { reference } => {
                PostgresError::new(
                    pg::FEATURE_NOT_SUPPORTED,
                    format!("cross-database references are not implemented: {reference}"),
                )
            }
            Self::Improper { reference } => {
                PostgresError::new(
                    pg::SYNTAX_ERROR,
                    format!("improper qualified name (too many dotted names): {reference}"),
                )
            }
        }
    }
}

/// The primary refusal for a required column.
pub(super) fn missing_column(
    table_name: &str,
    column_name: &str,
    site: ColumnLookupSite,
) -> PostgresError {
    let column = quoted(column_name);
    let message: String = match site {
        ColumnLookupSite::Column => format!("column {column} does not exist"),
        ColumnLookupSite::Key => format!("column {column} named in key does not exist"),
        ColumnLookupSite::Relation => {
            format!("column {column} of relation {} does not exist", quoted(table_name))
        }
        ColumnLookupSite::ForeignKey => {
            format!("column {column} referenced in foreign key constraint does not exist")
        }
    };
    PostgresError::new(pg::UNDEFINED_COLUMN, message)
}

/// The refusal for a date/time literal, excluding unavailable session state.
pub(super) fn policy_literal(
    literal: &str,
    data_type: sql_values::postgres::PgType,
    reason: sql_values::postgres::DateTimeError,
) -> Option<PostgresError> {
    use sql_values::postgres::DateTimeError;

    let sqlstate = match reason {
        DateTimeError::FieldValueOutOfRange
        | DateTimeError::DateOutOfRange
        | DateTimeError::TimestampOutOfRange => pg::DATETIME_FIELD_OVERFLOW,
        DateTimeError::TimeZoneDisplacementOutOfRange => pg::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
        DateTimeError::InvalidInputSyntax(_) | DateTimeError::TimeZoneNotRecognized => {
            pg::INVALID_DATETIME_FORMAT
        }
        _ => return None,
    };
    let reason = if reason == DateTimeError::TimeZoneNotRecognized {
        DateTimeError::InvalidInputSyntax(data_type)
    } else {
        reason
    };
    Some(PostgresError::new(sqlstate, format!("{reason}: {}", quoted(literal))))
}

/// The refusal for an ambiguous column or select-list name.
pub(super) fn ambiguous_lookup(
    name: &str,
    site: crate::errors::AmbiguousLookupSite,
) -> Option<PostgresError> {
    use crate::errors::AmbiguousLookupSite;

    let prefix = match site {
        AmbiguousLookupSite::Relation => return None,
        AmbiguousLookupSite::Column => "column reference",
        AmbiguousLookupSite::OrderBy => "ORDER BY",
        AmbiguousLookupSite::GroupBy => "GROUP BY",
        AmbiguousLookupSite::DistinctOn => "DISTINCT ON",
    };
    Some(PostgresError::new(
        pg::AMBIGUOUS_COLUMN,
        format!("{prefix} {} is ambiguous", quoted(name)),
    ))
}
