//! A read a `SECURITY DEFINER` function makes as its owner that the roles it
//! runs for might not make themselves, as
//! [`TriggerLike::unconfined_definer_read`](crate::traits::TriggerLike::unconfined_definer_read)
//! and
//! [`FunctionLike::unconfined_definer_read`](crate::traits::FunctionLike::unconfined_definer_read)
//! find it.

use alloc::{string::String, vec::Vec};

use crate::traits::DatabaseLike;

/// What a consumer asks of a read a trigger's function chain makes as the
/// owner of a `SECURITY DEFINER` function.
///
/// PostgreSQL checks `USING` before row triggers, `WITH CHECK` after `BEFORE`
/// row triggers, and immediate foreign keys in the `RI_ConstraintTrigger_c_`
/// triggers ordered by stored name. A foreign key holds `NEW` only when it is
/// `NOT DEFERRABLE`, is validated for an `UPDATE`, and the table cannot be
/// rewritten within the statement. A `BEFORE INSERT` guard cannot rely on keys
/// when uniqueness or exclusion constraints let `ON CONFLICT` skip the proposed
/// row. Write-policy facts require a firing that cannot originate from
/// owner-run views, rules, foreign definer writers or writing referential
/// actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefinerReading {
    /// Whether the reads can change a branch, raise, assigned or returned value
    /// or `FOUND` when only rows the acting role reads are available, allowing
    /// later checks only on an unchanged written row.
    Outcome,
    /// Whether the acting role learns nothing of a row it cannot read, as a
    /// lint reporting a disclosure needs. Every read counts, a raise's
    /// message included, and only the facts PostgreSQL has checked before
    /// the read, since a raise before a later check tells the role what the
    /// read found.
    Disclosure,
}

/// Why a read may reach a row the roles a `SECURITY DEFINER` function runs
/// for cannot read.
#[derive(Debug)]
pub enum Unconfinement<'db, DB: DatabaseLike> {
    /// No fact the read rests on implies this restrictive `SELECT` policy.
    Restrictive(&'db DB::Policy),
    /// No fact the read rests on implies any of these permissive `SELECT`
    /// policies, none when no permissive policy applies to the role, which
    /// then reads no row of the table.
    Permissive(Vec<&'db DB::Policy>),
    /// The role may not select this column of the table read.
    HiddenColumn(&'db DB::Column),
    /// The read is one the proof cannot follow: a view, a table read under
    /// an alias renaming its columns, a writing statement, SQL built at run
    /// time, a body that cannot be parsed, a setting the body changes
    /// besides `search_path`, or a call cycle.
    Unfollowed,
}

/// A read, made as the owner of a `SECURITY DEFINER` function, that may
/// reach a row the roles the function runs for cannot read.
#[derive(Debug)]
pub struct UnconfinedRead<'db, DB: DatabaseLike> {
    pub(crate) through: Vec<&'db DB::Function>,
    pub(crate) table: Option<&'db DB::Table>,
    pub(crate) statement: Option<String>,
    pub(crate) reason: Unconfinement<'db, DB>,
}

impl<'db, DB: DatabaseLike> UnconfinedRead<'db, DB> {
    /// The functions the read is made through, the one the trigger or the
    /// caller runs first and the one whose body makes the read last.
    #[must_use]
    pub fn through(&self) -> &[&'db DB::Function] {
        &self.through
    }

    /// The table read, `None` when the read is no read of one table, such as
    /// a body that cannot be parsed.
    #[must_use]
    pub fn table(&self) -> Option<&'db DB::Table> {
        self.table
    }

    /// The statement or expression making the read, as the body writes it,
    /// `None` when the read is no one statement of the body.
    #[must_use]
    pub fn statement(&self) -> Option<&str> {
        self.statement.as_deref()
    }

    /// Why the read may reach a row the roles cannot read.
    #[must_use]
    pub fn reason(&self) -> &Unconfinement<'db, DB> {
        &self.reason
    }
}
