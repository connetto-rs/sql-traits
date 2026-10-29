//! Classifies declared SQL types by scalar value family.

use core::iter::Peekable;

/// A builtin scalar family that remains exhaustive so new families break
/// consumers that classify every value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarFamily {
    /// Boolean values.
    Bool,
    /// Signed or unsigned integer values.
    Int,
    /// Floating-point values.
    Float,
    /// Exact decimal values.
    Decimal,
    /// Text values.
    String,
    /// Binary values.
    Bytes,
    /// Universally unique identifiers.
    Uuid,
    /// Calendar dates.
    Date,
    /// Times of day.
    Time,
    /// Timestamps without a timezone.
    Timestamp,
    /// Timestamps with a timezone.
    TimestampTz,
    /// Durations, as an interval type holds them.
    Interval,
    /// JSON values.
    Json,
    /// Binary JSON values.
    Jsonb,
}

impl ScalarFamily {
    pub(crate) const fn token(self) -> &'static str {
        match self {
            Self::Bool => "BOOL",
            Self::Int => "INT",
            Self::Float => "FLOAT",
            Self::Decimal => "DECIMAL",
            Self::String => "STRING",
            Self::Bytes => "BYTES",
            Self::Uuid => "UUID",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::Timestamp => "TIMESTAMP",
            Self::TimestampTz => "TIMESTAMPTZ",
            Self::Interval => "INTERVAL",
            Self::Json => "JSON",
            Self::Jsonb => "JSONB",
        }
    }
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Word(&'a str),
    Parameters,
    Invalid,
}

struct Tokens<'a> {
    remaining: &'a str,
}

impl<'a> Tokens<'a> {
    const fn new(input: &'a str) -> Self {
        Self { remaining: input }
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining = self.remaining.trim_start_matches(char::is_whitespace);
        let first = self.remaining.chars().next()?;

        if first == '(' {
            let mut depth = 0_u32;
            for (index, ch) in self.remaining.char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            self.remaining = &self.remaining[index + ch.len_utf8()..];
                            return Some(Token::Parameters);
                        }
                    }
                    _ => {}
                }
            }
            self.remaining = "";
            return Some(Token::Invalid);
        }

        if matches!(first, ')' | ',' | '[' | ']') {
            self.remaining = &self.remaining[first.len_utf8()..];
            return Some(Token::Invalid);
        }

        let end = self
            .remaining
            .char_indices()
            .find_map(|(index, ch)| {
                (ch.is_whitespace() || matches!(ch, '(' | ')' | ',' | '[' | ']')).then_some(index)
            })
            .unwrap_or(self.remaining.len());
        let word = &self.remaining[..end];
        self.remaining = &self.remaining[end..];
        Some(Token::Word(word))
    }
}

fn is_word(token: Option<Token<'_>>, expected: &str) -> bool {
    matches!(token, Some(Token::Word(word)) if word.eq_ignore_ascii_case(expected))
}

fn consume_parameters(tokens: &mut Peekable<Tokens<'_>>) {
    if matches!(tokens.peek(), Some(Token::Parameters)) {
        tokens.next();
    }
}

fn has_no_tail(tokens: &mut Peekable<Tokens<'_>>) -> bool {
    tokens.next().is_none()
}

fn integer_tail(tokens: &mut Peekable<Tokens<'_>>) -> bool {
    consume_parameters(tokens);
    if matches!(tokens.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case("UNSIGNED")) {
        tokens.next();
    }
    has_no_tail(tokens)
}

fn simple_parameterized_tail(tokens: &mut Peekable<Tokens<'_>>) -> bool {
    consume_parameters(tokens);
    has_no_tail(tokens)
}

fn timezone_tail(tokens: &mut Peekable<Tokens<'_>>) -> Option<bool> {
    consume_parameters(tokens);
    let Some(token) = tokens.next() else {
        return Some(false);
    };

    if is_word(Some(token), "WITH") {
        let time = tokens.next();
        if is_word(time, "TIMEZONE") {
            return has_no_tail(tokens).then_some(true);
        }
        if is_word(time, "TIME") && is_word(tokens.next(), "ZONE") && has_no_tail(tokens) {
            return Some(true);
        }
        return None;
    }

    if is_word(Some(token), "WITHOUT")
        && is_word(tokens.next(), "TIME")
        && is_word(tokens.next(), "ZONE")
        && has_no_tail(tokens)
    {
        return Some(false);
    }

    None
}

fn is_interval_field(token: Option<Token<'_>>) -> bool {
    ["YEAR", "MONTH", "DAY", "HOUR", "MINUTE", "SECOND"].iter().any(|field| is_word(token, field))
}

/// Accepts what may follow `INTERVAL`: a precision alone, or a field, a
/// field range such as `DAY TO SECOND`, and an optional precision.
fn interval_tail(tokens: &mut Peekable<Tokens<'_>>) -> bool {
    if matches!(tokens.peek(), Some(Token::Parameters)) {
        tokens.next();
        return has_no_tail(tokens);
    }
    let Some(first) = tokens.next() else {
        return true;
    };
    if !is_interval_field(Some(first)) {
        return false;
    }
    if is_word(tokens.peek().copied(), "TO") {
        tokens.next();
        if !is_interval_field(tokens.next()) {
            return false;
        }
    }
    simple_parameterized_tail(tokens)
}

fn integer_family(base: &str) -> bool {
    [
        "INT",
        "INTEGER",
        "SMALLINT",
        "BIGINT",
        "TINYINT",
        "MEDIUMINT",
        "INT2",
        "INT4",
        "INT8",
        "SERIAL",
        "SMALLSERIAL",
        "BIGSERIAL",
        "HUGEINT",
        "INT16",
        "INT32",
        "INT64",
        "INT128",
        "INT256",
        "UINT8",
        "UINT16",
        "UINT32",
        "UINT64",
        "UINT128",
        "UINT256",
    ]
    .iter()
    .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

fn float_family(base: &str) -> bool {
    ["FLOAT", "REAL", "FLOAT4", "FLOAT8", "FLOAT32", "FLOAT64"]
        .iter()
        .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

fn decimal_family(base: &str) -> bool {
    ["DECIMAL", "NUMERIC", "NUMBER", "MONEY"]
        .iter()
        .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

fn string_family(base: &str) -> bool {
    [
        "TEXT",
        "VARCHAR",
        "CHAR",
        "NVARCHAR",
        "NCHAR",
        "CLOB",
        "STRING",
        "TINYTEXT",
        "MEDIUMTEXT",
        "LONGTEXT",
        "FIXEDSTRING",
    ]
    .iter()
    .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

fn bytes_family(base: &str) -> bool {
    ["BYTEA", "BLOB", "BINARY", "VARBINARY", "BYTES", "TINYBLOB", "MEDIUMBLOB", "LONGBLOB"]
        .iter()
        .any(|candidate| base.eq_ignore_ascii_case(candidate))
}

/// Returns the builtin scalar family of a declared SQL type.
///
/// Tokenization advances through input segments once without heap allocation,
/// while bounded case-insensitive comparisons may reread short token slices.
#[must_use]
#[inline]
pub fn scalar_family(declared_type: &str) -> Option<ScalarFamily> {
    let declared_type = declared_type.trim();
    let declared_type = declared_type
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(declared_type);
    let mut tokens = Tokens::new(declared_type).peekable();
    let Some(Token::Word(base)) = tokens.next() else {
        return None;
    };

    if integer_family(base) {
        return integer_tail(&mut tokens).then_some(ScalarFamily::Int);
    }
    if float_family(base) {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::Float);
    }
    if base.eq_ignore_ascii_case("DOUBLE") {
        if matches!(tokens.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case("PRECISION"))
        {
            tokens.next();
        }
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::Float);
    }
    if decimal_family(base) {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::Decimal);
    }
    if base.eq_ignore_ascii_case("BOOL") || base.eq_ignore_ascii_case("BOOLEAN") {
        return has_no_tail(&mut tokens).then_some(ScalarFamily::Bool);
    }
    if base.eq_ignore_ascii_case("CHARACTER") {
        if matches!(tokens.peek(), Some(Token::Word(word)) if word.eq_ignore_ascii_case("VARYING"))
        {
            tokens.next();
        }
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::String);
    }
    if string_family(base) {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::String);
    }
    if bytes_family(base) {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::Bytes);
    }
    if base.eq_ignore_ascii_case("DATE") || base.eq_ignore_ascii_case("DATE32") {
        return has_no_tail(&mut tokens).then_some(ScalarFamily::Date);
    }
    if base.eq_ignore_ascii_case("TIME") {
        return timezone_tail(&mut tokens).map(|_| ScalarFamily::Time);
    }
    if base.eq_ignore_ascii_case("TIMETZ") {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::Time);
    }
    if base.eq_ignore_ascii_case("TIMESTAMP") {
        return timezone_tail(&mut tokens).map(|with_timezone| {
            if with_timezone { ScalarFamily::TimestampTz } else { ScalarFamily::Timestamp }
        });
    }
    if base.eq_ignore_ascii_case("TIMESTAMPTZ") {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::TimestampTz);
    }
    if base.eq_ignore_ascii_case("DATETIME") || base.eq_ignore_ascii_case("DATETIME64") {
        return simple_parameterized_tail(&mut tokens).then_some(ScalarFamily::Timestamp);
    }
    if base.eq_ignore_ascii_case("UUID") {
        return has_no_tail(&mut tokens).then_some(ScalarFamily::Uuid);
    }
    if base.eq_ignore_ascii_case("JSONB") {
        return has_no_tail(&mut tokens).then_some(ScalarFamily::Jsonb);
    }
    if base.eq_ignore_ascii_case("JSON") {
        return has_no_tail(&mut tokens).then_some(ScalarFamily::Json);
    }
    if base.eq_ignore_ascii_case("INTERVAL") {
        return interval_tail(&mut tokens).then_some(ScalarFamily::Interval);
    }

    None
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use proptest::prelude::*;

    use super::{ScalarFamily, scalar_family};
    use crate::utils::fingerprint_type_token::canonical_type_token;

    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/scalar_family_cases.rs"));

    fn expected_token(family: ScalarFamily) -> &'static str {
        match family {
            ScalarFamily::Bool => "BOOL",
            ScalarFamily::Int => "INT",
            ScalarFamily::Float => "FLOAT",
            ScalarFamily::Decimal => "DECIMAL",
            ScalarFamily::String => "STRING",
            ScalarFamily::Bytes => "BYTES",
            ScalarFamily::Uuid => "UUID",
            ScalarFamily::Date => "DATE",
            ScalarFamily::Time => "TIME",
            ScalarFamily::Timestamp => "TIMESTAMP",
            ScalarFamily::TimestampTz => "TIMESTAMPTZ",
            ScalarFamily::Interval => "INTERVAL",
            ScalarFamily::Json => "JSON",
            ScalarFamily::Jsonb => "JSONB",
        }
    }

    fn assert_cases(cases: impl Iterator<Item = ScalarFamilyCase>) {
        for (declared_type, expected) in cases {
            assert_eq!(scalar_family(declared_type), expected, "failed for {declared_type}");
            if let Some(expected) = expected {
                assert_eq!(
                    canonical_type_token(declared_type),
                    expected_token(expected),
                    "token failed for {declared_type}"
                );
            }
        }
    }

    #[test]
    fn classifies_shared_corpus() {
        assert_cases(all_scalar_family_cases());
    }

    fn push_whitespace(output: &mut String, width: usize, kind: u8) {
        let whitespace = match kind {
            0 => ' ',
            1 => '\t',
            2 => '\n',
            _ => '\r',
        };
        for _ in 0..width {
            output.push(whitespace);
        }
    }

    fn vary_declaration(
        declared_type: &str,
        case_bits: u128,
        leading_width: usize,
        trailing_width: usize,
        whitespace_width: usize,
        whitespace_kind: u8,
    ) -> String {
        let mut varied = String::new();
        push_whitespace(&mut varied, leading_width, whitespace_kind);

        let case_bytes = case_bits.to_le_bytes();
        let mut in_whitespace = false;
        for (index, character) in declared_type.chars().enumerate() {
            if character.is_whitespace() {
                if !in_whitespace {
                    push_whitespace(&mut varied, whitespace_width, whitespace_kind);
                }
                in_whitespace = true;
                continue;
            }

            in_whitespace = false;
            let case_byte = case_bytes[index % case_bytes.len()];
            let uppercase = case_byte & (1_u8 << (index % 8)) != 0;
            varied.push(if uppercase {
                character.to_ascii_uppercase()
            } else {
                character.to_ascii_lowercase()
            });
        }

        push_whitespace(&mut varied, trailing_width, whitespace_kind);
        varied
    }

    proptest! {
        #[test]
        fn generated_declarations_have_independent_expectations(
            case_bits in any::<u128>(),
            leading_width in 0_usize..=3,
            trailing_width in 0_usize..=3,
            whitespace_width in 1_usize..=4,
            whitespace_kind in 0_u8..4,
        ) {
            for &(declared_type, expected_family) in FAMILY_CASES
                .iter()
                .chain(DECORATED_CASES.iter())
                .chain(QUOTED_POSTGRES_CASES.iter())
            {
                let Some(expected_family) = expected_family else {
                    unreachable!()
                };
                let generated = vary_declaration(
                    declared_type,
                    case_bits,
                    leading_width,
                    trailing_width,
                    whitespace_width,
                    whitespace_kind,
                );
                prop_assert_eq!(
                    scalar_family(&generated),
                    Some(expected_family),
                    "family failed for {:?}",
                    generated
                );
                prop_assert_eq!(
                    canonical_type_token(&generated),
                    expected_token(expected_family),
                    "token failed for {:?}",
                    generated
                );
            }
        }
    }
}
