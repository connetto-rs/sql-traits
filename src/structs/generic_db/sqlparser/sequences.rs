//! Recording and dropping sequences.
//!
//! Every rule here was measured against PostgreSQL 18.4 in Docker rather than
//! read from documentation.
//!
//! A sequence takes its name from the pool tables, views, materialized views
//! and indexes of its schema share, and lands where a table would, in the
//! schema it names, in the first schema on the search path the input creates,
//! or in `pg_temp` when declared `TEMP`. `IF NOT EXISTS` skips whatever
//! relation holds the name.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{ObjectName, SequenceOptions};

use super::{
    ParserDBBuilder, object_name_last_identifier, place_relation, relation_name_holder,
    require_named_in_catalog, views,
};
use crate::{
    errors::{Error, ObjectKind},
    structs::{IdentifierCase, Sequence},
    traits::SequenceLike,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{
            object_name_identifiers, qualifier_of, resolve_sequence_on_search_path_in_iter,
            stored_sequence_key, target_name_of_idents,
        },
    },
};

/// Records a `CREATE SEQUENCE`.
///
/// # Errors
///
/// Refuses a name another relation in the schema already holds unless the
/// statement wrote `IF NOT EXISTS`, a schema the input never creates, and a
/// temporary sequence outside the temporary schema.
pub(super) fn create_sequence(
    builder: ParserDBBuilder,
    temporary: bool,
    if_not_exists: bool,
    mut name: ObjectName,
    options: Vec<SequenceOptions>,
) -> Result<ParserDBBuilder, Error> {
    require_named_in_catalog(&mut name, ObjectKind::Sequence, builder.catalog_name())?;
    place_relation(&builder, &mut name, temporary, ObjectKind::Sequence)?;
    // The name is known to be static and non-empty here, so it ends in an
    // identifier.
    let name_ident = object_name_last_identifier(&name)
        .ok_or(Error::UnnamedObject { object_kind: ObjectKind::Sequence })?;
    let schema = qualifier_of(&name).named();

    if let Some(conflicting_kind) = relation_name_holder(&builder, name_ident, schema) {
        if if_not_exists {
            return Ok(builder);
        }
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::Sequence,
            conflicting_kind,
            object_name: name_ident.value.clone(),
        });
    }

    let sequence = Sequence::new(
        (name_ident.value.clone(), name_ident.quote_style.is_some()),
        schema.map(|(value, quoted)| (value.to_string(), quoted)),
        options,
    );
    Ok(builder.add_sequence(Arc::new(sequence), ()))
}

/// Resolves a written sequence name through the session's path, the first
/// schema holding the name under any relation kind ending the walk.
fn resolve_sequence<'builder>(
    builder: &'builder ParserDBBuilder,
    name: &ObjectName,
) -> Result<Option<&'builder Sequence>, Error> {
    let (schema_ident, name_ident) = object_name_identifiers(name)?;
    let case = builder.identifier_case();
    resolve_sequence_on_search_path_in_iter(
        builder.sequences().iter().map(|(sequence, ())| sequence.as_ref()),
        &target_name_of_idents(schema_ident, name_ident),
        || builder.search_path(),
        case,
        |key| builder.relation_key_is_held(key, case),
    )
    .map_err(Error::from)
}

/// Drops the sequences a `DROP SEQUENCE` names.
///
/// # Errors
///
/// Returns [`Error::RelationKindMismatch`] for a name another relation kind
/// holds, [`Error::RelationNotFound`] for a name nothing holds unless the
/// statement wrote `IF EXISTS`, and [`Error::RelationHasDependents`] when a
/// view reads the sequence and the statement wrote no `CASCADE`.
pub(super) fn drop_sequences(
    mut builder: ParserDBBuilder,
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
) -> Result<ParserDBBuilder, Error> {
    for name in names {
        if object_name_last_identifier(name).is_none() {
            return Err(Error::UnnamedObject { object_kind: ObjectKind::Sequence });
        }

        let doomed = resolve_sequence(&builder, name)?.and_then(|found| {
            builder
                .sequences()
                .iter()
                .map(|(stored, ())| stored)
                .find(|stored| core::ptr::eq(stored.as_ref(), found))
                .cloned()
        });
        if let Some(doomed) = doomed {
            let key = stored_sequence_key(doomed.as_ref(), IdentifierCase::AsWritten);
            if cascade {
                views::remove_dependent_views(&mut builder, &key);
            } else {
                views::refuse_dependent_views(
                    &builder,
                    &key,
                    ObjectKind::Sequence,
                    &name.to_string(),
                )?;
            }
            forget(&mut builder, &[doomed]);
            continue;
        }

        match builder.relation_reached(name).map(|(kind, _)| kind) {
            Some(actual_kind) => {
                return Err(Error::RelationKindMismatch {
                    object_name: name.to_string(),
                    expected_kind: ObjectKind::Sequence,
                    actual_kind,
                });
            }
            None if !if_exists => {
                return Err(Error::RelationNotFound {
                    object_kind: ObjectKind::Sequence,
                    object_name: name.to_string(),
                });
            }
            None => {}
        }
    }
    Ok(builder)
}

/// Drops every sequence stored in a schema `DROP SCHEMA ... CASCADE` drops.
pub(super) fn drop_sequences_in(
    builder: &mut ParserDBBuilder,
    schema_name: &str,
    schema_quoted: bool,
) {
    let doomed: Vec<Arc<Sequence>> = builder
        .sequences()
        .iter()
        .map(|(sequence, ())| sequence)
        .filter(|sequence| {
            sequence.sequence_schema().is_some_and(|stored| {
                identifiers_match(
                    stored,
                    sequence.sequence_schema_is_quoted(),
                    schema_name,
                    schema_quoted,
                )
            })
        })
        .cloned()
        .collect();
    forget(builder, &doomed);
}

/// Removes the `doomed` sequences.
fn forget(builder: &mut ParserDBBuilder, doomed: &[Arc<Sequence>]) {
    builder
        .sequences_mut()
        .retain(|(sequence, ())| !doomed.iter().any(|dead| Arc::ptr_eq(dead, sequence)));
}
