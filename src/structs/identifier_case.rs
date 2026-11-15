//! The comparison a lookup applies to the identifiers it matches.

use alloc::borrow::Cow;
use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::utils::identifier_resolution::{parse_lookup_identifier, stored_identifier};

/// How a lookup compares a written identifier against a stored one.
///
/// The three engines this crate serves do not share one rule, and no catalog
/// can carry the rule for them. PostgreSQL folds an unquoted identifier and
/// reads a quoted one literally, with no setting that changes it. SQLite
/// compares a relation name case-insensitively for ASCII whether or not it was
/// quoted. MySQL follows `lower_case_table_names`, which is fixed when the
/// server is initialised and appears in no DDL, so one catalog parsed from a
/// dump can be read by two servers that disagree about it. The comparison is
/// therefore an argument to every lookup rather than a property of the
/// database.
///
/// No rule trims an identifier or normalizes its Unicode composition, so
/// `"amount "` and `"amount"`, or a precomposed `é` and an `e` followed by a
/// combining accent, are two names under each of them, as they are on
/// PostgreSQL and SQLite.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
///
/// // A table the DDL wrote quoted, so the catalog stores its case.
/// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE \"Docs\" (id INT);")?;
/// let written = || TargetName::new("docs", false);
///
/// // PostgreSQL reads the stored name literally and this misses.
/// assert!(db.resolve_target_table(written(), IdentifierCase::AsWritten)?.is_none());
/// // SQLite, and MySQL with `lower_case_table_names` at 1 or 2, resolve it.
/// assert!(db.resolve_target_table(written(), IdentifierCase::Folded)?.is_some());
/// # Ok::<(), sql_traits::errors::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentifierCase {
    /// Quoting decides, which is PostgreSQL's rule: an unquoted identifier
    /// folds and a quoted one is compared as it stands.
    AsWritten,
    /// Both sides fold, whatever quoting either carried.
    ///
    /// Folding is ASCII-only, which is SQLite's rule rather than an
    /// approximation of it: every schema comparison goes through
    /// `sqlite3StrICmp`, whose table maps bytes 65 to 90 and leaves every byte
    /// above 127 as itself. A MySQL utf8 collation does fold beyond ASCII, so
    /// a name outside ASCII compares case-sensitively here even for a server
    /// that would fold it.
    Folded,
    /// Neither side folds, whatever quoting either carried, so the stored name
    /// is matched as the catalog holds it.
    ///
    /// This is MySQL with `lower_case_table_names = 0`, the Unix default,
    /// where a table created `Docs` is not reached by `docs`.
    Exact,
}

impl IdentifierCase {
    /// Whether an identifier carrying this quoting folds under the rule.
    #[must_use]
    #[inline]
    pub const fn folds(self, quoted: bool) -> bool {
        match self {
            Self::AsWritten => !quoted,
            Self::Folded => true,
            Self::Exact => false,
        }
    }

    /// The form an identifier is compared under.
    ///
    /// Under [`AsWritten`](Self::AsWritten) this is the name PostgreSQL
    /// stores, as [`stored_identifier`] answers. [`Folded`](Self::Folded)
    /// lowers the ASCII letters of every name and [`Exact`](Self::Exact) keeps
    /// every name as written.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// assert_eq!(IdentifierCase::AsWritten.compared_form("Docs", true), "Docs");
    /// assert_eq!(IdentifierCase::AsWritten.compared_form("Docs", false), "docs");
    /// assert_eq!(IdentifierCase::Folded.compared_form("Docs", true), "docs");
    /// assert_eq!(IdentifierCase::Exact.compared_form("Docs", false), "Docs");
    /// assert_eq!(IdentifierCase::Folded.compared_form(" Docs ", true), " docs ");
    /// ```
    #[must_use]
    #[inline]
    pub fn compared_form(self, value: &str, quoted: bool) -> Cow<'_, str> {
        match self {
            Self::AsWritten => stored_identifier(value, quoted),
            Self::Folded if value.bytes().any(|byte| byte.is_ascii_uppercase()) => {
                Cow::Owned(value.to_ascii_lowercase())
            }
            Self::Folded | Self::Exact => Cow::Borrowed(value),
        }
    }

    /// Whether two identifiers are one name under the rule.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// // SQLite reads these as one name, PostgreSQL as two.
    /// assert!(IdentifierCase::Folded.identifiers_match("docs", false, "Docs", true));
    /// assert!(!IdentifierCase::AsWritten.identifiers_match("docs", false, "Docs", true));
    /// // A case-sensitive MySQL reads an unquoted pair as two names.
    /// assert!(!IdentifierCase::Exact.identifiers_match("docs", false, "Docs", false));
    /// ```
    #[must_use]
    pub fn identifiers_match(
        self,
        left: &str,
        left_quoted: bool,
        right: &str,
        right_quoted: bool,
    ) -> bool {
        self.compared_form(left, left_quoted) == self.compared_form(right, right_quoted)
    }

    /// Whether a stored identifier is named by a written lookup, reading the
    /// lookup's own quoting out of its text.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// // The lookup carries its quoting, so PostgreSQL's rule needs it back.
    /// assert!(IdentifierCase::AsWritten.names_stored("Body", true, "\"Body\""));
    /// assert!(!IdentifierCase::AsWritten.names_stored("Body", true, "body"));
    /// // A folding engine reads a column name either way.
    /// assert!(IdentifierCase::Folded.names_stored("Body", true, "body"));
    /// ```
    #[must_use]
    pub fn names_stored(self, stored: &str, stored_quoted: bool, lookup: &str) -> bool {
        let written = parse_lookup_identifier(lookup);
        self.identifiers_match(stored, stored_quoted, written.value(), written.is_quoted())
    }
}

impl Display for IdentifierCase {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::AsWritten => f.write_str("quoting decides"),
            Self::Folded => f.write_str("folded"),
            Self::Exact => f.write_str("exact"),
        }
    }
}
