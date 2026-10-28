//! The table owner's own privilege entry, and the grants an owner change hands
//! over, as PostgreSQL's `aclnewowner` renames the old owner on the table.

use alloc::{sync::Arc, vec, vec::Vec};

use sqlparser::ast::{
    CreateTable, Grant, GrantObjects, Grantee, GranteeName, Ident, ObjectName, ObjectNamePart,
    Owner, Revoke,
};

use super::{ParserDBBuilder, StoredTable, grantee_role_ident, stored_role_name};
use crate::{
    impls::{grantees_match, schema_matches_table},
    structs::IdentifierCase,
};

/// Takes the privileges `revoke` names from the owner's entry on each table it
/// reaches that one of its grantees owns.
pub(super) fn revoke(builder: &mut ParserDBBuilder, revoke: &Revoke) {
    // Ownership confers the grant option, so the owner keeps it.
    if revoke.grant_option_for {
        return;
    }
    let reached: Vec<StoredTable> = match &revoke.objects {
        Some(GrantObjects::Tables(names)) => {
            names
                .iter()
                .filter_map(|name| builder.resolve_table_object_name(name).ok().flatten())
                .map(StoredTable::of)
                .collect()
        }
        Some(GrantObjects::AllTablesInSchema { schemas }) => {
            builder
                .tables()
                .iter()
                .map(|(table, _, _)| table.as_ref())
                .filter(|table| schemas.iter().any(|schema| schema_matches_table(schema, *table)))
                .map(StoredTable::of)
                .collect()
        }
        _ => Vec::new(),
    };
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
/// `next`, splitting a schema-wide grant into explicit ones since a stored
/// schema-wide grant cannot leave one table out.
pub(super) fn change_owner(
    builder: &mut ParserDBBuilder,
    handed: &CreateTable,
    previous: &str,
    next: &Owner,
) {
    let tables: Vec<Arc<CreateTable>> =
        builder.tables().iter().map(|(table, _, _)| Arc::clone(table)).collect();
    let table = StoredTable::of(handed);
    let handover = Handover {
        table: &table,
        handed,
        tables: &tables,
        previous,
        next: owner_ident(next),
        case: builder.identifier_case(),
    };
    handover.apply(builder.table_grants_mut());
    handover.apply(builder.column_grants_mut());
}

/// One owner change of one table.
struct Handover<'a> {
    /// The table changing hands.
    table: &'a StoredTable,
    /// The stored node of that table.
    handed: &'a CreateTable,
    /// Every table the builder holds as the owner changes.
    tables: &'a [Arc<CreateTable>],
    /// The canonical name of the old owner.
    previous: &'a str,
    /// The new owner, as a grant would spell it.
    next: Ident,
    /// The comparison stored names follow.
    case: IdentifierCase,
}

impl Handover<'_> {
    /// Rewrites one grant store.
    fn apply(&self, grants: &mut Vec<(Arc<Grant>, ())>) {
        let mut rewritten = Vec::with_capacity(grants.len());
        for (grant, ()) in core::mem::take(grants) {
            match self.split(&grant) {
                Some(parts) => rewritten.extend(parts.into_iter().map(|part| (Arc::new(part), ()))),
                None => rewritten.push((grant, ())),
            }
        }
        *grants = rewritten;
    }

    /// Returns the grants `grant` becomes, or `None` when it keeps its shape.
    fn split(&self, grant: &Grant) -> Option<Vec<Grant>> {
        let names_previous =
            grant.grantees.iter().any(|grantee| names_role(grantee, self.previous))
                || grant
                    .granted_by
                    .as_ref()
                    .is_some_and(|grantor| stored_role_name(grantor) == self.previous);
        if !names_previous {
            return None;
        }

        let (on_table, elsewhere): (Vec<ObjectName>, Vec<ObjectName>) = match &grant.objects {
            Some(GrantObjects::Tables(names)) => {
                names.iter().cloned().partition(|name| self.table.named_by(name))
            }
            Some(GrantObjects::AllTablesInSchema { schemas })
                if schemas.iter().any(|schema| schema_matches_table(schema, self.handed)) =>
            {
                let elsewhere = self
                    .tables
                    .iter()
                    .filter(|candidate| !self.table.matches(candidate, self.case))
                    .filter(|candidate| {
                        schemas
                            .iter()
                            .any(|schema| schema_matches_table(schema, candidate.as_ref()))
                    })
                    .map(|candidate| candidate.name.clone())
                    .collect();
                (vec![self.handed.name.clone()], elsewhere)
            }
            _ => return None,
        };
        if on_table.is_empty() {
            return None;
        }

        let mut parts = Vec::with_capacity(2);
        if !elsewhere.is_empty() {
            parts.push(on_tables(
                grant,
                elsewhere,
                grant.grantees.clone(),
                grant.granted_by.clone(),
            ));
        }
        parts.push(self.handed_over(grant, on_table));
        Some(parts)
    }

    /// The part of `grant` on the table changing hands, with the old owner
    /// renamed to the new one.
    fn handed_over(&self, grant: &Grant, on_table: Vec<ObjectName>) -> Grant {
        let mut grantees: Vec<Grantee> = Vec::with_capacity(grant.grantees.len());
        for grantee in &grant.grantees {
            let grantee = if names_role(grantee, self.previous) {
                Grantee {
                    name: Some(GranteeName::ObjectName(ObjectName(vec![
                        ObjectNamePart::Identifier(self.next.clone()),
                    ]))),
                    ..grantee.clone()
                }
            } else {
                grantee.clone()
            };
            if !grantees.iter().any(|kept| grantees_match(kept, &grantee)) {
                grantees.push(grantee);
            }
        }
        let granted_by = grant.granted_by.as_ref().map(|grantor| {
            if stored_role_name(grantor) == self.previous {
                self.next.clone()
            } else {
                grantor.clone()
            }
        });
        on_tables(grant, on_table, grantees, granted_by)
    }
}

/// Copies `grant` onto `tables` with the given grantees and grantor.
fn on_tables(
    grant: &Grant,
    tables: Vec<ObjectName>,
    grantees: Vec<Grantee>,
    granted_by: Option<Ident>,
) -> Grant {
    Grant {
        privileges: grant.privileges.clone(),
        objects: Some(GrantObjects::Tables(tables)),
        grantees,
        with_grant_option: grant.with_grant_option,
        as_grantor: grant.as_grantor.clone(),
        granted_by,
        current_grants: grant.current_grants.clone(),
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
