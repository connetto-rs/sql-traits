//! Role-independent policy clause applicability and diagnostics.

use sqlparser::ast::{CreatePolicyCommand, CreatePolicyType};

use super::{GatedCommand, PolicyClause, PolicyExpression};
use crate::errors::PolicyClauseError;

/// A command's visibility or new-row check context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyTarget {
    /// `SELECT` visibility.
    Select,
    /// `INSERT` new-row checks.
    Insert,
    /// `UPDATE` visibility.
    UpdateUsing,
    /// `UPDATE` new-row checks.
    UpdateCheck,
    /// `DELETE` visibility.
    Delete,
}

impl PolicyTarget {
    /// Every policy clause context in command order.
    pub(crate) const ALL: [Self; 5] =
        [Self::Select, Self::Insert, Self::UpdateUsing, Self::UpdateCheck, Self::Delete];

    /// The command whose policy supplies this context.
    #[must_use]
    pub const fn command(self) -> GatedCommand {
        match self {
            Self::Select => GatedCommand::Select,
            Self::Insert => GatedCommand::Insert,
            Self::UpdateUsing | Self::UpdateCheck => GatedCommand::Update,
            Self::Delete => GatedCommand::Delete,
        }
    }

    /// The nominal clause before a missing check falls back to `USING`.
    #[must_use]
    pub const fn clause(self) -> PolicyClause {
        match self {
            Self::Select | Self::UpdateUsing | Self::Delete => PolicyClause::Using,
            Self::Insert | Self::UpdateCheck => PolicyClause::WithCheck,
        }
    }
}

/// The legal contexts of a declared policy command.
pub(crate) const fn command_targets(command: CreatePolicyCommand) -> &'static [PolicyTarget] {
    match command {
        CreatePolicyCommand::All => &PolicyTarget::ALL,
        CreatePolicyCommand::Select => &[PolicyTarget::Select],
        CreatePolicyCommand::Insert => &[PolicyTarget::Insert],
        CreatePolicyCommand::Update => &[PolicyTarget::UpdateUsing, PolicyTarget::UpdateCheck],
        CreatePolicyCommand::Delete => &[PolicyTarget::Delete],
    }
}

/// Resolves a nominal clause from stored-clause presence.
fn stored_source(clause: PolicyClause, using: bool, check: bool) -> Option<PolicyClause> {
    match clause {
        PolicyClause::WithCheck if check => Some(PolicyClause::WithCheck),
        _ if using => Some(PolicyClause::Using),
        _ => None,
    }
}

/// An absent clause and the commands whose first unmet requirement it supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingPolicyClause {
    clause: PolicyClause,
    targets: &'static [PolicyTarget],
}

impl MissingPolicyClause {
    /// The clause that is absent without a fallback.
    #[must_use]
    pub fn clause(self) -> PolicyClause {
        self.clause
    }

    /// The affected commands, with `UPDATE` diagnosed by its missing visibility
    /// clause.
    pub fn commands(self) -> impl Iterator<Item = GatedCommand> {
        self.targets
            .iter()
            .copied()
            .filter(move |&target| {
                target.clause() == self.clause && target != PolicyTarget::UpdateCheck
            })
            .map(PolicyTarget::command)
    }
}

/// A policy's declared metadata, stored clauses and role-independent clause
/// rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyAnalysis<'db> {
    command: CreatePolicyCommand,
    mode: CreatePolicyType,
    using: Option<PolicyExpression<'db>>,
    check: Option<PolicyExpression<'db>>,
    invalid: Option<PolicyClauseError>,
}

impl<'db> PolicyAnalysis<'db> {
    /// Analyzes the clauses retained by a catalog.
    pub(crate) fn new(
        command: CreatePolicyCommand,
        mode: CreatePolicyType,
        using: Option<PolicyExpression<'db>>,
        check: Option<PolicyExpression<'db>>,
    ) -> Self {
        let invalid = match command {
            CreatePolicyCommand::Insert if using.is_some() => {
                Some(PolicyClauseError::UsingForInsert)
            }
            CreatePolicyCommand::Select | CreatePolicyCommand::Delete if check.is_some() => {
                Some(PolicyClauseError::WithCheckForSelectOrDelete)
            }
            _ => None,
        };
        Self { command, mode, using, check, invalid }
    }

    /// The declared command, including `ALL` when no effective clause exists.
    #[must_use]
    pub fn command(&self) -> CreatePolicyCommand {
        self.command
    }

    /// The declared permissive or restrictive composition mode.
    #[must_use]
    pub fn mode(&self) -> CreatePolicyType {
        self.mode
    }

    /// A stored clause, including unreadable expressions and illegal
    /// combinations.
    #[must_use]
    pub fn clause(&self, clause: PolicyClause) -> Option<&PolicyExpression<'db>> {
        match clause {
            PolicyClause::Using => self.using.as_ref(),
            PolicyClause::WithCheck => self.check.as_ref(),
        }
    }

    /// The illegal combination that prevents this policy from supplying any
    /// target.
    #[must_use]
    pub fn invalid_clause(&self) -> Option<PolicyClauseError> {
        self.invalid
    }

    /// The stored source of a legal target, with fallback decided by clause
    /// presence.
    #[must_use]
    pub fn source(&self, target: PolicyTarget) -> Option<PolicyClause> {
        if self.invalid.is_some() || !command_targets(self.command).contains(&target) {
            return None;
        }
        stored_source(target.clause(), self.using.is_some(), self.check.is_some())
    }

    /// The legal classification and effective command targets of a stored
    /// clause.
    pub fn targets(
        &self,
        clause: PolicyClause,
    ) -> impl Iterator<Item = PolicyTarget> + Clone + use<> {
        let targets = if self.invalid.is_some() { &[] } else { command_targets(self.command) };
        let using = self.using.is_some();
        let check = self.check.is_some();
        targets
            .iter()
            .copied()
            .filter(move |target| stored_source(target.clause(), using, check) == Some(clause))
    }

    /// Missing prerequisites grouped by clause, excluding illegal policies.
    pub fn missing_clauses(&self) -> impl Iterator<Item = MissingPolicyClause> + '_ {
        [PolicyClause::Using, PolicyClause::WithCheck].into_iter().filter_map(move |clause| {
            if self.invalid.is_some()
                || self.clause(clause).is_some()
                || (clause == PolicyClause::WithCheck && self.using.is_some())
            {
                return None;
            }
            let missing = MissingPolicyClause { clause, targets: command_targets(self.command) };
            missing.commands().next().map(|_| missing)
        })
    }

    /// Consumes the analysis without copying its stored expressions.
    pub(crate) fn into_clauses(
        self,
    ) -> (Option<PolicyExpression<'db>>, Option<PolicyExpression<'db>>) {
        (self.using, self.check)
    }
}
