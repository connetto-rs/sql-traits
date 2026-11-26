//! Values held identically by constrained columns.

use alloc::{
    borrow::{Cow, ToOwned},
    string::String,
    vec,
    vec::Vec,
};

use sqlparser::ast::{BinaryOperator, DataType, Expr, InTarget};

use super::terms::{conjuncts, string_value, unwrapped};
use crate::{
    errors::LookupError,
    traits::{
        CheckConstraintLike, ColumnCollation, ColumnLike, DataTypeLike, DatabaseLike,
        EnumerationKind,
    },
    utils::{identifier_resolution::normalize_identifier, scalar_family::ScalarFamily},
};

/// The stored name of the column `expression` names under parentheses, bare
/// or qualified.
pub(super) fn stored_name(mut expression: &Expr) -> Option<Cow<'_, str>> {
    while let Expr::Nested(inner) = expression {
        expression = inner;
    }
    let name = match expression {
        Expr::Identifier(name) => name,
        Expr::CompoundIdentifier(parts) => parts.last()?,
        _ => return None,
    };
    Some(normalize_identifier(&name.value, name.quote_style.is_some()))
}

/// What `=` compares the values of a column by, when two values it holds
/// equal are the same value.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum Identity {
    /// A builtin type of this family.
    Family(ScalarFamily),
    /// The enum type of this name.
    Enumeration(String),
}

/// What `=` compares the values of `column` by, or `None` when two equal
/// values of it may differ.
pub(super) fn identity<DB: DatabaseLike>(
    column: &DB::Column,
    database: &DB,
) -> Result<Option<Identity>, LookupError> {
    let Some(identity) = type_identity(column.data_type(database), database) else {
        return Ok(None);
    };
    if identity == Identity::Family(ScalarFamily::String)
        && !matches!(column.collation(database)?, ColumnCollation::DatabaseDefault)
    {
        return Ok(None);
    }
    Ok(Some(identity))
}

/// What `=` compares values of the type `declared` by under the database's
/// default collation, or `None` when two equal values of it may differ.
pub(super) fn type_identity<DB: DatabaseLike>(
    declared: &DB::DataType,
    database: &DB,
) -> Option<Identity> {
    let data_type = declared.base_type(database).unwrap_or(declared);
    if data_type.array_element(database).is_some() {
        return None;
    }
    if let Some(enumeration) = data_type.enumeration(database) {
        return (enumeration.kind() == EnumerationKind::Enum)
            .then(|| Identity::Enumeration(data_type.name(database).into_owned()));
    }
    match data_type.scalar_family(database) {
        Some(
            family @ (ScalarFamily::Bool
            | ScalarFamily::Int
            | ScalarFamily::Uuid
            | ScalarFamily::Date
            | ScalarFamily::Timestamp
            | ScalarFamily::TimestampTz),
        ) => Some(Identity::Family(family)),
        Some(ScalarFamily::String)
            if declared.domain(database).is_none()
                && !data_type.normalized_name(database).starts_with("CHAR") =>
        {
            Some(Identity::Family(ScalarFamily::String))
        }
        _ => None,
    }
}

/// Whether `=` holds between `left` and `right` only for identical values,
/// compared alike for both.
pub(super) fn same_identity<DB: DatabaseLike>(
    database: &DB,
    left: &DB::Column,
    right: &DB::Column,
) -> Result<bool, LookupError> {
    let left = identity(left, database)?;
    Ok(left.is_some() && left == identity(right, database)?)
}

/// The values every row of its table holds `column` to, the expression it is
/// generated as or the values [`checked_values`] reads.
pub(super) fn held_values<'a, DB: DatabaseLike>(
    database: &'a DB,
    column: &'a DB::Column,
) -> Result<Option<Vec<&'a Expr>>, LookupError> {
    if let Some(expression) = column.generation_expression() {
        return Ok(Some(vec![expression]));
    }
    checked_values(database, column)
}

/// The values a `NOT NULL` column is held to by a top-level conjunct of a
/// validated check, `=` one value or `IN` a list of values, those of the
/// first such check.
fn checked_values<'a, DB: DatabaseLike>(
    database: &'a DB,
    column: &'a DB::Column,
) -> Result<Option<Vec<&'a Expr>>, LookupError> {
    if column.is_nullable(database)? {
        return Ok(None);
    }
    let stored = column.stored_column_name();
    for check in column.check_constraints(database)? {
        if !check.is_validated(database)? {
            continue;
        }
        if let Some((_, values)) = conjuncts(check.expression(database))
            .into_iter()
            .filter_map(fixed_by)
            .find(|(name, _)| *name == stored)
        {
            return Ok(Some(values));
        }
    }
    Ok(None)
}

/// A column with the texts its row may hold in it.
pub(super) type Fixed<'a, DB> = (&'a <DB as DatabaseLike>::Column, Vec<String>);

/// `column` with the texts of `values`, when it is an enum or a text column
/// `=` compares by identity and each value a string literal it holds as
/// written.
pub(super) fn pin<'a, DB: DatabaseLike>(
    database: &DB,
    column: &'a DB::Column,
    values: &[&Expr],
) -> Result<Option<Fixed<'a, DB>>, LookupError> {
    let enumeration = match identity(column, database)? {
        Some(Identity::Enumeration(_)) => true,
        Some(Identity::Family(ScalarFamily::String)) => false,
        _ => return Ok(None),
    };
    Ok(values
        .iter()
        .map(|value| kept_literal(value, enumeration).map(str::to_owned))
        .collect::<Option<Vec<String>>>()
        .map(|values| (column, values)))
}

/// The string literal `expression` writes under parentheses and casts that
/// keep its text: to `text`, to `varchar` of no length, and, for an enum
/// column, to a type of no modifiers, which `=` accepts only as the enum.
fn kept_literal(mut expression: &Expr, enumeration: bool) -> Option<&str> {
    loop {
        match expression {
            Expr::Nested(inner) => expression = inner,
            Expr::Cast { expr, data_type, .. }
                if matches!(
                    data_type,
                    DataType::Text | DataType::Varchar(None) | DataType::CharacterVarying(None)
                ) || (enumeration
                    && matches!(data_type, DataType::Custom(_, modifiers) if modifiers.is_empty())) =>
            {
                expression = expr;
            }
            Expr::Value(value) => return string_value(&value.value),
            _ => return None,
        }
    }
}

/// The column `condition` fixes with `=` or `IN` to values reading no
/// column, with the values.
pub(super) fn fixed_by(condition: &Expr) -> Option<(Cow<'_, str>, Vec<&Expr>)> {
    match unwrapped(condition) {
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
            [(left, right), (right, left)]
                .into_iter()
                .find_map(|(column, value)| Some((stored_name(column)?, vec![value.as_ref()])))
        }
        Expr::In { expr, target: InTarget::List(list), negated: false, .. } => {
            Some((stored_name(expr)?, list.iter().collect()))
        }
        _ => None,
    }
}
