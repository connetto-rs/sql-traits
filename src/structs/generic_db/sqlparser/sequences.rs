//! Recording and dropping sequences.
//!
//! Every rule here was measured against PostgreSQL 18.4 in Docker rather than
//! read from documentation.
//!
//! A sequence takes its name from the pool tables, views, materialized views
//! and indexes of its schema share, and lands where a table would, in the
//! schema it names, in the first schema on the search path the input creates,
//! or in `pg_temp` when declared `TEMP`. `IF NOT EXISTS` skips whatever
//! relation holds the name, before `OWNED BY` is looked at.
//!
//! `OWNED BY` ties the sequence to a column of a table or of a plain view in
//! the sequence's own schema, and the sequence leaves when that column or its
//! relation does. The tie is kept by identity, so renaming either keeps it.

use alloc::{collections::BTreeSet, string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{Grant, GrantObjects, Ident, ObjectName, ObjectNamePart, SequenceOptions};

use super::{
    ParserDBBuilder, SchemaQualifier, object_name_last_identifier,
    object_name_matches_resolved_identity, place_relation, relation_name_holder,
    require_named_in_catalog, schema_qualifiers_match, views,
};
use crate::{
    errors::{Error, ObjectKind},
    structs::{IdentifierCase, Sequence, StoredSequenceOwner, TableAttribute},
    traits::{ColumnLike, SequenceLike},
    utils::{
        identifier_resolution::identifiers_match,
        last_str,
        object_name::{
            object_name_identifiers, qualifier_of, require_static_object_name,
            resolve_sequence_on_search_path_in_iter, stored_sequence_key, target_name_of_idents,
        },
    },
};

/// Records a `CREATE SEQUENCE`.
///
/// # Errors
///
/// Refuses a name another relation in the schema already holds unless the
/// statement wrote `IF NOT EXISTS`, a schema the input never creates, a
/// temporary sequence outside the temporary schema, and every `OWNED BY` the
/// errors of [`resolve_owner`] name.
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

    let mut owner = None;
    let mut kept = Vec::with_capacity(options.len());
    for option in options {
        match option {
            SequenceOptions::OwnedBy(Some(owner_name)) => {
                owner = Some(resolve_owner(&builder, &owner_name, name_ident, schema)?);
            }
            SequenceOptions::OwnedBy(None) => {}
            other => kept.push(other),
        }
    }

    let sequence = Sequence::new(
        (name_ident.value.clone(), name_ident.quote_style.is_some()),
        schema.map(|(value, quoted)| (value.to_string(), quoted)),
        kept,
        owner,
    );
    Ok(builder.add_sequence(Arc::new(sequence), ()))
}

/// Resolves the column an `OWNED BY` names, through the search path, into the
/// identity that keeps owning the sequence.
///
/// PostgreSQL looks the relation up first, then checks its kind, then that it
/// shares the sequence's schema, and only then looks for the column, which is
/// the order the refusals here come in.
///
/// # Errors
///
/// Returns [`Error::InvalidSequenceOwner`] for a name carrying no relation
/// before the column, [`Error::RelationNotFoundForSequenceOwner`] for a
/// relation nothing holds, [`Error::SequenceOwnerKindUnsupported`] for a
/// relation that is neither a table nor a plain view,
/// [`Error::SequenceOwnerInAnotherSchema`] for one outside the sequence's
/// schema, [`Error::ColumnNotFoundForSequenceOwner`] for a column the relation
/// lacks, and [`Error::UnsupportedSequenceOwner`] for a view whose columns
/// cannot be listed.
fn resolve_owner(
    builder: &ParserDBBuilder,
    owner: &ObjectName,
    sequence: &Ident,
    sequence_schema: SchemaQualifier<'_>,
) -> Result<StoredSequenceOwner, Error> {
    require_static_object_name(owner)?;
    let sequence_name = || sequence.value.clone();
    // A static name ends in an identifier, so this refuses only a name with
    // no relation before the column.
    let Some((ObjectNamePart::Identifier(column), relation_parts)) =
        owner.0.split_last().filter(|(_, relation_parts)| !relation_parts.is_empty())
    else {
        return Err(Error::InvalidSequenceOwner {
            sequence_name: sequence_name(),
            owner: owner.to_string(),
        });
    };
    let mut relation = ObjectName(relation_parts.to_vec());
    require_named_in_catalog(&mut relation, ObjectKind::Table, builder.catalog_name())?;
    let relation_name = || last_str(&relation).to_string();

    let Some((kind, relation_schema)) = builder.relation_reached(&relation) else {
        return Err(Error::RelationNotFoundForSequenceOwner {
            relation_name: relation_name(),
            sequence_name: sequence_name(),
        });
    };
    if !matches!(kind, ObjectKind::Table | ObjectKind::View) {
        return Err(Error::SequenceOwnerKindUnsupported {
            sequence_name: sequence_name(),
            relation_name: relation_name(),
            relation_kind: kind,
        });
    }
    let case = builder.identifier_case();
    if !schema_qualifiers_match(Some(relation_schema), sequence_schema, case) {
        return Err(Error::SequenceOwnerInAnotherSchema {
            sequence_name: sequence_name(),
            relation_name: relation_name(),
        });
    }
    let column_not_found = || {
        Error::ColumnNotFoundForSequenceOwner {
            column_name: column.value.clone(),
            relation_name: relation_name(),
            sequence_name: sequence_name(),
        }
    };

    if kind == ObjectKind::Table {
        return builder
            .resolve_table_object_name(&relation)?
            .and_then(|table| {
                builder.columns().iter().find(|(candidate, _, _)| {
                    core::ptr::eq(TableAttribute::table(candidate.as_ref()), table)
                        && case.identifiers_match(
                            candidate.column_name(),
                            candidate.column_name_is_quoted(),
                            &column.value,
                            column.quote_style.is_some(),
                        )
                })
            })
            .map(|(_, _, identity)| StoredSequenceOwner::Column(*identity))
            .ok_or_else(column_not_found);
    }

    let (view, identity) = builder
        .resolve_view_object_name(&relation)?
        .and_then(|view| {
            builder
                .views()
                .iter()
                .find(|(candidate, _, _)| core::ptr::eq(candidate.as_ref(), view))
                .map(|(candidate, _, identity)| (candidate.as_ref(), *identity))
        })
        .ok_or_else(column_not_found)?;
    let (names, enumerable) = views::output_names(builder, view.declaration())?;
    match names.iter().position(|(name, quoted)| {
        case.identifiers_match(name, *quoted, &column.value, column.quote_style.is_some())
    }) {
        Some(ordinal) => Ok(StoredSequenceOwner::ViewColumn { view: identity, ordinal }),
        None if enumerable => Err(column_not_found()),
        None => {
            Err(Error::UnsupportedSequenceOwner {
                sequence_name: sequence_name(),
                owner: owner.to_string(),
            })
        }
    }
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

/// Whether a written relation name reaches a sequence through the session's
/// path.
pub(super) fn holds_sequence(builder: &ParserDBBuilder, name: &ObjectName) -> bool {
    resolve_sequence(builder, name).ok().flatten().is_some()
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
            // A view the cascade took may have owned other sequences.
            drop_orphaned_sequences(&mut builder);
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

/// Drops every sequence whose owning column or view is gone.
///
/// Called wherever a table, a column or a view leaves, after the stores are
/// back in shape, since a rebuilt table drops its columns and adds them again
/// under the identities they held.
pub(super) fn drop_orphaned_sequences(builder: &mut ParserDBBuilder) {
    if builder.sequences().iter().all(|(sequence, ())| sequence.stored_owner().is_none()) {
        return;
    }
    let columns: BTreeSet<_> = builder.columns().iter().map(|(_, _, identity)| *identity).collect();
    let views: BTreeSet<_> = builder.views().iter().map(|(_, _, identity)| *identity).collect();
    let doomed: Vec<Arc<Sequence>> = builder
        .sequences()
        .iter()
        .map(|(sequence, ())| sequence)
        .filter(|sequence| {
            match sequence.stored_owner() {
                None => false,
                Some(StoredSequenceOwner::Column(column)) => !columns.contains(&column),
                Some(StoredSequenceOwner::ViewColumn { view, .. }) => !views.contains(&view),
            }
        })
        .cloned()
        .collect();
    forget(builder, &doomed);
}

/// Removes the `doomed` sequences and takes them out of every grant naming
/// them, dropping a grant left naming nothing.
///
/// A grant's names were bound to the relation they reached when it ran, so a
/// name is compared against a sequence's stored identity, as a dropped table's
/// grants are.
fn forget(builder: &mut ParserDBBuilder, doomed: &[Arc<Sequence>]) {
    if doomed.is_empty() {
        return;
    }
    builder
        .sequences_mut()
        .retain(|(sequence, ())| !doomed.iter().any(|dead| Arc::ptr_eq(dead, sequence)));

    let names_doomed = |name: &ObjectName| {
        doomed.iter().any(|sequence| {
            object_name_matches_resolved_identity(
                name,
                sequence.sequence_name(),
                sequence.sequence_name_is_quoted(),
                sequence.sequence_schema(),
                sequence.sequence_schema_is_quoted(),
            )
        })
    };
    let forget_in = |grants: &mut Vec<(Arc<Grant>, ())>| {
        grants.retain_mut(|(grant, ())| {
            let Some(GrantObjects::Sequences(names) | GrantObjects::Tables(names)) = &grant.objects
            else {
                return true;
            };
            if !names.iter().any(names_doomed) {
                return true;
            }
            let Some(GrantObjects::Sequences(names) | GrantObjects::Tables(names)) =
                &mut Arc::make_mut(grant).objects
            else {
                return true;
            };
            names.retain(|name| !names_doomed(name));
            !names.is_empty()
        });
    };
    forget_in(builder.table_grants_mut());
    forget_in(builder.column_grants_mut());
}
