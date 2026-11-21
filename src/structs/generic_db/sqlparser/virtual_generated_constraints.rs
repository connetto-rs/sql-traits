//! PostgreSQL index and constraint restrictions on virtual generated columns.

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateIndex, CreateTable, Expr, ForeignKeyConstraint,
    GeneratedExpressionMode, Ident, IndexColumn, TableConstraint,
};

use super::{ParserDBBuilder, case_idents_match};
use crate::{
    errors::Error,
    impls::{SqlparserDialect, generation_storage},
    utils::{columns_in_expression::visit_columns, object_name::target_name_of_object_name},
};

/// Identifies a virtual column under PostgreSQL generation defaults.
fn virtual_column(builder: &ParserDBBuilder, column: &ColumnDef) -> bool {
    generation_storage(column, *builder.dialect()) == Some(GeneratedExpressionMode::Virtual)
}

/// Resolves a column name against the table's virtual columns.
fn virtual_name(builder: &ParserDBBuilder, table: &CreateTable, name: &Ident) -> bool {
    table.columns.iter().any(|column| {
        case_idents_match(&column.name, name, builder.identifier_case())
            && virtual_column(builder, column)
    })
}

/// Detects virtual references without treating argument labels as columns.
fn reads_virtual(builder: &ParserDBBuilder, table: &CreateTable, expression: &Expr) -> bool {
    let mut found = false;
    let _ = visit_columns(
        expression,
        builder.catalog_name(),
        &target_name_of_object_name(&table.name),
        builder.identifier_case(),
        *builder.dialect(),
        |name| {
            found |= virtual_name(builder, table, name);
            true
        },
    );
    found
}

/// Checks index keys and included columns for virtual references.
fn indexed_virtual(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    columns: &[IndexColumn],
    include: &[Ident],
) -> bool {
    columns.iter().any(|column| reads_virtual(builder, table, &column.column.expr))
        || include.iter().any(|name| virtual_name(builder, table, name))
}

/// Refuses virtual references in an index's keys, included columns or
/// predicate.
pub(super) fn index(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    index: &CreateIndex,
) -> Result<(), Error> {
    if *builder.dialect() == SqlparserDialect::PostgreSql
        && (indexed_virtual(builder, table, &index.columns, &index.include)
            || index
                .predicate
                .as_ref()
                .is_some_and(|predicate| reads_virtual(builder, table, predicate)))
    {
        return Err(Error::VirtualGeneratedIndex);
    }
    Ok(())
}

/// Refuses virtual references in one constraint after its ordinary name
/// binding.
pub(super) fn constraint(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    constraint: &TableConstraint,
) -> Result<(), Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql {
        return Ok(());
    }
    match constraint {
        TableConstraint::PrimaryKey(key)
            if indexed_virtual(builder, table, &key.columns, &key.include) =>
        {
            Err(Error::VirtualGeneratedPrimaryKey)
        }
        TableConstraint::Unique(key)
            if indexed_virtual(builder, table, &key.columns, &key.include) =>
        {
            Err(Error::VirtualGeneratedUnique)
        }
        TableConstraint::Exclude(exclusion)
            if exclusion
                .elements
                .iter()
                .any(|element| reads_virtual(builder, table, &element.column.column.expr))
                || exclusion.include.iter().any(|name| virtual_name(builder, table, name))
                || exclusion
                    .where_clause
                    .as_ref()
                    .is_some_and(|predicate| reads_virtual(builder, table, predicate)) =>
        {
            Err(Error::VirtualGeneratedUnique)
        }
        _ => Ok(()),
    }
}

/// Checks primary keys before the remaining index constraints of a new table.
pub(super) fn table_keys(builder: &ParserDBBuilder, table: &CreateTable) -> Result<(), Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql {
        return Ok(());
    }
    for primary in [true, false] {
        for column in &table.columns {
            if virtual_column(builder, column) {
                for option in &column.options {
                    match &option.option {
                        ColumnOption::PrimaryKey(_) if primary => {
                            return Err(Error::VirtualGeneratedPrimaryKey);
                        }
                        ColumnOption::Unique(_) if !primary => {
                            return Err(Error::VirtualGeneratedUnique);
                        }
                        _ => {}
                    }
                }
            }
        }
        for declared in &table.constraints {
            if !matches!(declared, TableConstraint::ForeignKey(_))
                && matches!(declared, TableConstraint::PrimaryKey(_)) == primary
            {
                constraint(builder, table, declared)?;
            }
        }
    }
    Ok(())
}

/// Refuses a virtual foreign-key source after target key binding succeeds.
pub(super) fn foreign_key(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    foreign_key: &ForeignKeyConstraint,
) -> Result<(), Error> {
    if *builder.dialect() == SqlparserDialect::PostgreSql
        && foreign_key.columns.iter().any(|name| virtual_name(builder, table, name))
    {
        return Err(Error::VirtualGeneratedForeignKey);
    }
    Ok(())
}
