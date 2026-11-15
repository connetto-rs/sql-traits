//! Implement the `IndexLike` trait for `sqlparser`'s `CreateIndex`.

use alloc::{borrow::Cow, string::ToString};

use sqlparser::ast::{ColumnDef, CreateIndex, CreateTable, Expr, Function, IndexType};

use super::index_clauses::{included_columns, written_access_method};
use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, TableAttribute, metadata::IndexMetadata},
    traits::{DatabaseLike, IndexLike, Metadata, TableLike},
    utils::object_name::object_name_last_part,
};

impl Metadata for TableAttribute<CreateTable, CreateIndex> {
    type Meta = IndexMetadata<Self>;
}

impl TableAttribute<CreateTable, CreateIndex> {
    /// The metadata `database` keeps for this index, or the error saying it
    /// holds no such index.
    fn metadata<'db>(
        &self,
        database: &'db ParserDB,
    ) -> Result<&'db IndexMetadata<Self>, LookupError> {
        database.index_metadata(self).ok_or_else(|| {
            match self.attribute().name.as_ref() {
                Some(name) => ObjectKind::Index.not_in_database(&name.to_string()),
                None => ObjectKind::Index.anonymous_not_in_database(self.table().table_name()),
            }
        })
    }
}

impl IndexLike for TableAttribute<CreateTable, CreateIndex> {
    type DB = ParserDB;

    /// Always `None`: `CREATE INDEX` builds an index backing no constraint.
    #[inline]
    fn constraint_name<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        Ok(None)
    }

    #[inline]
    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.table()
    }

    #[inline]
    fn is_unique(&self) -> bool {
        self.attribute().unique
    }

    #[inline]
    fn name(&self) -> Option<&str> {
        self.attribute().name.as_ref().and_then(object_name_last_part).map(|(name, _)| name)
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        self.attribute()
            .name
            .as_ref()
            .and_then(object_name_last_part)
            .is_some_and(|(_, quoted)| quoted)
    }

    #[inline]
    fn schema(&self) -> Option<&str> {
        self.table().table_schema()
    }

    #[inline]
    fn schema_is_quoted(&self) -> bool {
        self.table().table_schema_is_quoted()
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
        let index = self.attribute();
        written_access_method(index.using.as_ref(), &index.index_options)
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

    #[inline]
    fn predicate(&self) -> Option<&Expr> {
        self.attribute().predicate.as_ref()
    }

    /// The calls of the whole statement are bound in one record, so a call
    /// of the predicate is counted after every call of the key.
    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        self.metadata(database)?
            .reached_by(self.attribute(), call)
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }

    #[inline]
    fn nulls_distinct(&self) -> bool {
        self.attribute().nulls_distinct != Some(false)
    }
}
