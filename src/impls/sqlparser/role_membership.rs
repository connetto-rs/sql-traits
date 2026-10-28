//! Implementation of the `RoleMembershipLike` trait for the `RoleMembership`
//! struct.

use crate::{
    structs::{ParserDB, RoleMembership},
    traits::{Metadata, RoleMembershipLike},
};

impl Metadata for RoleMembership {
    type Meta = ();
}

impl RoleMembershipLike for RoleMembership {
    type DB = ParserDB;

    fn role_name(&self) -> &str {
        &self.role.value
    }

    fn role_name_is_quoted(&self) -> bool {
        self.role.quote_style.is_some()
    }

    fn member_name(&self) -> &str {
        &self.member.value
    }

    fn member_name_is_quoted(&self) -> bool {
        self.member.quote_style.is_some()
    }

    fn admin_option(&self) -> bool {
        self.grants.iter().any(|grant| grant.admin)
    }

    fn inherit_option(&self) -> bool {
        self.grants.iter().any(|grant| grant.inherit)
    }

    fn set_option(&self) -> bool {
        self.grants.iter().any(|grant| grant.set)
    }
}
