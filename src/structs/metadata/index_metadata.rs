//! Submodule defining a generic `IndexMetadata` struct.

use alloc::sync::Arc;

use sqlparser::ast::{Expr, Visit};

use super::{ExpressionCalls, expression_calls::call_site};
use crate::{
    structs::{KeyIdentity, WrittenCall},
    traits::{DatabaseLike, IndexLike},
};

#[derive(Debug, Clone)]
/// Struct collecting metadata about an index.
pub struct IndexMetadata<I: IndexLike> {
    /// The expression defining the index.
    expression: Expr,
    /// The calls of the index's expressions and predicate.
    calls: ExpressionCalls<<I::DB as DatabaseLike>::Function>,
    /// The identity the index was given when it was created on a table,
    /// which orders it among the table's keys.
    key_identity: Option<KeyIdentity>,
    /// Whether the index is valid, which an index created `ON ONLY` a
    /// partitioned table holding a partition is not.
    valid: bool,
}

impl<I: IndexLike> IndexMetadata<I> {
    /// Creates a new `IndexMetadata` instance.
    #[must_use]
    #[inline]
    pub fn new(expression: Expr) -> Self {
        Self { expression, calls: ExpressionCalls::default(), key_identity: None, valid: true }
    }

    /// Stores whether the index is valid.
    #[must_use]
    pub(crate) fn with_validity(mut self, valid: bool) -> Self {
        self.valid = valid;
        self
    }

    /// Whether the index is valid.
    pub(crate) fn is_valid(&self) -> bool {
        self.valid
    }

    /// Marks the index valid or invalid.
    pub(crate) fn set_validity(&mut self, valid: bool) {
        self.valid = valid;
    }

    /// Stores the identity the index was given when it was created.
    #[must_use]
    pub(crate) fn with_key_identity(mut self, identity: Option<KeyIdentity>) -> Self {
        self.key_identity = identity;
        self
    }

    /// Returns the identity the index was given when it was created on a
    /// table.
    pub(crate) fn key_identity(&self) -> Option<KeyIdentity> {
        self.key_identity
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

    /// Returns the functions `call` reaches, or `None` when `call` is a node
    /// of neither [`Self::expression`] nor `statement`, the node the calls
    /// were bound from.
    ///
    /// The expression copies the key of `statement`, which a walk of
    /// `statement` meets before any other call, so a call counts from the
    /// start of whichever of the two holds it.
    pub(crate) fn reached_by<'s, N: Visit + ?Sized>(
        &'s self,
        statement: &N,
        call: WrittenCall<'_>,
    ) -> Option<impl Iterator<Item = &'s <I::DB as DatabaseLike>::Function> + use<'s, I, N>> {
        let site = call_site(&self.expression, call).or_else(|| call_site(statement, call))?;
        self.calls.reached_at(site)
    }

    /// Returns the calls of the index's expressions and predicate, to settle
    /// them.
    pub(crate) fn calls_mut(&mut self) -> &mut ExpressionCalls<<I::DB as DatabaseLike>::Function> {
        &mut self.calls
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
}

/// Type alias for `IndexMetadata` to be used with unique indices.
pub type UniqueIndexMetadata<U> = IndexMetadata<U>;
