//! The table owner's own privilege entry, and the grants an owner change hands
//! over, as PostgreSQL's `aclnewowner` renames the old owner on the table.

use alloc::{sync::Arc, vec, vec::Vec};

use sqlparser::ast::{
    CreateTable, Grant, GrantObjects, Grantee, GranteeName, Ident, ObjectName, ObjectNamePart,
    Owner, Revoke,
};

use super::{ParserDBBuilder, StoredTable, grantee_role_ident, stored_role_name};
use crate::impls::grantees_match;

/// Takes the privileges `revoke` names from the owner's entry on each table it
/// reaches that one of its grantees owns.
///
/// The names `revoke` carries are bound, so each is read as written.
pub(super) fn revoke(builder: &mut ParserDBBuilder, revoke: &Revoke) {
    // Ownership confers the grant option, so the owner keeps it.
    if revoke.grant_option_for {
        return;
    }
    let Some(GrantObjects::Tables(names)) = &revoke.objects else {
        return;
    };
    let reached: Vec<StoredTable> = names
        .iter()
        .filter_map(|name| builder.resolve_bound_table_name(name).ok().flatten())
        .map(StoredTable::of)
        .collect();
    if reached.is_empty() {
        return;
    }

    let case = builder.identifier_case();
    for (table, metadata, _) in builder.tables_mut() {
        if !reached.iter().any(|stored| stored.matches(table, case)) {
            continue;
        }
        let Some(owner) = metadata.owner() else {
            continue;
        };
        if revoke.grantees.iter().any(|grantee| names_role(grantee, owner)) {
            metadata.revoke_owner_privileges(&revoke.privileges);
        }
    }
}

/// Hands every grant on `handed` naming `previous`, as grantee or grantor, to
/// `next`.
///
/// Each relation grant names one relation, so a grant on `handed` is handed
/// over whole.
pub(super) fn change_owner(
    builder: &mut ParserDBBuilder,
    handed: &CreateTable,
    previous: &str,
    next: &Owner,
) {
    let table = StoredTable::of(handed);
    let handover = Handover { table: &table, previous, next: owner_ident(next) };
    handover.apply(builder.table_grants_mut());
    handover.apply(builder.column_grants_mut());
}

/// One owner change of one table.
struct Handover<'a> {
    /// The table changing hands.
    table: &'a StoredTable,
    /// The canonical name of the old owner.
    previous: &'a str,
    /// The new owner, as a grant would spell it.
    next: Ident,
}

impl Handover<'_> {
    /// Rewrites one grant store.
    fn apply(&self, grants: &mut [(Arc<Grant>, ())]) {
        for (grant, ()) in grants {
            if self.hands_over(grant) {
                self.rename_previous(Arc::make_mut(grant));
            }
        }
    }

    /// Whether `grant` is on the table changing hands and names the old owner
    /// as grantee or grantor.
    fn hands_over(&self, grant: &Grant) -> bool {
        let on_table = matches!(&grant.objects, Some(GrantObjects::Tables(names))
            if names.iter().any(|name| self.table.named_by(name)));
        on_table
            && (grant.grantees.iter().any(|grantee| names_role(grantee, self.previous))
                || grant
                    .granted_by
                    .as_ref()
                    .is_some_and(|grantor| stored_role_name(grantor) == self.previous))
    }

    /// Renames the old owner to the new one in `grant`, as grantee and as
    /// grantor, keeping one grantee where the new owner was already one.
    fn rename_previous(&self, grant: &mut Grant) {
        let mut grantees: Vec<Grantee> = Vec::with_capacity(grant.grantees.len());
        for grantee in core::mem::take(&mut grant.grantees) {
            let grantee = if names_role(&grantee, self.previous) {
                Grantee {
                    name: Some(GranteeName::ObjectName(ObjectName(vec![
                        ObjectNamePart::Identifier(self.next.clone()),
                    ]))),
                    ..grantee
                }
            } else {
                grantee
            };
            if !grantees.iter().any(|kept| grantees_match(kept, &grantee)) {
                grantees.push(grantee);
            }
        }
        grant.grantees = grantees;
        if let Some(grantor) = &mut grant.granted_by
            && stored_role_name(grantor) == self.previous
        {
            *grantor = self.next.clone();
        }
    }
}

/// Whether `grantee` names the role stored as `role`.
fn names_role(grantee: &Grantee, role: &str) -> bool {
    grantee_role_ident(grantee).is_some_and(|ident| stored_role_name(ident) == role)
}

/// The identifier a grant spells `owner` with. The session principals keep
/// their keyword, which names no role the schema declares.
fn owner_ident(owner: &Owner) -> Ident {
    match owner {
        Owner::Ident(ident) => ident.clone(),
        Owner::CurrentUser => Ident::new("CURRENT_USER"),
        Owner::CurrentRole => Ident::new("CURRENT_ROLE"),
        Owner::SessionUser => Ident::new("SESSION_USER"),
    }
}
