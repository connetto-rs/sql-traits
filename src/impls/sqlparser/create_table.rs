//! Submodule implementing the [`TableLike`] trait for `sqlparser`'s
//! [`CreateTable`] struct.

use alloc::{string::ToString, vec::Vec};

use ::sqlparser::ast::{CreateTable, Expr, ObjectNamePart};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, TableMetadata},
    traits::{DatabaseLike, Metadata, PartitionStrategy, TableLike},
    utils::{last_str, object_name::qualifier_of},
};

impl Metadata for CreateTable {
    type Meta = TableMetadata<CreateTable>;
}

/// Resolves the metadata `database` holds for `table`.
///
/// A [`CreateTable`] node and a [`ParserDB`] are independent values, so a node
/// the database does not hold (renamed away, dropped, or parsed from different
/// input) has no metadata to report.
fn table_metadata<'db>(
    table: &CreateTable,
    database: &'db ParserDB,
) -> Result<&'db TableMetadata<CreateTable>, LookupError> {
    database
        .table_metadata(table)
        .ok_or_else(|| ObjectKind::Table.not_in_database(&table.name.to_string()))
}

/// Reads the strategy off a `PARTITION BY` clause.
///
/// PostgreSQL takes one of three words there and parses the clause as a call,
/// so `RANGE (id)` arrives as a call to `RANGE`, whatever the case and quoting
/// of the word. Other dialects put an expression of their own in the same
/// field, and BigQuery's `PARTITION BY DATE(ts)` describes how one ordinary
/// table is laid out rather than a hierarchy of them, so anything else here
/// answers [`None`].
fn declared_strategy(clause: &Expr) -> Option<PartitionStrategy> {
    let Expr::Function(function) = clause else {
        return None;
    };
    let [ObjectNamePart::Identifier(strategy)] = function.name.0.as_slice() else {
        return None;
    };
    let strategy = strategy.value.as_str();
    if strategy.eq_ignore_ascii_case("range") {
        Some(PartitionStrategy::Range)
    } else if strategy.eq_ignore_ascii_case("list") {
        Some(PartitionStrategy::List)
    } else if strategy.eq_ignore_ascii_case("hash") {
        Some(PartitionStrategy::Hash)
    } else {
        None
    }
}

impl TableLike for CreateTable {
    type DB = ParserDB;

    #[inline]
    fn table_name(&self) -> &str {
        last_str(&self.name)
    }

    #[inline]
    fn table_name_is_quoted(&self) -> bool {
        self.name.0.last().is_some_and(
            |part| matches!(part, ObjectNamePart::Identifier(ident) if ident.quote_style.is_some()),
        )
    }

    #[inline]
    fn table_doc<'db>(&'db self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.documentation())
    }

    #[inline]
    fn table_schema(&self) -> Option<&str> {
        qualifier_of(&self.name).named().map(|(schema, _)| schema)
    }

    #[inline]
    fn table_schema_is_quoted(&self) -> bool {
        if self.name.0.len() <= 1 {
            return false;
        }
        self.name.0.first().is_some_and(
            |part| matches!(part, ObjectNamePart::Identifier(ident) if ident.quote_style.is_some()),
        )
    }

    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.columns())
    }

    fn local_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.local_columns())
    }

    fn inherits_from<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        // Membership is what every accessor here checks first, and the parent
        // list is read off the node rather than the metadata.
        self.require_in_database(database)?;
        let mut parents = Vec::new();
        for name in self.inherits.iter().flatten() {
            if let Some(parent) = database.resolve_table_object_name(name)? {
                parents.push(parent);
            }
        }
        Ok(parents.into_iter())
    }

    fn partition_root<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Table>, LookupError>
    where
        Self: 'db,
    {
        self.require_in_database(database)?;
        match &self.partition_of {
            Some(root) => database.resolve_table_object_name(root),
            None => Ok(None),
        }
    }

    #[inline]
    fn partition_strategy(&self) -> Option<PartitionStrategy> {
        declared_strategy(self.partition_by.as_deref()?)
    }

    fn primary_key_columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.primary_key_columns())
    }

    fn unique_indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::UniqueIndex>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.unique_indices())
    }

    fn indices<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Index>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.indices())
    }

    fn check_constraints<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::CheckConstraint>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.check_constraints())
    }

    fn foreign_keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::ForeignKey>, LookupError>
    where
        Self: 'db,
    {
        Ok(table_metadata(self, database)?.foreign_keys())
    }

    #[inline]
    fn has_row_level_security(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(table_metadata(self, database)?.rls_enabled())
    }

    #[inline]
    fn has_forced_row_level_security(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(table_metadata(self, database)?.rls_forced())
    }

    #[inline]
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        Ok(table_metadata(self, database)?.owner())
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::ast::{
        Ident, ObjectName, ObjectNamePart, ObjectNamePartFunction,
        helpers::stmt_create_table::CreateTableBuilder,
    };

    use super::*;

    /// A qualifier some dialects spell as a call, `IDENTIFIER('app')`, names
    /// whatever the call returns when the statement runs, so no schema can be
    /// read from it. Reporting the producing call's own name recorded a table
    /// in a schema nobody wrote.
    #[test]
    fn a_run_time_qualifier_names_no_schema() {
        let name = ObjectName(vec![
            ObjectNamePart::Function(ObjectNamePartFunction {
                name: Ident::new("IDENTIFIER"),
                args: alloc::vec::Vec::new(),
            }),
            ObjectNamePart::Identifier(Ident::new("t")),
        ]);
        let create_table = CreateTableBuilder::new(name).build();
        assert_eq!(create_table.table_schema(), None);
        assert_eq!(create_table.table_name(), "t");
    }
}
