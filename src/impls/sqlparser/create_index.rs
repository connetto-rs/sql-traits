//! Implement the `IndexLike` trait for `sqlparser`'s `CreateIndex`, on a table
//! and on a materialized view.

use alloc::{borrow::Cow, string::ToString};

use sqlparser::ast::{ColumnDef, CreateIndex, CreateTable, Expr, IndexType};

use super::index_clauses::{
    included_columns, written_access_method, written_include_names, written_keys,
};
use crate::{
    errors::{LookupError, ObjectKind},
    impls::SqlparserDialect,
    structs::{
        IndexKey, MaterializedViewIndex, ParserDB, SqliteIndexColumn, TableAttribute, WrittenCall,
        generic_db::created_index_record, metadata::IndexMetadata,
    },
    traits::{DatabaseLike, IndexLike, Metadata, Relation, TableIndexLike, TableLike},
    utils::object_name::{object_name_last_part, qualifier_of},
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

/// The identifier a `CREATE INDEX` names its index by, with whether it was
/// quoted.
fn written_name(index: &CreateIndex) -> Option<(&str, bool)> {
    index.name.as_ref().and_then(object_name_last_part)
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
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        self.metadata(database)?;
        Ok(Relation::Table(self.table()))
    }

    #[inline]
    fn is_unique(&self) -> bool {
        self.attribute().unique
    }

    #[inline]
    fn is_valid(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(self.metadata(database)?.is_valid())
    }

    #[inline]
    fn name(&self) -> Option<&str> {
        written_name(self.attribute()).map(|(name, _)| name)
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        written_name(self.attribute()).is_some_and(|(_, quoted)| quoted)
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
    fn keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = IndexKey<'db>>, LookupError>
    where
        Self: 'db,
    {
        self.metadata(database)?;
        Ok(written_keys(&self.attribute().columns))
    }

    #[inline]
    fn access_method(&self) -> Option<&IndexType> {
        let index = self.attribute();
        written_access_method(index.using.as_ref(), &index.index_options)
    }

    #[inline]
    fn include_column_names(&self) -> impl Iterator<Item = (&str, bool)> {
        written_include_names(&self.attribute().include)
    }

    #[inline]
    fn predicate(&self) -> Option<&Expr> {
        self.attribute().predicate.as_ref()
    }

    /// The calls of the whole statement are bound in one record, so a call
    /// of the predicate is counted after every call of the key.
    fn functions_reached_by<'db, 'call>(
        &'db self,
        call: impl Into<WrittenCall<'call>>,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        let call = call.into();
        self.metadata(database)?
            .reached_by(self.attribute(), call)
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }

    #[inline]
    fn nulls_distinct(&self) -> bool {
        self.attribute().nulls_distinct != Some(false)
    }
}

impl TableIndexLike for TableAttribute<CreateTable, CreateIndex> {
    #[inline]
    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.table()
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
    fn sqlite_index_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<
        Option<
            impl Iterator<Item = SqliteIndexColumn<'db, TableAttribute<CreateTable, ColumnDef>>>,
        >,
        LookupError,
    >
    where
        Self: 'db,
    {
        self.metadata(database)?;
        if *database.dialect() != SqlparserDialect::SQLite {
            return Ok(None);
        }
        let metadata = super::create_table::table_metadata(self.table(), database)?;
        let keys = &self.attribute().columns;
        let record = created_index_record(keys, self.table(), metadata, database.identifier_case());
        Ok(Some(record.into_iter()))
    }
}

impl Metadata for MaterializedViewIndex {
    type Meta = IndexMetadata<Self>;
}

impl MaterializedViewIndex {
    /// The error saying a database holds no such index.
    fn not_in_database(&self) -> LookupError {
        ObjectKind::Index.not_in_database(&match self.index().name.as_ref() {
            Some(name) => name.to_string(),
            None => self.index().to_string(),
        })
    }

    /// The metadata `database` keeps for this index, or the error saying it
    /// holds no such index.
    fn metadata<'db>(
        &self,
        database: &'db ParserDB,
    ) -> Result<&'db IndexMetadata<Self>, LookupError> {
        database.materialized_view_index_metadata(self).ok_or_else(|| self.not_in_database())
    }
}

impl IndexLike for MaterializedViewIndex {
    type DB = ParserDB;

    /// Always `None`, since an index on a materialized view backs no
    /// constraint.
    #[inline]
    fn constraint_name<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        Ok(None)
    }

    /// An index is held with the view it is on, so a database lacking either
    /// reports the index as one it does not hold.
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        database
            .materialized_view_index_metadata(self)
            .and_then(|_| database.materialized_view_by_identity(self.view()))
            .map(Relation::MaterializedView)
            .ok_or_else(|| self.not_in_database())
    }

    #[inline]
    fn is_unique(&self) -> bool {
        self.index().unique
    }

    #[inline]
    fn name(&self) -> Option<&str> {
        written_name(self.index()).map(|(name, _)| name)
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        written_name(self.index()).is_some_and(|(_, quoted)| quoted)
    }

    /// The qualifier the relation name was bound to, which is the schema the
    /// materialized view sits in.
    #[inline]
    fn schema(&self) -> Option<&str> {
        qualifier_of(&self.index().table_name).named().map(|(schema, _)| schema)
    }

    #[inline]
    fn schema_is_quoted(&self) -> bool {
        qualifier_of(&self.index().table_name).named().is_some_and(|(_, quoted)| quoted)
    }

    #[inline]
    fn expression<'db>(&'db self, database: &'db Self::DB) -> Result<&'db Expr, LookupError>
    where
        Self: 'db,
    {
        Ok(self.metadata(database)?.expression())
    }

    #[inline]
    fn keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = IndexKey<'db>>, LookupError>
    where
        Self: 'db,
    {
        self.metadata(database)?;
        Ok(written_keys(&self.index().columns))
    }

    #[inline]
    fn access_method(&self) -> Option<&IndexType> {
        let index = self.index();
        written_access_method(index.using.as_ref(), &index.index_options)
    }

    #[inline]
    fn include_column_names(&self) -> impl Iterator<Item = (&str, bool)> {
        written_include_names(&self.index().include)
    }

    #[inline]
    fn predicate(&self) -> Option<&Expr> {
        self.index().predicate.as_ref()
    }

    /// The calls of the whole statement are bound in one record, so a call
    /// of the predicate is counted after every call of the key. The rows of a
    /// materialized view are not typed while the input is read, so a call
    /// overloaded on its argument types reaches every overload of its name
    /// and argument count.
    fn functions_reached_by<'db, 'call>(
        &'db self,
        call: impl Into<WrittenCall<'call>>,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        let call = call.into();
        self.metadata(database)?
            .reached_by(self.index(), call)
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }

    #[inline]
    fn nulls_distinct(&self) -> bool {
        self.index().nulls_distinct != Some(false)
    }
}
