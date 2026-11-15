//! Implement the [`CheckConstraint`] trait for the `sqlparser` crate's
//! [`TableAttribute`] wrapper.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use sqlparser::ast::{CheckConstraint, CreateTable, Expr, Function};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{
        ClauseExpression, ConstraintObject, ParserDB, TableAttribute,
        metadata::{CheckLiterals, CheckMetadata},
    },
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

    fn check_clause<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<ClauseExpression<'db>, LookupError> {
        let metadata = check_constraint_metadata(self, database)?;
        let expression = metadata.expression();
        Ok(match metadata.literals() {
            CheckLiterals::Unreadable(literal, _) => {
                let check = self.constraint_name(database)?.map(Cow::into_owned);
                ClauseExpression::Written {
                    expression,
                    unreadable: literal.check_error(self.table().table_name(), check),
                }
            }
            CheckLiterals::Pending(_) | CheckLiterals::AsWritten | CheckLiterals::Stored(_) => {
                ClauseExpression::Stored(expression)
            }
        })
    }

    #[inline]
    fn written_expression<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<Option<&'db Expr>, LookupError> {
        Ok(Some(self.attribute().expr.as_ref()))
    }

    #[inline]
    fn no_inherit(&self) -> bool {
        self.attribute().no_inherit
    }

    fn is_validated(&self, database: &Self::DB) -> Result<bool, LookupError> {
        super::create_table::holds_validated(self.table(), self.attribute(), database)
    }

    fn constraint_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        let table = CheckConstraintLike::table(self, database)?;
        let held: Vec<_> = table.check_constraints(database)?.collect();
        Ok(super::create_table::position_among(&held, self).and_then(|position| {
            database.constraint_object_name(table, ConstraintObject::Check, position)
        }))
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

    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        // A call stands at the same place among the calls of the written
        // check as in the stored one.
        let metadata = check_constraint_metadata(self, database)?;
        let calls = metadata.calls();
        calls
            .reached_by(self.attribute().expr.as_ref(), call)
            .or_else(|| calls.reached_by(metadata.expression(), call))
            .ok_or_else(|| LookupError::CallNotInExpression { call: call.to_string() })
    }
}
