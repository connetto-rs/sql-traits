//! Relation schema relocation and its namespace checks.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use sqlparser::ast::{Ident, ObjectName};

use super::{
    ParserDB, ParserDBBuilder, StoredTable, TypeArrays, domains, object_name_last_identifier,
    postgres_catalog::PostgresCatalog, qualifier_of, relation_name_holder, schema_is_declared,
    schema_qualifiers_match, sequences, stored_ident, views,
};
use crate::{
    errors::{Error, ObjectKind},
    impls::SqlparserDialect,
    structs::{TargetName, metadata::TableAttribute, sequence::StoredSequenceOwner},
    traits::{IndexLike, SequenceLike, ViewLike},
    utils::object_name::target_name_from_object_name,
};

/// Refuses schema relocation outside PostgreSQL.
pub(super) fn require_dialect(builder: &ParserDBBuilder) -> Result<(), Error> {
    if *builder.dialect() == SqlparserDialect::PostgreSql {
        return Ok(());
    }
    Err(sqlparser::parser::ParserError::ParserError(if builder.dialect().is_sqlite() {
        "near \"SET\": syntax error".to_string()
    } else {
        "syntax error at or near \"SET\"".to_string()
    })
    .into())
}

/// Builds the destination namespace collision diagnostic.
fn collision(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: &Ident,
    type_collision: bool,
) -> Error {
    Error::SetSchemaNameCollision {
        object_name: builder.dialect().stored_name(name),
        schema_name: builder.dialect().stored_name(schema),
        type_collision,
    }
}

/// Checks one relation name in the destination namespace.
fn require_free_relation(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: &Ident,
) -> Result<(), Error> {
    if relation_name_holder(
        builder,
        name,
        Some((&schema.value, schema.quote_style.is_some())),
        None,
    )
    .is_some()
    {
        return Err(collision(builder, name, schema, false));
    }
    Ok(())
}

/// Returns the relation owning a sequence column.
fn sequence_owner(builder: &ParserDBBuilder, owner: StoredSequenceOwner) -> Option<TargetName<'_>> {
    match owner {
        StoredSequenceOwner::Column(identity) => {
            let (column, _, _) = builder.columns().iter().find(|(_, _, held)| *held == identity)?;
            target_name_from_object_name(&TableAttribute::table(column.as_ref()).name)
        }
        StoredSequenceOwner::ViewColumn { view, .. } => {
            let (view, _, _) = builder.views().iter().find(|(_, _, held)| *held == view)?;
            Some(
                TargetName::new(view.view_name(), view.view_name_is_quoted()).with_schema(
                    view.view_schema().unwrap_or("public"),
                    view.view_schema_is_quoted(),
                ),
            )
        }
    }
}

/// Moves one relation with its dependent namespace objects and bound
/// references.
pub(super) fn move_relation(
    mut builder: ParserDBBuilder,
    name: &ObjectName,
    schema_name: &ObjectName,
    if_exists: bool,
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<ParserDBBuilder, Error> {
    let Some((kind, source)) = builder.relation_reached(name) else {
        if if_exists {
            return Ok(builder);
        }
        return Err(Error::RelationNotFound {
            object_kind: ObjectKind::Table,
            object_name: builder.dialect().stored_object_name(name),
            drop_statement: false,
        });
    };
    if matches!(kind, ObjectKind::Index | ObjectKind::UniqueIndex | ObjectKind::Type) {
        return Err(Error::SetSchemaWrongKind {
            relation_name: builder.dialect().stored_object_name_last(name),
            object_kind: if kind == ObjectKind::UniqueIndex { ObjectKind::Index } else { kind },
        });
    }
    let schema = object_name_last_identifier(schema_name)
        .ok_or(Error::UnnamedObject { object_kind: ObjectKind::Schema })?;
    let same_schema = check_namespace(&builder, source, schema)?;
    let destination = (schema.value.as_str(), schema.quote_style.is_some());
    let ident =
        object_name_last_identifier(name).ok_or(Error::UnnamedObject { object_kind: kind })?;
    let original = ObjectName::from(alloc::vec![stored_ident(source.0, source.1), ident.clone()]);
    let moved = ObjectName::from(alloc::vec![schema.clone(), ident.clone()]);
    if kind == ObjectKind::Sequence {
        return move_sequence_checked(builder, &original, schema, same_schema);
    }
    if same_schema {
        return Ok(builder);
    }
    require_free_relation(&builder, ident, schema)?;
    if domains::rename_type_holder(&builder, ident, Some(destination)).is_some() {
        return Err(collision(&builder, ident, schema, true));
    }
    move_row_type(&builder, &original, &moved, schema, arrays, catalog)?;
    let owned: Vec<usize> = builder
        .sequences()
        .iter()
        .enumerate()
        .filter_map(|(position, (sequence, ()))| {
            let owner = sequence_owner(&builder, sequence.stored_owner()?)?;
            super::schema_qualifiers_match(
                builder.namespaces(),
                owner.schema().map(|schema| (schema, owner.schema_is_quoted())),
                qualifier_of(&original).named(),
                builder.identifier_case(),
            )
            .then_some(())?;
            super::identifiers_match(
                owner.name(),
                owner.name_is_quoted(),
                &ident.value,
                ident.quote_style.is_some(),
            )
            .then_some(position)
        })
        .collect();
    let view_position = check_indexes(&builder, &original, kind, schema)?;
    for position in &owned {
        let sequence = &builder.sequences()[*position].0;
        require_free_relation(
            &builder,
            &stored_ident(sequence.sequence_name(), sequence.sequence_name_is_quoted()),
            schema,
        )?;
    }
    if let Some(position) = view_position {
        builder = views::retarget_view(builder, kind, position, Some(destination), ident)?;
        Arc::make_mut(catalog).rename_type_identity(&original, &moved);
    } else {
        let table = builder.resolve_table_object_name(&original)?.ok_or_else(|| {
            ObjectKind::Table.not_in_database(&builder.dialect().stored_object_name(&original))
        })?;
        let renamed = super::StoredTable::of(table);
        let key = super::stored_table_key(builder.namespaces(), table, builder.identifier_case());
        let target = super::RenameTarget::new(moved, &table.name)?;
        builder = ParserDB::retarget_table(builder, &renamed, key, &original, &target)?;
        Arc::make_mut(catalog).rename_type_identity(&original, &target.name);
    }
    for position in owned {
        builder = sequences::move_sequence(builder, position, destination)?;
    }
    Ok(builder)
}

/// Checks schema existence and restrictions before deciding whether a move is a
/// no-op.
fn check_namespace(
    builder: &ParserDBBuilder,
    source: (&str, bool),
    schema: &Ident,
) -> Result<bool, Error> {
    let destination = (schema.value.as_str(), schema.quote_style.is_some());
    let toast = super::identifiers_match(destination.0, destination.1, "pg_toast", false);
    if !toast
        && !super::identifiers_match(destination.0, destination.1, "pg_catalog", false)
        && !super::identifiers_match(destination.0, destination.1, "information_schema", false)
        && !schema_is_declared(builder, destination.0, destination.1)
    {
        return Err(Error::SchemaNotFound { schema_name: builder.dialect().stored_name(schema) });
    }
    if builder.namespaces().is_temporary(source, builder.identifier_case())
        || builder.namespaces().is_temporary(destination, builder.identifier_case())
    {
        return Err(Error::SetSchemaRestrictedNamespace { toast: false });
    }
    if toast || super::identifiers_match(source.0, source.1, "pg_toast", false) {
        return Err(Error::SetSchemaRestrictedNamespace { toast: true });
    }
    Ok(schema_qualifiers_match(
        builder.namespaces(),
        Some(source),
        Some(destination),
        builder.identifier_case(),
    ))
}

/// Moves a directly named sequence after checking ownership and destination
/// collisions.
fn move_sequence_checked(
    builder: ParserDBBuilder,
    original: &ObjectName,
    schema: &Ident,
    same_schema: bool,
) -> Result<ParserDBBuilder, Error> {
    let sequence = sequences::resolve_sequence(&builder, original)?.ok_or_else(|| {
        Error::RelationNotFound {
            object_kind: ObjectKind::Sequence,
            object_name: original.to_string(),
            drop_statement: false,
        }
    })?;
    if let Some(owner) = sequence.stored_owner() {
        return Err(Error::SetSchemaOwnedSequence {
            sequence_name: builder
                .dialect()
                .stored_spelling(sequence.sequence_name(), sequence.sequence_name_is_quoted()),
            table_name: sequence_owner(&builder, owner).map_or_else(String::new, |name| {
                builder.dialect().stored_spelling(name.name(), name.name_is_quoted())
            }),
        });
    }
    if same_schema {
        return Ok(builder);
    }
    let position = builder
        .sequences()
        .iter()
        .position(|(held, ())| core::ptr::eq(held.as_ref(), sequence))
        .ok_or_else(|| {
            Error::RelationNotFound {
                object_kind: ObjectKind::Sequence,
                object_name: original.to_string(),
                drop_statement: false,
            }
        })?;
    require_free_relation(
        &builder,
        &stored_ident(sequence.sequence_name(), sequence.sequence_name_is_quoted()),
        schema,
    )?;
    sequences::move_sequence(builder, position, (&schema.value, schema.quote_style.is_some()))
}

/// Moves generated array identities before any index or sequence collision is
/// checked.
fn move_row_type(
    builder: &ParserDBBuilder,
    original: &ObjectName,
    moved: &ObjectName,
    schema: &Ident,
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<(), Error> {
    let array_move =
        Arc::make_mut(arrays).move_schema(builder, original, moved).map_err(|error| {
            match error {
                Error::RelationNameAlreadyTaken { object_name, .. } => {
                    Error::SetSchemaNameCollision {
                        object_name,
                        schema_name: builder.dialect().stored_name(schema),
                        type_collision: true,
                    }
                }
                other => other,
            }
        })?;
    if let Some((previous, current)) = array_move {
        Arc::make_mut(catalog).rename_type_identity(&previous, current);
    }
    Ok(())
}

/// Checks attached index names in creation order and resolves a view's
/// position.
fn check_indexes(
    builder: &ParserDBBuilder,
    original: &ObjectName,
    kind: ObjectKind,
    schema: &Ident,
) -> Result<Option<usize>, Error> {
    if matches!(kind, ObjectKind::Table | ObjectKind::ForeignTable) {
        let table = builder.resolve_table_object_name(original)?.ok_or_else(|| {
            Error::RelationNotFound {
                object_kind: kind,
                object_name: original.to_string(),
                drop_statement: false,
            }
        })?;
        let stored = StoredTable::of(table);
        let mut indexes = Vec::new();
        if let Some((_, metadata, _)) =
            builder.tables().iter().find(|(held, _, _)| core::ptr::eq(held.as_ref(), table))
        {
            for recorded in
                metadata.recorded_constraint_names().iter().filter(|name| name.names_index)
            {
                let identity = metadata
                    .key_creations()
                    .iter()
                    .find(|creation| creation.slot == recorded.slot)
                    .map(|creation| creation.identity);
                indexes.push((identity, recorded.name.as_str(), true));
            }
        }
        for (index, metadata) in builder.indices() {
            if stored.matches(index.table(), builder.identifier_case())
                && let Some(name) = index.name()
            {
                indexes.push((metadata.key_identity(), name, index.name_is_quoted()));
            }
        }
        indexes.sort_by_key(|(identity, _, _)| *identity);
        for (_, name, quoted) in indexes {
            require_free_relation(builder, &stored_ident(name, quoted), schema)?;
        }
        return Ok(None);
    }
    let position = if kind == ObjectKind::MaterializedView {
        views::materialized_view_position(builder, original)
    } else {
        views::plain_view_position(builder, original)
    }
    .ok_or_else(|| {
        Error::RelationNotFound {
            object_kind: kind,
            object_name: original.to_string(),
            drop_statement: false,
        }
    })?;
    if kind == ObjectKind::MaterializedView {
        let identity = builder.materialized_views()[position].2;
        for (index, _) in builder.materialized_view_indices() {
            if index.view() == identity
                && let Some(name) = index.name()
            {
                require_free_relation(
                    builder,
                    &stored_ident(name, index.name_is_quoted()),
                    schema,
                )?;
            }
        }
    }
    Ok(Some(position))
}
