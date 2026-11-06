//! One entry of an object's access list, naming who holds a privilege
//! directly.

use alloc::borrow::Cow;

use crate::utils::identifier_resolution::SessionPrincipal;

/// Who an entry of an access list grants a privilege to, as PostgreSQL's
/// `aclexplode` lists the entries of an object's access list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PrivilegeHolder<'a> {
    /// Every role, through the `PUBLIC` pseudo-role.
    Public,
    /// A role, by the name PostgreSQL stores, whether or not the input
    /// declares it.
    Role(Cow<'a, str>),
    /// The role a `CURRENT_USER`, `CURRENT_ROLE` or `SESSION_USER` grantee
    /// named, which PostgreSQL resolved when the statement ran to a role the
    /// input does not say.
    SessionPrincipal(SessionPrincipal),
}
