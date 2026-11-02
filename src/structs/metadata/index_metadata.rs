//! Submodule defining a generic `IndexMetadata` struct.

use alloc::sync::Arc;

use sqlparser::ast::Expr;

use super::ExpressionCalls;
use crate::traits::{DatabaseLike, IndexLike};

#[derive(Debug, Clone)]
/// Struct collecting metadata about an index.
pub struct IndexMetadata<I: IndexLike> {
    /// The expression defining the index.
    expression: Expr,
    /// The table on which the index is defined.
    table: Arc<<I::DB as DatabaseLike>::Table>,
    /// The calls of the index's expressions and predicate.
    calls: ExpressionCalls<<I::DB as DatabaseLike>::Function>,
}

impl<I: IndexLike> IndexMetadata<I> {
    /// Creates a new `IndexMetadata` instance.
    #[inline]
    pub fn new(expression: Expr, table: Arc<<I::DB as DatabaseLike>::Table>) -> Self {
        Self { expression, table, calls: ExpressionCalls::default() }
    }

    /// Stores the calls of the index's expressions and predicate.
    #[must_use]
    pub(crate) fn with_calls(
        mut self,
        calls: ExpressionCalls<<I::DB as DatabaseLike>::Function>,
    ) -> Self {
        self.calls = calls;
        self
    }

    /// Returns the calls of the index's expressions and predicate.
    pub(crate) fn calls(&self) -> &ExpressionCalls<<I::DB as DatabaseLike>::Function> {
        &self.calls
    }

    /// Re-points every call reaching the function node `stale` at `fresh`.
    pub(crate) fn replace_function(
        &mut self,
        stale: &Arc<<I::DB as DatabaseLike>::Function>,
        fresh: &Arc<<I::DB as DatabaseLike>::Function>,
    ) {
        self.calls.replace_function(stale, fresh);
    }

    /// Returns a reference to the expression defining the index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db =
    ///     ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT); CREATE INDEX idx ON t(id);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.indices(&db)?.next().unwrap();
    /// let meta = db.index_metadata(index).unwrap();
    /// // The expression is the parenthesized column list as parsed.
    /// assert_eq!(meta.expression().to_string(), "(id)");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    #[inline]
    pub fn expression(&self) -> &Expr {
        &self.expression
    }

    /// Returns a reference to the table on which the index is defined.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE my_t (id INT); CREATE INDEX idx ON my_t(id);",
    /// )?;
    /// let index = db
    ///     .table_by_target(TargetName::new("my_t", false), IdentifierCase::AsWritten)?
    ///     .unwrap()
    ///     .indices(&db)?
    ///     .next()
    ///     .unwrap();
    /// let meta = db.index_metadata(index).unwrap();
    /// assert_eq!(meta.table().table_name(), "my_t");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    #[inline]
    pub fn table(&self) -> &<I::DB as DatabaseLike>::Table {
        &self.table
    }
}

/// Type alias for `IndexMetadata` to be used with unique indices.
pub type UniqueIndexMetadata<U> = IndexMetadata<U>;
