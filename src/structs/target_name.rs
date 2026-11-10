//! A name an object writes for one of its targets, before any resolution.

use alloc::{borrow::Cow, string::ToString};
use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::errors::TargetNameParseError;

/// A name exactly as a schema object wrote it, with no resolution applied.
///
/// This is what the target readers hand back: a single one on
/// [`PolicyLike`](crate::traits::PolicyLike),
/// [`TriggerLike`](crate::traits::TriggerLike) and
/// [`ForeignKeyLike`](crate::traits::ForeignKeyLike), and a list on
/// [`GrantLike`](crate::traits::GrantLike). The four questions it answers are
/// the ones
/// [`TableLike`](crate::traits::TableLike) already asks of a stored table
/// name, so a caller applying its own resolution rules has everything the SQL
/// carried: the identifier, whether it was quoted, the qualifier if one was
/// written, and whether that was quoted. [`Display`] renders it back to SQL
/// text and [`TargetName::parse`] reads that text again, so a name this crate
/// wrote and this crate reads back is the same name.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
///
/// let bare = TargetName::new("docs", false);
/// assert_eq!(bare.name(), "docs");
/// assert_eq!(bare.schema(), None);
///
/// let qualified = TargetName::new("Docs", true).with_schema("app", false);
/// assert_eq!(qualified.name(), "Docs");
/// assert!(qualified.name_is_quoted());
/// assert_eq!(qualified.schema(), Some("app"));
/// assert!(!qualified.schema_is_quoted());
/// assert_eq!(qualified.to_string(), "app.\"Docs\"");
///
/// // Reading written text back: a dot inside a quoted part belongs to the
/// // identifier instead of separating the qualifier.
/// let parsed = TargetName::parse(r#""my.schema"."Docs""#).expect("a qualified name parses");
/// assert_eq!(parsed.schema(), Some("my.schema"));
/// assert!(parsed.schema_is_quoted());
/// assert_eq!(parsed.name(), "Docs");
/// assert!(parsed.name_is_quoted());
/// assert_eq!(parsed.to_string(), r#""my.schema"."Docs""#);
/// ```
///
/// Under the `serde` feature the two parts deserialise owned rather than
/// borrowed from the input, so a name read back outlives the bytes it came
/// from and carries escaped text that a borrow could not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TargetName<'a> {
    name: Cow<'a, str>,
    name_is_quoted: bool,
    schema: Option<Cow<'a, str>>,
    schema_is_quoted: bool,
}

/// What went wrong while taking one identifier off the front of a text, before
/// the whole text is known.
enum TakeError {
    Empty,
    UnterminatedQuote,
}

impl TakeError {
    fn into_error(self, text: &str) -> TargetNameParseError {
        match self {
            Self::Empty => TargetNameParseError::EmptyPart { text: text.to_string() },
            Self::UnterminatedQuote => {
                TargetNameParseError::UnterminatedQuote { text: text.to_string() }
            }
        }
    }
}

/// Takes one identifier off the front of `text`, returning it with its quote
/// state and whatever text is left over. A quoted part runs to its closing
/// quote with `""` standing for one quote, and a bare part runs to the next
/// dot or quote.
fn take_identifier(text: &str) -> Result<(Cow<'_, str>, bool, &str), TakeError> {
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'"') {
        let mut index = 1;
        loop {
            match bytes.get(index) {
                None => return Err(TakeError::UnterminatedQuote),
                Some(b'"') if bytes.get(index + 1) == Some(&b'"') => index += 2,
                Some(b'"') => break,
                Some(_) => index += 1,
            }
        }
        let inner = &text[1..index];
        if inner.is_empty() {
            return Err(TakeError::Empty);
        }
        let value = if inner.contains("\"\"") {
            Cow::Owned(inner.replace("\"\"", "\""))
        } else {
            Cow::Borrowed(inner)
        };
        return Ok((value, true, &text[index + 1..]));
    }
    let end = bytes.iter().position(|byte| *byte == b'.' || *byte == b'"').unwrap_or(text.len());
    if end == 0 {
        return Err(TakeError::Empty);
    }
    Ok((Cow::Borrowed(&text[..end]), false, &text[end..]))
}

impl<'a> TargetName<'a> {
    /// Creates an unqualified target name.
    #[must_use]
    pub fn new(name: &'a str, name_is_quoted: bool) -> Self {
        Self { name: Cow::Borrowed(name), name_is_quoted, schema: None, schema_is_quoted: false }
    }

    /// Adds the qualifier the SQL wrote in front of the name.
    #[must_use]
    pub fn with_schema(self, schema: &'a str, schema_is_quoted: bool) -> Self {
        Self { schema: Some(Cow::Borrowed(schema)), schema_is_quoted, ..self }
    }

    /// Reads a name back from the text [`Display`] writes, the inverse of it.
    ///
    /// A dot separates the qualifier from the name only when it sits outside
    /// quotes. A part wrapped in double quotes keeps everything written
    /// between them, with a doubled quote (`""`) standing for one, and
    /// reports itself quoted. Every other part keeps its text and reports
    /// itself unquoted.
    ///
    /// The parse is strict the way the rest of the crate refuses bad SQL:
    /// empty text, an empty part, a quote that never closes, text following
    /// an identifier without a separating dot, and more than one dot each
    /// return an error instead of a name that would resolve to nothing.
    ///
    /// Text with nothing to unescape borrows from `text`, and only a part
    /// carrying a doubled quote allocates.
    ///
    /// # Errors
    ///
    /// Returns a [`TargetNameParseError`] describing the first problem when
    /// `text` is not one identifier, or a qualifier and a name separated by
    /// one dot.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// let target = TargetName::parse(r#""we""ird""#).expect("a doubled quote is one quote");
    /// assert_eq!(target.name(), r#"we"ird"#);
    /// assert!(target.name_is_quoted());
    ///
    /// // Anything the grammar cannot read is refused, not tolerated.
    /// assert!(TargetName::parse("app.").is_err());
    /// assert!(TargetName::parse("database.app.docs").is_err());
    /// ```
    pub fn parse(text: &'a str) -> Result<Self, TargetNameParseError> {
        if text.is_empty() {
            return Err(TargetNameParseError::Empty);
        }
        let (first, first_quoted, rest) =
            take_identifier(text).map_err(|problem| problem.into_error(text))?;
        if !rest.is_empty() && !rest.starts_with('.') {
            return Err(TargetNameParseError::UnexpectedText {
                text: text.to_string(),
                found: rest.to_string(),
            });
        }
        if rest.is_empty() {
            return Ok(Self {
                name: first,
                name_is_quoted: first_quoted,
                schema: None,
                schema_is_quoted: false,
            });
        }
        let (name, name_quoted, rest) =
            take_identifier(&rest[1..]).map_err(|problem| problem.into_error(text))?;
        if !rest.is_empty() {
            return Err(if rest.starts_with('.') {
                TargetNameParseError::TooManyParts { text: text.to_string() }
            } else {
                TargetNameParseError::UnexpectedText {
                    text: text.to_string(),
                    found: rest.to_string(),
                }
            });
        }
        Ok(Self {
            name,
            name_is_quoted: name_quoted,
            schema: Some(first),
            schema_is_quoted: first_quoted,
        })
    }

    /// Returns the identifier as written, without its surrounding quotes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns whether the identifier was quoted in SQL.
    #[must_use]
    pub fn name_is_quoted(&self) -> bool {
        self.name_is_quoted
    }

    /// Returns the qualifier written in front of the name, if any.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// Returns whether that qualifier was quoted in SQL.
    ///
    /// This only matters when [`Self::schema`] returns `Some`.
    #[must_use]
    pub fn schema_is_quoted(&self) -> bool {
        self.schema_is_quoted
    }

    /// Takes ownership of both parts, so the name outlives the SQL it was read
    /// from.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// let owned = {
    ///     let text = String::from("app.\"Docs\"");
    ///     TargetName::parse(&text).expect("a qualified name parses").into_owned()
    /// };
    /// assert_eq!(owned.schema(), Some("app"));
    /// assert_eq!(owned.name(), "Docs");
    /// ```
    #[must_use]
    pub fn into_owned(self) -> TargetName<'static> {
        TargetName {
            name: Cow::Owned(self.name.into_owned()),
            name_is_quoted: self.name_is_quoted,
            schema: self.schema.map(|schema| Cow::Owned(schema.into_owned())),
            schema_is_quoted: self.schema_is_quoted,
        }
    }

    /// Transfers the identifier, qualifier and their quoting flags.
    pub(crate) fn into_parts(self) -> (Cow<'a, str>, bool, Option<Cow<'a, str>>, bool) {
        (self.name, self.name_is_quoted, self.schema, self.schema_is_quoted)
    }
}

/// Writes one identifier, quoting it and doubling any embedded quote when it
/// was quoted in SQL.
fn write_identifier(f: &mut Formatter<'_>, value: &str, quoted: bool) -> FmtResult {
    if !quoted {
        return f.write_str(value);
    }
    f.write_str("\"")?;
    for (index, part) in value.split('"').enumerate() {
        if index > 0 {
            f.write_str("\"\"")?;
        }
        f.write_str(part)?;
    }
    f.write_str("\"")
}

impl Display for TargetName<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        if let Some(schema) = self.schema.as_deref() {
            write_identifier(f, schema, self.schema_is_quoted)?;
            f.write_str(".")?;
        }
        write_identifier(f, &self.name, self.name_is_quoted)
    }
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, string::ToString};

    use super::{TakeError, TargetName};
    use crate::errors::TargetNameParseError;

    fn parts(text: &str) -> TargetName<'_> {
        TargetName::parse(text).unwrap_or_else(|problem| panic!("`{text}` should parse: {problem}"))
    }

    /// A quoted identifier carrying a quote of its own is rendered with that
    /// quote doubled, which is the only escape SQL has, and reading the result
    /// back recovers the value the target held.
    #[test]
    fn an_embedded_quote_is_doubled_and_survives_a_round_trip() {
        let target = TargetName::new("A\"B", true);
        let rendered = target.to_string();
        assert_eq!(rendered, "\"A\"\"B\"");

        let reparsed = parts(&rendered);
        assert_eq!(reparsed.name(), "A\"B");
        assert!(reparsed.name_is_quoted());
    }

    /// Both parts escape independently of each other.
    #[test]
    fn each_part_escapes_on_its_own() {
        let both = TargetName::new("T\"1", true).with_schema("S\"2", true);
        assert_eq!(both.to_string(), "\"S\"\"2\".\"T\"\"1\"");

        let unquoted = TargetName::new("plain", false).with_schema("app", false);
        assert_eq!(unquoted.to_string(), "app.plain");

        // A lone quote is the shortest input that exercises the escape twice.
        assert_eq!(TargetName::new("\"", true).to_string(), "\"\"\"\"");
    }

    /// A dot only separates when it sits outside quotes, so a quoted schema
    /// name keeps dots of its own.
    #[test]
    fn a_dot_inside_quotes_does_not_separate() {
        let target = parts(r#""my.schema"."Docs""#);
        assert_eq!(target.schema(), Some("my.schema"));
        assert!(target.schema_is_quoted());
        assert_eq!(target.name(), "Docs");
        assert!(target.name_is_quoted());
    }

    /// Unquoted parts keep their text and report themselves unquoted.
    #[test]
    fn unquoted_parts_stay_unquoted() {
        let qualified = parts("app.docs");
        assert_eq!(qualified.schema(), Some("app"));
        assert!(!qualified.schema_is_quoted());
        assert_eq!(qualified.name(), "docs");
        assert!(!qualified.name_is_quoted());

        let bare = parts("docs");
        assert_eq!(bare.schema(), None);
        assert_eq!(bare.name(), "docs");
        assert!(!bare.name_is_quoted());
    }

    /// A doubled quote inside a quoted part is one quote, and only the part
    /// that needed unescaping owns its text.
    #[test]
    fn doubled_quotes_unescape_and_only_they_allocate() {
        let weird = parts(r#""we""ird""#);
        assert_eq!(weird.name(), r#"we"ird"#);
        assert!(weird.name_is_quoted());

        let mixed = parts(r#""we""ird".plain"#);
        assert!(matches!(mixed.schema, Some(Cow::Owned(_))));
        assert!(matches!(mixed.name, Cow::Borrowed(_)));
    }

    /// Every text that parses renders back to exactly the text it was.
    #[test]
    fn parse_inverts_display() {
        for written in [
            "docs",
            "app.docs",
            r#""Docs""#,
            r#""my.schema"."Docs""#,
            r#""we""ird""#,
            r#""S""2"."T""1""#,
            r#""Docs"."my.schema""#,
            "\"\"\"\".\"\"\"\"",
            r#""a.b".c"#,
        ] {
            assert_eq!(parts(written).to_string(), written, "round trip of `{written}`");
        }
    }

    /// A name built by hand renders to text that parses back to the same
    /// parts and the same quote states.
    #[test]
    fn built_names_parse_back_to_the_same_parts() {
        let built = [
            TargetName::new("docs", false),
            TargetName::new("Docs", true),
            TargetName::new("a\"b", true),
            TargetName::new("Tab le", false),
            TargetName::new("T\"1", true).with_schema("S\"2", true),
        ];
        for name in built {
            let rendered = name.to_string();
            let reparsed = parts(&rendered);
            assert_eq!(reparsed.name(), name.name(), "{rendered}");
            assert_eq!(reparsed.name_is_quoted(), name.name_is_quoted(), "{rendered}");
            assert_eq!(reparsed.schema(), name.schema(), "{rendered}");
            assert_eq!(
                reparsed.schema_is_quoted(),
                name.schema_is_quoted(),
                "quote state of the qualifier of {rendered}"
            );
        }
    }

    /// Empty text names nothing and is refused as such.
    #[test]
    fn empty_text_is_refused() {
        assert_eq!(TargetName::parse(""), Err(TargetNameParseError::Empty));
    }

    /// An empty part in any spelling is refused.
    #[test]
    fn an_empty_part_is_refused() {
        for text in [".", "app.", ".docs", r#""""#, "app..docs", "\"a\".\"\""] {
            assert!(
                matches!(TargetName::parse(text), Err(TargetNameParseError::EmptyPart { .. })),
                "`{text}` should report an empty part"
            );
        }
    }

    /// A quote that never closes is refused instead of read as part of the
    /// identifier.
    #[test]
    fn an_unterminated_quote_is_refused() {
        for text in [r#""docs"#, r#"""#, "\"a\"\"", r#""a"."b"#] {
            assert!(
                matches!(
                    TargetName::parse(text),
                    Err(TargetNameParseError::UnterminatedQuote { .. })
                ),
                "`{text}` should report an unterminated quote"
            );
        }
    }

    /// Text with no separating dot between identifiers is refused, naming
    /// what was found there.
    #[test]
    fn text_without_a_separating_dot_is_refused() {
        assert_eq!(
            TargetName::parse(r#"a"b"#),
            Err(TargetNameParseError::UnexpectedText {
                text: r#"a"b"#.to_string(),
                found: r#""b"#.to_string(),
            })
        );
        assert_eq!(
            TargetName::parse(r#""a"x"#),
            Err(TargetNameParseError::UnexpectedText {
                text: r#""a"x"#.to_string(),
                found: "x".to_string(),
            })
        );
        assert!(matches!(
            TargetName::parse(r#"app.docs"x"#),
            Err(TargetNameParseError::UnexpectedText { .. })
        ));
    }

    /// A third part is refused, because a name holds a qualifier and an
    /// identifier and nothing else.
    #[test]
    fn a_third_part_is_refused() {
        assert!(matches!(
            TargetName::parse("database.app.docs"),
            Err(TargetNameParseError::TooManyParts { .. })
        ));
        assert!(matches!(
            TargetName::parse(r#""a"."b"."c""#),
            Err(TargetNameParseError::TooManyParts { .. })
        ));
    }

    /// The empty slice reaches the same refusal whether the whole text or a
    /// tail hands it over.
    #[test]
    fn the_quoted_empty_part_reports_empty_not_unterminated() {
        assert!(matches!(super::take_identifier(""), Err(TakeError::Empty)));
        assert!(matches!(super::take_identifier(r#""""#), Err(TakeError::Empty)));
        assert!(matches!(super::take_identifier(r#""a"#), Err(TakeError::UnterminatedQuote)));
    }
}
