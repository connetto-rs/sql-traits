//! Submodule defining a `ViewMetadata` struct.

use alloc::{string::String, sync::Arc};

use sqlparser::ast::CreateFunction;

use super::ExpressionCalls;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Metadata about a database view.
///
/// Holds what the input says about a view that the view's own declaration
/// cannot carry, which is the role owning it and the functions its definition
/// was bound to call.
pub struct ViewMetadata {
    /// The role the input names as the view's owner, if it names one.
    owner: Option<String>,
    /// The calls of the definition, bound when the view was written.
    calls: ExpressionCalls<CreateFunction>,
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

    /// Stores the calls of the view's definition.
    pub(crate) fn set_calls(&mut self, calls: ExpressionCalls<CreateFunction>) {
        self.calls = calls;
    }

    /// Returns the calls of the view's definition.
    pub(crate) fn calls(&self) -> &ExpressionCalls<CreateFunction> {
        &self.calls
    }

    /// Returns the calls of the view's definition, to settle them.
    pub(crate) fn calls_mut(&mut self) -> &mut ExpressionCalls<CreateFunction> {
        &mut self.calls
    }

    /// Re-points every call reaching the function node `stale` at `fresh`.
    pub(crate) fn replace_function(
        &mut self,
        stale: &Arc<CreateFunction>,
        fresh: &Arc<CreateFunction>,
    ) {
        self.calls.replace_function(stale, fresh);
    }
}
