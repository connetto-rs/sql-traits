//! The answer behind [`TableLike::row_security`].

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{CreatePolicyCommand, CreatePolicyType, Owner};

use crate::{
    errors::RowSecurityError,
    structs::{
        GateFailure, GateTerm, GatedCommand, PolicyClause, RowGate, RowSecurity,
        RowSecurityExemption, RowStatement, RowVersion,
    },
    traits::{DatabaseLike, PolicyLike, RoleLike, TableLike, role::privilege_holders},
    utils::identifier_resolution::{is_public_pseudo_role, normalize_identifier},
};

/// Which clause a gate reads: `USING` alone, or `WITH CHECK` falling back to
/// `USING`.
#[derive(Clone, Copy)]
enum Reads {
    Using,
    Check,
}

/// One row of PostgreSQL's table: whose policies, which clause, which row,
/// and what failing does.
type Spec = (GatedCommand, Reads, RowVersion, GateFailure);

/// The gates `statement` puts on a row, measured on PostgreSQL 18.6.
///
/// A `SELECT` gate on a new row reads `USING` even where the policy carries a
/// `WITH CHECK`, as PostgreSQL forces it.
fn specs(statement: RowStatement) -> Vec<Spec> {
    use GateFailure::{Filter, Raise};
    use GatedCommand::{Delete, Insert, Select, Update};
    use Reads::{Check, Using};
    use RowVersion::{Existing, Inserted, Updated};

    let mut specs = Vec::new();
    match statement {
        RowStatement::Select => specs.push((Select, Using, Existing, Filter)),
        RowStatement::SelectLocking => {
            specs.extend([(Select, Using, Existing, Filter), (Update, Using, Existing, Filter)]);
        }
        RowStatement::Insert { returning } | RowStatement::MergeInsert { returning } => {
            specs.push((Insert, Check, Inserted, Raise));
            if returning {
                specs.push((Select, Using, Inserted, Raise));
            }
        }
        RowStatement::Update { reads } => {
            specs.extend([(Update, Using, Existing, Filter), (Update, Check, Updated, Raise)]);
            if reads {
                specs.extend([(Select, Using, Existing, Filter), (Select, Using, Updated, Raise)]);
            }
        }
        RowStatement::Delete { reads } => {
            specs.push((Delete, Using, Existing, Filter));
            if reads {
                specs.push((Select, Using, Existing, Filter));
            }
        }
        RowStatement::InsertOnConflictDoNothing { arbiter } => {
            specs.push((Insert, Check, Inserted, Raise));
            if arbiter {
                specs.push((Select, Using, Inserted, Raise));
            }
        }
        RowStatement::InsertOnConflictDoUpdate => {
            specs.extend([
                (Insert, Check, Inserted, Raise),
                (Select, Using, Inserted, Raise),
                (Select, Using, Existing, Raise),
                (Update, Using, Existing, Raise),
                (Select, Using, Updated, Raise),
                (Update, Check, Updated, Raise),
            ]);
        }
        RowStatement::MergeUpdate => {
            specs.extend([
                (Update, Using, Existing, Raise),
                (Select, Using, Updated, Raise),
                (Update, Check, Updated, Raise),
            ]);
        }
        RowStatement::MergeDelete => specs.push((Delete, Using, Existing, Raise)),
        RowStatement::MergeDoNothing => {}
    }
    if matches!(
        statement,
        RowStatement::MergeInsert { .. }
            | RowStatement::MergeUpdate
            | RowStatement::MergeDelete
            | RowStatement::MergeDoNothing
    ) {
        specs.push((Select, Using, Existing, Filter));
    }
    specs
}

/// Whether `policy` is written for `command`, directly or through `FOR ALL`.
fn covers<P: PolicyLike>(policy: &P, command: GatedCommand) -> bool {
    match policy.command() {
        CreatePolicyCommand::All => true,
        CreatePolicyCommand::Select => command == GatedCommand::Select,
        CreatePolicyCommand::Insert => command == GatedCommand::Insert,
        CreatePolicyCommand::Update => command == GatedCommand::Update,
        CreatePolicyCommand::Delete => command == GatedCommand::Delete,
    }
}

/// Whether `policy`'s `TO` list reaches `role`.
///
/// # Errors
///
/// Returns [`RowSecurityError::UnsettledRole`] when no entry reaches `role`
/// and one names a role the schema does not declare, or `CURRENT_USER`,
/// `CURRENT_ROLE` or `SESSION_USER`, which name whoever ran `CREATE POLICY`.
fn applies<DB: DatabaseLike>(
    policy: &DB::Policy,
    role: &DB::Role,
    database: &DB,
) -> Result<bool, RowSecurityError> {
    if policy.applies_to_public() || policy.applies_to_role(role, database) {
        return Ok(true);
    }
    let unsettled = policy.roles(database).find_map(|owner| {
        match owner {
            Owner::Ident(ident) => {
                let stored = normalize_identifier(&ident.value, ident.quote_style.is_some());
                let known = is_public_pseudo_role(&ident.value, ident.quote_style.is_some())
                    || database.role(&stored).is_some();
                (!known).then(|| ident.value.clone())
            }
            Owner::CurrentRole => Some("CURRENT_ROLE".to_string()),
            Owner::CurrentUser => Some("CURRENT_USER".to_string()),
            Owner::SessionUser => Some("SESSION_USER".to_string()),
        }
    });
    match unsettled {
        Some(unsettled) => {
            Err(RowSecurityError::UnsettledRole {
                policy: policy.name().to_string(),
                role: unsettled,
            })
        }
        None => Ok(false),
    }
}

/// Why `role` is exempt from `table`'s policies, if it is.
fn exemption<T: TableLike>(
    table: &T,
    role: &<T::DB as DatabaseLike>::Role,
    database: &T::DB,
) -> Result<Option<RowSecurityExemption>, RowSecurityError> {
    if !table.has_row_level_security(database)? {
        return Ok(Some(RowSecurityExemption::Disabled));
    }
    if role.is_superuser() {
        return Ok(Some(RowSecurityExemption::Superuser));
    }
    if role.can_bypass_rls() {
        return Ok(Some(RowSecurityExemption::BypassRls));
    }
    if !table.has_forced_row_level_security(database)?
        && let Some(owner) = table.owner(database)?
        && privilege_holders(role, database).any(|holder| holder.stored_name() == owner)
    {
        return Ok(Some(RowSecurityExemption::Owner));
    }
    Ok(None)
}

/// See [`TableLike::row_security`].
pub(crate) fn row_security<'db, T: TableLike>(
    table: &'db T,
    statement: RowStatement,
    role: &'db <T::DB as DatabaseLike>::Role,
    database: &'db T::DB,
) -> Result<RowSecurity<'db, <T::DB as DatabaseLike>::Policy>, RowSecurityError> {
    if let Some(exemption) = exemption(table, role, database)? {
        return Ok(RowSecurity::Exempt(exemption));
    }
    let mut policies = Vec::new();
    for policy in table.policies(database)? {
        if applies(policy, role, database)? {
            policies.push(policy);
        }
    }
    let gates = specs(statement)
        .into_iter()
        .map(|(command, reads, row, failure)| {
            let mut gate =
                RowGate { command, row, failure, permissive: Vec::new(), restrictive: Vec::new() };
            for &policy in policies.iter().filter(|policy| covers(**policy, command)) {
                let using = || policy.using_expression(database).map(|e| (PolicyClause::Using, e));
                let read = match reads {
                    Reads::Using => using(),
                    Reads::Check => {
                        policy
                            .check_expression(database)
                            .map(|e| (PolicyClause::WithCheck, e))
                            .or_else(using)
                    }
                };
                let Some((clause, expression)) = read else { continue };
                let term = GateTerm { policy, clause, expression };
                match policy.policy_type() {
                    CreatePolicyType::Permissive => gate.permissive.push(term),
                    CreatePolicyType::Restrictive => gate.restrictive.push(term),
                }
            }
            gate
        })
        .collect();
    Ok(RowSecurity::Gated(gates))
}
