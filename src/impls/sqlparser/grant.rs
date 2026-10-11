//! Implementation of grant traits for sqlparser's `Grant` struct.
//!
//! In sqlparser, both table-level and column-level grants are represented
//! by the same `Grant` struct. This module implements all grant traits
//! on `Grant` to support both use cases.

use alloc::{string::ToString, vec, vec::Vec};
use core::mem;

use sqlparser::ast::{
    Action, CreateRole, CreateTable, Grant, GrantObjects, Grantee, GranteeName, GranteesType,
    Ident, ObjectName, ObjectNamePart, Privileges, Revoke,
};

use crate::{
    errors::LookupError,
    structs::{IdentifierCase, ParserDB, TargetName},
    traits::{
        ColumnGrantLike, ColumnLike, DatabaseLike, GrantLike, Metadata, RoleLike, TableGrantLike,
        TableLike, ViewLike, grant::GrantRelation,
    },
    utils::{
        identifier_resolution::{
            SessionPrincipal, identifiers_match, is_public_pseudo_role, session_principal,
        },
        object_name::{
            RelationKey, object_name_identifiers, object_name_last_part, resolve_bound_table,
            resolve_table_object_name_on_search_path_in_iter, table_matches_object_name,
            target_name_from_object_name,
        },
    },
};

fn object_names_match(left: &ObjectName, right: &ObjectName) -> bool {
    if left.0.len() != right.0.len() {
        return false;
    }

    left.0.iter().zip(right.0.iter()).all(|(left_part, right_part)| {
        match (left_part, right_part) {
            (ObjectNamePart::Identifier(left_ident), ObjectNamePart::Identifier(right_ident)) => {
                identifiers_match(
                    left_ident.value.as_str(),
                    left_ident.quote_style.is_some(),
                    right_ident.value.as_str(),
                    right_ident.quote_style.is_some(),
                )
            }
            // A part built when the statement runs names nothing yet, so it
            // matches no part, including another such part.
            _ => false,
        }
    })
}

fn role_matches_ident(role: &CreateRole, lookup_name: &str, lookup_quoted: bool) -> bool {
    role.names.iter().any(|role_name| {
        object_name_last_part(role_name).is_some_and(|(role_name, role_quoted)| {
            identifiers_match(role_name, role_quoted, lookup_name, lookup_quoted)
        })
    })
}

/// Returns whether a grantee names the `PUBLIC` pseudo-role, in either
/// spelling the parser produces: the dedicated grantee type for the keyword,
/// and a bare unquoted `PUBLIC` identifier for dialects that reserve it.
fn grantee_is_public(grantee: &Grantee) -> bool {
    if grantee.grantee_type == GranteesType::Public {
        return true;
    }

    matches!(&grantee.name, Some(GranteeName::ObjectName(name))
        if object_name_last_part(name)
            .is_some_and(|(value, quoted)| is_public_pseudo_role(value, quoted)))
}

/// Returns whether a grantee names whoever runs the statement, spelled
/// `CURRENT_USER`, `CURRENT_ROLE` or `SESSION_USER`, rather than a role
/// somebody declared.
fn grantee_is_session_principal(grantee: &Grantee) -> Option<SessionPrincipal> {
    let Some(GranteeName::ObjectName(name)) = &grantee.name else {
        return None;
    };
    let (value, quoted) = object_name_last_part(name)?;
    session_principal(value, quoted)
}

fn grantee_matches_role(grantee: &Grantee, role: &CreateRole) -> bool {
    if grantee_is_public(grantee) {
        return true;
    }

    // The keyword names the session, so no declared role receives the grant,
    // whatever a role of that spelling is called.
    if grantee_is_session_principal(grantee).is_some() {
        return false;
    }

    if let Some(GranteeName::ObjectName(name)) = &grantee.name {
        role.names.iter().any(|role_name| object_names_match(name, role_name))
    } else {
        let role_name = role.name();
        format!("{grantee}").eq_ignore_ascii_case(role_name)
    }
}

/// Whether a schema-wide grant's schema name covers a relation stored in
/// `relation_schema`, with the quote state that decides case sensitivity.
///
/// A relation stored without a schema is not covered: this crate leaves a
/// `public` relation's qualifier unwritten, and `ALL TABLES IN SCHEMA public`
/// naming it would then read differently from the same grant written against
/// any other schema.
fn schema_matches_relation(
    schema_name: &ObjectName,
    relation_schema: Option<(&str, bool)>,
) -> bool {
    let Some((relation_schema, relation_schema_quoted)) = relation_schema else {
        return false;
    };
    let Some((lookup_schema, lookup_schema_quoted)) = object_name_last_part(schema_name) else {
        return false;
    };

    identifiers_match(relation_schema, relation_schema_quoted, lookup_schema, lookup_schema_quoted)
}

fn schema_matches_table<T: TableLike>(schema_name: &ObjectName, table: &T) -> bool {
    schema_matches_relation(
        schema_name,
        table.table_schema().map(|schema| (schema, table.table_schema_is_quoted())),
    )
}

fn grantees_match(left_grantee: &Grantee, right_grantee: &Grantee) -> bool {
    if left_grantee.grantee_type == GranteesType::Public
        || right_grantee.grantee_type == GranteesType::Public
    {
        return left_grantee.grantee_type == right_grantee.grantee_type;
    }

    // A keyword principal is the same principal only as the same keyword, and
    // never the role of that spelling.
    match (grantee_is_session_principal(left_grantee), grantee_is_session_principal(right_grantee))
    {
        (Some(left), Some(right)) => return left == right,
        (Some(_), None) | (None, Some(_)) => return false,
        (None, None) => {}
    }

    match (&left_grantee.name, &right_grantee.name) {
        (Some(GranteeName::ObjectName(left_name)), Some(GranteeName::ObjectName(right_name))) => {
            object_names_match(left_name, right_name)
        }
        _ => format!("{left_grantee}").eq_ignore_ascii_case(&format!("{right_grantee}")),
    }
}

fn grantee_matches_any(grantee: &Grantee, candidates: &[Grantee]) -> bool {
    candidates.iter().any(|candidate| grantees_match(grantee, candidate))
}

fn grantees_overlap(left: &[Grantee], right: &[Grantee]) -> bool {
    left.iter().any(|left_grantee| grantee_matches_any(left_grantee, right))
}

pub(crate) fn partition_grantees_for_revoke(
    grant_grantees: &[Grantee],
    revoke_grantees: &[Grantee],
) -> (Vec<Grantee>, Vec<Grantee>) {
    grant_grantees
        .iter()
        .cloned()
        .partition(|grant_grantee| grantee_matches_any(grant_grantee, revoke_grantees))
}

fn object_name_lists_match(left: &[ObjectName], right: &[ObjectName]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(left_name, right_name)| object_names_match(left_name, right_name))
}

fn grant_objects_inner_match(left: &GrantObjects, right: &GrantObjects) -> bool {
    match (left, right) {
        (
            GrantObjects::AllSequencesInSchema { schemas: left_schemas },
            GrantObjects::AllSequencesInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::AllTablesInSchema { schemas: left_schemas },
            GrantObjects::AllTablesInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::AllViewsInSchema { schemas: left_schemas },
            GrantObjects::AllViewsInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::AllMaterializedViewsInSchema { schemas: left_schemas },
            GrantObjects::AllMaterializedViewsInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::AllExternalTablesInSchema { schemas: left_schemas },
            GrantObjects::AllExternalTablesInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::AllFunctionsInSchema { schemas: left_schemas },
            GrantObjects::AllFunctionsInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::FutureTablesInSchema { schemas: left_schemas },
            GrantObjects::FutureTablesInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::FutureViewsInSchema { schemas: left_schemas },
            GrantObjects::FutureViewsInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::FutureExternalTablesInSchema { schemas: left_schemas },
            GrantObjects::FutureExternalTablesInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::FutureMaterializedViewsInSchema { schemas: left_schemas },
            GrantObjects::FutureMaterializedViewsInSchema { schemas: right_schemas },
        )
        | (
            GrantObjects::FutureSequencesInSchema { schemas: left_schemas },
            GrantObjects::FutureSequencesInSchema { schemas: right_schemas },
        ) => object_name_lists_match(left_schemas, right_schemas),
        (
            GrantObjects::FutureSchemasInDatabase { databases: left_databases },
            GrantObjects::FutureSchemasInDatabase { databases: right_databases },
        ) => object_name_lists_match(left_databases, right_databases),
        (GrantObjects::Databases(left_objects), GrantObjects::Databases(right_objects))
        | (GrantObjects::Schemas(left_objects), GrantObjects::Schemas(right_objects))
        | (GrantObjects::Sequences(left_objects), GrantObjects::Sequences(right_objects))
        | (GrantObjects::Tables(left_objects), GrantObjects::Tables(right_objects))
        | (GrantObjects::Views(left_objects), GrantObjects::Views(right_objects))
        | (GrantObjects::Warehouses(left_objects), GrantObjects::Warehouses(right_objects))
        | (GrantObjects::Integrations(left_objects), GrantObjects::Integrations(right_objects))
        | (
            GrantObjects::ResourceMonitors(left_objects),
            GrantObjects::ResourceMonitors(right_objects),
        )
        | (GrantObjects::Users(left_objects), GrantObjects::Users(right_objects))
        | (GrantObjects::ComputePools(left_objects), GrantObjects::ComputePools(right_objects))
        | (GrantObjects::Connections(left_objects), GrantObjects::Connections(right_objects))
        | (GrantObjects::FailoverGroup(left_objects), GrantObjects::FailoverGroup(right_objects))
        | (
            GrantObjects::ReplicationGroup(left_objects),
            GrantObjects::ReplicationGroup(right_objects),
        )
        | (
            GrantObjects::ExternalVolumes(left_objects),
            GrantObjects::ExternalVolumes(right_objects),
        ) => object_name_lists_match(left_objects, right_objects),
        (
            GrantObjects::Procedure { name: left_name, arg_types: left_arg_types },
            GrantObjects::Procedure { name: right_name, arg_types: right_arg_types },
        )
        | (
            GrantObjects::Function { name: left_name, arg_types: left_arg_types },
            GrantObjects::Function { name: right_name, arg_types: right_arg_types },
        ) => object_names_match(left_name, right_name) && left_arg_types == right_arg_types,
        (left_objects, right_objects) => left_objects == right_objects,
    }
}

fn grant_objects_match(left: Option<&GrantObjects>, right: Option<&GrantObjects>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left_objects), Some(right_objects)) => {
            grant_objects_inner_match(left_objects, right_objects)
        }
        _ => false,
    }
}

fn action_columns(action: &Action) -> Option<&[Ident]> {
    match action {
        Action::Select { columns }
        | Action::Insert { columns }
        | Action::Update { columns }
        | Action::References { columns } => columns.as_deref(),
        _ => None,
    }
}

/// Enforces that every column a grant or a revoke names exists on each table it
/// applies to.
///
/// The database requires the column on every table in the list, and names the
/// one that lacks it, so this walks them in order and reports the same way.
/// `ALL TABLES IN SCHEMA` carries no per-table column list to check and the
/// database accepts a column list beside it, so that form is left alone. A
/// one-part or two-part name resolves through `search_path`, ending at the
/// first schema `claimed` answers for, and a shape the strict splitter rejects
/// keeps the lenient last-two-parts matching.
///
/// # Errors
///
/// Returns [`crate::errors::Error::ColumnNotFoundForGrant`] for the first
/// column a listed table does not have, and
/// [`crate::errors::Error::IdentifierLookupError`] when a resolvable name
/// matches more than one table.
pub(crate) fn validate_granted_columns(
    privileges: &Privileges,
    objects: Option<&GrantObjects>,
    database_tables: &[&CreateTable],
    search_path: &[(&str, bool)],
    case: IdentifierCase,
    claimed: impl Fn(&RelationKey) -> bool,
) -> Result<(), crate::errors::Error> {
    let Privileges::Actions(actions) = privileges else {
        return Ok(());
    };
    let Some(GrantObjects::Tables(names)) = objects else {
        return Ok(());
    };

    for name in names {
        let resolved = if object_name_identifiers(name).is_ok() {
            resolve_table_object_name_on_search_path_in_iter(
                database_tables.iter().copied(),
                name,
                || search_path.iter().copied(),
                case,
                &claimed,
            )
            .map_err(crate::errors::Error::IdentifierLookupError)?
        } else {
            database_tables.iter().copied().find(|table| table_matches_object_name(*table, name))
        };
        let Some(table) = resolved else {
            // The table did not resolve, which the open world excuses and the
            // closed world has already refused, so there is nothing to check
            // the columns against.
            continue;
        };

        for column in actions.iter().filter_map(action_columns).flatten() {
            let known = table.columns.iter().any(|declared| {
                case.identifiers_match(
                    declared.name.value.as_str(),
                    declared.name.quote_style.is_some(),
                    column.value.as_str(),
                    column.quote_style.is_some(),
                )
            });
            if !known {
                return Err(crate::errors::Error::ColumnNotFoundForGrant {
                    column_name: column.value.clone(),
                    table_name: table.name.to_string(),
                });
            }
        }
    }

    Ok(())
}

fn is_column_scoped_action(action: &Action) -> bool {
    matches!(
        action,
        Action::Select { .. }
            | Action::Insert { .. }
            | Action::Update { .. }
            | Action::References { .. }
    )
}

fn action_with_columns(action: &Action, columns: Option<Vec<Ident>>) -> Action {
    match action {
        Action::Select { .. } => Action::Select { columns },
        Action::Insert { .. } => Action::Insert { columns },
        Action::Update { .. } => Action::Update { columns },
        Action::References { .. } => Action::References { columns },
        _ => action.clone(),
    }
}

struct ActionRevokeApplication {
    retained: Option<Action>,
    revoked: Option<Action>,
    unsupported: bool,
}

fn partition_action_for_revoke(
    grant_action: Action,
    revoke_action: &Action,
) -> ActionRevokeApplication {
    if mem::discriminant(&grant_action) != mem::discriminant(revoke_action) {
        return ActionRevokeApplication {
            retained: Some(grant_action),
            revoked: None,
            unsupported: false,
        };
    }

    if !is_column_scoped_action(&grant_action) {
        return ActionRevokeApplication {
            retained: None,
            revoked: Some(grant_action),
            unsupported: false,
        };
    }

    match (action_columns(&grant_action), action_columns(revoke_action)) {
        (None, Some(_)) => {
            ActionRevokeApplication {
                retained: Some(grant_action),
                revoked: None,
                unsupported: true,
            }
        }
        (None | Some(_), None) => {
            ActionRevokeApplication {
                retained: None,
                revoked: Some(grant_action),
                unsupported: false,
            }
        }
        (Some(grant_columns), Some(revoke_columns)) => {
            let (withdrawn_columns, retained_columns): (Vec<Ident>, Vec<Ident>) =
                grant_columns.iter().cloned().partition(|grant_ident| {
                    revoke_columns.iter().any(|revoke_ident| {
                        identifiers_match(
                            grant_ident.value.as_str(),
                            grant_ident.quote_style.is_some(),
                            revoke_ident.value.as_str(),
                            revoke_ident.quote_style.is_some(),
                        )
                    })
                });
            let retained = (!retained_columns.is_empty())
                .then(|| action_with_columns(&grant_action, Some(retained_columns)));
            let revoked = (!withdrawn_columns.is_empty())
                .then(|| action_with_columns(&grant_action, Some(withdrawn_columns)));

            ActionRevokeApplication { retained, revoked, unsupported: false }
        }
    }
}

struct PrivilegeRevokeApplication {
    retained: Option<Privileges>,
    revoked: Option<Privileges>,
    unsupported: bool,
}

fn partition_privileges_for_revoke(
    grant: &Privileges,
    revoke: &Privileges,
) -> PrivilegeRevokeApplication {
    match (grant, revoke) {
        (_, Privileges::All { .. }) => {
            PrivilegeRevokeApplication {
                retained: None,
                revoked: Some(grant.clone()),
                unsupported: false,
            }
        }
        (Privileges::All { .. }, Privileges::Actions(_)) => {
            PrivilegeRevokeApplication {
                retained: Some(grant.clone()),
                revoked: None,
                unsupported: true,
            }
        }
        (Privileges::Actions(grant_actions), Privileges::Actions(revoke_actions)) => {
            let mut retained_actions = Vec::with_capacity(grant_actions.len());
            let mut withdrawn_actions = Vec::new();

            for grant_action in grant_actions {
                let mut current = Some(grant_action.clone());
                for revoke_action in revoke_actions {
                    let Some(action) = current.take() else {
                        break;
                    };
                    let application = partition_action_for_revoke(action, revoke_action);
                    if application.unsupported {
                        return PrivilegeRevokeApplication {
                            retained: Some(grant.clone()),
                            revoked: None,
                            unsupported: true,
                        };
                    }
                    current = application.retained;
                    if let Some(withdrawn) = application.revoked {
                        withdrawn_actions.push(withdrawn);
                    }
                }
                if let Some(retained) = current {
                    retained_actions.push(retained);
                }
            }

            PrivilegeRevokeApplication {
                retained: (!retained_actions.is_empty())
                    .then_some(Privileges::Actions(retained_actions)),
                revoked: (!withdrawn_actions.is_empty())
                    .then_some(Privileges::Actions(withdrawn_actions)),
                unsupported: false,
            }
        }
    }
}

fn updated_grant(grant: &Grant, privileges: Privileges, with_grant_option: bool) -> Grant {
    let mut updated = grant.clone();
    updated.privileges = privileges;
    updated.with_grant_option = with_grant_option;
    updated
}

/// Result of applying a `REVOKE` statement to a single grant.
#[derive(Debug, Clone)]
pub struct RevokeApplication {
    /// Whether the revoke matched this grant.
    pub matched: bool,
    /// Grants carrying the remaining privileges and grant options.
    pub updated_grants: Vec<Grant>,
    /// Whether the result cannot be represented by this grant model.
    pub unsupported: bool,
}

impl RevokeApplication {
    fn unmatched(grant: &Grant) -> Self {
        Self { matched: false, updated_grants: vec![grant.clone()], unsupported: false }
    }
}

/// Applies a `REVOKE` statement to a grant.
#[must_use]
pub fn apply_revoke_to_grant(grant: &Grant, revoke: &Revoke) -> RevokeApplication {
    if !grant_objects_match(grant.objects.as_ref(), revoke.objects.as_ref())
        || !grantees_overlap(&grant.grantees, &revoke.grantees)
    {
        return RevokeApplication::unmatched(grant);
    }

    let application = partition_privileges_for_revoke(&grant.privileges, &revoke.privileges);
    let matched = application.revoked.is_some() || application.unsupported;
    if !matched {
        return RevokeApplication::unmatched(grant);
    }
    if application.unsupported {
        let all_minus_actions = matches!(
            (&grant.privileges, &revoke.privileges),
            (Privileges::All { .. }, Privileges::Actions(_))
        );
        return RevokeApplication {
            matched,
            updated_grants: vec![grant.clone()],
            unsupported: !all_minus_actions || revoke.grant_option_for,
        };
    }

    if !revoke.grant_option_for {
        return RevokeApplication {
            matched,
            updated_grants: application
                .retained
                .into_iter()
                .map(|privileges| updated_grant(grant, privileges, grant.with_grant_option))
                .collect(),
            unsupported: false,
        };
    }

    if !grant.with_grant_option {
        return RevokeApplication {
            matched,
            updated_grants: vec![grant.clone()],
            unsupported: false,
        };
    }

    let mut updated_grants = Vec::with_capacity(2);
    updated_grants
        .extend(application.retained.map(|privileges| updated_grant(grant, privileges, true)));
    updated_grants
        .extend(application.revoked.map(|privileges| updated_grant(grant, privileges, false)));
    RevokeApplication { matched, updated_grants, unsupported: false }
}

impl Metadata for Grant {
    type Meta = ();
}

impl GrantLike for Grant {
    type DB = ParserDB;

    fn privileges<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Action>
    where
        Self: 'db,
    {
        match &self.privileges {
            Privileges::All { .. } => {
                // Return an empty iterator for ALL privileges
                // Users should check is_all_privileges() separately
                [].iter()
            }
            Privileges::Actions(actions) => actions.iter(),
        }
    }

    fn is_all_privileges(&self) -> bool {
        matches!(&self.privileges, Privileges::All { .. })
    }

    fn grantees<'db>(&'db self, _database: &'db Self::DB) -> impl Iterator<Item = &'db Grantee>
    where
        Self: 'db,
    {
        self.grantees.iter()
    }

    fn applies_to_public(&self) -> bool {
        self.grantees.iter().any(grantee_is_public)
    }

    fn target_table_names(&self) -> impl Iterator<Item = TargetName<'_>> {
        let names: &[ObjectName] = match &self.objects {
            Some(GrantObjects::Tables(tables)) => tables,
            _ => &[],
        };
        names.iter().filter_map(target_name_from_object_name)
    }

    fn target_schema_names(&self) -> impl Iterator<Item = TargetName<'_>> {
        let names: &[ObjectName] = match &self.objects {
            Some(GrantObjects::AllTablesInSchema { schemas }) => schemas,
            _ => &[],
        };
        names.iter().filter_map(target_name_from_object_name)
    }

    fn with_grant_option(&self) -> bool {
        self.with_grant_option
    }

    fn granted_by<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> Option<&'a <Self::DB as DatabaseLike>::Role> {
        self.granted_by.as_ref().and_then(|ident| {
            database.roles().find(|role| {
                let role: &CreateRole = role;
                role_matches_ident(role, ident.value.as_str(), ident.quote_style.is_some())
            })
        })
    }

    fn applies_to_role(&self, role: &<Self::DB as DatabaseLike>::Role) -> bool {
        let role: &CreateRole = role;
        self.grantees.iter().any(|grantee| grantee_matches_role(grantee, role))
    }
}

/// Resolves each table a grant names as ingestion bound it, so a name keeps
/// reaching the table it reached when the grant was made.
fn granted_tables<'a>(
    grant: &'a Grant,
    database: &'a ParserDB,
) -> Vec<&'a <ParserDB as DatabaseLike>::Table> {
    match &grant.objects {
        Some(GrantObjects::Tables(names)) => {
            names
                .iter()
                .filter_map(|name| resolve_bound_table(name, database).ok().flatten())
                .collect()
        }
        Some(GrantObjects::AllTablesInSchema { schemas }) => {
            database
                .tables()
                .filter(|table| {
                    schemas.iter().any(|schema_name| schema_matches_table(schema_name, *table))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Resolves each relation a grant names, in the order the grant wrote them,
/// looking for a table first and then each view kind, since one name holds at
/// most one of the three.
///
/// `ALL TABLES IN SCHEMA` covers views and materialized views as well as
/// tables, which is what PostgreSQL grants, so all three are walked.
fn granted_relations<'a>(
    grant: &'a Grant,
    database: &'a ParserDB,
) -> Vec<GrantRelation<'a, ParserDB>> {
    match &grant.objects {
        Some(GrantObjects::Tables(names)) => {
            names.iter().filter_map(|name| resolve_relation_name(name, database)).collect()
        }
        Some(GrantObjects::AllTablesInSchema { schemas }) => {
            database
                .tables()
                .filter(|table| schemas.iter().any(|schema| schema_matches_table(schema, *table)))
                .map(GrantRelation::Table)
                .chain(
                    database
                        .views()
                        .filter(|view| {
                            let stored =
                                view.view_schema().map(|s| (s, view.view_schema_is_quoted()));
                            schemas.iter().any(|schema| schema_matches_relation(schema, stored))
                        })
                        .map(GrantRelation::View),
                )
                .chain(
                    database
                        .materialized_views()
                        .filter(|view| {
                            let stored =
                                view.view_schema().map(|s| (s, view.view_schema_is_quoted()));
                            schemas.iter().any(|schema| schema_matches_relation(schema, stored))
                        })
                        .map(GrantRelation::MaterializedView),
                )
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Resolves one bound name to whichever relation kind holds it.
fn resolve_relation_name<'a>(
    name: &ObjectName,
    database: &'a ParserDB,
) -> Option<GrantRelation<'a, ParserDB>> {
    if let Some(table) = resolve_bound_table(name, database).ok().flatten() {
        return Some(GrantRelation::Table(table));
    }
    let target = target_name_from_object_name(name)?;
    if let Some(view) =
        database.view_by_target(target.clone(), IdentifierCase::AsWritten).ok().flatten()
    {
        return Some(GrantRelation::View(view));
    }
    database
        .materialized_view_by_target(target, IdentifierCase::AsWritten)
        .ok()
        .flatten()
        .map(GrantRelation::MaterializedView)
}

impl TableGrantLike for Grant {
    fn tables<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Table> {
        granted_tables(self, database).into_iter()
    }

    fn relations<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> impl Iterator<Item = GrantRelation<'a, Self::DB>> {
        granted_relations(self, database).into_iter()
    }

    fn applies_to_table(
        &self,
        table: &<Self::DB as DatabaseLike>::Table,
        database: &Self::DB,
    ) -> bool {
        match &self.objects {
            Some(GrantObjects::AllTablesInSchema { schemas }) => {
                schemas.iter().any(|schema_name| schema_matches_table(schema_name, table))
            }
            _ => {
                granted_tables(self, database).iter().any(|granted| core::ptr::eq(*granted, table))
            }
        }
    }
}

impl ColumnGrantLike for Grant {
    fn columns<'a>(
        &'a self,
        table: &'a <Self::DB as DatabaseLike>::Table,
        database: &'a Self::DB,
    ) -> Result<impl Iterator<Item = &'a <Self::DB as DatabaseLike>::Column>, LookupError> {
        let column_idents: Vec<&Ident> = match &self.privileges {
            Privileges::All { .. } => Vec::new(),
            Privileges::Actions(actions) => {
                actions
                    .iter()
                    .flat_map(|action| {
                        match action {
                            Action::Select { columns } => {
                                columns.as_ref().map(|c| c.iter()).into_iter().flatten().collect()
                            }
                            Action::Insert { columns } => {
                                columns.as_ref().map(|c| c.iter()).into_iter().flatten().collect()
                            }
                            Action::Update { columns } => {
                                columns.as_ref().map(|c| c.iter()).into_iter().flatten().collect()
                            }
                            Action::References { columns } => {
                                columns.as_ref().map(|c| c.iter()).into_iter().flatten().collect()
                            }
                            _ => Vec::new(),
                        }
                    })
                    .collect()
            }
        };

        Ok(table
            .columns(database)?
            .filter(move |col| column_idents.iter().any(|ident| ident.value == col.column_name())))
    }

    fn table<'a>(
        &'a self,
        database: &'a Self::DB,
    ) -> Option<&'a <Self::DB as DatabaseLike>::Table> {
        granted_tables(self, database).into_iter().next()
    }

    fn relation<'a>(&'a self, database: &'a Self::DB) -> Option<GrantRelation<'a, Self::DB>> {
        granted_relations(self, database).into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CreateRole, Grantee, GranteeName, GranteesType, Ident, ObjectName, ObjectNamePart,
        grantee_matches_role, grantees_match,
    };
    use crate::prelude::*;

    fn named(value: &str, quoted: bool) -> Grantee {
        let ident = if quoted { Ident::with_quote('"', value) } else { Ident::new(value) };
        Grantee {
            grantee_type: GranteesType::None,
            name: Some(GranteeName::ObjectName(ObjectName(vec![ObjectNamePart::Identifier(
                ident,
            )]))),
        }
    }

    fn unnamed() -> Grantee {
        Grantee { grantee_type: GranteesType::Role, name: None }
    }

    fn role(sql: &str) -> CreateRole {
        let db = ParserDB::parse::<sqlparser::dialect::PostgreSqlDialect>(sql)
            .expect("the role declaration parses");
        db.roles().next().expect("one role").clone()
    }

    /// A keyword principal names the session, so it is the same principal
    /// only as the same keyword, and never a declared role of that spelling.
    #[test]
    fn a_session_principal_matches_only_itself() {
        let current_user = named("CURRENT_USER", false);
        let session_user = named("SESSION_USER", false);
        let quoted = named("current_user", true);

        assert!(grantees_match(&current_user, &named("current_user", false)));
        assert!(!grantees_match(&current_user, &session_user));
        assert!(!grantees_match(&current_user, &quoted));
        assert!(!grantees_match(&quoted, &current_user));

        let declared = role("CREATE ROLE \"current_user\";");
        assert!(!grantee_matches_role(&current_user, &declared));
        assert!(grantee_matches_role(&quoted, &declared));
    }

    /// A grantee the grammar leaves unnamed carries no identifier to read, so
    /// it names no session principal and falls back to the rendered form.
    #[test]
    fn an_unnamed_grantee_names_no_principal() {
        let declared = role("CREATE ROLE reader;");

        assert!(!grantee_matches_role(&unnamed(), &declared));
        assert!(grantees_match(&unnamed(), &unnamed()));
        assert!(!grantees_match(&unnamed(), &named("CURRENT_USER", false)));
    }
}
