//! Implement the [`UniqueConstraint`] trait for the `sqlparser` crate's
//! [`TableAttribute`] wrapper.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use sqlparser::ast::{
    ColumnDef, CreateTable, Expr, IndexType, NullsDistinctOption, UniqueConstraint,
};

use super::index_clauses::{
    included_columns, written_access_method, written_include_names, written_keys,
};
use crate::{
    errors::{LookupError, ObjectKind},
    impls::SqlparserDialect,
    structs::{
        ConstraintDeferral, ConstraintObject, IndexKey, ParserDB, SqliteIndexColumn,
        TableAttribute, WrittenCall, generic_db::constraint_index_record,
        metadata::UniqueIndexMetadata,
    },
    traits::{DatabaseLike, IndexLike, Metadata, Relation, TableIndexLike, TableLike},
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
        let held: Vec<_> = table.unique_indices(database)?.collect();
        Ok(super::create_table::position_among(&held, self).and_then(|position| {
            database.constraint_object_name(table, ConstraintObject::UniqueKey, position)
        }))
    }

    fn is_valid(&self, database: &Self::DB) -> Result<bool, LookupError> {
        let table = self.table();
        let held: Vec<_> = table.unique_indices(database)?.collect();
        Ok(super::create_table::position_among(&held, self)
            .is_none_or(|position| database.constraint_index_is_valid(table, position)))
    }

    fn backing_index_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.metadata(database)?;
        match database.dialect() {
            SqlparserDialect::SQLite => {
                Ok(super::create_table::table_metadata(self.table(), database)?
                    .sqlite_autoindex(self)
                    .map(|autoindex| Cow::Borrowed(autoindex.name())))
            }
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
        Ok(written_keys(&self.attribute().columns))
    }

    #[inline]
    fn access_method(&self) -> Option<&IndexType> {
        let constraint = self.attribute();
        written_access_method(constraint.index_type.as_ref(), &constraint.index_options)
    }

    #[inline]
    fn include_column_names(&self) -> impl Iterator<Item = (&str, bool)> {
        written_include_names(&self.attribute().include)
    }

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
        self.attribute().nulls_distinct != NullsDistinctOption::NotDistinct
    }
}

impl TableIndexLike for TableAttribute<CreateTable, UniqueConstraint> {
    #[inline]
    fn table<'db>(&'db self, _database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db,
    {
        self.table()
    }

    #[inline]
    fn deferral(&self, _database: &Self::DB) -> ConstraintDeferral {
        ConstraintDeferral::of(self.attribute().characteristics)
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
        let record = constraint_index_record(self, metadata, database.identifier_case());
        Ok(record.map(IntoIterator::into_iter))
    }
}
