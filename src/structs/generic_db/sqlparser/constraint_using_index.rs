//! Adoption of standalone indexes by key constraints.

use sqlparser::ast::{CreateIndex, KeyOrIndexDisplay, NullsDistinctOption};

use super::{
    IndexSlot, ParserDB, ParserDBBuilder, PrimaryKeyConstraint, StoredTable, TableConstraint,
    UniqueConstraint, constraint_names, index_holds_name, replica_identity, table_schema_qualifier,
};
use crate::{errors::Error, utils::identifier_resolution::stored_ident_name};

/// Materializes an adopted index as an ordinary key constraint.
pub(super) fn adopt(
    builder: &mut ParserDBBuilder,
    stored: &StoredTable,
    constraint: &mut TableConstraint,
) -> Result<(), Error> {
    let primary = matches!(constraint, TableConstraint::PrimaryKeyUsingIndex(_));
    let (TableConstraint::PrimaryKeyUsingIndex(adoption)
    | TableConstraint::UniqueUsingIndex(adoption)) = constraint
    else {
        return Ok(());
    };
    let table = ParserDB::stored_node(builder, stored)?;
    if table.partition_by.is_some() {
        return Err(Error::AdoptedIndexOnPartitionedTable);
    }
    let schema = table_schema_qualifier(table);
    let index_name = &adoption.index_name;
    if constraint_names::index_name_held(builder, index_name, schema)
        || super::index_name_holder(builder, index_name, schema)
            == Some(super::ObjectKind::UniqueIndex)
    {
        return Err(Error::AdoptedIndexAlreadyConstrained { index_name: index_name.value.clone() });
    }
    let position = builder
        .indices()
        .iter()
        .position(|(index, _)| {
            index_holds_name(
                index,
                index_name,
                schema,
                builder.namespaces(),
                builder.identifier_case(),
            )
        })
        .ok_or_else(|| Error::AdoptedIndexNotFound { index_name: index_name.value.clone() })?;
    let index = builder.indices()[position].0.as_ref();
    if !stored.matches(index.table(), builder.identifier_case()) {
        return Err(Error::AdoptedIndexWrongTable {
            index_name: index_name.value.clone(),
            table_name: stored.name.clone(),
        });
    }
    let index = index.attribute();
    validate(index, index_name)?;
    let key = adoption.name.as_ref().unwrap_or(index_name);
    let identity = replica_identity::created_index_name(index)
        .map(|adopted| (adopted.into_owned(), stored_ident_name(key).into_owned()));
    let name = Some(key.clone());
    *constraint = if primary {
        TableConstraint::PrimaryKey(PrimaryKeyConstraint {
            name,
            index_name: None,
            index_type: index.using.clone(),
            columns: index.columns.clone(),
            include: index.include.clone(),
            index_options: index.index_options.clone(),
            characteristics: adoption.characteristics,
            on_conflict: None,
        })
    } else {
        TableConstraint::Unique(UniqueConstraint {
            name,
            index_name: None,
            index_type_display: KeyOrIndexDisplay::None,
            index_type: index.using.clone(),
            columns: index.columns.clone(),
            include: index.include.clone(),
            index_options: index.index_options.clone(),
            characteristics: adoption.characteristics,
            nulls_distinct: match index.nulls_distinct {
                Some(true) => NullsDistinctOption::Distinct,
                Some(false) => NullsDistinctOption::NotDistinct,
                None => NullsDistinctOption::None,
            },
            on_conflict: None,
        })
    };
    builder.detach_index_at(IndexSlot::Table(position));
    // PostgreSQL keeps the replica identity on the index under its new name.
    let case = builder.identifier_case();
    for (table, metadata, _) in builder.tables_mut() {
        if let Some((adopted, key)) = &identity
            && stored.matches(table, case)
        {
            replica_identity::follow_rename(metadata, adopted, key);
        }
    }
    Ok(())
}

/// Refuses index definitions that cannot back a key constraint.
fn validate(index: &CreateIndex, name: &sqlparser::ast::Ident) -> Result<(), Error> {
    if !index.unique {
        return Err(Error::AdoptedIndexNotUnique { index_name: name.value.clone() });
    }
    if index
        .columns
        .iter()
        .any(|column| !matches!(column.column.expr, sqlparser::ast::Expr::Identifier(_)))
    {
        return Err(Error::AdoptedIndexExpressions { index_name: name.value.clone() });
    }
    if index.predicate.is_some() {
        return Err(Error::AdoptedIndexPartial { index_name: name.value.clone() });
    }
    if let Some(position) = index.columns.iter().position(|column| {
        matches!(column.column.options.sort, Some(sqlparser::ast::OrderBySort::Desc))
            || column.column.options.nulls_first == Some(true)
    }) {
        return Err(Error::AdoptedIndexSorting {
            index_name: name.value.clone(),
            column_number: position + 1,
        });
    }
    Ok(())
}
