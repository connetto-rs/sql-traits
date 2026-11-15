//! Temporal array elements and range bounds under the creating session.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
};

use sql_values::{DateTimeError, InputContext, PgDate, PgTimestamp, PgTimestampTz};

use super::Temporal;

/// Decodes one container field, leaving its delimiter unconsumed.
fn field<'a>(text: &mut &'a str, delimiters: &[char], range: bool) -> Option<(Cow<'a, str>, bool)> {
    *text = text.trim_start();
    let source = *text;
    let quoted = source.starts_with('"');
    let start = usize::from(quoted);
    let mut escaped = false;
    let mut decoded = None::<String>;
    let mut copied = start;
    let mut characters = source.char_indices().skip(start).peekable();
    while let Some((index, character)) = characters.next() {
        if character == '\\' {
            let (next, escaped_character) = characters.next()?;
            let output = decoded.get_or_insert_with(String::new);
            output.push_str(&source[copied..index]);
            output.push(escaped_character);
            copied = next + escaped_character.len_utf8();
            escaped = true;
        } else if quoted && character == '"' {
            if range && characters.peek().is_some_and(|(_, next)| *next == '"') {
                characters.next();
                let output = decoded.get_or_insert_with(String::new);
                output.push_str(&source[copied..index]);
                output.push('"');
                copied = index + 2;
            } else {
                *text = source[index + 1..].trim_start();
                let value = match decoded {
                    Some(mut output) => {
                        output.push_str(&source[copied..index]);
                        Cow::Owned(output)
                    }
                    None => Cow::Borrowed(&source[start..index]),
                };
                return Some((value, true));
            }
        } else if !quoted && delimiters.contains(&character) {
            *text = &source[index..];
            let value = match decoded {
                Some(mut output) => {
                    output.push_str(source[copied..index].trim_end());
                    Cow::Owned(output)
                }
                None => Cow::Borrowed(source[..index].trim_end()),
            };
            return Some((value, escaped));
        }
    }
    None
}

/// Quotes a stored container member when its output grammar requires it.
fn member(output: &mut String, value: &str, range: bool) {
    let quote = value.is_empty()
        || (!range && value.eq_ignore_ascii_case("NULL"))
        || value.chars().any(|character| {
            character.is_ascii_whitespace()
                || matches!(character, '"' | '\\' | ',')
                || if range {
                    matches!(character, '(' | ')' | '[' | ']')
                } else {
                    matches!(character, '{' | '}')
                }
        });
    if quote {
        output.push('"');
    }
    for character in value.chars() {
        if matches!(character, '"' | '\\') {
            output.push('\\');
        }
        output.push(character);
    }
    if quote {
        output.push('"');
    }
}

/// Reads an array body and its dimensions, with PostgreSQL's six-dimension
/// limit.
fn array_body(
    text: &mut &str,
    temporal: Temporal,
    context: InputContext<'_>,
    output: &mut String,
    depth: usize,
) -> Result<([usize; 6], usize), DateTimeError> {
    let invalid = DateTimeError::InvalidInputSyntax(temporal.pg_type());
    if depth == 6 {
        return Err(invalid);
    }
    *text = text.trim_start().strip_prefix('{').ok_or(invalid)?.trim_start();
    output.push('{');
    let mut dimensions = [0; 6];
    let mut dimensions_count = 1;
    let nested = text.starts_with('{');
    while !text.starts_with('}') {
        if dimensions[0] != 0 {
            output.push(',');
        }
        if nested {
            let (child, count) = array_body(text, temporal, context, output, depth + 1)?;
            if dimensions[0] != 0
                && (dimensions_count != count + 1 || dimensions[1..=count] != child[..count])
            {
                return Err(invalid);
            }
            dimensions[1..=count].copy_from_slice(&child[..count]);
            dimensions_count = count + 1;
        } else {
            let (value, protected) = field(text, &[',', '}'], false).ok_or(invalid)?;
            if !protected && value.eq_ignore_ascii_case("NULL") {
                output.push_str("NULL");
            } else {
                member(output, &temporal.read(&value, context)?, false);
            }
        }
        dimensions[0] += 1;
        *text = text.trim_start();
        if let Some(rest) = text.strip_prefix(',') {
            *text = rest.trim_start();
            if text.starts_with('}') {
                return Err(invalid);
            }
        } else if !text.starts_with('}') {
            return Err(invalid);
        }
    }
    *text = &text[1..];
    output.push('}');
    Ok((dimensions, dimensions_count))
}

/// Reads each array element under the creating session.
pub(super) fn read_array(
    text: &str,
    temporal: Temporal,
    context: InputContext<'_>,
) -> Result<String, DateTimeError> {
    let invalid = DateTimeError::InvalidInputSyntax(temporal.pg_type());
    let mut text = text.trim();
    let mut bounds = [(1_i32, 0_i32); 6];
    let mut count = 0;
    while let Some(rest) = text.strip_prefix('[') {
        let (dimension, rest) = rest.split_once(']').ok_or(invalid)?;
        let (lower, upper) = dimension.split_once(':').ok_or(invalid)?;
        let slot = bounds.get_mut(count).ok_or(invalid)?;
        *slot = (
            lower.trim().parse().map_err(|_| invalid)?,
            upper.trim().parse().map_err(|_| invalid)?,
        );
        count += 1;
        text = rest.trim_start();
    }
    if count != 0 {
        text = text.strip_prefix('=').ok_or(invalid)?;
    }
    let mut output = String::new();
    if bounds[..count].iter().any(|(lower, _)| *lower != 1) {
        use core::fmt::Write;
        for (lower, upper) in &bounds[..count] {
            let _ = write!(output, "[{lower}:{upper}]");
        }
        output.push('=');
    }
    let (dimensions, dimensions_count) = array_body(&mut text, temporal, context, &mut output, 0)?;
    if !text.trim().is_empty()
        || (count != 0
            && (count != dimensions_count
                || bounds[..count].iter().zip(dimensions).any(|(&(lower, upper), length)| {
                    i64::from(upper) - i64::from(lower) + 1 != i64::try_from(length).unwrap_or(-1)
                })))
    {
        return Err(invalid);
    }
    if dimensions[..dimensions_count].contains(&0) {
        return Ok("{}".to_string());
    }
    Ok(output)
}

/// A range bound whose ordering is the subtype's ordering.
#[derive(PartialEq, PartialOrd)]
enum RangeBound {
    Date(PgDate),
    Timestamp(PgTimestamp),
    TimestampTz(PgTimestampTz),
}

impl RangeBound {
    fn read(
        text: &str,
        temporal: Temporal,
        context: InputContext<'_>,
    ) -> Result<Self, DateTimeError> {
        match temporal {
            Temporal::Date => PgDate::parse(text, context).map(Self::Date),
            Temporal::Timestamp => PgTimestamp::parse(text, context).map(Self::Timestamp),
            Temporal::TimestampTz => PgTimestampTz::parse(text, context).map(Self::TimestampTz),
            _ => Err(DateTimeError::InvalidInputSyntax(temporal.pg_type())),
        }
    }

    fn next_date(&mut self) -> Result<bool, DateTimeError> {
        if let Self::Date(date) = self
            && date.is_finite()
        {
            *date =
                PgDate::from_pg_days(date.pg_days() + 1).ok_or(DateTimeError::DateOutOfRange)?;
            return Ok(true);
        }
        Ok(false)
    }

    fn write(&self, output: &mut String) {
        let text = match self {
            Self::Date(value) => value.to_string(),
            Self::Timestamp(value) => value.to_string(),
            Self::TimestampTz(value) => value.to_string(),
        };
        member(output, &text, true);
    }
}

/// Reads range bounds and canonicalizes discrete date ranges.
pub(super) fn read_range(
    text: &str,
    temporal: Temporal,
    context: InputContext<'_>,
) -> Result<String, DateTimeError> {
    let invalid = DateTimeError::InvalidInputSyntax(temporal.pg_type());
    let mut text = text.trim();
    if text == "empty" {
        return Ok(text.to_string());
    }
    let mut lower_inclusive = text.starts_with('[');
    if !lower_inclusive && !text.starts_with('(') {
        return Err(invalid);
    }
    text = &text[1..];
    let (lower, lower_quoted) = field(&mut text, &[','], true).ok_or(invalid)?;
    text = text.strip_prefix(',').ok_or(invalid)?;
    let (upper, upper_quoted) = field(&mut text, &[']', ')'], true).ok_or(invalid)?;
    let mut upper_inclusive = text == "]";
    if !upper_inclusive && text != ")" {
        return Err(invalid);
    }
    let mut lower = if lower.is_empty() && !lower_quoted {
        None
    } else {
        Some(RangeBound::read(&lower, temporal, context)?)
    };
    let mut upper = if upper.is_empty() && !upper_quoted {
        None
    } else {
        Some(RangeBound::read(&upper, temporal, context)?)
    };
    lower_inclusive &= lower.is_some();
    upper_inclusive &= upper.is_some();
    if let (Some(lower), Some(upper)) = (&lower, &upper) {
        if lower > upper {
            return Err(invalid);
        }
        if lower == upper && !(lower_inclusive && upper_inclusive) {
            return Ok("empty".to_string());
        }
    }
    if temporal == Temporal::Date {
        if let Some(lower) = &mut lower
            && !lower_inclusive
        {
            lower_inclusive = lower.next_date()?;
        }
        if let Some(upper) = &mut upper
            && upper_inclusive
        {
            upper_inclusive = !upper.next_date()?;
        }
        if lower.is_some() && lower == upper && !(lower_inclusive && upper_inclusive) {
            return Ok("empty".to_string());
        }
    }
    let mut output = String::new();
    output.push(if lower_inclusive { '[' } else { '(' });
    if let Some(lower) = lower {
        lower.write(&mut output);
    }
    output.push(',');
    if let Some(upper) = upper {
        upper.write(&mut output);
    }
    output.push(if upper_inclusive { ']' } else { ')' });
    Ok(output)
}
