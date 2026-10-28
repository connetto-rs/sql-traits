//! Submodule defining a generic `IndexMetadata` struct.

use alloc::{sync::Arc, vec::Vec};

use sqlparser::ast::Expr;

use super::ExpressionCalls;
use crate::traits::{CheckConstraintLike, DatabaseLike};

#[derive(Debug, Clone)]
/// Struct collecting metadata about a check constraint.
pub struct CheckMetadata<U: CheckConstraintLike> {
    /// The expression defining the constraint.
    expression: Expr,
    /// The table on which the constraint is defined.
    table: Arc<<U::DB as DatabaseLike>::Table>,
    /// The columns involved in the constraint.
    columns: Vec<Arc<<U::DB as DatabaseLike>::Column>>,
    /// The calls of the constraint's expression.
    calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
}

impl<U: CheckConstraintLike> CheckMetadata<U> {
    /// Creates a new `CheckMetadata` instance.
    #[inline]
    pub fn new(
        expression: Expr,
        table: Arc<<U::DB as DatabaseLike>::Table>,
        columns: Vec<Arc<<U::DB as DatabaseLike>::Column>>,
        calls: ExpressionCalls<<U::DB as DatabaseLike>::Function>,
    ) -> Self {
        Self { expression, table, columns, calls }
    }

    /// Returns a reference to the expression defining the constraint.
    #[must_use]
    #[inline]
    pub fn expression(&self) -> &Expr {
        &self.expression
    }

    /// Returns a reference to the table on which the constraint is defined.
    #[must_use]
    #[inline]
    pub fn table(&self) -> &<U::DB as DatabaseLike>::Table {
        &self.table
    }

    /// Returns an iterator over the columns involved in the constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (a INT, b INT, CHECK (a < b));")?;
    /// let t = db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check = t.check_constraints(&db)?.next().unwrap();
    /// let meta = db.check_constraint_metadata(check).unwrap();
    /// let names: Vec<&str> = meta.columns().map(|c| c.column_name()).collect();
    /// assert!(names.contains(&"a") && names.contains(&"b"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn columns(&self) -> impl Iterator<Item = &<U::DB as DatabaseLike>::Column> {
        self.columns.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns an iterator over the functions involved in the constraint.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE FUNCTION is_valid(x INT) RETURNS BOOLEAN AS 'SELECT $1 > 0';
    ///     CREATE TABLE t (id INT, CHECK (is_valid(id)));
    ///     ",
    /// )?;
    /// let t = db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check = t.check_constraints(&db)?.next().unwrap();
    /// let meta = db.check_constraint_metadata(check).unwrap();
    /// let names: Vec<&str> = meta.functions().map(|f| f.name()).collect();
    /// assert!(names.contains(&"is_valid"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn functions(&self) -> impl Iterator<Item = &<U::DB as DatabaseLike>::Function> {
        self.calls.functions()
    }

    /// Returns the calls of the constraint's expression.
    #[must_use]
    #[inline]
    pub fn calls(&self) -> &ExpressionCalls<<U::DB as DatabaseLike>::Function> {
        &self.calls
    }

    /// Re-points every cached function still referencing the allocation
    /// `stale` names at `fresh` instead.
    ///
    /// `ALTER FUNCTION` and `CREATE OR REPLACE` both rewrite the
    /// canonical store, so a cache resolved at constraint creation would
    /// otherwise keep answering with the superseded node.
    #[inline]
    pub fn replace_function(
        &mut self,
        stale: &Arc<<U::DB as DatabaseLike>::Function>,
        fresh: &Arc<<U::DB as DatabaseLike>::Function>,
    ) {
        self.calls.replace_function(stale, fresh);
    }
}
