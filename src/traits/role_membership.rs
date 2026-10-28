//! Submodule providing a trait for describing role memberships.

use alloc::borrow::Cow;
use core::fmt::Debug;

use crate::{
    traits::{DatabaseLike, Metadata},
    utils::identifier_resolution::normalize_identifier,
};

/// A trait for types that can be treated as the membership of one role in
/// another.
///
/// A membership lets its member use the privileges of the granted role, and
/// PostgreSQL 16 gives each membership its own `ADMIN`, `INHERIT` and `SET`
/// options. The options reported are the ones in effect. When several
/// grantors granted the same membership, an option holds when any of their
/// grants carries it, which is how PostgreSQL decides `SET ROLE` and
/// privilege inheritance.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "
/// CREATE ROLE bypasser BYPASSRLS;
/// CREATE ROLE app_user;
/// GRANT bypasser TO app_user WITH ADMIN TRUE, INHERIT FALSE;
/// ",
/// )?;
///
/// let membership = db.role_memberships().next().unwrap();
/// assert_eq!(membership.role_name(), "bypasser");
/// assert_eq!(membership.member_name(), "app_user");
/// assert!(membership.admin_option());
/// assert!(!membership.inherit_option());
/// assert!(membership.set_option());
/// assert!(membership.role(&db).unwrap().can_bypass_rls());
/// # Ok(())
/// # }
/// ```
pub trait RoleMembershipLike: Debug + Clone + Ord + Eq + Metadata + Send + Sync {
    /// The database type the membership belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the granted role, as written.
    fn role_name(&self) -> &str;

    /// Returns whether the granted role's name was quoted.
    fn role_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the canonical stored name of the granted role.
    fn stored_role_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.role_name(), self.role_name_is_quoted())
    }

    /// Returns the name of the member, as written.
    fn member_name(&self) -> &str;

    /// Returns whether the member's name was quoted.
    fn member_name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the canonical stored name of the member.
    fn stored_member_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.member_name(), self.member_name_is_quoted())
    }

    /// Returns the granted role, or `None` when the database holds no role
    /// by its name, as a schema checked under an open world may not.
    fn role<'db>(&self, database: &'db Self::DB) -> Option<&'db <Self::DB as DatabaseLike>::Role> {
        database.role(&self.stored_role_name())
    }

    /// Returns the member, or `None` when the database holds no role by its
    /// name.
    fn member<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Role> {
        database.role(&self.stored_member_name())
    }

    /// Returns whether the member may grant and revoke the membership to
    /// others, the `ADMIN` option.
    fn admin_option(&self) -> bool;

    /// Returns whether the member uses the privileges of the granted role
    /// without `SET ROLE`, the `INHERIT` option.
    fn inherit_option(&self) -> bool;

    /// Returns whether the member may `SET ROLE` to the granted role, the
    /// `SET` option.
    fn set_option(&self) -> bool;
}
