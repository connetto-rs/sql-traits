//! Implement the index traits for the [`ExclusionIndex`] an `EXCLUDE`
//! constraint of a parsed table builds.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use sqlparser::ast::{ColumnDef, CreateTable, Expr, IndexType};

use super::index_clauses::{included_columns, written_include_names, written_keys};
use crate::{
    errors::{LookupError, ObjectKind},
    impls::SqlparserDialect,
    structs::{
        ConstraintDeferral, ConstraintObject, ExclusionIndex, IndexKey, ParserDB, TableAttribute,
        WrittenCall, metadata::IndexMetadata,
    },
    traits::{DatabaseLike, IndexLike, Metadata, Relation, TableIndexLike, TableLike},
};

impl Metadata for ExclusionIndex {
    type Meta = IndexMetadata<Self>;
}

impl ExclusionIndex {
    /// The metadata `database` keeps for this index, or the error saying it
    /// holds no such index.
    fn metadata<'db>(
        &self,
        database: &'db ParserDB,
    ) -> Result<&'db IndexMetadata<Self>, LookupError> {
        database.exclusion_index_metadata(self).ok_or_else(|| {
            match &self.constraint().attribute().name {
                Some(name) => ObjectKind::Index.not_in_database(&name.value),
                None => ObjectKind::Index.anonymous_not_in_database(self.table().table_name()),
            }
        })
    }

    /// The table declaring the constraint.
    fn table(&self) -> &CreateTable {
        self.constraint().table()
    }
}

impl IndexLike for ExclusionIndex {
    type DB = ParserDB;

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

    /// Always `false`, since an exclusion constraint's operators need not be
    /// equality.
    #[inline]
    fn is_unique(&self) -> bool {
        false
    }

    /// The name written for the constraint, which PostgreSQL gives its index
    /// too. A constraint declared without one is anonymous.
    #[inline]
    fn name(&self) -> Option<&str> {
        self.constraint().attribute().name.as_ref().map(|name| name.value.as_str())
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        self.constraint().attribute().name.as_ref().is_some_and(|name| name.quote_style.is_some())
    }

    #[inline]
    fn schema(&self) -> Option<&str> {
        self.table().table_schema()
    }

    #[inline]
    fn schema_is_quoted(&self) -> bool {
        self.table().table_schema_is_quoted()
    }

    fn constraint_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        let table = self.table();
        let held: Vec<_> = table.exclusion_indices(database)?.collect();
        Ok(super::create_table::position_among(&held, self).and_then(|position| {
            database.constraint_object_name(table, ConstraintObject::Exclusion, position)
        }))
    }

    fn is_valid(&self, database: &Self::DB) -> Result<bool, LookupError> {
        let table = self.table();
        let held: Vec<_> = table.exclusion_indices(database)?.collect();
        Ok(super::create_table::position_among(&held, self).is_none_or(|position| {
            database.constraint_index_is_valid(table, ConstraintObject::Exclusion, position)
        }))
    }

    fn backing_index_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.metadata(database)?;
        match database.dialect() {
            SqlparserDialect::PostgreSql => self.constraint_name(database),
            _ => Ok(self.name().map(Cow::Borrowed)),
        }
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
        Ok(written_keys(
            self.constraint().attribute().elements.iter().map(|element| &element.column),
        ))
    }

    #[inline]
    fn access_method(&self) -> Option<&IndexType> {
        ExclusionIndex::access_method(self)
    }

    #[inline]
    fn include_column_names(&self) -> impl Iterator<Item = (&str, bool)> {
        written_include_names(&self.constraint().attribute().include)
    }

    #[inline]
    fn predicate(&self) -> Option<&Expr> {
        self.constraint().attribute().where_clause.as_deref()
    }

    fn functions_reached_by<'db, 'call>(
        &'db self,
        call: impl Into<WrittenCall<'call>>,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        let call = call.into();
        self.metadata(database)?
            .reached_by(self.constraint().attribute(), call)
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }
}

impl TableIndexLike for ExclusionIndex {
    #[inline]
    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        ExclusionIndex::table(self)
    }

    #[inline]
    fn deferral(&self, _database: &Self::DB) -> ConstraintDeferral {
        ConstraintDeferral::of(self.constraint().attribute().characteristics)
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
        included_columns(&self.constraint().attribute().include, self.table(), database)
    }
}
