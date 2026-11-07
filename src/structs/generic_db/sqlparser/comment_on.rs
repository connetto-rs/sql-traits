//! Documentation `COMMENT ON` sets on tables, foreign tables and their
//! columns.
//!
//! Measured against PostgreSQL 18.6. The relation is looked up first, then
//! its kind, then the column. `COMMENT ON TABLE` takes a table and nothing
//! else, `COMMENT ON FOREIGN TABLE` a foreign table and nothing else.
//! `COMMENT ON COLUMN` takes a column of a table, a foreign table, a view, a
//! materialized view or a composite type, and the model keeps the
//! documentation of the first two. A later comment replaces an earlier one,
//! including the comment block written above the declaration, and an empty
//! text removes it as `IS NULL` does. The other objects `COMMENT ON` names
//! keep no documentation in the model.

use alloc::string::{String, ToString};

use sqlparser::ast::{CommentObject, ObjectName, ObjectNamePart};

use super::{
    ParserDBBuilder, object_name_last_identifier, require_named_in_catalog, user_defined_types,
    views,
};
use crate::{
    errors::{Error, LookupError, ObjectKind},
    structs::TableAttribute,
    traits::ColumnLike,
    utils::object_name::require_static_object_name,
};

/// Applies a `COMMENT ON` statement to the documentation of the object it
/// names.
///
/// # Errors
///
/// Returns [`Error::RelationNotFound`] for a relation nothing holds unless the
/// statement wrote `IF EXISTS`, [`Error::RelationKindMismatch`] for a relation
/// of a kind the spelling does not take, [`Error::UnqualifiedCommentColumn`]
/// for a column named without its relation, [`LookupError::ColumnNotFound`]
/// for a column the relation lacks, and [`LookupError::InvalidObjectName`] for
/// a name built at run time or naming another catalog.
pub(super) fn comment_on(
    builder: ParserDBBuilder,
    object_type: CommentObject,
    name: ObjectName,
    comment: Option<String>,
    if_exists: bool,
) -> Result<ParserDBBuilder, Error> {
    let documentation = comment.filter(|text| !text.is_empty());
    match object_type {
        CommentObject::Table => {
            comment_on_table(builder, name, ObjectKind::Table, documentation, if_exists)
        }
        CommentObject::ForeignTable => {
            comment_on_table(builder, name, ObjectKind::ForeignTable, documentation, if_exists)
        }
        CommentObject::Column => comment_on_column(builder, name, documentation, if_exists),
        _ => Ok(builder),
    }
}

/// Documents the table or foreign table `name` reaches, `expected` being the
/// kind the statement's spelling takes.
fn comment_on_table(
    mut builder: ParserDBBuilder,
    mut name: ObjectName,
    expected: ObjectKind,
    documentation: Option<String>,
    if_exists: bool,
) -> Result<ParserDBBuilder, Error> {
    require_named_in_catalog(&mut name, expected, builder.catalog_name())?;
    let Some((kind, _)) = reached(&builder, &name, expected, if_exists)? else {
        return Ok(builder);
    };
    if kind != expected {
        return Err(kind_mismatch(&name, expected, kind));
    }
    let position = table_position(&builder, &name)?;
    builder.tables_mut()[position].1.set_documentation(documentation);
    Ok(builder)
}

/// Documents the column of a table or foreign table `name` reaches, and checks
/// that a view, materialized view or composite type has the column it names.
fn comment_on_column(
    mut builder: ParserDBBuilder,
    name: ObjectName,
    documentation: Option<String>,
    if_exists: bool,
) -> Result<ParserDBBuilder, Error> {
    require_static_object_name(&name)?;
    let mut relation = name;
    let column = match relation.0.pop() {
        Some(ObjectNamePart::Identifier(column)) if !relation.0.is_empty() => column,
        part => {
            return Err(Error::UnqualifiedCommentColumn {
                column_name: part.map(|part| part.to_string()).unwrap_or_default(),
            });
        }
    };
    require_named_in_catalog(&mut relation, ObjectKind::Table, builder.catalog_name())?;
    let Some((kind, schema)) = reached(&builder, &relation, ObjectKind::Table, if_exists)? else {
        return Ok(builder);
    };
    match kind {
        ObjectKind::Table | ObjectKind::ForeignTable => {}
        ObjectKind::View | ObjectKind::MaterializedView => {
            views::require_exposed_column(&builder, &relation, &column)?;
            return Ok(builder);
        }
        ObjectKind::Type => {
            if let Some(composite) = object_name_last_identifier(&relation) {
                user_defined_types::require_composite_attribute(
                    &builder, composite, schema, &column,
                )?;
            }
            return Ok(builder);
        }
        other => return Err(kind_mismatch(&relation, ObjectKind::Table, other)),
    }

    let table = &builder.tables()[table_position(&builder, &relation)?].0;
    let case = builder.identifier_case();
    let position = builder
        .columns()
        .iter()
        .position(|(candidate, _, _)| {
            core::ptr::eq(TableAttribute::table(candidate.as_ref()), table.as_ref())
                && case.identifiers_match(
                    candidate.column_name(),
                    candidate.column_name_is_quoted(),
                    &column.value,
                    column.quote_style.is_some(),
                )
        })
        .ok_or_else(|| {
            LookupError::ColumnNotFound {
                table_name: relation.to_string(),
                column_name: column.value,
            }
        })?;
    builder.columns_mut()[position].1.set_documentation(documentation);
    Ok(builder)
}

/// The kind of relation a name reaches and the schema holding it.
type Reached<'name> = (ObjectKind, (&'name str, bool));

/// The relation `name` reaches, or `None` for a relation nothing holds under
/// `IF EXISTS`.
fn reached<'name>(
    builder: &'name ParserDBBuilder,
    name: &'name ObjectName,
    expected: ObjectKind,
    if_exists: bool,
) -> Result<Option<Reached<'name>>, Error> {
    match builder.relation_reached(name) {
        Some(reached) => Ok(Some(reached)),
        None if if_exists => Ok(None),
        None => {
            Err(Error::RelationNotFound { object_kind: expected, object_name: name.to_string() })
        }
    }
}

/// The position among the stored tables of the table or foreign table `name`
/// reaches, once the relation pool has shown it holds one.
fn table_position(builder: &ParserDBBuilder, name: &ObjectName) -> Result<usize, Error> {
    builder
        .resolve_table_object_name(name)?
        .and_then(|table| {
            builder.tables().iter().position(|(stored, _, _)| core::ptr::eq(stored.as_ref(), table))
        })
        .ok_or_else(|| ObjectKind::Table.not_in_database(&name.to_string()).into())
}

fn kind_mismatch(name: &ObjectName, expected: ObjectKind, actual: ObjectKind) -> Error {
    Error::RelationKindMismatch {
        object_name: name.to_string(),
        expected_kind: expected,
        actual_kind: actual,
    }
}
