//! Where the rows of a foreign table come from.

use alloc::{borrow::Cow, string::String, vec::Vec};

use crate::utils::identifier_resolution::normalize_identifier;

/// The `SERVER` and table-level `OPTIONS` of a `CREATE FOREIGN TABLE`, as
/// PostgreSQL records them in `pg_foreign_table`.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE FOREIGN TABLE sales (id INT) SERVER Remote OPTIONS (Table_Name 'Sales');",
/// )?;
/// let sales = db
///     .foreign_table_by_target(TargetName::new("sales", false), IdentifierCase::AsWritten)?
///     .expect("the foreign table is recorded");
/// let source = sales.foreign_source(&db)?.expect("a foreign table names its server");
/// assert_eq!(source.server_name(), "Remote");
/// assert_eq!(source.stored_server_name(), "remote");
/// assert_eq!(source.options(), &[("table_name".to_string(), "Sales".to_string())]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ForeignTableSource {
    /// The server identifier, exactly as written.
    server_name: String,
    /// Whether that identifier was quoted.
    server_name_is_quoted: bool,
    /// Each option as PostgreSQL stores it, the key folded unless quoted and
    /// the value verbatim, in the order written.
    options: Vec<(String, String)>,
}

impl ForeignTableSource {
    /// Records the server a foreign table reads from and its table-level
    /// options, each key with its quote state.
    ///
    /// An unquoted key folds to lowercase, as PostgreSQL stores it.
    #[must_use]
    pub fn new<'a>(
        server_name: impl Into<String>,
        server_name_is_quoted: bool,
        options: impl IntoIterator<Item = (&'a str, bool, &'a str)>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            server_name_is_quoted,
            options: options
                .into_iter()
                .map(|(key, quoted, value)| {
                    (normalize_identifier(key, quoted).into_owned(), value.into())
                })
                .collect(),
        }
    }

    /// Returns the server identifier, exactly as written.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Returns whether the server identifier was quoted.
    #[must_use]
    pub fn server_name_is_quoted(&self) -> bool {
        self.server_name_is_quoted
    }

    /// Returns the server name PostgreSQL stores: an unquoted identifier
    /// folds to lowercase, a quoted one keeps its case.
    #[must_use]
    pub fn stored_server_name(&self) -> Cow<'_, str> {
        normalize_identifier(&self.server_name, self.server_name_is_quoted)
    }

    /// Returns the table-level options as PostgreSQL stores them, each key
    /// with its value, in the order written.
    #[must_use]
    pub fn options(&self) -> &[(String, String)] {
        &self.options
    }
}
