//! An index a parsed script builds on a materialized view.

use sqlparser::ast::CreateIndex;

use crate::structs::RelationIdentity;

/// A `CREATE INDEX` on a materialized view, held with the identity of the
/// view it is on.
///
/// The view is reached through its identity rather than a copy of its node,
/// so the index answers the view as it stands after every rename and rewrite.
/// The statement's relation name is bound to the view's stored name and
/// follows its renames, which is where the index reads its schema from.
///
/// # Example
///
/// ```rust
/// # fn main() -> Result<(), sql_traits::errors::Error> {
/// use sql_traits::prelude::*;
/// use sqlparser::dialect::PostgreSqlDialect;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>(
///     "CREATE TABLE documents (id int, title text);
///      CREATE MATERIALIZED VIEW titles AS SELECT id, title FROM documents;
///      CREATE INDEX titles_title ON titles (title);
///      ALTER MATERIALIZED VIEW titles RENAME TO headings;",
/// )?;
/// let index = db.materialized_view_indexes().next().unwrap();
/// assert_eq!(index.index().table_name.to_string(), "headings");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaterializedViewIndex {
    /// The statement creating the index, its relation name bound.
    index: CreateIndex,
    /// The identity of the materialized view the index is on.
    view: RelationIdentity,
}

impl MaterializedViewIndex {
    /// Pairs `index` with the identity of the materialized view it is on.
    pub(crate) fn new(index: CreateIndex, view: RelationIdentity) -> Self {
        Self { index, view }
    }

    /// Returns the statement creating the index.
    #[must_use]
    #[inline]
    pub fn index(&self) -> &CreateIndex {
        &self.index
    }

    /// Returns the statement creating the index, to follow a rename.
    pub(crate) fn index_mut(&mut self) -> &mut CreateIndex {
        &mut self.index
    }

    /// Returns the identity of the materialized view the index is on.
    #[must_use]
    #[inline]
    pub fn view(&self) -> RelationIdentity {
        self.view
    }
}
