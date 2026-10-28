//! The kind of a function, as PostgreSQL's `pg_proc.prokind` records it.

/// What a call of the function computes, mirroring `pg_proc.prokind`.
///
/// The kind decides how a call behaves over the rows it is evaluated on. An
/// aggregate without `OVER` folds all of its input rows into one, so a query
/// with no `GROUP BY` that calls one yields a row even when no input row
/// passes its `WHERE`. A window function computes one value per row and so
/// yields nothing from no input, as a normal function does.
///
/// A function the input declares with `CREATE FUNCTION` is normal. The
/// aggregates and window functions are the built-ins PostgreSQL ships, since
/// the parser reads no `CREATE AGGREGATE`.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE members (member TEXT);")?;
/// let kind_of = |name: &str| {
///     db.functions()
///         .find(|function| function.name() == name)
///         .map(|function| function.kind(&db))
///         .transpose()
/// };
/// assert_eq!(kind_of("count")?, Some(FunctionKind::Aggregate));
/// assert_eq!(kind_of("row_number")?, Some(FunctionKind::Window));
/// assert_eq!(kind_of("lower")?, Some(FunctionKind::Normal));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum FunctionKind {
    /// `prokind = 'f'`, a function returning one value per call.
    #[default]
    Normal,
    /// `prokind = 'a'`, an aggregate folding its input rows into one value.
    Aggregate,
    /// `prokind = 'w'`, a window function computing one value per row of its
    /// window.
    Window,
}
