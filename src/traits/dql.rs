//! Submodule providing the [`DQLLike`] trait for SQL data-query statements
//! (`SELECT`), analyzed against a [`DatabaseLike`].

use crate::{
    errors::LookupError,
    traits::{DataStatementLike, DatabaseLike},
};

/// A parsed SQL query (`SELECT`) analyzed against a [`DatabaseLike`].
///
/// Extends [`DataStatementLike`] with projection analysis. It is implemented on
/// sqlparser's `Query` node.
pub trait DQLLike<DB: DatabaseLike>: DataStatementLike<DB> {
    /// Returns the single base table that every projected column comes from,
    /// when exactly one such table exists.
    ///
    /// This is the eligibility rule for "single-table row re-execution": the
    /// query's output rows are rows of one base table, so they can be delivered
    /// as a primary-key-keyed patchset. The cases that qualify are:
    ///
    /// - qualified columns (`t.c`) or a qualified wildcard (`t.*`) all bound to
    ///   the same base table, or
    /// - unqualified columns over a single-table `FROM` (no joins).
    ///
    /// A qualified reference may reach a CTE reference or derived subquery
    /// too, resolving to the table its projection passes the column through
    /// from, but only while that body preserves row identity (no `DISTINCT`,
    /// grouping, window filtering, null-extending outer join, or set operation
    /// that deduplicates rows), since collapsed rows are not source rows.
    ///
    /// Returns `Ok(None)` (not eligible) when the output rows are not rows of a
    /// single base table, specifically when:
    ///
    /// - the projection draws from more than one base table,
    /// - the projection draws from a table read without `ONLY` that other
    ///   tables inherit from through `INHERITS`, whose rows join the table's
    ///   own without its keys (partitions share their root's keys, so a
    ///   partitioned table still answers),
    /// - any projected item is a computed expression (arithmetic, a function
    ///   call, an aggregate, a scalar subquery, `CASE`, ...) rather than a
    ///   pass-through column or wildcard,
    /// - the statement uses `GROUP BY` or `DISTINCT`,
    /// - the `FROM` includes a relation that cannot carry a row-identity answer
    ///   (a table function, an unresolvable name, or a derived subquery or CTE
    ///   whose body groups or deduplicates rows or whose columns cannot be
    ///   enumerated), and the projection could draw from it,
    /// - `*` is projected over anything other than exactly one base table or
    ///   one row-preserving derived relation, or
    /// - the query body is a set operation (`UNION`/`EXCEPT`/`INTERSECT`),
    ///   `VALUES`, or `TABLE`.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when an unqualified column
    /// is exposed by more than one table in the `FROM` clause, or when a
    /// qualified reference names a derived relation whose output carries that
    /// column name twice, and [`LookupError::InvalidObjectName`] /
    /// [`LookupError::AmbiguousTableLookup`] when a `FROM` relation name is
    /// malformed or resolves ambiguously.
    fn projection_source_table<'db>(
        &self,
        database: &'db DB,
    ) -> Result<Option<&'db DB::Table>, LookupError>;
}
