//! The answer behind [`TableLike::row_security`].

use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{ops::ControlFlow, ptr};

use sqlparser::ast::{
    CreatePolicyType, Expr, FunctionArguments, InTarget, Owner, Query, Visit, Visitor,
};

use crate::{
    errors::{LookupError, RowSecurityError},
    structs::{
        ActingRole, GateFailure, GateTerm, PolicyClause, PolicyTarget, RowGate, RowSecurity,
        RowSecurityExemption, RowStatement, RowVersion, policy_analysis::command_targets,
    },
    traits::{
        DataStatementLike, DatabaseLike, PolicyLike, Relation, RoleLike, TableLike, ViewLike,
        role::privilege_holders,
    },
    utils::identifier_resolution::stored_identifier,
};

/// One statement gate's clause context, row version and failure behavior.
type Spec = (PolicyTarget, RowVersion, GateFailure);

/// The gates `statement` puts on a row, measured on PostgreSQL 18.6.
///
/// A `SELECT` gate on a new row reads `USING` even where the policy carries a
/// `WITH CHECK`, as PostgreSQL forces it.
fn specs(statement: RowStatement) -> Vec<Spec> {
    use GateFailure::{Filter, Raise};
    use PolicyTarget::{Delete, Insert, Select, UpdateCheck, UpdateUsing};
    use RowVersion::{Existing, Inserted, Updated};

    let mut specs = Vec::new();
    match statement {
        RowStatement::Select | RowStatement::MergeDoNothing => {
            specs.push((Select, Existing, Filter));
        }
        RowStatement::SelectLocking => {
            specs.extend([(Select, Existing, Filter), (UpdateUsing, Existing, Filter)]);
        }
        RowStatement::Insert { returning } => {
            specs.push((Insert, Inserted, Raise));
            if returning {
                specs.push((Select, Inserted, Raise));
            }
        }
        RowStatement::MergeInsert { reads, returning } => {
            specs.push((Insert, Inserted, Raise));
            if reads {
                specs.push((Select, Existing, Filter));
                if returning {
                    specs.push((Select, Inserted, Raise));
                }
            }
        }
        RowStatement::Update { reads } => {
            specs.extend([(UpdateUsing, Existing, Filter), (UpdateCheck, Updated, Raise)]);
            if reads {
                specs.extend([(Select, Existing, Filter), (Select, Updated, Raise)]);
            }
        }
        RowStatement::Delete { reads } => {
            specs.push((Delete, Existing, Filter));
            if reads {
                specs.push((Select, Existing, Filter));
            }
        }
        RowStatement::InsertOnConflictDoNothing { arbiter } => {
            specs.push((Insert, Inserted, Raise));
            if arbiter {
                specs.push((Select, Inserted, Raise));
            }
        }
        RowStatement::InsertOnConflictDoUpdate => {
            specs.extend([
                (Insert, Inserted, Raise),
                (Select, Inserted, Raise),
                (Select, Existing, Raise),
                (UpdateUsing, Existing, Raise),
                (Select, Updated, Raise),
                (UpdateCheck, Updated, Raise),
            ]);
        }
        RowStatement::MergeUpdate { reads } => {
            specs.extend([(UpdateUsing, Existing, Raise), (UpdateCheck, Updated, Raise)]);
            if reads {
                specs.extend([(Select, Existing, Filter), (Select, Updated, Raise)]);
            }
        }
        RowStatement::MergeDelete { reads } => {
            specs.push((Delete, Existing, Raise));
            if reads {
                specs.push((Select, Existing, Filter));
            }
        }
    }
    specs
}

/// Whether `policy`'s `TO` list reaches `role`.
///
/// # Errors
///
/// Returns [`RowSecurityError::RoleRequired`] when a caller holding no role
/// meets a policy scoped to a role, and [`RowSecurityError::UnsettledRole`]
/// when no entry reaches a declared `role` and one names a role the schema
/// does not declare, or `CURRENT_USER`, `CURRENT_ROLE` or `SESSION_USER`,
/// which name whoever ran `CREATE POLICY`.
fn applies<DB: DatabaseLike>(
    policy: &DB::Policy,
    role: &ActingRole<'_, DB::Role>,
    database: &DB,
) -> Result<bool, RowSecurityError> {
    if policy.applies_to_public() {
        return Ok(true);
    }
    let ActingRole::Declared(role) = role else {
        return Err(RowSecurityError::RoleRequired { policy: policy.name().to_string() });
    };
    if policy.applies_to_role(role, database) {
        return Ok(true);
    }
    let unsettled = policy.roles(database).find_map(|owner| {
        match owner {
            Owner::Ident(ident) => {
                let stored = stored_identifier(&ident.value, ident.quote_style.is_some());
                database.role(&stored).is_none().then(|| ident.value.clone())
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
    role: &ActingRole<'_, <T::DB as DatabaseLike>::Role>,
    database: &T::DB,
) -> Result<Option<RowSecurityExemption>, RowSecurityError> {
    if !table.has_row_level_security(database)? {
        return Ok(Some(RowSecurityExemption::Disabled));
    }
    let &ActingRole::Declared(role) = role else { return Ok(None) };
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
    role: &ActingRole<'db, <T::DB as DatabaseLike>::Role>,
    database: &'db T::DB,
) -> Result<RowSecurity<'db, <T::DB as DatabaseLike>::Policy>, RowSecurityError> {
    if let Some(exemption) = exemption(table, role, database)? {
        return Ok(RowSecurity::Exempt(exemption));
    }
    let specs = specs(statement);
    let mut gates: Vec<_> = specs
        .iter()
        .map(|&(target, row, failure)| {
            RowGate {
                command: target.command(),
                row,
                failure,
                permissive: Vec::new(),
                restrictive: Vec::new(),
            }
        })
        .collect();
    for policy in table.policies(database)? {
        let consulted = command_targets(policy.command())
            .iter()
            .any(|target| specs.iter().any(|(read, _, _)| target == read));
        if !consulted || !applies(policy, role, database)? {
            continue;
        }
        let analysis = policy.clause_analysis(database)?;
        let mode = analysis.mode();
        let using_targets = analysis.targets(PolicyClause::Using);
        let check_targets = analysis.targets(PolicyClause::WithCheck);
        let (using, check) = analysis.into_clauses();
        for (clause, expression, targets) in [
            (PolicyClause::Using, using, using_targets),
            (PolicyClause::WithCheck, check, check_targets),
        ] {
            let Some(expression) = expression else { continue };
            let mut destinations = specs
                .iter()
                .zip(&mut gates)
                .filter(|((target, _, _), _)| targets.clone().any(|reached| reached == *target))
                .map(|(_, gate)| {
                    match mode {
                        CreatePolicyType::Permissive => &mut gate.permissive,
                        CreatePolicyType::Restrictive => &mut gate.restrictive,
                    }
                })
                .peekable();
            while let Some(bucket) = destinations.next() {
                if destinations.peek().is_none() {
                    bucket.push(GateTerm { policy, clause, expression });
                    break;
                }
                bucket.push(GateTerm { policy, clause, expression: expression.clone() });
            }
        }
    }
    let expressions = gates
        .iter()
        .flat_map(|gate| gate.permissive.iter().chain(&gate.restrictive))
        .map(|term| term.expression.expression())
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
        role: &ActingRole<'db, DB::Role>,
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
        role: &ActingRole<'db, DB::Role>,
    ) -> Result<Option<String>, RowSecurityError> {
        match relation {
            Relation::Table(table) => {
                if exemption(table, role, self.database)?.is_some() {
                    return Ok(None);
                }
                let mut expressions = Vec::new();
                for policy in table.policies(self.database)? {
                    if command_targets(policy.command()).contains(&PolicyTarget::Select)
                        && applies(policy, role, self.database)?
                    {
                        let analysis = policy.clause_analysis(self.database)?;
                        if let Some(source) = analysis.source(PolicyTarget::Select) {
                            expressions.extend(
                                analysis
                                    .clause(source)
                                    .map(crate::structs::PolicyExpression::expression),
                            );
                        }
                    }
                }
                self.table(table, expressions, role)
            }
            Relation::View(view) => {
                let owner;
                let reader = if view.is_security_invoker() {
                    role
                } else {
                    owner = view
                        .owner(self.database)?
                        .and_then(|owner| self.database.role(owner))
                        .map(ActingRole::Declared)
                        .ok_or_else(|| {
                            RowSecurityError::UnsettledViewOwner {
                                view: view.view_name().to_string(),
                            }
                        })?;
                    &owner
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
            // A foreign table carries no policies, so nothing expands below it.
            Relation::ForeignTable(_) | Relation::MaterializedView(_) | Relation::Sequence(_) => {
                Ok(None)
            }
        }
    }
}
