//! The grantor PostgreSQL records on the privileges a `GRANT` gives, which
//! `select_best_grantor` chooses from the role the statement acts as.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{mem::discriminant, ptr};

use sqlparser::ast::{Action, Grant, GrantObjects, Ident, ObjectName, Privileges};

use super::{
    NamedColumn, ParserDBBuilder, action_columns, grantee_role_ident, role_matches_lookup_ident,
    stored_role_name, views,
};
use crate::{
    impls::grant_objects_match,
    structs::IdentifierCase,
    traits::RoleLike,
    utils::{
        identifier_resolution::{SessionPrincipal, identifiers_match, session_principal},
        object_name::object_name_last_part,
    },
};

/// The identifier a stored grant spells the grantor stored as `name` with,
/// quoted so that its written value is the stored name.
pub(super) fn grantor_ident(name: &str) -> Ident {
    Ident::with_quote('"', name)
}

/// The stored name of the role a `GRANT` acts as: the one `GRANTED BY` names,
/// a session principal standing for the role the session holds, or else the
/// role statements run as. `None` when the input never names that role.
pub(super) fn acting_role(
    written: Option<&Ident>,
    current_role: Option<&str>,
    session_role: Option<&str>,
) -> Option<String> {
    let Some(written) = written else {
        return current_role.map(ToString::to_string);
    };
    match session_principal(&written.value, written.quote_style.is_some()) {
        Some(SessionPrincipal::CurrentUser) => current_role.map(ToString::to_string),
        Some(SessionPrincipal::SessionUser) => session_role.map(ToString::to_string),
        None => Some(stored_role_name(written)),
    }
}

/// `part`, a grant naming one object, recording the grantor PostgreSQL
/// chooses when `acting` runs it, or no grantor when `acting` is unknown.
pub(super) fn record(builder: &ParserDBBuilder, acting: Option<&str>, mut part: Grant) -> Grant {
    part.granted_by = acting
        .and_then(|acting| best_grantor(builder, &part, acting))
        .map(|name| grantor_ident(&name));
    part
}

/// The stored name of the role recorded as the grantor of `part` when
/// `acting` runs it, `None` when that is the owner and the owner is unknown.
///
/// A role holding none of the options the grant needs grants as the owner,
/// since the statement could only succeed for a superuser or a member of the
/// owner, which a role the input does not declare may be.
fn best_grantor(builder: &ParserDBBuilder, part: &Grant, acting: &str) -> Option<String> {
    let owner = object_owner(builder, part.objects.as_ref());
    if owner == Some(acting) || is_superuser(builder, acting) {
        return owner.map(ToString::to_string);
    }
    let options: Vec<&Grant> = builder
        .table_grants()
        .iter()
        .chain(builder.column_grants())
        .map(|(held, ())| held.as_ref())
        .filter(|held| {
            held.with_grant_option
                && grant_objects_match(held.objects.as_ref(), part.objects.as_ref())
        })
        .collect();
    if options.is_empty() {
        return owner.map(ToString::to_string);
    }
    let needed = needed_privileges(builder, part);
    let case = builder.identifier_case();
    let mut best: Option<(String, usize)> = None;
    for candidate in privilege_chain(builder, acting) {
        if owner == Some(candidate.as_str()) {
            return Some(candidate);
        }
        let held = needed
            .iter()
            .filter(|privilege| holds_option(&options, &candidate, privilege, case))
            .count();
        if held > 0 && held == needed.len() {
            return Some(candidate);
        }
        if held > best.as_ref().map_or(0, |(_, most)| *most) {
            best = Some((candidate, held));
        }
    }
    best.map(|(candidate, _)| candidate).or_else(|| owner.map(ToString::to_string))
}

/// The privileges `part` grants, `ALL PRIVILEGES` read as those of the object
/// it names, or none when that kind is unknown.
fn needed_privileges<'a>(builder: &ParserDBBuilder, part: &'a Grant) -> Cow<'a, [Action]> {
    match &part.privileges {
        Privileges::Actions(actions) => Cow::Borrowed(actions),
        Privileges::All { .. } => {
            Cow::Owned(
                builder
                    .privilege_kind(part.objects.as_ref())
                    .map(crate::impls::PrivilegeKind::all_privileges)
                    .unwrap_or_default(),
            )
        }
    }
}

/// The stored name of the role owning the one object `objects` names, when
/// the input records it.
fn object_owner<'b>(
    builder: &'b ParserDBBuilder,
    objects: Option<&GrantObjects>,
) -> Option<&'b str> {
    match objects? {
        GrantObjects::Tables(names) => relation_owner(builder, names.first()?),
        GrantObjects::Schemas(names) => {
            let (name, quoted) = object_name_last_part(names.first()?)?;
            builder
                .schemas()
                .iter()
                .find(|(schema, ())| {
                    identifiers_match(schema.name(), schema.is_quoted(), name, quoted)
                })
                .and_then(|(schema, ())| schema.authorization())
        }
        _ => None,
    }
}

/// The stored name of the role owning the table, view or materialized view
/// `name` reaches. A sequence records no owner.
fn relation_owner<'b>(builder: &'b ParserDBBuilder, name: &ObjectName) -> Option<&'b str> {
    if let Ok(Some(table)) = builder.resolve_table_object_name(name) {
        return builder
            .tables()
            .iter()
            .find(|(stored, _, _)| ptr::eq(stored.as_ref(), table))
            .and_then(|(_, metadata, _)| metadata.owner());
    }
    views::view_owner(builder, name)
}

/// Whether the input declares the role stored as `role` a superuser.
fn is_superuser(builder: &ParserDBBuilder, role: &str) -> bool {
    let role = grantor_ident(role);
    builder
        .roles()
        .iter()
        .any(|(declared, ())| role_matches_lookup_ident(declared, &role) && declared.is_superuser())
}

/// The stored names of `acting` and of every role whose privileges it uses
/// without `SET ROLE`, nearest first, as PostgreSQL's `roles_is_member_of`
/// lists them.
fn privilege_chain(builder: &ParserDBBuilder, acting: &str) -> Vec<String> {
    let mut chain = vec![acting.to_string()];
    let mut next = 0;
    while next < chain.len() {
        for (membership, ()) in builder.role_memberships() {
            if stored_role_name(&membership.member) != chain[next]
                || !membership.grants.iter().any(|grant| grant.inherit)
            {
                continue;
            }
            let role = stored_role_name(&membership.role);
            if !chain.contains(&role) {
                chain.push(role);
            }
        }
        next += 1;
    }
    chain
}

/// Whether one of `options`, the grants carrying an option on the object, gives
/// `role` the option to grant `privilege` there. A grant to `PUBLIC` never
/// carries an option.
fn holds_option(options: &[&Grant], role: &str, privilege: &Action, case: IdentifierCase) -> bool {
    options.iter().any(|held| {
        held.grantees.iter().any(|grantee| {
            grantee_role_ident(grantee).is_some_and(|ident| stored_role_name(ident) == role)
        }) && match &held.privileges {
            Privileges::All { .. } => true,
            Privileges::Actions(actions) => {
                actions.iter().any(|action| {
                    discriminant(action) == discriminant(privilege)
                        && match (action_columns(action), action_columns(privilege)) {
                            (None, _) => true,
                            (Some(_), None) => false,
                            (Some(held_columns), Some(needed)) => {
                                needed.iter().all(|column| {
                                    NamedColumn::of(column, case).in_idents(held_columns)
                                })
                            }
                        }
                })
            }
        }
    })
}
