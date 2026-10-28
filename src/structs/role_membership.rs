//! Role membership struct recording the grants of one role to another.

use alloc::vec::Vec;

use sqlparser::ast::Ident;

/// The membership of one role in another, recorded from `GRANT role TO role`
/// or from a membership clause of `CREATE ROLE`.
///
/// PostgreSQL keeps one membership row for each grantor, so the grants of
/// the membership stay apart by the grantor written in `GRANTED BY`, or by
/// its absence. The options [`RoleMembershipLike`] reports are those in
/// effect, any grant carrying an option being enough.
///
/// [`RoleMembershipLike`]: crate::traits::RoleMembershipLike
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleMembership {
    /// The granted role, as written.
    pub(crate) role: Ident,
    /// The member, as written.
    pub(crate) member: Ident,
    /// One entry for each grantor, in the order first granted.
    pub(crate) grants: Vec<MembershipGrant>,
}

/// One grantor's grant of a membership, with the options it carries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MembershipGrant {
    /// The role written in `GRANTED BY`, or `None` when the grant names none.
    pub(crate) grantor: Option<Ident>,
    /// The `ADMIN` option.
    pub(crate) admin: bool,
    /// The `INHERIT` option.
    pub(crate) inherit: bool,
    /// The `SET` option.
    pub(crate) set: bool,
}
