//! When PostgreSQL checks a constraint, as `pg_constraint.condeferrable` and
//! `pg_constraint.condeferred` record it.

/// When a constraint is checked within a transaction, mirroring
/// `pg_constraint.condeferrable` and `pg_constraint.condeferred`.
///
/// PostgreSQL refuses `NOT DEFERRABLE INITIALLY DEFERRED`, so these three are
/// every state a constraint can hold.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE accounts (id INT PRIMARY KEY);
///     CREATE TABLE notes (account INT REFERENCES accounts (id) INITIALLY DEFERRED);",
/// )?;
/// let notes = db.table_by_target(TargetName::new("notes", false), IdentifierCase::AsWritten)?;
/// let key = notes.unwrap().foreign_keys(&db)?.next().unwrap();
/// let deferral = key.deferral(&db);
/// assert_eq!(deferral, ConstraintDeferral::InitiallyDeferred);
/// assert!(deferral.is_deferrable());
/// assert!(deferral.is_initially_deferred());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConstraintDeferral {
    /// `NOT DEFERRABLE`, checked at the end of every statement. What a
    /// constraint declaring neither clause holds.
    NotDeferrable,
    /// `DEFERRABLE INITIALLY IMMEDIATE`, checked at the end of every statement
    /// until `SET CONSTRAINTS` defers it.
    InitiallyImmediate,
    /// `DEFERRABLE INITIALLY DEFERRED`, checked at commit until `SET
    /// CONSTRAINTS` makes it immediate.
    InitiallyDeferred,
}

impl ConstraintDeferral {
    /// Returns whether `SET CONSTRAINTS` can move the check of the
    /// constraint, matching `pg_constraint.condeferrable`.
    #[must_use]
    #[inline]
    pub const fn is_deferrable(self) -> bool {
        !matches!(self, Self::NotDeferrable)
    }

    /// Returns whether a transaction starts with the check of the constraint
    /// deferred to commit, matching `pg_constraint.condeferred`.
    #[must_use]
    #[inline]
    pub const fn is_initially_deferred(self) -> bool {
        matches!(self, Self::InitiallyDeferred)
    }
}
