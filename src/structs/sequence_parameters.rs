//! The values a sequence counts with.

use alloc::string::ToString;
use core::fmt;

use sqlparser::ast::{DataType, Expr, ObjectNamePart, SequenceOptions, UnaryOperator, Value};

use crate::errors::SequenceParameterError;

/// The integer type a sequence counts in, which bounds its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SequenceType {
    /// `smallint`, two bytes.
    SmallInt,
    /// `integer`, four bytes.
    Integer,
    /// `bigint`, eight bytes, the type a sequence takes when `AS` names none.
    BigInt,
}

impl SequenceType {
    /// Returns the smallest value the type holds.
    #[must_use]
    pub fn min(self) -> i64 {
        match self {
            Self::SmallInt => i64::from(i16::MIN),
            Self::Integer => i64::from(i32::MIN),
            Self::BigInt => i64::MIN,
        }
    }

    /// Returns the largest value the type holds.
    #[must_use]
    pub fn max(self) -> i64 {
        match self {
            Self::SmallInt => i64::from(i16::MAX),
            Self::Integer => i64::from(i32::MAX),
            Self::BigInt => i64::MAX,
        }
    }

    /// The sequence type `data_type` names, or `None` for a type PostgreSQL
    /// refuses a sequence.
    ///
    /// Besides the SQL spellings, the catalog names `int2`, `int4` and `int8`
    /// are accepted, quoted or qualified with `pg_catalog`.
    pub(crate) fn of(data_type: &DataType) -> Option<Self> {
        match data_type {
            DataType::SmallInt(_) | DataType::Int2(_) => Some(Self::SmallInt),
            DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => Some(Self::Integer),
            DataType::BigInt(_) | DataType::Int8(_) => Some(Self::BigInt),
            DataType::Custom(name, modifiers) if modifiers.is_empty() => {
                let type_name = match name.0.as_slice() {
                    [type_name] => type_name,
                    [schema, type_name] if names(schema, "pg_catalog") => type_name,
                    _ => return None,
                };
                [("int2", Self::SmallInt), ("int4", Self::Integer), ("int8", Self::BigInt)]
                    .into_iter()
                    .find_map(|(catalog_name, kind)| names(type_name, catalog_name).then_some(kind))
            }
            _ => None,
        }
    }
}

impl fmt::Display for SequenceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SmallInt => "smallint",
            Self::Integer => "integer",
            Self::BigInt => "bigint",
        })
    }
}

/// The values a sequence counts with, each option a declaration left out
/// filled with PostgreSQL's default, as `pg_sequence` records them.
///
/// The values always satisfy what PostgreSQL demands of a sequence: a nonzero
/// increment, bounds the type holds with the minimum below the maximum, a
/// start between them and a positive cache.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
///
/// let parameters =
///     SequenceParameters::new(SequenceType::Integer, -1, i64::from(i32::MIN), -1, -1, 1, false)?;
/// assert_eq!(parameters.start(), -1);
/// assert!(SequenceParameters::new(SequenceType::SmallInt, 1, 1, 40_000, 1, 1, false).is_err());
/// # Ok::<(), sql_traits::errors::SequenceParameterError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceParameters {
    /// The integer type the sequence counts in.
    data_type: SequenceType,
    /// The step between two values, negative for a descending sequence.
    increment: i64,
    /// The smallest value the sequence yields.
    min_value: i64,
    /// The largest value the sequence yields.
    max_value: i64,
    /// The first value the sequence yields.
    start: i64,
    /// How many values a session draws ahead.
    cache: i64,
    /// Whether the sequence wraps around past a bound.
    cycle: bool,
}

impl SequenceParameters {
    /// Returns the parameters, or the first refusal PostgreSQL makes of them,
    /// checked in PostgreSQL's order.
    ///
    /// # Errors
    ///
    /// Returns [`SequenceParameterError::ZeroIncrement`],
    /// [`SequenceParameterError::MaxValueOutOfRange`],
    /// [`SequenceParameterError::MinValueOutOfRange`],
    /// [`SequenceParameterError::MinValueNotBelowMaxValue`],
    /// [`SequenceParameterError::StartBelowMinValue`],
    /// [`SequenceParameterError::StartAboveMaxValue`] or
    /// [`SequenceParameterError::CacheNotPositive`].
    pub fn new(
        data_type: SequenceType,
        increment: i64,
        min_value: i64,
        max_value: i64,
        start: i64,
        cache: i64,
        cycle: bool,
    ) -> Result<Self, SequenceParameterError> {
        let holds = |value: i64| (data_type.min()..=data_type.max()).contains(&value);
        if increment == 0 {
            return Err(SequenceParameterError::ZeroIncrement);
        }
        if !holds(max_value) {
            return Err(SequenceParameterError::MaxValueOutOfRange { max_value, data_type });
        }
        if !holds(min_value) {
            return Err(SequenceParameterError::MinValueOutOfRange { min_value, data_type });
        }
        if min_value >= max_value {
            return Err(SequenceParameterError::MinValueNotBelowMaxValue { min_value, max_value });
        }
        if start < min_value {
            return Err(SequenceParameterError::StartBelowMinValue { start, min_value });
        }
        if start > max_value {
            return Err(SequenceParameterError::StartAboveMaxValue { start, max_value });
        }
        if cache <= 0 {
            return Err(SequenceParameterError::CacheNotPositive { cache });
        }
        Ok(Self { data_type, increment, min_value, max_value, start, cache, cycle })
    }

    /// Reads the parameters a `CREATE SEQUENCE` or an identity writes, filling
    /// every option left out, or `NO MINVALUE` and `NO MAXVALUE`, with
    /// PostgreSQL's default for the type and direction. `OWNED BY` plays no
    /// part, and the parser has already refused an option written twice.
    pub(crate) fn from_options(
        options: &[SequenceOptions],
    ) -> Result<Self, SequenceParameterError> {
        let (mut data_type, mut increment, mut cycle) = (None, None, false);
        let (mut min_value, mut max_value, mut start, mut cache) = (None, None, None, None);
        for option in options {
            match option {
                SequenceOptions::DataType(written) => data_type = Some(written),
                SequenceOptions::IncrementBy(value, _) => increment = Some(value),
                SequenceOptions::MinValue(value) => min_value = value.as_ref(),
                SequenceOptions::MaxValue(value) => max_value = value.as_ref(),
                SequenceOptions::StartWith(value, _) => start = Some(value),
                SequenceOptions::Cache(value) => cache = Some(value),
                SequenceOptions::Cycle(no) => cycle = !no,
                SequenceOptions::OwnedBy(_) => {}
            }
        }

        let data_type = match data_type {
            Some(written) => {
                SequenceType::of(written).ok_or_else(|| {
                    SequenceParameterError::UnsupportedType { data_type: written.to_string() }
                })?
            }
            None => SequenceType::BigInt,
        };
        let increment = increment.map_or(Ok(1), |value| bigint("INCREMENT", value))?;
        let max_value = match max_value {
            Some(value) => bigint("MAXVALUE", value)?,
            None if increment > 0 => data_type.max(),
            None => -1,
        };
        let min_value = match min_value {
            Some(value) => bigint("MINVALUE", value)?,
            None if increment < 0 => data_type.min(),
            None => 1,
        };
        let start = match start {
            Some(value) => bigint("START", value)?,
            None if increment > 0 => min_value,
            None => max_value,
        };
        let cache = cache.map_or(Ok(1), |value| bigint("CACHE", value))?;
        Self::new(data_type, increment, min_value, max_value, start, cache, cycle)
    }

    /// Returns the parameters after `ALTER SEQUENCE ... AS data_type`, which a
    /// column's type change issues for its identity sequence.
    ///
    /// A bound left at the old type's limit moves to the new type's, and every
    /// other value stays, so the change is refused when a kept value falls
    /// outside the new type.
    pub(crate) fn with_data_type(
        self,
        data_type: SequenceType,
    ) -> Result<Self, SequenceParameterError> {
        let min_value =
            if self.min_value == self.data_type.min() { data_type.min() } else { self.min_value };
        let max_value =
            if self.max_value == self.data_type.max() { data_type.max() } else { self.max_value };
        Self::new(
            data_type,
            self.increment,
            min_value,
            max_value,
            self.start,
            self.cache,
            self.cycle,
        )
    }

    /// Returns the integer type the sequence counts in.
    #[must_use]
    pub fn data_type(&self) -> SequenceType {
        self.data_type
    }

    /// Returns the step between two values, negative for a descending
    /// sequence.
    #[must_use]
    pub fn increment(&self) -> i64 {
        self.increment
    }

    /// Returns the smallest value the sequence yields.
    #[must_use]
    pub fn min_value(&self) -> i64 {
        self.min_value
    }

    /// Returns the largest value the sequence yields.
    #[must_use]
    pub fn max_value(&self) -> i64 {
        self.max_value
    }

    /// Returns the first value the sequence yields.
    #[must_use]
    pub fn start(&self) -> i64 {
        self.start
    }

    /// Returns how many values a session draws ahead.
    #[must_use]
    pub fn cache(&self) -> i64 {
        self.cache
    }

    /// Returns whether the sequence wraps around past a bound.
    #[must_use]
    pub fn cycles(&self) -> bool {
        self.cycle
    }
}

/// The value `option` writes as a `bigint`, which PostgreSQL reads from a
/// numeric literal with an optional sign.
fn bigint(option: &'static str, value: &Expr) -> Result<i64, SequenceParameterError> {
    let refused = || SequenceParameterError::NotABigint { option, value: value.to_string() };
    let (negative, literal) = match value {
        Expr::Value(literal) => (false, &literal.value),
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => {
            match expr.as_ref() {
                Expr::Value(literal) => (false, &literal.value),
                _ => return Err(refused()),
            }
        }
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => {
            match expr.as_ref() {
                Expr::Value(literal) => (true, &literal.value),
                _ => return Err(refused()),
            }
        }
        _ => return Err(refused()),
    };
    let Value::Number(digits, _) = literal else {
        return Err(refused());
    };
    parse_integer(digits, negative).ok_or_else(refused)
}

/// Reads `digits` the way PostgreSQL's `bigint` input does: decimal, or
/// hexadecimal, octal or binary behind `0x`, `0o` or `0b`, with single
/// underscores between digits or after the prefix.
fn parse_integer(digits: &str, negative: bool) -> Option<i64> {
    let (radix, body) = match digits.as_bytes() {
        [b'0', b'x' | b'X', ..] => (16, &digits[2..]),
        [b'0', b'o' | b'O', ..] => (8, &digits[2..]),
        [b'0', b'b' | b'B', ..] => (2, &digits[2..]),
        _ => (10, digits),
    };
    let mut magnitude: i128 = 0;
    let mut ends_in_digit = false;
    let mut underscore_allowed = radix != 10;
    for character in body.chars() {
        if character == '_' {
            if !underscore_allowed {
                return None;
            }
            underscore_allowed = false;
            ends_in_digit = false;
            continue;
        }
        let digit = character.to_digit(radix)?;
        magnitude = magnitude.checked_mul(i128::from(radix))?.checked_add(i128::from(digit))?;
        underscore_allowed = true;
        ends_in_digit = true;
    }
    if !ends_in_digit {
        return None;
    }
    i64::try_from(if negative { -magnitude } else { magnitude }).ok()
}

/// Whether `part` is the identifier `name`, which an unquoted spelling matches
/// in any case.
fn names(part: &ObjectNamePart, name: &str) -> bool {
    part.as_ident().is_some_and(|ident| {
        if ident.quote_style.is_some() {
            ident.value == name
        } else {
            ident.value.eq_ignore_ascii_case(name)
        }
    })
}
