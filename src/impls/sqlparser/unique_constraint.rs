//! Implement the [`UniqueConstraint`] trait for the `sqlparser` crate's
//! [`TableAttribute`] wrapper.

use sqlparser::ast::{ColumnDef, CreateTable, Expr, IndexType, UniqueConstraint};

use super::index_clauses::{included_columns, written_access_method};
use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, TableAttribute, metadata::UniqueIndexMetadata},
    traits::{DatabaseLike, IndexLike, Metadata, TableLike},
};

impl Metadata for TableAttribute<CreateTable, UniqueConstraint> {
    type Meta = UniqueIndexMetadata<Self>;
}

/// Returns the name a unique constraint was declared with.
///
/// PostgreSQL spells it as a constraint name (`CONSTRAINT uq UNIQUE (..)`)
/// while MySQL spells it as an index name (`UNIQUE KEY uq (..)`), and sqlparser
/// keeps the two in separate fields.
fn unique_constraint_name(constraint: &UniqueConstraint) -> Option<&sqlparser::ast::Ident> {
    constraint.name.as_ref().or(constraint.index_name.as_ref())
}

impl TableAttribute<CreateTable, UniqueConstraint> {
    /// The metadata `database` keeps for the index backing this constraint,
    /// or the error saying it holds no such index.
    fn metadata<'db>(
        &self,
        database: &'db ParserDB,
    ) -> Result<&'db UniqueIndexMetadata<Self>, LookupError> {
        database.unique_index_metadata(self).ok_or_else(|| {
            match unique_constraint_name(self.attribute()) {
                Some(name) => ObjectKind::UniqueIndex.not_in_database(&name.value),
                None => {
                    ObjectKind::UniqueIndex.anonymous_not_in_database(self.table().table_name())
                }
            }
        })
    }
}

impl IndexLike for TableAttribute<CreateTable, UniqueConstraint> {
    type DB = ParserDB;

    #[inline]
    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.table()
    }

    /// Always `true`, since the parser stores here only primary keys and
    /// unique constraints, whose backing indexes are unique by definition.
    #[inline]
    fn is_unique(&self) -> bool {
        true
    }

    /// PostgreSQL builds the backing index under the constraint name and MySQL
    /// writes it as an index name directly, so whichever spelling the SQL used
    /// is this index's name. A constraint declared without one is anonymous.
    #[inline]
    fn name(&self) -> Option<&str> {
        unique_constraint_name(self.attribute()).map(|name| name.value.as_str())
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        unique_constraint_name(self.attribute()).is_some_and(|name| name.quote_style.is_some())
    }

    /// Always `None`: the parser stores a unique constraint name as a bare
    /// identifier, which cannot carry a qualifier.
    #[inline]
    fn schema(&self) -> Option<&str> {
        None
    }

    #[inline]
    fn expression<'db>(&'db self, database: &'db Self::DB) -> Result<&'db Expr, LookupError>
    where
        Self: 'db,
    {
        Ok(self.metadata(database)?.expression())
    }

    #[inline]
    fn access_method(&self) -> Option<&IndexType> {
        let constraint = self.attribute();
        written_access_method(constraint.index_type.as_ref(), &constraint.index_options)
    }

    #[inline]
    fn include_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db TableAttribute<CreateTable, ColumnDef>>, LookupError>
    where
        Self: 'db,
    {
        self.metadata(database)?;
        included_columns(&self.attribute().include, self.table(), database)
    }
}
