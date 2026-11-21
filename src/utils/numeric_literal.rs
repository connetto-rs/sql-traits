//! Reads the numeric literals PostgreSQL accepts as option values, such as a
//! sequence's `INCREMENT` or a function's `ROWS`.

use sqlparser::ast::{Expr, UnaryOperator, Value, ValueWithSpan};

/// Whether a minus sign negates the numeric literal `value`, and the
/// literal's digits, since PostgreSQL's `NumericOnly` writes an option value
/// as a literal with an optional sign. [`None`] for any other expression.
pub(crate) fn signed_number(value: &Expr) -> Option<(bool, &str)> {
    let (negative, literal) = match value {
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => (true, expr.as_ref()),
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => (false, expr.as_ref()),
        literal => (false, literal),
    };
    let Expr::Value(ValueWithSpan { value: Value::Number(digits, _), .. }) = literal else {
        return None;
    };
    Some((negative, digits))
}

/// Reads `digits` the way PostgreSQL's `bigint` input does: decimal, or
/// hexadecimal, octal or binary behind `0x`, `0o` or `0b`, with single
/// underscores between digits or after the prefix.
pub(crate) fn parse_integer(digits: &str, negative: bool) -> Option<i64> {
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

/// The value PostgreSQL's `defGetNumeric` reads from the literal `digits`.
///
/// The lexer keeps an integer literal that fits `integer` as that integer.
/// Any other literal reaches `atof` as written, which reads the longest
/// prefix `strtod` accepts, so it stops at the first underscore and at the
/// `o` or `b` of an octal or binary prefix.
fn numeric_option(digits: &str, negative: bool) -> f64 {
    let magnitude = match parse_integer(digits, false).and_then(|value| i32::try_from(value).ok()) {
        Some(integer) => f64::from(integer),
        None => strtod_prefix(digits),
    };
    if negative { -magnitude } else { magnitude }
}

/// The value PostgreSQL stores in a `float4` catalog column such as
/// `pg_proc.prorows` for the option value `value`. `defGetNumeric` reads it
/// and the column narrows it, so a magnitude past the `float4` range is
/// infinite and one below it is zero. [`None`] for anything but a numeric
/// literal.
pub(crate) fn float4_option(value: &Expr) -> Option<f32> {
    let (negative, digits) = signed_number(value)?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "PostgreSQL narrows the option value to a float4 the same way"
    )]
    Some(numeric_option(digits, negative) as f32)
}

/// The value C's `strtod` reads from the start of `text`, which sqlparser
/// tokenized as a number. That is the hexadecimal digits behind `0x`, or else
/// the longest decimal mantissa with an optional exponent, and zero when no
/// digit starts it.
fn strtod_prefix(text: &str) -> f64 {
    if let Some(hexadecimal) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return hexadecimal
            .chars()
            .map_while(|character| character.to_digit(16))
            .fold(0.0, |value, digit| value * 16.0 + f64::from(digit));
    }
    let bytes = text.as_bytes();
    let digits_from = |start: usize| {
        start + bytes.iter().skip(start).take_while(|byte| byte.is_ascii_digit()).count()
    };
    let mut end = digits_from(0);
    if bytes.get(end) == Some(&b'.') {
        end = digits_from(end + 1);
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent_start = end + 1 + usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let exponent_end = digits_from(exponent_start);
        if exponent_end > exponent_start {
            end = exponent_end;
        }
    }
    text[..end].parse().unwrap_or(0.0)
}
