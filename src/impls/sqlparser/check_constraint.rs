//! Implement the [`CheckConstraint`] trait for the `sqlparser` crate's
//! [`TableAttribute`] wrapper.

use sqlparser::ast::{CheckConstraint, CreateTable, Expr};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, TableAttribute, metadata::CheckMetadata},
    traits::{CheckConstraintLike, DatabaseLike, Metadata, TableLike},
};

impl Metadata for TableAttribute<CreateTable, CheckConstraint> {
    type Meta = CheckMetadata<Self>;
}

/// Resolves the metadata `database` holds for `constraint`.
fn check_constraint_metadata<'db>(
    constraint: &TableAttribute<CreateTable, CheckConstraint>,
    database: &'db ParserDB,
) -> Result<&'db CheckMetadata<TableAttribute<CreateTable, CheckConstraint>>, LookupError> {
    database.check_constraint_metadata(constraint).ok_or_else(|| {
        match constraint.attribute().name.as_ref() {
            Some(name) => ObjectKind::CheckConstraint.not_in_database(&name.value),
            None => {
                ObjectKind::CheckConstraint
                    .anonymous_not_in_database(constraint.table().table_name())
            }
        }
    })
}

impl CheckConstraintLike for TableAttribute<CreateTable, CheckConstraint> {
    type DB = ParserDB;

    #[inline]
    fn expression<'db>(&'db self, _database: &'db Self::DB) -> &'db Expr {
        self.attribute().expr.as_ref()
    }

    #[inline]
    fn no_inherit(&self) -> bool {
        self.attribute().no_inherit
    }

    fn is_validated(&self, database: &Self::DB) -> Result<bool, LookupError> {
        super::create_table::holds_validated(self.table(), self.attribute(), database)
    }

    #[inline]
    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError> {
        Ok(check_constraint_metadata(self, database)?.table())
    }

    #[inline]
    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError> {
        Ok(check_constraint_metadata(self, database)?.columns())
    }

    #[inline]
    fn functions<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        Ok(check_constraint_metadata(self, database)?.functions())
    }
}
