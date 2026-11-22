//! Typed tables, `CREATE TABLE name OF type`, whose columns are the
//! attributes of a composite type.

use alloc::{string::ToString, vec::Vec};

use sqlparser::{
    ast::{
        AlterColumnOperation, AlterTableOperation, ColumnDef, ColumnOption, ColumnOptionDef,
        CreateTable, Ident,
    },
    tokenizer::Span,
};

use super::{ParserDBBuilder, TypeArrays, case_idents_match, user_defined_types};
use crate::{
    errors::{ColumnLookupSite, Error, LookupError, TypedTableRefusal},
    impls::SqlparserDialect,
    utils::is_identity,
};

/// Refuses `OF` outside PostgreSQL, the only dialect with typed tables.
///
/// # Errors
///
/// Returns the dialect's syntax error for a table written with `OF`.
pub(super) fn require_dialect(builder: &ParserDBBuilder, table: &CreateTable) -> Result<(), Error> {
    if table.of_type.is_none() || *builder.dialect() == SqlparserDialect::PostgreSql {
        return Ok(());
    }
    Err(sqlparser::parser::ParserError::ParserError(if builder.dialect().is_sqlite() {
        "near \"OF\": syntax error".to_string()
    } else {
        "syntax error at or near \"OF\"".to_string()
    })
    .into())
}

/// Replaces the written entries of a typed table with one column per
/// attribute of its type, in attribute order, each carrying the attribute's
/// type and collation and the options its written entry adds.
///
/// # Errors
///
/// Refuses what [`user_defined_types::typed_table_composite`] refuses, then
/// an identity or generated entry, then an entry written twice, then an
/// entry naming no attribute, in PostgreSQL's order.
pub(super) fn expand(
    builder: &ParserDBBuilder,
    table: &mut CreateTable,
    arrays: &TypeArrays,
) -> Result<(), Error> {
    let Some(of_type) = table.of_type.as_mut() else {
        return Ok(());
    };
    let composite = user_defined_types::typed_table_composite(builder, of_type, arrays)?;
    let table_name = builder.dialect().stored_object_name_last(&table.name);
    let case = builder.identifier_case();
    for option in table.columns.iter().flat_map(|column| &column.options) {
        let refusal = match &option.option {
            option if is_identity(option) => TypedTableRefusal::IdentityColumn,
            ColumnOption::Generated { generation_expr: Some(_), .. } => {
                TypedTableRefusal::GeneratedColumn
            }
            _ => continue,
        };
        return Err(Error::TypedTable { table_name, refusal });
    }
    for (index, column) in table.columns.iter().enumerate() {
        if table.columns[..index]
            .iter()
            .any(|earlier| case_idents_match(&earlier.name, &column.name, case))
        {
            return Err(Error::ColumnAlreadyExists {
                table_name,
                column_name: builder.dialect().stored_name(&column.name),
                declaration: true,
            });
        }
    }
    let attributes = composite.composite_attributes().unwrap_or_default();
    if let Some(unknown) = table.columns.iter().find(|column| {
        !attributes.iter().any(|attribute| case_idents_match(&attribute.name, &column.name, case))
    }) {
        return Err(LookupError::ColumnNotFound {
            table_name,
            column_name: builder.dialect().stored_name(&unknown.name),
            site: ColumnLookupSite::Column,
        }
        .into());
    }
    let written = core::mem::take(&mut table.columns);
    table.columns = attributes
        .iter()
        .map(|attribute| {
            let entry = written
                .iter()
                .find(|column| case_idents_match(&attribute.name, &column.name, case));
            let collation = attribute.collation.clone().map(|collation| {
                ColumnOptionDef { name: None, option: ColumnOption::Collation(collation) }
            });
            // PostgreSQL keeps the attribute's collation and ignores a
            // `COLLATE` written on the entry.
            let options: Vec<ColumnOptionDef> = collation
                .into_iter()
                .chain(entry.into_iter().flat_map(|column| {
                    column
                        .options
                        .iter()
                        .filter(|option| !matches!(option.option, ColumnOption::Collation(_)))
                        .cloned()
                }))
                .collect();
            ColumnDef {
                name: Ident {
                    span: entry.map_or_else(Span::empty, |column| column.name.span),
                    ..attribute.name.clone()
                },
                data_type: attribute.data_type.clone(),
                options,
            }
        })
        .collect();
    Ok(())
}

/// Refuses an `ALTER TABLE` command that changes the column list or the
/// inheritance of a typed table, which follow its type alone. A column type
/// change names a column first, so one naming no column is left to the
/// lookup that refuses it.
///
/// # Errors
///
/// Returns [`Error::TypedTable`] for `ADD COLUMN`, `DROP COLUMN`, `RENAME
/// COLUMN`, `INHERIT` and a type change of a column `table` holds.
pub(super) fn refuse_alteration(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    operation: &AlterTableOperation,
) -> Result<(), Error> {
    if table.of_type.is_none() {
        return Ok(());
    }
    let refusal = match operation {
        AlterTableOperation::AddColumn { .. } => TypedTableRefusal::AddColumn,
        AlterTableOperation::DropColumn { .. } => TypedTableRefusal::DropColumn,
        AlterTableOperation::RenameColumn { .. } => TypedTableRefusal::RenameColumn,
        AlterTableOperation::Inherit { .. } => TypedTableRefusal::ChangeInheritance,
        AlterTableOperation::AlterColumn {
            column_name,
            op: AlterColumnOperation::SetDataType { .. },
        } if table.columns.iter().any(|column| {
            case_idents_match(&column.name, column_name, builder.identifier_case())
        }) =>
        {
            TypedTableRefusal::AlterColumnType
        }
        _ => return Ok(()),
    };
    Err(Error::TypedTable {
        table_name: builder.dialect().stored_object_name_last(&table.name),
        refusal,
    })
}
