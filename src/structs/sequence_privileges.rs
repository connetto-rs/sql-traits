//! The privileges one role holds on one sequence, as PostgreSQL's
//! `has_sequence_privilege` answers them.

use sqlparser::ast::Action;

use crate::traits::{DatabaseLike, GrantLike, role::grant_reaches};

/// The privileges a role holds on a sequence, as
/// [`SequenceLike::privileges`](crate::traits::SequenceLike::privileges)
/// answers.
///
/// `nextval`, and with it a column default drawing from the sequence, needs
/// `USAGE` or `UPDATE`. `currval` and `lastval` need `USAGE` or `SELECT`, and
/// `setval` needs `UPDATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencePrivileges {
    /// `USAGE`.
    pub usage: bool,
    /// `SELECT`.
    pub select: bool,
    /// `UPDATE`.
    pub update: bool,
}

impl SequencePrivileges {
    pub(crate) const NONE: Self = Self { usage: false, select: false, update: false };

    pub(crate) const ALL: Self = Self { usage: true, select: true, update: true };

    /// Records `privilege`, leaving alone an action that is no sequence
    /// privilege, as PostgreSQL drops one a grant on a sequence names.
    pub(crate) fn take(&mut self, privilege: &Action) {
        match privilege {
            Action::Usage => self.usage = true,
            Action::Select { columns: None } => self.select = true,
            Action::Update { columns: None } => self.update = true,
            _ => {}
        }
    }

    /// Records what `grant` gives, when it reaches one of `holders`, or
    /// `PUBLIC`.
    pub(crate) fn take_grant<G: GrantLike>(
        &mut self,
        grant: &G,
        holders: &[&<G::DB as DatabaseLike>::Role],
        database: &G::DB,
    ) {
        if !grant_reaches(grant, holders) {
            return;
        }
        if grant.is_all_privileges() {
            *self = Self::ALL;
            return;
        }
        for privilege in grant.privileges(database) {
            self.take(privilege);
        }
    }
}
