//! A privilege a `REVOKE` took from one grantee on a table or one of its
//! columns.

use alloc::string::String;

use sqlparser::ast::{Action, CreateRole, Grantee, Ident};

use crate::impls::{grantee_is_public, grantee_matches_role};

/// One privilege a `REVOKE` took from one grantee on a table, or on one column
/// of it, which no later `GRANT` of that privilege at that level handed back.
///
/// The revocation is kept whether or not the statement matched a grant the
/// input recorded. A schema rarely carries every privilege a role holds, since
/// a platform may grant some through default privileges the migrations never
/// spell, and only an explicit `REVOKE` shows that a role lost them. PostgreSQL
/// takes only the entries whose grantor is the role the statement acts as,
/// which is the role named by `GRANTED BY` when there is one and the owner
/// when a superuser runs it, so [`Self::granted_by`] tells whose grants the
/// statement could reach.
/// [`TableLike::possible_privileges`](crate::traits::TableLike::possible_privileges)
/// reads these revocations into what a role may still hold.
///
/// `REVOKE ALL` revokes each table privilege on its own, and `REVOKE GRANT
/// OPTION FOR` revokes no privilege. A table-level `GRANT` hands back only the
/// table-level revocation, and a column-level one only the revocation on the
/// columns it lists.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "
/// CREATE ROLE app_user;
/// CREATE TABLE profiles (id TEXT, display_name TEXT, is_admin BOOL);
/// REVOKE UPDATE ON profiles FROM app_user;
/// GRANT UPDATE (display_name) ON profiles TO app_user;
/// ",
/// )?;
/// let app_user = db.role("app_user").unwrap();
/// let profiles =
///     db.table_by_target(TargetName::new("profiles", false), IdentifierCase::AsWritten)?.unwrap();
/// let revoked: Vec<String> = db
///     .table_metadata(profiles)
///     .unwrap()
///     .revoked_privileges()
///     .filter(|revoked| revoked.applies_to_role(app_user))
///     .map(|revoked| revoked.privilege().to_string())
///     .collect();
/// assert_eq!(revoked, ["UPDATE"]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RevokedPrivilege {
    /// The grantee the privilege was taken from, as written.
    grantee: Grantee,
    /// The privilege, naming no column.
    privilege: Action,
    /// The stored name of the role `GRANTED BY` names, if any.
    granted_by: Option<String>,
    /// The column the privilege was taken on, as the table declares it, or
    /// `None` for the table itself.
    column: Option<Ident>,
}

impl RevokedPrivilege {
    /// Records `privilege`, naming no column, taken from `grantee` on
    /// `column`, or on the whole table when `column` is `None`.
    pub(crate) fn new(
        grantee: Grantee,
        privilege: Action,
        granted_by: Option<String>,
        column: Option<Ident>,
    ) -> Self {
        Self { grantee, privilege, granted_by, column }
    }

    /// Returns the grantee the privilege was taken from, as written.
    #[must_use]
    pub fn grantee(&self) -> &Grantee {
        &self.grantee
    }

    /// Returns the privilege taken, naming no column.
    #[must_use]
    pub fn privilege(&self) -> &Action {
        &self.privilege
    }

    /// Returns the stored name of the role the statement's `GRANTED BY`
    /// names, or `None` when it names none.
    #[must_use]
    pub fn granted_by(&self) -> Option<&str> {
        self.granted_by.as_deref()
    }

    /// Returns whether the privilege was taken from `PUBLIC`, in either
    /// spelling the parser produces.
    #[must_use]
    pub fn applies_to_public(&self) -> bool {
        grantee_is_public(&self.grantee)
    }

    /// Returns whether the privilege was taken from `role`, which a
    /// revocation from `PUBLIC` reaches as it reaches every role.
    #[must_use]
    pub fn applies_to_role(&self, role: &CreateRole) -> bool {
        grantee_matches_role(&self.grantee, role)
    }

    /// The column the privilege was taken on, or `None` for the table.
    pub(crate) fn column(&self) -> Option<&Ident> {
        self.column.as_ref()
    }

    /// The grantee, for a role rename or an owner change to rewrite.
    pub(crate) fn grantee_mut(&mut self) -> &mut Grantee {
        &mut self.grantee
    }

    /// The stored grantor name, for a role rename or an owner change to
    /// rewrite.
    pub(crate) fn granted_by_mut(&mut self) -> &mut Option<String> {
        &mut self.granted_by
    }

    /// The column, for a column rename to rewrite.
    pub(crate) fn column_mut(&mut self) -> &mut Option<Ident> {
        &mut self.column
    }
}
