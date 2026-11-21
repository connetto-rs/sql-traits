//! PostgreSQL column and index transformation before relation creation.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{ColumnOption, CreateTable, DataType, Ident, Spanned, TableConstraint};

use super::{
    CreatedCollationMetadata, NotNullRequirement, ParserDB, ParserDBBuilder, PostgresCatalog,
    PreservedColumnMetadata, RelationNamePhase, StoredTable, TableDeclaration, TypeArrays,
    case_idents_match, column_collation_name, column_metadata_for_collations, domains,
    index_access_methods, index_opclasses, inheritance, partition_keys,
    refuse_no_inherit_constraints_on_partitioned, sequences, stored_expressions,
    validate_relation_names,
};
use crate::{
    errors::{Error, LookupError, ObjectKind},
    structs::IdentifierCase,
    traits::TableLike,
    utils::{
        columns_in_expression::validate_columns_in_expression,
        object_name::target_name_of_object_name,
    },
};

/// Binds column types and collations in declaration order.
pub(super) fn columns(
    builder: &ParserDBBuilder,
    table: &mut CreateTable,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
    arrays: &TypeArrays,
) -> Result<Vec<PreservedColumnMetadata>, Error> {
    let search_path: Vec<_> =
        builder.search_path().map(|(schema, quoted)| (schema.to_string(), quoted)).collect();
    let mut metadata = Vec::with_capacity(table.columns.len());
    for constraint in &table.constraints {
        if let TableConstraint::ForeignKey(foreign_key) = constraint {
            ParserDB::validate_foreign_key_deferral(builder, &table.name, foreign_key)?;
        }
    }
    for column in &mut table.columns {
        domains::bind_type(builder, &mut column.data_type, arrays)?;
        let column_metadata =
            if table.partition_of.is_some() && matches!(column.data_type, DataType::Unspecified) {
                None
            } else {
                Some(column_metadata_for_collations(
                    column,
                    collations,
                    catalog,
                    &search_path,
                    &[],
                    true,
                )?)
            };
        for option in &column.options {
            if let ColumnOption::ForeignKey(foreign_key) = &option.option {
                ParserDB::validate_foreign_key_deferral(builder, &table.name, foreign_key)?;
            }
        }
        NotNullRequirement::new(column, &[], builder.identifier_case(), *builder.dialect())
            .validate(&table.name, *builder.dialect())?;
        if let Some(column_metadata) = column_metadata {
            metadata.push(PreservedColumnMetadata {
                column_name: column.name.value.clone(),
                column_name_quoted: column.name.quote_style.is_some(),
                collation: column_collation_name(column, true)?.cloned(),
                metadata: column_metadata,
                identity: None,
            });
        }
    }
    Ok(metadata)
}

/// Checks index constraints in their written order across both AST lists.
pub(super) fn keys(builder: &ParserDBBuilder, table: &CreateTable) -> Result<bool, Error> {
    let mut constraints = table.constraints.iter().peekable();
    let mut has_primary_key = false;
    for column in &table.columns {
        while constraints
            .peek()
            .is_some_and(|constraint| constraint.span().start < column.name.span.start)
        {
            if let Some(constraint) = constraints.next() {
                key(builder, table, constraint, &mut has_primary_key)?;
            }
        }
        for option in &column.options {
            match &option.option {
                ColumnOption::PrimaryKey(primary) => {
                    primary_key(builder, table, &mut has_primary_key)?;
                    for name in &primary.include {
                        key_column(builder, table, name)?;
                    }
                }
                ColumnOption::Unique(unique) => {
                    for name in &unique.include {
                        key_column(builder, table, name)?;
                    }
                }
                _ => {}
            }
        }
    }
    for constraint in constraints {
        key(builder, table, constraint, &mut has_primary_key)?;
    }
    Ok(has_primary_key)
}

/// Refuses a second primary key before resolving its columns.
fn primary_key(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    held: &mut bool,
) -> Result<(), Error> {
    if *held {
        return Err(Error::MultiplePrimaryKeys {
            table_name: builder
                .dialect()
                .stored_spelling(table.table_name(), table.table_name_is_quoted()),
        });
    }
    *held = true;
    Ok(())
}

/// Resolves one table-level key after checking primary-key multiplicity.
fn key(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    constraint: &TableConstraint,
    has_primary_key: &mut bool,
) -> Result<(), Error> {
    if matches!(
        constraint,
        TableConstraint::PrimaryKey(_) | TableConstraint::PrimaryKeyUsingIndex(_)
    ) {
        primary_key(builder, table, has_primary_key)?;
    }
    ParserDB::validate_key_columns(constraint, table, builder, true)
}

/// Binds defaults and generated expressions in column order.
pub(super) fn expressions(
    builder: &ParserDBBuilder,
    table: &mut CreateTable,
    written_defaults: &[Ident],
) -> Result<(), Error> {
    let table_name = StoredTable::of(table);
    for column_index in 0..table.columns.len() {
        for option_index in 0..table.columns[column_index].options.len() {
            let option = &table.columns[column_index].options[option_index].option;
            stored_expressions::refuse_column_value(option)?;
            if let ColumnOption::Generated { generation_expr: Some(expression), .. } = option {
                validate_columns_in_expression(
                    expression,
                    builder.catalog_name(),
                    &target_name_of_object_name(&table.name),
                    table.columns.iter().map(|column| &column.name),
                    builder.identifier_case(),
                    *builder.dialect(),
                )?;
            }
            let column = &mut table.columns[column_index];
            if written_defaults
                .iter()
                .any(|name| case_idents_match(name, &column.name, builder.identifier_case()))
                && let ColumnOption::Default(expression) = &mut column.options[option_index].option
            {
                sequences::bind_default(builder, &table_name, &column.name, expression)?;
            }
        }
    }
    Ok(())
}

/// Resolves a key column against local columns and inherited parent columns.
pub(super) fn key_column(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    name: &Ident,
) -> Result<(), Error> {
    let declares = |table: &CreateTable| {
        table
            .columns
            .iter()
            .any(|column| case_idents_match(&column.name, name, builder.identifier_case()))
    };
    if declares(table) {
        return Ok(());
    }
    for parent_name in inheritance::parent_names(table) {
        let parent = inheritance::resolve_unbound_parent(builder, parent_name, &table.name)?;
        if declares(parent) {
            return Ok(());
        }
    }
    Err(LookupError::ColumnNotFound {
        table_name: builder.dialect().stored_object_name_last(&table.name),
        column_name: builder.dialect().stored_name(name),
    }
    .into())
}

/// Validates table constraints and indexes after expression binding.
pub(super) fn finish(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    declaration: &TableDeclaration,
) -> Result<(), Error> {
    for requirement in &declaration.not_null {
        requirement.validate(&table.name, *builder.dialect())?;
    }
    refuse_no_inherit_constraints_on_partitioned(*builder.dialect(), table)?;
    partition_keys::validate_partitioned_table(
        table,
        builder.identifier_case(),
        *builder.dialect(),
    )?;
    if declaration.primary_key_declared
        && let Some(parent_name) = &table.partition_of
        && let Some(parent) = builder.resolve_bound_table_name(parent_name)?
    {
        super::refuse_multiple_primary_keys(builder, table, Some(parent))?;
    }
    for constraint in &table.constraints {
        index_access_methods::refuse_in_constraint(builder, constraint)?;
    }
    index_opclasses::table(table, builder)?;
    validate_relation_names(builder, table, ObjectKind::Table, RelationNamePhase::Indexes)
}

/// Retains only local requirements that table-level nullability declarations
/// affect.
pub(super) fn not_null_requirements(
    table: &CreateTable,
    case: IdentifierCase,
    dialect: crate::impls::SqlparserDialect,
) -> Vec<NotNullRequirement<'static>> {
    table
        .columns
        .iter()
        .filter(|column| {
            table.constraints.iter().any(|constraint| {
                matches!(constraint, TableConstraint::NotNull(not_null)
                if case_idents_match(&not_null.column, &column.name, case))
            })
        })
        .map(|column| {
            NotNullRequirement::new(column, &table.constraints, case, dialect).into_owned()
        })
        .collect()
}
