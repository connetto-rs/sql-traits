//! The answer behind [`TableLike::row_security`].

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{ops::ControlFlow, ptr};

use sqlparser::ast::{
    CreatePolicyCommand, CreatePolicyType, Expr, FunctionArguments, InTarget, Owner, Query, Visit,
    Visitor,
};

use crate::{
    errors::{LookupError, RowSecurityError},
    structs::{
        GateFailure, GateTerm, GatedCommand, PolicyClause, RowGate, RowSecurity,
        RowSecurityExemption, RowStatement, RowVersion,
    },
    traits::{
        DataStatementLike, DatabaseLike, PolicyLike, Relation, RoleLike, TableLike, ViewLike,
        role::privilege_holders,
    },
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
    let policies: Vec<_> = table.policies(database)?.collect();
    let mut gates = Vec::new();
    for (command, reads, row, failure) in specs(statement) {
        let mut gate =
            RowGate { command, row, failure, permissive: Vec::new(), restrictive: Vec::new() };
        for &policy in &policies {
            // A policy the gate does not consult is never settled, so its `TO`
            // list cannot refuse a statement it plays no part in.
            if !covers(policy, command) || !applies(policy, role, database)? {
                continue;
            }
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
        gates.push(gate);
    }
    let expressions = gates
        .iter()
        .flat_map(|gate| gate.permissive.iter().chain(&gate.restrictive))
        .map(GateTerm::expression)
        .collect();
    let mut expansion = Expansion { database, stack: Vec::new() };
    if let Some(relation) = expansion.table(table.borrow(), expressions, role)? {
        return Ok(RowSecurity::InfiniteRecursion { relation });
    }
    Ok(RowSecurity::Gated(gates))
}

/// Collects the relations the outermost subqueries of an expression read.
///
/// A subquery inside another is read by the outer one already, and resolving
/// it alone would lose the `WITH` names the outer one binds.
struct SubqueryReads<'db, DB: DatabaseLike> {
    database: &'db DB,
    depth: usize,
    reads: Option<Vec<Relation<'db, DB>>>,
}

impl<DB: DatabaseLike> Visitor for SubqueryReads<'_, DB> {
    type Break = LookupError;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.depth > 0 {
            return ControlFlow::Continue(());
        }
        let query = match expr {
            Expr::Exists { subquery, .. }
            | Expr::In { target: InTarget::Subquery(subquery), .. }
            | Expr::Subquery(subquery) => subquery,
            Expr::Function(function) => {
                match &function.args {
                    FunctionArguments::Subquery(subquery) => subquery,
                    _ => return ControlFlow::Continue(()),
                }
            }
            _ => return ControlFlow::Continue(()),
        };
        match <Query as DataStatementLike<DB>>::referenced_relations(query, self.database) {
            Ok(relations) => {
                self.reads.get_or_insert_with(Vec::new).extend(relations);
                ControlFlow::Continue(())
            }
            Err(error) => ControlFlow::Break(error),
        }
    }
}

/// PostgreSQL's expansion of the policies a statement applies, following
/// every table their subqueries read into that table's `SELECT` policies.
///
/// A table enters the stack while the subqueries of its applied policies are
/// expanded, and meeting it there again is the recursion PostgreSQL refuses.
/// A table whose applied policies hold no subquery is never entered, which is
/// why a write policy may read its own table through an open read policy.
struct Expansion<'db, DB: DatabaseLike> {
    database: &'db DB,
    stack: Vec<&'db DB::Table>,
}

impl<'db, DB: DatabaseLike> Expansion<'db, DB> {
    /// Expands `table`'s applied `expressions` as `role`, answering the
    /// relation PostgreSQL's recursion error names, if it raises one.
    fn table(
        &mut self,
        table: &'db DB::Table,
        expressions: Vec<&'db Expr>,
        role: &'db DB::Role,
    ) -> Result<Option<String>, RowSecurityError> {
        let mut collector = SubqueryReads { database: self.database, depth: 0, reads: None };
        for expression in expressions {
            if let ControlFlow::Break(error) = expression.visit(&mut collector) {
                return Err(error.into());
            }
        }
        let Some(reads) = collector.reads else { return Ok(None) };
        if self.stack.iter().any(|entered| ptr::eq(*entered, table)) {
            return Ok(Some(table.table_name().to_string()));
        }
        self.stack.push(table);
        let mut found = None;
        for relation in reads {
            found = self.relation(relation, role)?;
            if found.is_some() {
                break;
            }
        }
        self.stack.pop();
        Ok(found)
    }

    /// Expands one relation a subquery reads, as `role`.
    ///
    /// A plain view reads as its owner and an invoker view as the reader. A
    /// parent table read without `ONLY` applies its own policies alone.
    fn relation(
        &mut self,
        relation: Relation<'db, DB>,
        role: &'db DB::Role,
    ) -> Result<Option<String>, RowSecurityError> {
        match relation {
            Relation::Table(table) => {
                if exemption(table, role, self.database)?.is_some() {
                    return Ok(None);
                }
                let mut expressions = Vec::new();
                for policy in table.policies(self.database)? {
                    if covers(policy, GatedCommand::Select) && applies(policy, role, self.database)?
                    {
                        expressions.extend(policy.using_expression(self.database));
                    }
                }
                self.table(table, expressions, role)
            }
            Relation::View(view) => {
                let reader = if view.is_security_invoker() {
                    role
                } else {
                    view.owner(self.database)?
                        .and_then(|owner| self.database.role(owner))
                        .ok_or_else(|| {
                            RowSecurityError::UnsettledViewOwner {
                                view: view.view_name().to_string(),
                            }
                        })?
                };
                let reads = <Query as DataStatementLike<DB>>::referenced_relations(
                    view.definition(),
                    self.database,
                )?;
                for relation in reads {
                    if let Some(found) = self.relation(relation, reader)? {
                        return Ok(Some(found));
                    }
                }
                Ok(None)
            }
            Relation::MaterializedView(_) | Relation::Sequence(_) => Ok(None),
        }
    }
}
