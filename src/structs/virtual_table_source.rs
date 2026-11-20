//! The module a SQLite virtual table runs on.

use alloc::{string::String, vec::Vec};

/// The `USING` clause of a `CREATE VIRTUAL TABLE`: the module name and its
/// arguments exactly as written, which SQLite stores in `sqlite_schema` and
/// hands to the module unread.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::SQLiteDialect;
///
/// let db = ParserDB::parse::<SQLiteDialect>(
///     "CREATE VIRTUAL TABLE notes USING fts5(body, tokenize = 'porter');",
/// )?;
/// let notes = db.virtual_tables().next().expect("notes is virtual");
/// let source = notes.virtual_source(&db)?.expect("a virtual table names its module");
/// assert_eq!(source.module_name(), "fts5");
/// assert_eq!(source.arguments(), ["body", "tokenize = 'porter'"]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtualTableSource {
    module_name: String,
    arguments: Vec<String>,
}

impl VirtualTableSource {
    /// Records the module a virtual table runs on and its arguments, each as
    /// written.
    #[must_use]
    pub fn new(
        module_name: impl Into<String>,
        arguments: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            module_name: module_name.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns the module name, as written.
    #[must_use]
    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    /// Returns the module arguments, each as written, in order.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}
