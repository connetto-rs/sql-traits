//! The index a parsed script's `EXCLUDE` constraint builds.

use sqlparser::ast::{CreateTable, ExcludeConstraint, Ident, IndexType};

use crate::structs::TableAttribute;

/// The index PostgreSQL builds to enforce an `EXCLUDE` constraint, listed by
/// [`TableLike::exclusion_indices`](crate::traits::TableLike::exclusion_indices).
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE reserved (period INT4RANGE, EXCLUDE USING gist (period WITH &&));",
/// )?;
/// let table =
///     db.table_by_target(TargetName::new("reserved", false), IdentifierCase::AsWritten)?.unwrap();
/// let index = table.exclusion_indices(&db)?.next().unwrap();
/// assert_eq!(index.constraint().attribute().elements[0].operator.to_string(), "&&");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExclusionIndex {
    constraint: TableAttribute<CreateTable, ExcludeConstraint>,
    /// The `USING` clause read as `CREATE INDEX` reads it, which the
    /// constraint keeps as a bare identifier.
    access_method: Option<IndexType>,
}

impl ExclusionIndex {
    /// The index `constraint` builds.
    pub(crate) fn new(constraint: TableAttribute<CreateTable, ExcludeConstraint>) -> Self {
        let access_method = constraint.attribute().index_method.as_ref().map(index_type);
        Self { constraint, access_method }
    }

    /// Returns the constraint the index enforces, with the table declaring
    /// it.
    #[must_use]
    #[inline]
    pub fn constraint(&self) -> &TableAttribute<CreateTable, ExcludeConstraint> {
        &self.constraint
    }

    /// Returns the access method the constraint names, or `None` when it
    /// names none.
    #[must_use]
    #[inline]
    pub(crate) fn access_method(&self) -> Option<&IndexType> {
        self.access_method.as_ref()
    }
}

/// The access method `method` names, as sqlparser reads the `USING` of
/// `CREATE INDEX`, which matches a built-in method only when unquoted.
fn index_type(method: &Ident) -> IndexType {
    let built_in = [
        ("btree", IndexType::BTree),
        ("hash", IndexType::Hash),
        ("gin", IndexType::GIN),
        ("gist", IndexType::GiST),
        ("spgist", IndexType::SPGiST),
        ("brin", IndexType::BRIN),
        ("bloom", IndexType::Bloom),
    ];
    built_in
        .into_iter()
        .find(|(name, _)| method.quote_style.is_none() && method.value.eq_ignore_ascii_case(name))
        .map_or_else(|| IndexType::Custom(method.clone()), |(_, built_in)| built_in)
}
