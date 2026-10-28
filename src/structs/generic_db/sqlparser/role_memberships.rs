//! `GRANT role TO role` and `REVOKE role FROM role`, which change the roles a
//! role is a member of.
//!
//! A membership is kept where `CREATE ROLE ... IN ROLE` keeps it, on the
//! member, so [`RoleLike::member_of`](crate::traits::RoleLike::member_of)
//! answers for both spellings. The options a grant carries (`ADMIN`,
//! `INHERIT`, `SET`) are not recorded, so revoking one leaves the membership.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{GrantRole, Grantee, Ident, RevokeRole};

use super::{
    AccessResolution, ParserDBBuilder, grantee_role_ident, idents_match, role_matches_lookup_ident,
};
use crate::{errors::Error, impls::grantee_is_public};

/// Makes each grantee a member of each role the grant names.
pub(super) fn grant(
    builder: &mut ParserDBBuilder,
    grant: &GrantRole,
    access_resolution: AccessResolution,
) -> Result<(), Error> {
    let roles = resolve(builder, &grant.roles, access_resolution)?;
    let member_names = member_idents(&grant.grantees)?;
    let members = resolve(builder, &member_names, access_resolution)?;

    for &(role, _) in &roles {
        for &(member, member_position) in &members {
            if is_member_of(builder, role, member) {
                return Err(Error::CircularRoleMembership {
                    role_name: role.value.clone(),
                    member_name: member.value.clone(),
                });
            }
            let Some(position) = member_position else {
                continue;
            };
            let in_role = &mut Arc::make_mut(&mut builder.roles_mut()[position].0).in_role;
            if !in_role.iter().any(|parent| idents_match(parent, role)) {
                in_role.push(role.clone());
            }
        }
    }
    Ok(())
}

/// Ends each grantee's membership in each role the revoke names, or, for
/// `REVOKE ... OPTION FOR`, leaves it in place since options are not kept.
pub(super) fn revoke(
    builder: &mut ParserDBBuilder,
    revoke: &RevokeRole,
    access_resolution: AccessResolution,
) -> Result<(), Error> {
    let roles = resolve(builder, &revoke.roles, access_resolution)?;
    let member_names = member_idents(&revoke.grantees)?;
    let members = resolve(builder, &member_names, access_resolution)?;
    if revoke.option_for.is_some() {
        return Ok(());
    }

    for &(_, member_position) in &members {
        let Some(position) = member_position else {
            continue;
        };
        Arc::make_mut(&mut builder.roles_mut()[position].0)
            .in_role
            .retain(|parent| !roles.iter().any(|&(role, _)| idents_match(parent, role)));
    }
    Ok(())
}

/// The roles the grantees name. `PUBLIC` is no role a membership can go to,
/// and a session keyword names no role the input spells, so it is passed over.
fn member_idents(grantees: &[Grantee]) -> Result<Vec<Ident>, Error> {
    let mut members = Vec::with_capacity(grantees.len());
    for grantee in grantees {
        if grantee_is_public(grantee) {
            return Err(Error::RoleNotFoundForGrant { role_name: "public".to_string() });
        }
        members.extend(grantee_role_ident(grantee).cloned());
    }
    Ok(members)
}

/// Pairs each name with the position of the role it reaches, refusing an
/// absent one under the closed world.
fn resolve<'a>(
    builder: &ParserDBBuilder,
    names: &'a [Ident],
    access_resolution: AccessResolution,
) -> Result<Vec<(&'a Ident, Option<usize>)>, Error> {
    names
        .iter()
        .map(|name| {
            let position =
                builder.roles().iter().position(|(role, ())| role_matches_lookup_ident(role, name));
            if position.is_none() && access_resolution == AccessResolution::ClosedWorld {
                return Err(Error::RoleNotFoundForGrant { role_name: name.value.clone() });
            }
            Ok((name, position))
        })
        .collect()
}

/// Whether `role` is `target` or a member of it, directly or through other
/// memberships.
fn is_member_of(builder: &ParserDBBuilder, role: &Ident, target: &Ident) -> bool {
    let mut pending = alloc::vec![role.clone()];
    let mut visited: Vec<Ident> = Vec::new();
    while let Some(current) = pending.pop() {
        if idents_match(&current, target) {
            return true;
        }
        if visited.iter().any(|seen| idents_match(seen, &current)) {
            continue;
        }
        if let Some((node, ())) =
            builder.roles().iter().find(|(node, ())| role_matches_lookup_ident(node, &current))
        {
            pending.extend(node.in_role.iter().cloned());
        }
        visited.push(current);
    }
    false
}
