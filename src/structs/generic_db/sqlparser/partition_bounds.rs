//! What PostgreSQL requires of the bound a partition takes, alone and against
//! the bounds of its sibling partitions.
//!
//! Every check that needs no value comparison is exact. Values are compared
//! only when every value involved is a literal the model can read as the type
//! of its key column without the database's locale: an integer type, an
//! unconstrained `numeric`, a `date`, a `timestamp` without time zone or
//! precision, and text under an explicit `"C"` or `"POSIX"` collation. Any
//! other key or bound expression leaves the comparison undecided, and an
//! undecided comparison accepts.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{cmp::Ordering, ops::ControlFlow};

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, DataType, ExactNumberInfo, Expr, ForValues,
    ObjectNamePart, PartitionBoundValue, TimezoneInfo, UnaryOperator, Value, ValueWithSpan, Visit,
    Visitor,
};

use super::{ParserDB, ParserDBBuilder, StoredTable, inheritance, partition_keys};
use crate::{
    errors::{Error, InvalidBound},
    traits::{PartitionStrategy, TableLike},
};

/// The type a key column's values are read as, for the types the model can
/// order on its own.
#[derive(Clone, Copy)]
enum KeyKind {
    /// An integer type holding values between the two limits.
    Integer { min: i128, max: i128 },
    /// An unconstrained `numeric`.
    Numeric,
    /// A `date`.
    Date,
    /// A `timestamp` without time zone or precision.
    Timestamp,
    /// Text under a collation ordering it by byte.
    Text,
}

/// A bound value read as the type of its key column.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Datum {
    /// An integer.
    Integer(i128),
    /// A decimal number.
    Numeric(Decimal),
    /// A date or a timestamp, as its fields from the year down.
    Moment([i64; 7]),
    /// Text, compared by byte.
    Text(String),
}

/// How a bound value reads as its key column's type.
enum Reading {
    /// A value the model orders.
    Known(Datum),
    /// A value the model cannot order, or an expression it does not evaluate.
    Undecided,
    /// A literal PostgreSQL refuses to read as the type.
    Invalid,
}

/// A decimal number as a sign, its significant digits and the power of ten
/// of its last digit, with no leading or trailing zero digit, so equal numbers
/// share one form.
#[derive(Clone, PartialEq, Eq)]
struct Decimal {
    negative: bool,
    digits: Vec<u8>,
    exponent: i64,
}

impl Decimal {
    /// Reads `text` as PostgreSQL reads a numeric literal: an optional sign,
    /// digits with an optional point, and an optional exponent.
    fn parse(text: &str) -> Option<Self> {
        let (negative, rest) = match text.as_bytes().first()? {
            b'-' => (true, &text[1..]),
            b'+' => (false, &text[1..]),
            _ => (false, text),
        };
        let (mantissa, exponent) = match rest.find(['e', 'E']) {
            Some(at) => (&rest[..at], rest[at + 1..].parse::<i64>().ok()?),
            None => (rest, 0),
        };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if whole.is_empty() && fraction.is_empty()
            || !whole.bytes().chain(fraction.bytes()).all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let mut digits: Vec<u8> =
            whole.bytes().chain(fraction.bytes()).map(|byte| byte - b'0').collect();
        let mut exponent = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;
        while digits.last() == Some(&0) {
            digits.pop();
            exponent += 1;
        }
        let leading = digits.iter().take_while(|digit| **digit == 0).count();
        digits.drain(..leading);
        if digits.is_empty() {
            return Some(Self { negative: false, digits, exponent: 0 });
        }
        Some(Self { negative, digits, exponent })
    }

    /// The power of ten just above the most significant digit, which orders
    /// two non-zero magnitudes before their digits are compared.
    fn magnitude(&self) -> i64 {
        i64::try_from(self.digits.len()).unwrap_or(i64::MAX).saturating_add(self.exponent)
    }

    /// Compares the absolute values.
    fn compare_magnitude(&self, other: &Self) -> Ordering {
        match (self.digits.is_empty(), other.digits.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        self.magnitude().cmp(&other.magnitude()).then_with(|| {
            let longest = self.digits.len().max(other.digits.len());
            (0..longest)
                .map(|at| {
                    let left = self.digits.get(at).copied().unwrap_or(0);
                    let right = other.digits.get(at).copied().unwrap_or(0);
                    left.cmp(&right)
                })
                .find(|ordering| ordering.is_ne())
                .unwrap_or(Ordering::Equal)
        })
    }

    /// The integer PostgreSQL casts this number to, rounding half away from
    /// zero, or `None` when it does not fit.
    fn round(&self) -> Option<i128> {
        let point = i64::try_from(self.digits.len()).ok()?.checked_add(self.exponent)?;
        if point < 0 {
            return Some(0);
        }
        let mut value: i128 = 0;
        for at in 0..usize::try_from(point).ok()? {
            let digit = self.digits.get(at).copied().unwrap_or(0);
            value = value.checked_mul(10)?.checked_add(i128::from(digit))?;
        }
        let first_dropped = usize::try_from(point).ok().and_then(|at| self.digits.get(at));
        if first_dropped.is_some_and(|digit| *digit >= 5) {
            value = value.checked_add(1)?;
        }
        Some(if self.negative { -value } else { value })
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.compare_magnitude(other),
            (true, true) => other.compare_magnitude(self),
        }
    }
}

/// The kind a key column's values are read as, or `None` for a type the
/// model leaves undecided.
fn key_kind(column: &ColumnDef) -> Option<KeyKind> {
    let integer = |bits: u32| {
        let max = (1_i128 << (bits - 1)) - 1;
        Some(KeyKind::Integer { min: -max - 1, max })
    };
    match &column.data_type {
        DataType::SmallInt(_) | DataType::Int2(_) => integer(16),
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => integer(32),
        DataType::BigInt(_) | DataType::Int8(_) => integer(64),
        DataType::Numeric(ExactNumberInfo::None)
        | DataType::Decimal(ExactNumberInfo::None)
        | DataType::Dec(ExactNumberInfo::None) => Some(KeyKind::Numeric),
        DataType::Date => Some(KeyKind::Date),
        DataType::Timestamp(None, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
            Some(KeyKind::Timestamp)
        }
        DataType::Text | DataType::Varchar(None) | DataType::CharacterVarying(None)
            if orders_by_byte(column) =>
        {
            Some(KeyKind::Text)
        }
        _ => None,
    }
}

/// Whether the column is written with the `"C"` or `"POSIX"` collation, which
/// order text by byte whatever the database's locale.
fn orders_by_byte(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        let ColumnOption::Collation(name) = &option.option else {
            return false;
        };
        matches!(
            name.0.last(),
            Some(ObjectNamePart::Identifier(ident))
                if ident.quote_style.is_some() && matches!(ident.value.as_str(), "C" | "POSIX")
        )
    })
}

/// The literal a bound value writes, with a leading sign applied to a number,
/// or `None` for anything else.
fn literal(expr: &Expr) -> Option<(bool, &Value)> {
    match expr {
        Expr::Value(ValueWithSpan { value, .. }) => Some((false, value)),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => {
            match literal(expr)? {
                (negated, value @ Value::Number(..)) => Some((!negated, value)),
                _ => None,
            }
        }
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => {
            literal(expr).filter(|(_, value)| matches!(value, Value::Number(..)))
        }
        Expr::Nested(inner) => literal(inner),
        _ => None,
    }
}

/// Reads a bound value as `kind`.
fn read(kind: KeyKind, expr: &Expr) -> Reading {
    let Some((negated, value)) = literal(expr) else {
        return Reading::Undecided;
    };
    let number = |text: &str| {
        Decimal::parse(text).map(|mut decimal| {
            if negated && !decimal.digits.is_empty() {
                decimal.negative = !decimal.negative;
            }
            decimal
        })
    };
    match (kind, value) {
        (KeyKind::Integer { min, max }, Value::Number(text, _)) => {
            match number(text).and_then(|decimal| decimal.round()) {
                Some(value) if (min..=max).contains(&value) => {
                    Reading::Known(Datum::Integer(value))
                }
                _ => Reading::Invalid,
            }
        }
        (KeyKind::Integer { min, max }, Value::SingleQuotedString(text)) => {
            read_integer_text(text.trim(), min, max)
        }
        (KeyKind::Numeric, Value::Number(text, _)) => {
            number(text)
                .map_or(Reading::Undecided, |decimal| Reading::Known(Datum::Numeric(decimal)))
        }
        (KeyKind::Numeric, Value::SingleQuotedString(text)) => {
            match Decimal::parse(text.trim()) {
                Some(decimal) => Reading::Known(Datum::Numeric(decimal)),
                None if is_special_number(text) => Reading::Undecided,
                None => Reading::Invalid,
            }
        }
        (KeyKind::Date | KeyKind::Timestamp, Value::SingleQuotedString(text)) => {
            read_moment(kind, text.trim())
        }
        (KeyKind::Text, Value::SingleQuotedString(text)) => {
            Reading::Known(Datum::Text(text.clone()))
        }
        (KeyKind::Text, Value::Number(text, _)) if !negated => {
            Reading::Known(Datum::Text(text.clone()))
        }
        _ => Reading::Undecided,
    }
}

/// Whether `text` names one of the numeric values without digits.
fn is_special_number(text: &str) -> bool {
    let text = text.trim().trim_start_matches(['+', '-']);
    ["nan", "infinity", "inf"].iter().any(|name| text.eq_ignore_ascii_case(name))
}

/// Reads quoted text as an integer the way PostgreSQL does: decimal digits
/// with an optional sign are read, a number with a point or an exponent is
/// refused, and any other spelling, such as PostgreSQL 16's hexadecimal and
/// underscore forms, is left undecided.
fn read_integer_text(text: &str, min: i128, max: i128) -> Reading {
    let digits = text.trim_start_matches(['+', '-']);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return match text.parse::<i128>() {
            Ok(value) if (min..=max).contains(&value) => Reading::Known(Datum::Integer(value)),
            _ => Reading::Invalid,
        };
    }
    if Decimal::parse(text).is_some() { Reading::Invalid } else { Reading::Undecided }
}

/// Reads `Y-M-D`, followed for a timestamp by an optional `H:M[:S[.F]]`, as
/// the fields of a moment, refusing a field out of its range. Any other
/// spelling is left undecided, and so are `24:00` and a leap second, which
/// PostgreSQL carries into the next day or minute.
fn read_moment(kind: KeyKind, text: &str) -> Reading {
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) if matches!(kind, KeyKind::Timestamp) => (date, Some(time.trim())),
        Some(_) => return Reading::Undecided,
        None => (text, None),
    };
    let field = |part: &str, widths: core::ops::RangeInclusive<usize>| {
        (widths.contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<i64>().ok())
            .flatten()
    };
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day), None) = (
        parts.next().and_then(|part| field(part, 4..=4)),
        parts.next().and_then(|part| field(part, 1..=2)),
        parts.next().and_then(|part| field(part, 1..=2)),
        parts.next(),
    ) else {
        return Reading::Undecided;
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Reading::Invalid;
    }
    let mut fields = [year, month, day, 0, 0, 0, 0];
    if let Some(time) = time {
        let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
        let mut parts = clock.split(':');
        let (Some(hour), Some(minute), second, None) = (
            parts.next().and_then(|part| field(part, 1..=2)),
            parts.next().and_then(|part| field(part, 2..=2)),
            parts.next().map(|part| field(part, 2..=2)),
            parts.next(),
        ) else {
            return Reading::Undecided;
        };
        let Some(second) = second.unwrap_or(Some(0)) else {
            return Reading::Undecided;
        };
        if hour == 24 || second == 60 || fraction.len() > 6 {
            return Reading::Undecided;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Reading::Invalid;
        }
        let Some(micros) = (fraction.is_empty()
            || fraction.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| format_micros(fraction)) else {
            return Reading::Undecided;
        };
        fields[3..].copy_from_slice(&[hour, minute, second, micros]);
    }
    Reading::Known(Datum::Moment(fields))
}

/// The microseconds a fraction of a second written with up to six digits
/// stands for.
fn format_micros(fraction: &str) -> i64 {
    fraction
        .bytes()
        .chain(core::iter::repeat(b'0'))
        .take(6)
        .fold(0, |micros, byte| micros * 10 + i64::from(byte - b'0'))
}

/// The days of `month` in `year` of the proleptic Gregorian calendar.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// One entry of a range bound once read.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RangeEntry {
    /// `MINVALUE`.
    MinValue,
    /// A value.
    Value(Datum),
    /// `MAXVALUE`.
    MaxValue,
}

/// The bound of a partition read as the key's types, with every comparison it
/// needs decided, or `None` where one stays undecided.
enum ReadBound {
    /// `FOR VALUES FROM ... TO ...`.
    Range { from: Option<Vec<RangeEntry>>, to: Option<Vec<RangeEntry>> },
    /// `FOR VALUES IN ...`: whether it lists `NULL`, and each value read.
    List { null: bool, values: Vec<Option<Datum>> },
    /// `FOR VALUES WITH ...`.
    Hash { modulus: u64, remainder: u64 },
    /// `DEFAULT`.
    Default,
}

/// Whether the expression reads a column.
fn reads_column(expr: &Expr) -> bool {
    struct Columns;
    impl Visitor for Columns {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
    }
    expr.visit(&mut Columns).is_break()
}

/// Whether the expression is a `NULL` literal.
fn is_null(expr: &Expr) -> bool {
    matches!(literal(expr), Some((_, Value::Null)))
}

/// Reads one side of a range bound, refusing `NULL`, a column reference and a
/// value after an infinity, or answering `None` when a value stays undecided.
fn read_range_side(
    kinds: &[Option<KeyKind>],
    columns: &[Option<&ColumnDef>],
    side: &[PartitionBoundValue],
) -> Result<Option<Vec<RangeEntry>>, InvalidBound> {
    if side.len() != kinds.len() {
        return Err(InvalidBound::ValueCount { expected: kinds.len(), found: side.len() });
    }
    let mut entries = Some(Vec::with_capacity(side.len()));
    let mut infinity = None;
    for (at, value) in side.iter().enumerate() {
        let entry = match value {
            PartitionBoundValue::MinValue => Some(RangeEntry::MinValue),
            PartitionBoundValue::MaxValue => Some(RangeEntry::MaxValue),
            PartitionBoundValue::Expr(expr) => {
                if reads_column(expr) {
                    return Err(InvalidBound::ColumnReference);
                }
                if is_null(expr) {
                    return Err(InvalidBound::NullInRange);
                }
                if infinity.is_some() {
                    return Err(InvalidBound::ValueAfterInfinity);
                }
                read_value(kinds[at], columns[at], expr)?.map(RangeEntry::Value)
            }
        };
        match (&infinity, value) {
            (Some(PartitionBoundValue::MinValue), PartitionBoundValue::MaxValue)
            | (Some(PartitionBoundValue::MaxValue), PartitionBoundValue::MinValue) => {
                return Err(InvalidBound::ValueAfterInfinity);
            }
            (None, PartitionBoundValue::MinValue | PartitionBoundValue::MaxValue) => {
                infinity = Some(value.clone());
            }
            _ => {}
        }
        entries = entries.zip(entry).map(|(mut entries, entry)| {
            entries.push(entry);
            entries
        });
    }
    Ok(entries)
}

/// Reads one value as its key column's kind, refusing a literal PostgreSQL
/// cannot read as the column's type, or answering `None` when undecided.
fn read_value(
    kind: Option<KeyKind>,
    column: Option<&ColumnDef>,
    expr: &Expr,
) -> Result<Option<Datum>, InvalidBound> {
    let (Some(kind), Some(column)) = (kind, column) else {
        return Ok(None);
    };
    match read(kind, expr) {
        Reading::Known(datum) => Ok(Some(datum)),
        Reading::Undecided => Ok(None),
        Reading::Invalid => {
            Err(InvalidBound::ValueNotOfKeyType {
                value: expr.to_string(),
                type_name: column.data_type.to_string(),
            })
        }
    }
}

/// Reads a bound as the key of a table partitioned by `strategy`, refusing
/// what PostgreSQL refuses of the bound alone.
fn read_bound(
    strategy: PartitionStrategy,
    columns: &[Option<&ColumnDef>],
    bound: &ForValues,
) -> Result<ReadBound, InvalidBound> {
    let kinds: Vec<Option<KeyKind>> =
        columns.iter().map(|column| column.and_then(key_kind)).collect();
    match (strategy, bound) {
        (PartitionStrategy::Hash, ForValues::Default) => Err(InvalidBound::DefaultOnHash),
        (_, ForValues::Default) => Ok(ReadBound::Default),
        (PartitionStrategy::Range, ForValues::From { from, to }) => {
            let from = read_range_side(&kinds, columns, from)?;
            let to = read_range_side(&kinds, columns, to)?;
            if let (Some(from), Some(to)) = (&from, &to)
                && from >= to
            {
                return Err(InvalidBound::EmptyRange);
            }
            Ok(ReadBound::Range { from, to })
        }
        (PartitionStrategy::List, ForValues::In(values)) => {
            let mut null = false;
            let mut read = Vec::with_capacity(values.len());
            for value in values {
                if reads_column(value) {
                    return Err(InvalidBound::ColumnReference);
                }
                if is_null(value) {
                    null = true;
                    continue;
                }
                read.push(read_value(
                    kinds.first().copied().flatten(),
                    columns.first().copied().flatten(),
                    value,
                )?);
            }
            Ok(ReadBound::List { null, values: read })
        }
        (PartitionStrategy::Hash, ForValues::With { modulus, remainder }) => {
            if *modulus == 0 {
                return Err(InvalidBound::ModulusNotPositive);
            }
            if remainder >= modulus {
                return Err(InvalidBound::RemainderNotBelowModulus);
            }
            Ok(ReadBound::Hash { modulus: *modulus, remainder: *remainder })
        }
        (strategy, _) => Err(InvalidBound::StrategyMismatch { strategy }),
    }
}

/// How a bound meets a sibling's.
enum Clash {
    /// Both take some rows.
    Overlap,
    /// Both are the default.
    SecondDefault,
    /// Their hash moduli do not divide one another.
    ModulusNotFactor,
}

/// Whether two bounds of one table take some row in common, as far as the
/// model can tell.
fn clash(new: &ReadBound, sibling: &ReadBound) -> Option<Clash> {
    match (new, sibling) {
        (ReadBound::Default, ReadBound::Default) => Some(Clash::SecondDefault),
        (
            ReadBound::Range { from: Some(from), to: Some(to) },
            ReadBound::Range { from: Some(other_from), to: Some(other_to) },
        ) => (from < other_to && other_from < to).then_some(Clash::Overlap),
        (
            ReadBound::List { null, values },
            ReadBound::List { null: other_null, values: other_values },
        ) => {
            let shared = values
                .iter()
                .flatten()
                .any(|value| other_values.iter().flatten().any(|other| other == value));
            (*null && *other_null || shared).then_some(Clash::Overlap)
        }
        (
            ReadBound::Hash { modulus, remainder },
            ReadBound::Hash { modulus: other_modulus, remainder: other_remainder },
        ) => {
            let (low, high) = (*modulus.min(other_modulus), *modulus.max(other_modulus));
            if high % low != 0 {
                return Some(Clash::ModulusNotFactor);
            }
            (remainder % low == other_remainder % low).then_some(Clash::Overlap)
        }
        _ => None,
    }
}

/// Refuses the bound `partition_name` would take in `root` when PostgreSQL
/// refuses it, alone or against a partition `root` already has.
pub(super) fn validate_bound(
    builder: &ParserDBBuilder,
    root: &CreateTable,
    partition_name: &str,
    bound: &ForValues,
) -> Result<(), Error> {
    let Some((strategy, columns)) = partition_keys::key_columns(root, builder.identifier_case())
    else {
        return Ok(());
    };
    let invalid = |reason| {
        Error::PartitionBoundInvalid {
            table_name: root.table_name().to_string(),
            partition_name: partition_name.to_string(),
            reason,
        }
    };
    let new = read_bound(strategy, &columns, bound).map_err(invalid)?;
    let partitions = inheritance::direct_children(builder, &StoredTable::of(root))
        .into_iter()
        .filter(|(kind, _)| *kind == inheritance::ParentKind::PartitionOf);
    for (_, sibling) in partitions {
        let sibling_node = ParserDB::stored_node(builder, &sibling)?;
        // A sibling's bound was checked when it was given, so it reads.
        let sibling_read = sibling_node
            .for_values
            .as_ref()
            .and_then(|sibling_bound| read_bound(strategy, &columns, sibling_bound).ok());
        let other_partition = sibling_node.table_name().to_string();
        match sibling_read.and_then(|sibling_read| clash(&new, &sibling_read)) {
            Some(Clash::Overlap) => {
                return Err(Error::PartitionBoundOverlaps {
                    table_name: root.table_name().to_string(),
                    partition_name: partition_name.to_string(),
                    other_partition,
                });
            }
            Some(Clash::SecondDefault) => {
                return Err(invalid(InvalidBound::DefaultExists {
                    default_partition: other_partition,
                }));
            }
            Some(Clash::ModulusNotFactor) => {
                return Err(invalid(InvalidBound::ModulusNotFactor { other_partition }));
            }
            None => {}
        }
    }
    Ok(())
}
