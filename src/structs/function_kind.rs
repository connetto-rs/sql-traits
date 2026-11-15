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

/// How an aggregate takes its arguments, mirroring `pg_aggregate.aggkind`
/// and, for an ordered-set aggregate, `pg_aggregate.aggnumdirectargs`, as
/// [the catalog](https://www.postgresql.org/docs/18/catalog-pg-aggregate.html)
/// records them.
///
/// An ordered-set aggregate takes its direct arguments in parentheses and
/// the values it aggregates in `WITHIN GROUP (ORDER BY ...)`, so a call
/// needs the clause and cannot run over a window. A hypothetical-set
/// aggregate is an ordered-set one whose direct arguments are one row
/// ranked among the ones it aggregates, as many values as `ORDER BY` lists.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>("")?;
/// let mut kinds = Vec::new();
/// for function in db.functions().filter(|function| function.name() == "percentile_disc") {
///     kinds.push(function.aggregate_kind(&db)?);
/// }
/// let kind = Some(AggregateKind::OrderedSet { direct_arguments: 1 });
/// assert_eq!(kinds, [kind, kind]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AggregateKind {
    /// `aggkind = 'n'`, an aggregate of the values its arguments take.
    Normal,
    /// `aggkind = 'o'`, an aggregate whose first `direct_arguments`
    /// arguments are passed once per call and whose others are what
    /// `WITHIN GROUP` orders.
    OrderedSet {
        /// How many arguments come before those `WITHIN GROUP` orders.
        direct_arguments: usize,
    },
    /// `aggkind = 'h'`, an ordered-set aggregate whose direct arguments name
    /// a hypothetical row of the values `WITHIN GROUP` orders.
    HypotheticalSet {
        /// How many arguments the catalog counts as direct, all of them
        /// variadic for every built-in one.
        direct_arguments: usize,
    },
}
