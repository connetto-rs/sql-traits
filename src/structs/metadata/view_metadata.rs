//! Submodule defining a `ViewMetadata` struct.

use alloc::string::String;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Metadata about a database view.
///
/// Holds what the input says about a view that the view's own declaration
/// cannot carry, which is the role owning it.
pub struct ViewMetadata {
    /// The role the input names as the view's owner, if it names one.
    owner: Option<String>,
}

impl ViewMetadata {
    /// Returns the role the input names as the view's owner.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE ROLE app_reader;
    ///      CREATE TABLE t (id INT);
    ///      CREATE VIEW v AS SELECT id FROM t;
    ///      ALTER TABLE v OWNER TO app_reader;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// let metadata = db.view_metadata(view).expect("metadata is recorded");
    /// assert_eq!(metadata.owner(), Some("app_reader"));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    #[inline]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Sets the role the input names as the view's owner.
    ///
    /// # Arguments
    ///
    /// * `owner` - The owning role, or [`None`] when the input names no role.
    #[inline]
    pub fn set_owner(&mut self, owner: Option<String>) {
        self.owner = owner;
    }
}
