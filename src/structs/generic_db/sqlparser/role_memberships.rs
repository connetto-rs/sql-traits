//! Role memberships, from `GRANT role TO role` and from the `IN ROLE`,
//! `IN GROUP`, `ROLE`, `USER` and `ADMIN` clauses of `CREATE ROLE`, and their
//! removal by `REVOKE role FROM role`.
//!
//! PostgreSQL keeps one membership row for each granted role, member and
//! grantor. A repeated grant from the same grantor changes only the options
//! it writes, and a new row takes `ADMIN FALSE`, `SET TRUE` and the member's
//! `INHERIT` attribute at that moment for the options it leaves out.

use alloc::{string::ToString, sync::Arc, vec::Vec};
use core::mem::take;

use sqlparser::ast::{
    CascadeOption, CreateRole, GrantRole, Grantee, GranteeName, GranteesType, Ident, RevokeRole,
    RoleGrantOption, RoleGrantOptionValue, RoleOption,
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

/// One of the options a membership grant carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MembershipOption {
    Admin,
    Inherit,
    Set,
}

impl MembershipOption {
    /// Reads an option name as PostgreSQL does, refusing any other name.
    fn read(name: &Ident) -> Result<Self, Error> {
        match normalize_identifier(&name.value, name.quote_style.is_some()).as_ref() {
            "admin" => Ok(Self::Admin),
            "inherit" => Ok(Self::Inherit),
            "set" => Ok(Self::Set),
            _ => Err(Error::UnrecognizedRoleOption { option: name.value.clone() }),
        }
    }
}

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
            let slot = match MembershipOption::read(&option.name)? {
                MembershipOption::Admin => &mut written.admin,
                MembershipOption::Inherit => &mut written.inherit,
                MembershipOption::Set => &mut written.set,
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
    let members = named_members(&grant.roles, &grant.grantees, builder, access_resolution)?;
    for role in &grant.roles {
        for member in &members {
            let inherits = member_inherits(builder, role_alterations, member);
            record(builder, role, member, grant.granted_by.as_ref(), written, inherits);
        }
    }
    Ok(())
}

/// Applies `REVOKE [option OPTION FOR] role [, ...] FROM grantee [, ...]` to
/// the grant each named member holds from the grantor in `GRANTED BY`, or
/// from no written grantor, one role after another as PostgreSQL does.
///
/// A revoke reaching no grant changes nothing, as PostgreSQL only warns.
/// Taking away a member's last `ADMIN` option strands the grants it made,
/// which only `CASCADE` revokes.
pub(super) fn revoke_role(
    builder: &mut ParserDBBuilder,
    revoke: &RevokeRole,
    access_resolution: AccessResolution,
) -> Result<(), Error> {
    let option = revoke.option_for.as_ref().map(MembershipOption::read).transpose()?;
    let members = named_members(&revoke.roles, &revoke.grantees, builder, access_resolution)?;
    let cascade = revoke.cascade == Some(CascadeOption::Cascade);
    for role in &revoke.roles {
        let rows = plan_revoke(
            builder.role_memberships(),
            role,
            &members,
            revoke.granted_by.as_ref(),
            option,
            cascade,
        )?;
        apply_revoke(builder, &rows);
    }
    Ok(())
}

/// What a revoke does to one grant of the revoked role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Revocation {
    Keep,
    Clear(MembershipOption),
    Delete,
}

/// One grant of the revoked role, by its membership's position in the
/// builder and its own position in that membership.
#[derive(Debug, Clone, Copy)]
struct PlannedGrant {
    membership: usize,
    grant: usize,
    revocation: Revocation,
}

/// Plans the revoke of `role` from each of `members` over every grant of
/// `role`, following PostgreSQL's `plan_single_revoke`.
fn plan_revoke(
    memberships: &[(Arc<RoleMembership>, ())],
    role: &Ident,
    members: &[&Ident],
    grantor: Option<&Ident>,
    option: Option<MembershipOption>,
    cascade: bool,
) -> Result<Vec<PlannedGrant>, Error> {
    let mut rows: Vec<PlannedGrant> = memberships
        .iter()
        .enumerate()
        .filter(|(_, (membership, ()))| idents_match(&membership.role, role))
        .flat_map(|(membership, (granted, ()))| {
            (0..granted.grants.len())
                .map(move |grant| PlannedGrant { membership, grant, revocation: Revocation::Keep })
        })
        .collect();
    for member in members {
        let Some(index) = rows.iter().position(|row| {
            let (membership, grant) = planned(memberships, row);
            idents_match(&membership.member, member)
                && same_grantor(grant.grantor.as_ref(), grantor)
        }) else {
            continue;
        };
        match option {
            Some(cleared @ (MembershipOption::Inherit | MembershipOption::Set)) => {
                rows[index].revocation = Revocation::Clear(cleared);
            }
            Some(MembershipOption::Admin) => {
                plan_recursive_revoke(memberships, &mut rows, index, true, cascade, role)?;
            }
            None => plan_recursive_revoke(memberships, &mut rows, index, false, cascade, role)?,
        }
    }
    Ok(rows)
}

/// Plans taking away the grant at `index`, or only its `ADMIN` option, and
/// with it the grants its member can no longer back, following
/// PostgreSQL's `plan_recursive_revoke`.
fn plan_recursive_revoke(
    memberships: &[(Arc<RoleMembership>, ())],
    rows: &mut [PlannedGrant],
    index: usize,
    admin_only: bool,
    cascade: bool,
    role: &Ident,
) -> Result<(), Error> {
    match rows[index].revocation {
        Revocation::Delete => return Ok(()),
        Revocation::Clear(MembershipOption::Admin) if admin_only => return Ok(()),
        _ => {}
    }
    let (membership, grant) = planned(memberships, &rows[index]);
    if !admin_only {
        rows[index].revocation = Revocation::Delete;
    }
    if !grant.admin {
        return Ok(());
    }
    if admin_only {
        rows[index].revocation = Revocation::Clear(MembershipOption::Admin);
    }

    let member = &membership.member;
    let still_admin = rows.iter().any(|row| {
        let (other, other_grant) = planned(memberships, row);
        row.revocation == Revocation::Keep
            && other_grant.admin
            && idents_match(&other.member, member)
    });
    if still_admin {
        return Ok(());
    }
    // Indexed because each recursion replans rows this loop has yet to reach.
    for dependent in 0..rows.len() {
        let (_, dependent_grant) = planned(memberships, &rows[dependent]);
        if rows[dependent].revocation == Revocation::Delete
            || !dependent_grant
                .grantor
                .as_ref()
                .is_some_and(|grantor| idents_match(grantor, member))
        {
            continue;
        }
        if !cascade {
            return Err(Error::DependentRoleMemberships {
                role_name: role.value.clone(),
                member_name: member.value.clone(),
            });
        }
        plan_recursive_revoke(memberships, rows, dependent, false, cascade, role)?;
    }
    Ok(())
}

/// The membership and grant a planned row stands for.
fn planned<'a>(
    memberships: &'a [(Arc<RoleMembership>, ())],
    row: &PlannedGrant,
) -> (&'a RoleMembership, &'a MembershipGrant) {
    let membership = memberships[row.membership].0.as_ref();
    (membership, &membership.grants[row.grant])
}

/// Applies a planned revoke, dropping the memberships it leaves without a
/// grant.
fn apply_revoke(builder: &mut ParserDBBuilder, rows: &[PlannedGrant]) {
    let memberships = builder.role_memberships_mut();
    let mut emptied = false;
    // Rows run in ascending grant order within a membership, so walking them
    // backwards removes each grant before the ones ahead of it shift.
    for row in rows.iter().rev().filter(|row| row.revocation != Revocation::Keep) {
        let membership = Arc::make_mut(&mut memberships[row.membership].0);
        let grant = &mut membership.grants[row.grant];
        match row.revocation {
            Revocation::Keep => {}
            Revocation::Clear(MembershipOption::Admin) => grant.admin = false,
            Revocation::Clear(MembershipOption::Inherit) => grant.inherit = false,
            Revocation::Clear(MembershipOption::Set) => grant.set = false,
            Revocation::Delete => {
                membership.grants.remove(row.grant);
                emptied |= membership.grants.is_empty();
            }
        }
    }
    if emptied {
        memberships.retain(|(membership, ())| !membership.grants.is_empty());
    }
}

/// Refuses `PUBLIC` and, under [`AccessResolution::ClosedWorld`], a role the
/// input has not created, among the roles and grantees of a membership
/// statement, and returns the members the grantees name.
fn named_members<'a>(
    roles: &[Ident],
    grantees: &'a [Grantee],
    builder: &ParserDBBuilder,
    access_resolution: AccessResolution,
) -> Result<Vec<&'a Ident>, Error> {
    let mut members = Vec::with_capacity(grantees.len());
    for grantee in grantees {
        members.extend(grantee_member(grantee)?);
    }
    for role in roles.iter().chain(members.iter().copied()) {
        refuse_public(role)?;
        require_role(builder, access_resolution, role)?;
    }
    Ok(members)
}

/// Whether a grant's grantor is the one a statement names, a grant written
/// without `GRANTED BY` matching only a statement written without it.
fn same_grantor(granted_by: Option<&Ident>, grantor: Option<&Ident>) -> bool {
    match (granted_by, grantor) {
        (None, None) => true,
        (Some(granted_by), Some(grantor)) => idents_match(granted_by, grantor),
        _ => false,
    }
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

    if let Some(grant) =
        membership.grants.iter_mut().find(|grant| same_grantor(grant.grantor.as_ref(), grantor))
    {
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
