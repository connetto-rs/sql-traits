//! The bound a partition takes in its partitioned table.

use alloc::{string::String, vec::Vec};

/// The rows a partition takes, as its `FOR VALUES` clause or `DEFAULT` wrote
/// them. Each value is the expression as written, before PostgreSQL casts it
/// to the type of the partition key.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE events (id INT) PARTITION BY RANGE (id);
///      CREATE TABLE early PARTITION OF events FOR VALUES FROM (MINVALUE) TO (100);",
/// )?;
/// let early =
///     db.table_by_target(TargetName::new("early", false), IdentifierCase::AsWritten)?.unwrap();
/// assert_eq!(
///     early.partition_bound(),
///     Some(PartitionBound::Range {
///         from: vec![RangeDatum::MinValue],
///         to: vec![RangeDatum::Value("100".to_string())],
///     })
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PartitionBound {
    /// `DEFAULT`, which takes every row no other partition takes.
    Default,
    /// `FOR VALUES FROM (...) TO (...)`, one entry per key column, the lower
    /// bound included and the upper excluded.
    Range {
        /// The lower bound.
        from: Vec<RangeDatum>,
        /// The upper bound.
        to: Vec<RangeDatum>,
    },
    /// `FOR VALUES IN (...)`.
    List {
        /// The listed values, in the order written.
        values: Vec<ListDatum>,
    },
    /// `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
    Hash {
        /// The modulus.
        modulus: u64,
        /// The remainder.
        remainder: u64,
    },
}

/// One entry of a range bound.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RangeDatum {
    /// `MINVALUE`, below every value.
    MinValue,
    /// `MAXVALUE`, above every value.
    MaxValue,
    /// A value, as written.
    Value(String),
}

/// One entry of a list bound.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ListDatum {
    /// `NULL`, which takes the rows whose key is null.
    Null,
    /// A value, as written.
    Value(String),
}
