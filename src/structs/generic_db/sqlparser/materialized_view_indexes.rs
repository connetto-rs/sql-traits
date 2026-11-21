//! Recording the indexes a script builds on materialized views, and keeping
//! each on the view it is on.
//!
//! An index on a materialized view is checked the way one on a table is: its
//! name joins the relation pool of the view's schema, a qualifier on it may
//! only name that schema, and its plain keys and `INCLUDE` entries must be
//! columns the view exposes. It follows the view's renames and column renames,
//! and goes when the view goes.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{CreateIndex, Ident, ObjectName, ObjectNamePart};

use super::{
    NamedColumn, ParserDB, ParserDBBuilder, SchemaQualifier, StoredTable, column_references,
    generated_names, refuse_index_in_another_schema, relation_name_holder, rename_column_idents,
    rename_column_in_expressions, schema_qualifiers_match, stored_expressions, stored_ident,
    stored_view_qualifier, views,
};
use crate::{
    errors::{Error, ObjectKind},
    structs::{
        IdentifierCase, MaterializedView, MaterializedViewIndex, RelationIdentity,
        metadata::IndexMetadata,
    },
    traits::ViewLike,
    utils::object_name::{Namespaces, object_name_last_part, qualifier_of},
};

/// The name a stored materialized view is reached by exactly: its schema when
/// it has one, then its own name.
fn view_reference(view: &MaterializedView) -> ObjectName {
    let name = stored_ident(view.view_name(), view.view_name_is_quoted());
    ObjectName::from(stored_view_qualifier(view).into_iter().chain([name]).collect::<Vec<_>>())
}

/// Records `create_index` on the materialized view stored at `position`.
///
/// A name some relation in the view's schema already holds is skipped under
/// `IF NOT EXISTS` and refused otherwise.
///
/// # Errors
///
/// Returns [`Error::IndexInAnotherSchema`] when the index name is qualified
/// by another schema, [`crate::errors::LookupError::ColumnNotFound`] when a
/// plain key or an `INCLUDE` entry is no column of the view,
/// [`Error::InvalidIndex`] when the index has no key,
/// [`Error::RelationNameAlreadyTaken`] when its name is taken, and the error
/// refusing a subquery in its expressions.
pub(super) fn create(
    builder: ParserDBBuilder,
    mut create_index: CreateIndex,
    position: usize,
) -> Result<ParserDBBuilder, Error> {
    let (view, _, identity) = &builder.materialized_views()[position];
    let (view, identity) = (Arc::clone(view), *identity);
    create_index.table_name = view_reference(&view);
    if create_index.name.is_none() {
        create_index.name =
            generated_names::create_index_name_on_view(&builder, view.as_ref(), &create_index)
                .map(|name| ObjectName::from(alloc::vec![name]));
    }
    let schema = view.view_schema().map(|schema| (schema, view.view_schema_is_quoted()));
    refuse_index_in_another_schema(
        &create_index,
        view.view_name(),
        schema,
        builder.namespaces(),
        builder.identifier_case(),
    )?;
    views::require_indexed_columns(
        &builder,
        position,
        &create_index.columns,
        &create_index.include,
    )?;
    super::btree_opclasses::index(&create_index, None, &builder)?;
    let Some(expression) = ParserDB::create_index_expression(&create_index.columns) else {
        return Err(Error::InvalidIndex {
            index_name: create_index
                .name
                .as_ref()
                .map_or_else(|| "<unnamed>".into(), ToString::to_string),
            reason: "index has no columns".into(),
        });
    };
    stored_expressions::refuse_in_index(&builder, &create_index)?;
    column_references::refuse_in_index(&builder, &create_index)?;

    if let Some(ObjectNamePart::Identifier(index_name)) =
        create_index.name.as_ref().and_then(|name| name.0.last())
        && let Some(conflicting_kind) = relation_name_holder(&builder, index_name, schema)
    {
        if create_index.if_not_exists {
            return Ok(builder);
        }
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::Index,
            conflicting_kind,
            object_name: index_name.value.clone(),
        });
    }

    let metadata = IndexMetadata::new(expression).with_calls(builder.calls_in(&create_index));
    Ok(builder.add_materialized_view_index(
        Arc::new(MaterializedViewIndex::new(create_index, identity)),
        metadata,
    ))
}

/// Whether the index on a materialized view `index` holds the name
/// `index_name` in `schema`, the schema of the view it is on.
pub(super) fn holds_name(
    index: &MaterializedViewIndex,
    (index_name, index_quoted): (&str, bool),
    schema: SchemaQualifier<'_>,
    namespaces: Namespaces,
    case: IdentifierCase,
) -> bool {
    index.index().name.as_ref().and_then(object_name_last_part).is_some_and(|(stored, quoted)| {
        case.identifiers_match(stored, quoted, index_name, index_quoted)
    }) && schema_qualifiers_match(
        namespaces,
        qualifier_of(&index.index().table_name).named(),
        schema,
        case,
    )
}

/// Drops every index on a materialized view the stores no longer hold.
pub(super) fn drop_orphaned(builder: &mut ParserDBBuilder) {
    let held: Vec<RelationIdentity> =
        builder.materialized_views().iter().map(|(_, _, identity)| *identity).collect();
    builder.materialized_view_indices_mut().retain(|(index, _)| held.contains(&index.view()));
}

/// Points every index on the materialized view stored at `position` at the
/// name the view is stored under now.
pub(super) fn follow_view_rename(builder: &mut ParserDBBuilder, position: usize) {
    let (view, _, identity) = &builder.materialized_views()[position];
    let (reference, identity) = (view_reference(view), *identity);
    for (index, _) in builder.materialized_view_indices_mut() {
        if index.view() == identity {
            Arc::make_mut(index).index_mut().table_name = reference.clone();
        }
    }
}

/// Renames the column `from` of the materialized view stored at `position`
/// to `to` in the keys, predicates and `INCLUDE` lists of the indexes on it.
pub(super) fn follow_column_rename(
    builder: &mut ParserDBBuilder,
    position: usize,
    from: &Ident,
    to: &Ident,
) {
    let (view, _, identity) = &builder.materialized_views()[position];
    let (view, identity) = (StoredTable::of_view(view.as_ref()), *identity);
    let from = NamedColumn::of(from, builder.identifier_case());
    for (index, _) in builder.materialized_view_indices_mut() {
        if index.view() == identity {
            let index = Arc::make_mut(index).index_mut();
            rename_column_in_expressions(index, &view, &[], &from, to);
            rename_column_idents(&mut index.include, &from, to);
        }
    }
}
