//! The privileges a `REVOKE` took on a table or its columns, kept whether or
//! not the statement matched a recorded grant, until a `GRANT` of the same
//! privilege at the same level hands them back.

use alloc::vec::Vec;

use sqlparser::ast::{Action, CreateTable, Grant, GrantObjects, Ident, Privileges, Revoke};

use super::{
    NamedColumn, ParserDBBuilder, StoredTable, action_columns, grantee_names, rename_grantee,
    stored_role_name,
};
use crate::{
    impls::{PrivilegeKind, grantees_match},
    structs::{RevokedPrivilege, TableMetadata},
};

/// Records on the table `part` names each privilege it takes from each of its
/// grantees. `part` names one object, bound to the relation it reaches.
pub(super) fn record(builder: &mut ParserDBBuilder, part: &Revoke) {
    if part.grant_option_for {
        return;
    }
    let case = builder.identifier_case();
    let granted_by = part.granted_by.as_ref().map(stored_role_name);
    let Some((node, metadata)) = reached_table(builder, part.objects.as_ref()) else {
        return;
    };

    for (privilege, columns) in revoked_privileges(&part.privileges) {
        let columns: Vec<Option<Ident>> = match columns {
            // A column list names columns the statement checked against the
            // table, each kept as the table declares it so renames find it.
            Some(columns) => {
                columns
                    .iter()
                    .map(|written| {
                        let named = NamedColumn::of(written, case);
                        node.columns
                            .iter()
                            .find(|declared| named.matches(&declared.name))
                            .map_or_else(|| written.clone(), |declared| declared.name.clone())
                    })
                    .map(Some)
                    .collect()
            }
            None => Vec::from([None]),
        };
        for column in columns {
            for grantee in &part.grantees {
                let revoked = RevokedPrivilege::new(
                    grantee.clone(),
                    privilege.clone(),
                    granted_by.clone(),
                    column.clone(),
                );
                if !metadata.revocations().iter().any(|held| same_revocation(held, &revoked)) {
                    metadata.revocations_mut().push(revoked);
                }
            }
        }
    }
}

/// Drops on the table `part` names the revocations of each privilege it
/// grants to each of its grantees at the level it grants it. `part` names one
/// object, bound to the relation it reaches.
pub(super) fn hand_back(builder: &mut ParserDBBuilder, part: &Grant) {
    let case = builder.identifier_case();
    let Some((_, metadata)) = reached_table(builder, part.objects.as_ref()) else {
        return;
    };

    let granted_to = |revoked: &RevokedPrivilege| {
        part.grantees.iter().any(|grantee| grantees_match(revoked.grantee(), grantee))
    };
    metadata.revocations_mut().retain(|revoked| {
        if !granted_to(revoked) {
            return true;
        }
        match (&part.privileges, revoked.column()) {
            (Privileges::All { .. }, column) => column.is_some(),
            (Privileges::Actions(actions), column) => {
                !actions.iter().any(|action| {
                    core::mem::discriminant(action) == core::mem::discriminant(revoked.privilege())
                        && match (action_columns(action), column) {
                            (None, None) => true,
                            (Some(columns), Some(column)) => {
                                NamedColumn::of(column, case).in_idents(columns)
                            }
                            (None, Some(_)) | (Some(_), None) => false,
                        }
                })
            }
        }
    });
}

/// Points every revocation naming `previous`, as grantee or as grantor, at
/// `replacement`.
pub(super) fn rename_role(builder: &mut ParserDBBuilder, previous: &Ident, replacement: &Ident) {
    let previous_name = stored_role_name(previous);
    let replacement_name = stored_role_name(replacement);
    for (_, metadata, _) in builder.tables_mut() {
        for revoked in metadata.revocations_mut() {
            if grantee_names(revoked.grantee(), previous) {
                rename_grantee(revoked.grantee_mut(), replacement);
            }
            let granted_by = revoked.granted_by_mut();
            if granted_by.as_deref() == Some(previous_name.as_str()) {
                *granted_by = Some(replacement_name.clone());
            }
        }
    }
}

/// Forgets every revocation naming `role`, as grantee or as grantor, which a
/// dropped role leaves nothing to say about.
pub(super) fn drop_role(builder: &mut ParserDBBuilder, role: &Ident) {
    let role_name = stored_role_name(role);
    for (_, metadata, _) in builder.tables_mut() {
        metadata.revocations_mut().retain(|revoked| {
            !grantee_names(revoked.grantee(), role)
                && revoked.granted_by() != Some(role_name.as_str())
        });
    }
}

/// Renames the column `from` to `to` in the revocations on `table`.
pub(super) fn rename_column(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    from: &NamedColumn,
    to: &Ident,
) {
    for revoked in revocations_on(builder, table) {
        if let Some(column) = revoked.column_mut()
            && from.matches(column)
        {
            column.clone_from(to);
        }
    }
}

/// Forgets the revocations on `column`, which `table` is dropping.
pub(super) fn drop_column(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    column: &NamedColumn,
) {
    let case = builder.identifier_case();
    for (_, metadata, _) in
        builder.tables_mut().iter_mut().filter(|(node, _, _)| table.matches(node, case))
    {
        metadata
            .revocations_mut()
            .retain(|revoked| revoked.column().is_none_or(|held| !column.matches(held)));
    }
}

/// Every revocation on `table`, for a statement to change.
fn revocations_on<'b>(
    builder: &'b mut ParserDBBuilder,
    table: &StoredTable,
) -> impl Iterator<Item = &'b mut RevokedPrivilege> {
    let case = builder.identifier_case();
    builder
        .tables_mut()
        .iter_mut()
        .filter(move |(node, _, _)| table.matches(node, case))
        .flat_map(|(_, metadata, _)| metadata.revocations_mut().iter_mut())
}

/// Folds away the revocations an owner change made identical, keeping the
/// first of each.
pub(super) fn deduplicate(revocations: &mut Vec<RevokedPrivilege>) {
    let mut kept: Vec<RevokedPrivilege> = Vec::with_capacity(revocations.len());
    for revoked in revocations.drain(..) {
        if !kept.iter().any(|held| same_revocation(held, &revoked)) {
            kept.push(revoked);
        }
    }
    *revocations = kept;
}

/// The table one object of a grant or a revoke names, with its metadata, or
/// `None` when it names a view, a sequence or anything else that is no table
/// the input created.
fn reached_table<'b>(
    builder: &'b mut ParserDBBuilder,
    objects: Option<&GrantObjects>,
) -> Option<(&'b CreateTable, &'b mut TableMetadata<CreateTable>)> {
    let name = match objects {
        Some(GrantObjects::Tables(names)) => names.first()?,
        _ => return None,
    };
    let table = StoredTable::of(builder.resolve_table_object_name(name).ok()??);
    let case = builder.identifier_case();
    builder
        .tables_mut()
        .iter_mut()
        .find(|(node, _, _)| table.matches(node, case))
        .map(|(node, metadata, _)| (&**node, metadata))
}

/// Each table privilege `privileges` names, without its columns, with the
/// columns it names, if any. `ALL` names every table privilege.
fn revoked_privileges(privileges: &Privileges) -> Vec<(Action, Option<&[Ident]>)> {
    let table_privileges = PrivilegeKind::Relation.all_privileges();
    match privileges {
        Privileges::All { .. } => {
            table_privileges.into_iter().map(|privilege| (privilege, None)).collect()
        }
        Privileges::Actions(actions) => {
            actions
                .iter()
                .filter_map(|action| {
                    let privilege = table_privileges.iter().find(|privilege| {
                        core::mem::discriminant(*privilege) == core::mem::discriminant(action)
                    })?;
                    Some((privilege.clone(), action_columns(action)))
                })
                .collect()
        }
    }
}

/// Whether two revocations take the same privilege from the same grantee at
/// the same level, under the same grantor.
fn same_revocation(left: &RevokedPrivilege, right: &RevokedPrivilege) -> bool {
    grantees_match(left.grantee(), right.grantee())
        && left.privilege() == right.privilege()
        && left.granted_by() == right.granted_by()
        && left.column() == right.column()
}
