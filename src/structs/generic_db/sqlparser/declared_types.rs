//! The declared type SQLite records for a column, read from the text of its
//! declaration the way `sqlite3AddColumn` reads it and reported the way
//! `sqlite3ColumnType` reports it.

use alloc::{
    borrow::{Cow, ToOwned},
    string::String,
    vec::Vec,
};

use sqlparser::ast::ColumnDef;

use super::{column_texts::ColumnTexts, parsed_source::SourceText};

/// The type names SQLite stores as a code rather than as text, each reported
/// in this spelling however the declaration wrote it.
const STANDARD_TYPES: [&str; 6] = ["ANY", "BLOB", "INT", "INTEGER", "REAL", "TEXT"];

/// The keywords opening a column constraint, which no type name absorbs.
const CONSTRAINT_KEYWORDS: [&str; 10] = [
    "AS",
    "CHECK",
    "COLLATE",
    "CONSTRAINT",
    "DEFAULT",
    "NOT",
    "NULL",
    "PRIMARY",
    "REFERENCES",
    "UNIQUE",
];

/// The declared type texts of `columns`, read from `text`, each column whose
/// declaration `text` does not hold left out.
pub(super) fn read<'column>(
    columns: impl IntoIterator<Item = &'column ColumnDef>,
    text: SourceText<'_>,
) -> ColumnTexts {
    columns
        .into_iter()
        .filter_map(|column| Some((column.name.clone(), declared_type(column, text)?)))
        .collect()
}

/// The declared type SQLite reports for `column`, whose declaration `text`
/// holds, or `None` when `text` does not hold it.
pub(super) fn declared_type(column: &ColumnDef, text: SourceText<'_>) -> Option<String> {
    let after_name = text.sql.get(text.offset(column.name.span.end)?..)?;
    Some(catalog_text(type_span(after_name)?.as_bytes()))
}

/// The type token `sqlite3AddColumn` receives for a column whose name `rest`
/// follows, from its first word to its last word or closing parenthesis, the
/// whitespace and comments between them included, or `None` for text
/// SQLite's tokenizer refuses.
fn type_span(rest: &str) -> Option<&str> {
    let bytes = rest.as_bytes();
    let mut span: Option<(usize, usize)> = None;
    let mut position = 0;
    loop {
        let start = skip_space(bytes, position);
        let Some(&first) = bytes.get(start) else { break };
        let end = match first {
            b'"' | b'\'' | b'`' => quoted_end(bytes, start)?,
            b'[' => start + bytes[start..].iter().position(|&byte| byte == b']')? + 1,
            b'(' => {
                let (opening, _) = span?;
                span = Some((opening, closing_parenthesis_end(bytes, start)?));
                break;
            }
            byte if is_identifier_start(byte) && !is_blob_literal(bytes, start) => {
                let end = bytes[start..]
                    .iter()
                    .position(|&byte| !is_identifier_byte(byte))
                    .map_or(bytes.len(), |length| start + length);
                let word = &bytes[start..end];
                if CONSTRAINT_KEYWORDS
                    .iter()
                    .any(|keyword| keyword.as_bytes().eq_ignore_ascii_case(word))
                {
                    break;
                }
                end
            }
            _ => break,
        };
        span = Some((span.map_or(start, |(opening, _)| opening), end));
        position = end;
    }
    Some(span.map_or("", |(opening, end)| &rest[opening..end]))
}

/// The position after the whitespace and comments starting at `position`.
fn skip_space(bytes: &[u8], mut position: usize) -> usize {
    loop {
        match bytes.get(position..) {
            Some([b' ' | b'\t' | b'\n' | b'\x0c' | b'\r', ..]) => position += 1,
            Some([b'-', b'-', ..]) => {
                position = bytes[position..]
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map_or(bytes.len(), |length| position + length);
            }
            Some([b'/', b'*', ..]) => {
                position = bytes[position + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(bytes.len(), |length| position + 2 + length + 2);
            }
            _ => return position,
        }
    }
}

/// The position after the quoted token opening at `start`, a doubled quote
/// standing for itself, or `None` when no quote closes it.
fn quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut position = start + 1;
    loop {
        let length = bytes.get(position..)?.iter().position(|&byte| byte == quote)?;
        position += length + 1;
        if bytes.get(position) != Some(&quote) {
            return Some(position);
        }
        position += 1;
    }
}

/// The position after the parenthesis closing the parameter list opening at
/// `start`.
fn closing_parenthesis_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut position = start + 1;
    loop {
        position = skip_space(bytes, position);
        match bytes.get(position)? {
            b')' => return Some(position + 1),
            _ => position += 1,
        }
    }
}

/// Whether `byte` opens an identifier or keyword for SQLite's tokenizer.
const fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

/// Whether `byte` continues an identifier or keyword for SQLite's tokenizer.
const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

/// Whether a blob literal, `x'…'`, opens at `start`.
fn is_blob_literal(bytes: &[u8], start: usize) -> bool {
    matches!(bytes.get(start..), Some([b'x' | b'X', b'\'', ..]))
}

/// Whether SQLite reads `byte` as an opening quote.
const fn is_quote(byte: u8) -> bool {
    matches!(byte, b'"' | b'\'' | b'`' | b'[')
}

/// Whether `byte` is a space for `sqlite3Isspace`.
const fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t'..=b'\r')
}

/// The text `sqlite3ColumnType` reports for the type token `declared`.
///
/// `sqlite3AddColumn` first strips a trailing `GENERATED ALWAYS` the type
/// absorbed, then unquotes a token quoted whole, then stores a standard type
/// name as a code, which reads back in its own spelling, and any other type
/// as its text with the leading quoted part unquoted.
fn catalog_text(declared: &[u8]) -> String {
    let mut declared = declared;
    if declared.len() >= 16 && ends_with_ignoring_case(declared, b"always") {
        declared = trim_end_space(&declared[..declared.len() - 6]);
        if declared.len() >= 9 && ends_with_ignoring_case(declared, b"generated") {
            declared = trim_end_space(&declared[..declared.len() - 9]);
        }
    }
    if declared.len() >= 3 {
        declared = dequote_token(declared);
        if let Some(standard) = STANDARD_TYPES
            .iter()
            .find(|standard| standard.as_bytes().eq_ignore_ascii_case(declared))
        {
            return (*standard).to_owned();
        }
    }
    String::from_utf8_lossy(&dequote(declared)).into_owned()
}

fn ends_with_ignoring_case(text: &[u8], suffix: &[u8]) -> bool {
    text.len() >= suffix.len() && text[text.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn trim_end_space(text: &[u8]) -> &[u8] {
    let kept = text.iter().rposition(|&byte| !is_space(byte)).map_or(0, |last| last + 1);
    &text[..kept]
}

/// `text` without its first and last byte when it opens with a quote and
/// holds no other quote before its last byte, as `sqlite3DequoteToken`
/// reads it.
fn dequote_token(text: &[u8]) -> &[u8] {
    match text {
        [first, inner @ .., _] if is_quote(*first) && !inner.iter().copied().any(is_quote) => inner,
        _ => text,
    }
}

/// `text` with its leading quoted part unquoted and everything after that
/// part dropped, as `sqlite3Dequote` reads it, or `text` itself when it
/// opens with no quote.
fn dequote(text: &[u8]) -> Cow<'_, [u8]> {
    let Some((&opening, rest)) = text.split_first().filter(|(first, _)| is_quote(**first)) else {
        return Cow::Borrowed(text);
    };
    let closing = if opening == b'[' { b']' } else { opening };
    let mut inside = Vec::with_capacity(rest.len());
    let mut bytes = rest.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == closing && bytes.next_if_eq(&closing).is_none() {
            break;
        }
        inside.push(byte);
    }
    Cow::Owned(inside)
}
