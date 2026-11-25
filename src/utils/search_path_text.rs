//! `search_path` as the text PostgreSQL stores and prints for it.
//!
//! PostgreSQL keeps a list setting as one string, each `search_path` entry
//! quoted as an identifier needs, and splits it again with
//! `SplitIdentifierString` whenever it looks a schema up.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{Expr, Value, ValueWithSpan};

use crate::utils::{
    identifier_resolution::stored_identifier, object_name::setting_value,
    quote_identifier::quote_identifier,
};

/// The text PostgreSQL stores for `search_path` set to `values`, or [`None`]
/// for a value its grammar refuses there.
///
/// A number prints as PostgreSQL spells it, and every other entry as the
/// identifier it names, quoted when it needs quotes, so `'App'` prints
/// `"App"` and the string `'a, b'` stays one entry.
pub(crate) fn printed_search_path(
    values: &[Expr],
    hex_strings_may_be_integers: bool,
) -> Option<String> {
    let mut printed = Vec::with_capacity(values.len());
    for value in values {
        let (text, exact) = setting_value(value, hex_strings_may_be_integers)?;
        let number = matches!(
            value,
            Expr::UnaryOp { .. }
                | Expr::Value(ValueWithSpan {
                    value: Value::Number(..) | Value::HexStringLiteral(_),
                    ..
                })
        );
        printed.push(if number {
            text.into_owned()
        } else {
            quote_identifier(&stored_identifier(&text, exact))
        });
    }
    Some(printed.join(", "))
}

/// The schemas `text` lists, each as stored and whether it was quoted, as
/// PostgreSQL's `SplitIdentifierString` reads a `search_path`, or [`None`]
/// for text it refuses as list syntax.
///
/// Entries are separated by commas, with whitespace around them ignored. A
/// double-quoted entry keeps its case, `""` standing for one quote, and any
/// other entry folds to lower case. Empty text is the empty path.
pub(crate) fn split_search_path(text: &str) -> Option<Vec<(String, bool)>> {
    let mut entries = Vec::new();
    let mut rest = text.trim_start_matches(is_space);
    if rest.is_empty() {
        return Some(entries);
    }
    loop {
        let after = if let Some(quoted) = rest.strip_prefix('"') {
            let mut name = String::new();
            let mut characters = quoted.char_indices();
            let end = loop {
                let (index, character) = characters.next()?;
                if character != '"' {
                    name.push(character);
                } else if quoted[index + 1..].starts_with('"') {
                    name.push('"');
                    characters.next();
                } else {
                    break index + 1;
                }
            };
            entries.push((stored_identifier(&name, true).into_owned(), true));
            &quoted[end..]
        } else {
            let end = rest.find(|character| character == ',' || is_space(character));
            let end = end.unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            entries.push((stored_identifier(&rest[..end], false).into_owned(), false));
            &rest[end..]
        };
        let after = after.trim_start_matches(is_space);
        match after.strip_prefix(',') {
            Some(next) => rest = next.trim_start_matches(is_space),
            None if after.is_empty() => return Some(entries),
            None => return None,
        }
    }
}

/// `entries` written back as a `search_path`, a quoted entry quoted again
/// when it needs quotes.
pub(crate) fn display_search_path<'path>(
    entries: impl IntoIterator<Item = (&'path str, bool)>,
) -> String {
    entries
        .into_iter()
        .map(|(name, quoted)| if quoted { quote_identifier(name) } else { name.to_string() })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether PostgreSQL's scanner reads `character` as whitespace.
fn is_space(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec::Vec};

    use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};

    use super::{printed_search_path, split_search_path};

    fn owned(path: &[(&str, bool)]) -> Vec<(String, bool)> {
        path.iter().map(|&(name, quoted)| (name.into(), quoted)).collect()
    }

    /// The lists `SHOW search_path` prints read back as the schemas they
    /// name, and the text PostgreSQL refuses as list syntax is refused.
    #[test]
    fn splits_as_postgres_reads_the_setting() {
        for (text, path) in [
            ("\"$user\", public", owned(&[("$user", true), ("public", false)])),
            (" API ,\t\"Odd\"\"Name\" ", owned(&[("api", false), ("Odd\"Name", true)])),
            ("\"a, b\"", owned(&[("a, b", true)])),
            ("", owned(&[])),
            ("\"\"", owned(&[("", true)])),
        ] {
            assert_eq!(split_search_path(text), Some(path), "{text}");
        }
        for refused in ["a,", ",a", "a b", "\"open", "a,,b"] {
            assert_eq!(split_search_path(refused), None, "{refused}");
        }
    }

    /// The values `SET search_path = ...` writes print as PostgreSQL stores
    /// them, and read back as the schemas the statement named.
    #[test]
    fn prints_each_entry_as_postgres_stores_it() {
        let printed = |written: &str| {
            let values = Parser::new(&PostgreSqlDialect {})
                .try_with_sql(written)
                .and_then(|mut parser| parser.parse_comma_separated(Parser::parse_expr))
                .expect("the values parse");
            printed_search_path(&values, false)
        };
        assert_eq!(printed("\"$user\", Public").as_deref(), Some("\"$user\", public"));
        assert_eq!(
            printed("'App', 'a, b', 007, true").as_deref(),
            Some("\"App\", \"a, b\", 7, \"true\"")
        );
        assert_eq!(
            split_search_path(&printed("'App', 'a, b'").expect("printed")),
            Some(owned(&[("App", true), ("a, b", true)]))
        );
        assert_eq!(printed("a.b"), None);
    }
}
