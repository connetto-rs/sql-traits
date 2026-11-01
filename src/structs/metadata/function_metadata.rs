//! Submodule defining a `FunctionMetadata` struct.

use alloc::{string::String, sync::Arc, vec::Vec};

use sqlparser::ast::{CreateFunction, CreateRole, Grantee, Ident};

use crate::{
    impls::{grantee_is_public, grantee_matches_role, grantees_match},
    structs::{FunctionKind, metadata::ExpressionCalls},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Metadata about a database function.
///
/// Holds what the input says about a function that the function's own
/// declaration cannot carry.
pub struct FunctionMetadata {
    /// The role the input names as the function's owner, if it names one.
    owner: Option<String>,
    /// Who holds `EXECUTE` on the function.
    execute: ExecutePrivileges,
    /// Call argument positions where `NULL` yields `NULL` despite
    /// non-strictness.
    null_returning_arguments: &'static [usize],
    /// The function's `pg_proc.prokind`.
    kind: FunctionKind,
    /// The name of the extension the function belongs to, which creates and
    /// drops it.
    extension: Option<Ident>,
    /// The calls of a body PostgreSQL binds at creation, with the functions
    /// each reached then, and none for a body read again each time it runs.
    body_calls: ExpressionCalls<CreateFunction>,
}

impl FunctionMetadata {
    /// Returns the role the input names as the function's owner.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE ROLE app_reader;
    ///      CREATE FUNCTION f() RETURNS INT AS 'SELECT 1';
    ///      ALTER FUNCTION f() OWNER TO app_reader;",
    /// )?;
    /// let function = db
    ///     .function_by_target(TargetName::new("f", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let metadata = db.function_metadata(function).expect("Metadata should exist");
    /// assert_eq!(metadata.owner(), Some("app_reader"));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    #[inline]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Sets the role the input names as the function's owner.
    ///
    /// # Arguments
    ///
    /// * `owner` - The owning role, or [`None`] when the input names no role.
    #[inline]
    pub fn set_owner(&mut self, owner: Option<String>) {
        self.owner = owner;
    }

    /// Returns whether a `NULL` call argument at `index` yields `NULL` despite
    /// non-strictness.
    #[must_use]
    #[inline]
    pub(crate) fn returns_null_on_null_argument(&self, index: usize) -> bool {
        self.null_returning_arguments.contains(&index)
    }

    /// Sets the call argument positions where `NULL` yields `NULL` despite
    /// non-strictness.
    #[inline]
    pub(crate) fn set_null_returning_arguments(&mut self, positions: &'static [usize]) {
        self.null_returning_arguments = positions;
    }

    /// Returns the function's `pg_proc.prokind`.
    #[must_use]
    #[inline]
    pub(crate) fn kind(&self) -> FunctionKind {
        self.kind
    }

    /// Sets the function's `pg_proc.prokind`.
    #[inline]
    pub(crate) fn set_kind(&mut self, kind: FunctionKind) {
        self.kind = kind;
    }

    /// Returns the name of the extension the function belongs to.
    #[must_use]
    #[inline]
    pub(crate) fn extension(&self) -> Option<&Ident> {
        self.extension.as_ref()
    }

    /// Records the extension the function belongs to.
    #[inline]
    pub(crate) fn set_extension(&mut self, extension: Ident) {
        self.extension = Some(extension);
    }

    /// Returns the calls of the function's body if PostgreSQL bound it at
    /// creation, with the functions each reached then.
    #[must_use]
    #[inline]
    pub(crate) fn body_calls(&self) -> &ExpressionCalls<CreateFunction> {
        &self.body_calls
    }

    /// Records the calls of the function's body, bound at creation.
    #[inline]
    pub(crate) fn set_body_calls(&mut self, calls: ExpressionCalls<CreateFunction>) {
        self.body_calls = calls;
    }

    /// Re-points every call of the body reaching the node `stale` at
    /// `fresh`.
    #[inline]
    pub(crate) fn replace_function(
        &mut self,
        stale: &Arc<CreateFunction>,
        fresh: &Arc<CreateFunction>,
    ) {
        self.body_calls.replace_function(stale, fresh);
    }

    /// Returns who holds `EXECUTE` on the function.
    #[must_use]
    pub(crate) fn execute_privileges(&self) -> &ExecutePrivileges {
        &self.execute
    }

    /// Returns who holds `EXECUTE` on the function, for a statement to change.
    pub(crate) fn execute_privileges_mut(&mut self) -> &mut ExecutePrivileges {
        &mut self.execute
    }

    /// Returns whether `role` is the role the input names as the owner.
    fn is_owned_by(&self, role: &CreateRole) -> bool {
        use crate::traits::RoleLike as _;

        self.owner.as_deref().is_some_and(|owner| owner == role.stored_name())
    }

    /// Returns whether `role` may execute the function, which it may when
    /// `PUBLIC` holds the privilege, when the role holds an entry of its own,
    /// or when the role is the recorded owner and still holds the owner's
    /// entry.
    #[must_use]
    pub(crate) fn can_execute(&self, role: &CreateRole) -> bool {
        self.execute.public
            || self.execute.grants.iter().any(|grant| grantee_matches_role(&grant.grantee, role))
            || (self.execute.owner && self.is_owned_by(role))
    }

    /// Returns whether `role` may grant `EXECUTE` on the function to others,
    /// which it may when an entry of its own carries the grant option, or when
    /// it is the recorded owner, whom ownership lets grant whatever its own
    /// entry says.
    #[must_use]
    pub(crate) fn can_grant_execute(&self, role: &CreateRole) -> bool {
        self.is_owned_by(role)
            || self
                .execute
                .grants
                .iter()
                .any(|grant| grant.with_grant_option && grantee_matches_role(&grant.grantee, role))
    }

    /// Takes `EXECUTE`, or only the right to grant it, from `grantees`.
    ///
    /// A grantee naming the recorded owner, as `names_owner` decides from the
    /// grantee and the owner's name, takes the owner's entry as well, which
    /// PostgreSQL keeps apart from the others only by whom it names.
    pub(crate) fn revoke_execute(
        &mut self,
        grantees: &[Grantee],
        grant_option_only: bool,
        names_owner: impl Fn(&Grantee, &str) -> bool,
    ) {
        let execute = &mut self.execute;
        if grant_option_only {
            // Ownership confers the grant option, so the owner keeps it.
            for grant in &mut execute.grants {
                if grantees.iter().any(|revoked| grantees_match(&grant.grantee, revoked)) {
                    grant.with_grant_option = false;
                }
            }
            return;
        }

        for revoked in grantees {
            if grantee_is_public(revoked) {
                execute.public = false;
            } else if self.owner.as_deref().is_some_and(|owner| names_owner(revoked, owner)) {
                execute.owner = false;
            }
        }
        execute.grants.retain(|grant| {
            !grantees.iter().any(|revoked| grantees_match(&grant.grantee, revoked))
        });
    }
}

/// Who holds `EXECUTE` on a function, as PostgreSQL's `pg_proc.proacl`
/// records it.
///
/// A function starts out executable by `PUBLIC` and by its owner. Each other
/// grantee holds one entry, which a later grant to the same grantee widens
/// rather than repeats. Grantors are not kept, since the input never says who
/// runs a statement, so the entries PostgreSQL tells apart by grantor fold
/// into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutePrivileges {
    /// Whether `PUBLIC` holds `EXECUTE`.
    public: bool,
    /// Whether the owner holds its own entry.
    owner: bool,
    /// The entries of every other grantee.
    grants: Vec<ExecuteGrant>,
}

/// One grantee's `EXECUTE` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecuteGrant {
    /// The role, or keyword principal, the entry names.
    grantee: Grantee,
    /// Whether the grantee may grant `EXECUTE` to others.
    with_grant_option: bool,
}

impl Default for ExecutePrivileges {
    /// The privileges PostgreSQL gives a new function.
    fn default() -> Self {
        Self { public: true, owner: true, grants: Vec::new() }
    }
}

impl ExecutePrivileges {
    /// Returns whether `PUBLIC` holds `EXECUTE`.
    #[must_use]
    pub(crate) fn public(&self) -> bool {
        self.public
    }

    /// Gives `EXECUTE` to `grantees`.
    ///
    /// PostgreSQL refuses a grant option for `PUBLIC`, so `PUBLIC` takes only
    /// the privilege.
    pub(crate) fn grant(&mut self, grantees: &[Grantee], with_grant_option: bool) {
        for grantee in grantees {
            if grantee_is_public(grantee) {
                self.public = true;
                continue;
            }
            match self.grants.iter_mut().find(|grant| grantees_match(&grant.grantee, grantee)) {
                Some(grant) => grant.with_grant_option |= with_grant_option,
                None => {
                    self.grants.push(ExecuteGrant { grantee: grantee.clone(), with_grant_option });
                }
            }
        }
    }

    /// Returns the grantees holding an entry of their own.
    pub(crate) fn grantees(&self) -> impl Iterator<Item = &Grantee> {
        self.grants.iter().map(|grant| &grant.grantee)
    }

    /// Returns the grantees holding an entry of their own, for a rename to
    /// rewrite.
    pub(crate) fn grantees_mut(&mut self) -> impl Iterator<Item = &mut Grantee> {
        self.grants.iter_mut().map(|grant| &mut grant.grantee)
    }
}
