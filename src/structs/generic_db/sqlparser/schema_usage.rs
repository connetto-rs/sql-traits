//! The `USAGE` entries a schema holds before any `GRANT`, `PUBLIC`'s on a new
//! database's `public` and the owner's own, and the grants an owner change
//! hands over, as PostgreSQL's `aclnewowner` renames the old owner.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::{Action, GrantObjects, Ident, Privileges, Revoke};

use super::{ParserDBBuilder, grantee_role_ident, rename_grantee, stored_role_name};
use crate::{
    impls::{grantee_is_public, grantees_match},
    utils::{identifier_resolution::identifiers_match, object_name::object_name_last_part},
};

/// Takes `USAGE` from `PUBLIC`'s initial entry and the owner's own entry on
/// each schema `revoke` names, where its grantees name them.
pub(super) fn revoke(builder: &mut ParserDBBuilder, revoke: &Revoke) {
    // A revoke of the grant option leaves the privilege itself.
    if revoke.grant_option_for {
        return;
    }
    let Some(GrantObjects::Schemas(names)) = &revoke.objects else { return };
    let takes_usage = match &revoke.privileges {
        Privileges::All { .. } => true,
        Privileges::Actions(actions) => {
            actions.iter().any(|action| matches!(action, Action::Usage))
        }
    };
    if !takes_usage {
        return;
    }
    let from_public = revoke.grantees.iter().any(grantee_is_public);
    for (schema, ()) in builder.schemas_mut() {
        let revoked_here = names.iter().any(|name| {
            object_name_last_part(name).is_some_and(|(name, quoted)| {
                identifiers_match(schema.name(), schema.is_quoted(), name, quoted)
            })
        });
        if !revoked_here {
            continue;
        }
        let from_owner = schema.authorization().is_some_and(|owner| {
            revoke.grantees.iter().any(|grantee| {
                grantee_role_ident(grantee).is_some_and(|ident| stored_role_name(ident) == owner)
            })
        });
        if from_public {
            Arc::make_mut(schema).revoke_public_usage();
        }
        if from_owner {
            Arc::make_mut(schema).revoke_owner_usage();
        }
    }
}

/// Hands every grant on the schema `(name, quoted)` naming `previous`, as
/// grantee or grantor, to `next`.
pub(super) fn change_owner(
    builder: &mut ParserDBBuilder,
    (name, quoted): (&str, bool),
    previous: &str,
    next: &Ident,
) {
    let names_previous =
        |ident: Option<&Ident>| ident.is_some_and(|ident| stored_role_name(ident) == previous);
    for (grant, ()) in builder.table_grants_mut() {
        let on_schema = matches!(
            &grant.objects,
            Some(GrantObjects::Schemas(names)) if names.iter().any(|granted| {
                object_name_last_part(granted).is_some_and(|(granted, granted_quoted)| {
                    identifiers_match(granted, granted_quoted, name, quoted)
                })
            })
        );
        let handed =
            grant.grantees.iter().any(|grantee| names_previous(grantee_role_ident(grantee)))
                || names_previous(grant.granted_by.as_ref());
        if !on_schema || !handed {
            continue;
        }
        let grant = Arc::make_mut(grant);
        let mut grantees = Vec::with_capacity(grant.grantees.len());
        for mut grantee in core::mem::take(&mut grant.grantees) {
            if names_previous(grantee_role_ident(&grantee)) {
                rename_grantee(&mut grantee, next);
            }
            if !grantees.iter().any(|kept| grantees_match(kept, &grantee)) {
                grantees.push(grantee);
            }
        }
        grant.grantees = grantees;
        if names_previous(grant.granted_by.as_ref()) {
            grant.granted_by = Some(next.clone());
        }
    }
}
