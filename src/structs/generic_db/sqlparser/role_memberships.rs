//! Role memberships, from `GRANT role TO role` and from the `IN ROLE`,
//! `IN GROUP`, `ROLE`, `USER` and `ADMIN` clauses of `CREATE ROLE`.
//!
//! PostgreSQL keeps one membership row for each granted role, member and
//! grantor. A repeated grant from the same grantor changes only the options
//! it writes, and a new row takes `ADMIN FALSE`, `SET TRUE` and the member's
//! `INHERIT` attribute at that moment for the options it leaves out.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::mem::take;

use sqlparser::ast::{
    CreateRole, GrantRole, Grantee, GranteeName, GranteesType, Ident, RoleGrantOption,
    RoleGrantOptionValue, RoleOption,
};

use super::{
    AccessResolution, ParserDBBuilder, UndeclaredRoleAlteration, grantee_role_ident, idents_match,
    object_name_last_identifier, role_matches_lookup_ident,
};
use crate::{
    errors::Error,
    structs::{RoleMembership, role_membership::MembershipGrant},
    traits::RoleLike,
    utils::identifier_resolution::{is_public_pseudo_role, normalize_identifier},
};

/// The options one grant writes, each `None` when the grant leaves it out.
#[derive(Debug, Clone, Copy, Default)]
struct WrittenOptions {
    admin: Option<bool>,
    inherit: Option<bool>,
    set: Option<bool>,
}

impl WrittenOptions {
    /// The options of the `ADMIN` clause of `CREATE ROLE`.
    const ADMIN: Self = Self { admin: Some(true), inherit: None, set: None };

    /// Reads the `WITH` list of `GRANT role TO role`, a later option of a
    /// kind replacing an earlier one as it does in PostgreSQL.
    fn read(options: &[RoleGrantOption]) -> Result<Self, Error> {
        let mut written = Self::default();
        for option in options {
            let name = normalize_identifier(&option.name.value, option.name.quote_style.is_some());
            let slot = match name.as_ref() {
                "admin" => &mut written.admin,
                "inherit" => &mut written.inherit,
                "set" => &mut written.set,
                _ => {
                    return Err(Error::UnrecognizedRoleOption {
                        option: option.name.value.clone(),
                    });
                }
            };
            *slot = Some(option.value != RoleGrantOptionValue::False);
        }
        Ok(written)
    }
}

/// The membership clauses of one `CREATE ROLE`, taken out of the statement so
/// the stored role holds its attributes alone and the memberships live with
/// every other membership.
pub(super) struct CreateRoleMemberships {
    /// Roles the created role becomes a member of, from `IN ROLE` and
    /// `IN GROUP`.
    parents: Vec<Ident>,
    /// Members of the created role, from `ROLE` and `USER`.
    members: Vec<Ident>,
    /// Members of the created role holding `ADMIN`, from `ADMIN`.
    admins: Vec<Ident>,
}

impl CreateRoleMemberships {
    /// Takes the membership clauses out of `create_role`.
    pub(super) fn take(create_role: &mut CreateRole) -> Self {
        let mut parents = take(&mut create_role.in_role);
        parents.append(&mut create_role.in_group);
        let mut members = take(&mut create_role.role);
        members.append(&mut create_role.user);
        Self { parents, members, admins: take(&mut create_role.admin) }
    }

    /// Refuses a clause naming `PUBLIC`, or, under
    /// [`AccessResolution::ClosedWorld`], a role the input has not created.
    pub(super) fn validate(
        &self,
        builder: &ParserDBBuilder,
        access_resolution: AccessResolution,
    ) -> Result<(), Error> {
        for role in self.parents.iter().chain(&self.members).chain(&self.admins) {
            refuse_public(role)?;
            require_role(builder, access_resolution, role)?;
        }
        Ok(())
    }

    /// Records the memberships of `created`, the role the statement stored.
    pub(super) fn record(
        self,
        builder: &mut ParserDBBuilder,
        created: &CreateRole,
        role_alterations: &[UndeclaredRoleAlteration],
    ) {
        for name in created.names.iter().filter_map(object_name_last_identifier) {
            for parent in &self.parents {
                let inherits = member_inherits(builder, role_alterations, name);
                record(builder, parent, name, None, WrittenOptions::default(), inherits);
            }
            for (member, written) in self
                .members
                .iter()
                .map(|member| (member, WrittenOptions::default()))
                .chain(self.admins.iter().map(|admin| (admin, WrittenOptions::ADMIN)))
            {
                let inherits = member_inherits(builder, role_alterations, member);
                record(builder, name, member, None, written, inherits);
            }
        }
    }
}

/// Records `GRANT role [, ...] TO grantee [, ...]` as one membership for
/// each granted role and member.
///
/// A grantee naming the session's own role, such as `CURRENT_USER`, names no
/// role the input could declare and records nothing. The grantor is kept to
/// tell grants apart and is not required to exist, since `pg_dumpall` names
/// grantors such as `postgres` that a schema rarely creates.
pub(super) fn grant_role(
    builder: &mut ParserDBBuilder,
    grant: &GrantRole,
    access_resolution: AccessResolution,
    role_alterations: &[UndeclaredRoleAlteration],
) -> Result<(), Error> {
    let written = WrittenOptions::read(&grant.options)?;
    let mut members = Vec::with_capacity(grant.grantees.len());
    for grantee in &grant.grantees {
        members.extend(grantee_member(grantee)?);
    }
    for role in grant.roles.iter().chain(members.iter().copied()) {
        refuse_public(role)?;
        require_role(builder, access_resolution, role)?;
    }

    for role in &grant.roles {
        for member in &members {
            let inherits = member_inherits(builder, role_alterations, member);
            record(builder, role, member, grant.granted_by.as_ref(), written, inherits);
        }
    }
    Ok(())
}

/// Repoints every membership naming `previous`, as the granted role, the
/// member or a grantor, to `replacement`.
pub(super) fn rename_role(builder: &mut ParserDBBuilder, previous: &Ident, replacement: &Ident) {
    for (membership, ()) in builder.role_memberships_mut() {
        if !membership_names(membership, previous) {
            continue;
        }
        let membership = Arc::make_mut(membership);
        let grantors = membership.grants.iter_mut().filter_map(|grant| grant.grantor.as_mut());
        for ident in [&mut membership.role, &mut membership.member].into_iter().chain(grantors) {
            if idents_match(ident, previous) {
                *ident = replacement.clone();
            }
        }
    }
}

/// Removes the memberships a dropped role held and the ones granted in it,
/// which PostgreSQL removes with the role.
pub(super) fn drop_role(builder: &mut ParserDBBuilder, removed: &Ident) {
    builder.role_memberships_mut().retain(|(membership, ())| {
        !idents_match(&membership.role, removed) && !idents_match(&membership.member, removed)
    });
}

/// Whether `grantor` granted a membership, which PostgreSQL holds against
/// dropping it.
pub(super) fn granted_by(builder: &ParserDBBuilder, grantor: &Ident) -> bool {
    builder.role_memberships().iter().any(|(membership, ())| {
        membership
            .grants
            .iter()
            .filter_map(|grant| grant.grantor.as_ref())
            .any(|granted_by| idents_match(granted_by, grantor))
    })
}

fn membership_names(membership: &RoleMembership, role: &Ident) -> bool {
    idents_match(&membership.role, role)
        || idents_match(&membership.member, role)
        || membership
            .grants
            .iter()
            .filter_map(|grant| grant.grantor.as_ref())
            .any(|grantor| idents_match(grantor, role))
}

/// The member a grantee names, or `None` when it names the session's own
/// role.
fn grantee_member(grantee: &Grantee) -> Result<Option<&Ident>, Error> {
    if grantee.grantee_type == GranteesType::Public {
        return Err(Error::RoleNotFoundForMembership { role_name: "public".to_string() });
    }
    if let Some(GranteeName::ObjectName(name)) = &grantee.name
        && let Some(ident) = object_name_last_identifier(name)
    {
        refuse_public(ident)?;
    }
    Ok(grantee_role_ident(grantee))
}

/// Refuses `PUBLIC`, which PostgreSQL answers with `role "public" does not
/// exist` whichever side of a membership it stands on.
fn refuse_public(role: &Ident) -> Result<(), Error> {
    if is_public_pseudo_role(&role.value, role.quote_style.is_some()) {
        return Err(Error::RoleNotFoundForMembership { role_name: role.value.clone() });
    }
    Ok(())
}

/// Enforces [`AccessResolution::ClosedWorld`] on a role a membership names.
fn require_role(
    builder: &ParserDBBuilder,
    access_resolution: AccessResolution,
    role: &Ident,
) -> Result<(), Error> {
    if access_resolution == AccessResolution::ClosedWorld
        && !builder.roles().iter().any(|(declared, ())| role_matches_lookup_ident(declared, role))
    {
        return Err(Error::RoleNotFoundForMembership { role_name: role.value.clone() });
    }
    Ok(())
}

/// The member's `INHERIT` attribute as it stands, which a new membership
/// takes for its `INHERIT` option when the grant leaves it out.
///
/// A member the input never created takes the attribute an `ALTER ROLE` kept
/// for it, and otherwise the PostgreSQL default, `INHERIT`.
fn member_inherits(
    builder: &ParserDBBuilder,
    role_alterations: &[UndeclaredRoleAlteration],
    member: &Ident,
) -> bool {
    if let Some((role, ())) =
        builder.roles().iter().find(|(role, ())| role_matches_lookup_ident(role, member))
    {
        return role.inherits();
    }
    role_alterations
        .iter()
        .filter(|kept| idents_match(&kept.role, member))
        .flat_map(|kept| &kept.options)
        .find_map(|option| {
            match option {
                RoleOption::Inherit(inherit) => Some(*inherit),
                _ => None,
            }
        })
        .unwrap_or(true)
}

/// Records one grant of `role` to `member` by `grantor`.
fn record(
    builder: &mut ParserDBBuilder,
    role: &Ident,
    member: &Ident,
    grantor: Option<&Ident>,
    written: WrittenOptions,
    member_inherits: bool,
) {
    let memberships = builder.role_memberships_mut();
    let position = memberships
        .iter()
        .position(|(membership, ())| {
            idents_match(&membership.role, role) && idents_match(&membership.member, member)
        })
        .unwrap_or_else(|| {
            let membership =
                RoleMembership { role: role.clone(), member: member.clone(), grants: Vec::new() };
            memberships.push((Arc::new(membership), ()));
            memberships.len() - 1
        });
    let membership = Arc::make_mut(&mut memberships[position].0);

    let same_grantor = |grant: &&mut MembershipGrant| {
        match (&grant.grantor, grantor) {
            (None, None) => true,
            (Some(granted_by), Some(grantor)) => idents_match(granted_by, grantor),
            _ => false,
        }
    };
    if let Some(grant) = membership.grants.iter_mut().find(same_grantor) {
        if let Some(admin) = written.admin {
            grant.admin = admin;
        }
        if let Some(inherit) = written.inherit {
            grant.inherit = inherit;
        }
        if let Some(set) = written.set {
            grant.set = set;
        }
    } else {
        membership.grants.push(MembershipGrant {
            grantor: grantor.cloned(),
            admin: written.admin.unwrap_or(false),
            inherit: written.inherit.unwrap_or(member_inherits),
            set: written.set.unwrap_or(true),
        });
    }
}
