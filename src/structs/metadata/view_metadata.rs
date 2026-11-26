//! Submodule defining a `ViewMetadata` struct.

use alloc::{string::String, sync::Arc, vec::Vec};

use sqlparser::ast::{Action, CreateFunction, Privileges};

use super::{ExpressionCalls, revoke_from_owner_entry};
use crate::impls::PrivilegeKind;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Metadata about a database view.
///
/// Holds what the input says about a view that the view's own declaration
/// cannot carry, which is the role owning it, the privileges the owner's own
/// entry holds and the functions its definition was bound to call.
pub struct ViewMetadata {
    /// The role the input names as the view's owner, if it names one.
    owner: Option<String>,
    /// The relation-level privileges the owner's own entry holds.
    ///
    /// Mirrors the owner's item in `pg_class.relacl`, which starts with every
    /// table privilege and loses the ones a `REVOKE` naming the owner takes.
    owner_privileges: Vec<Action>,
    /// The calls of the definition, bound when the view was written.
    calls: ExpressionCalls<CreateFunction>,
}

impl Default for ViewMetadata {
    fn default() -> Self {
        Self {
            owner: None,
            owner_privileges: PrivilegeKind::Relation.all_privileges(),
            calls: ExpressionCalls::default(),
        }
    }
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

    /// Returns the relation-level privileges the owner's own entry holds.
    #[inline]
    pub fn owner_privileges(&self) -> impl Iterator<Item = &Action> {
        self.owner_privileges.iter()
    }

    /// Replaces the owner's own entry.
    pub(crate) fn set_owner_privileges(&mut self, privileges: Vec<Action>) {
        self.owner_privileges = privileges;
    }

    /// Takes from the owner's own entry the relation-level privileges a
    /// `REVOKE` names.
    pub(crate) fn revoke_owner_privileges(&mut self, revoked: &Privileges) {
        revoke_from_owner_entry(&mut self.owner_privileges, revoked);
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
