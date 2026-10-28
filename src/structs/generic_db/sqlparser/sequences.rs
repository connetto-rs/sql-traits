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
//!
//! PostgreSQL also creates a sequence for every `serial` column and every
//! identity column a statement declares, named `<table>_<column>_seq` and
//! numbered when that name is taken, in the table's schema and owned by the
//! column. A column copied from a `LIKE` source or a parent brings none. An
//! identity sequence follows its column's type and cannot be dropped on its
//! own.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use sqlparser::ast::{
    ColumnDef, ColumnOption, DataType, Grant, GrantObjects, Ident, ObjectName, ObjectNamePart,
    SequenceOptions,
};

use super::{
    ParserDBBuilder, SchemaQualifier, StoredTable, object_name_last_identifier,
    object_name_matches_resolved_identity, place_relation, relation_name_holder,
    require_named_in_catalog, schema_qualifiers_match, views,
};
use crate::{
    errors::{Error, ObjectKind},
    impls::{SqlparserDialect, serial_sequence_type},
    structs::{IdentifierCase, Sequence, StoredSequenceOwner, TableAttribute},
    traits::{ColumnLike, SequenceLike, SequenceOrigin, TableLike},
    utils::{
        identifier_resolution::{identifiers_match, normalize_identifier},
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
        SequenceOrigin::Declared,
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
/// statement wrote `IF EXISTS`, [`Error::SequenceRequiredByColumn`] for the
/// sequence of an identity column, and [`Error::RelationHasDependents`] when
/// a view reads the sequence and the statement wrote no `CASCADE`.
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
            refuse_dropping_identity_sequence(&builder, &doomed)?;
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

/// Refuses dropping the sequence of an identity column on its own, which
/// PostgreSQL refuses even under `CASCADE`.
///
/// # Errors
///
/// Returns [`Error::SequenceRequiredByColumn`] for such a sequence.
fn refuse_dropping_identity_sequence(
    builder: &ParserDBBuilder,
    sequence: &Sequence,
) -> Result<(), Error> {
    if let (SequenceOrigin::Identity, Some(StoredSequenceOwner::Column(identity))) =
        (sequence.origin(), sequence.stored_owner())
        && let Some((column, _, _)) =
            builder.columns().iter().find(|(_, _, held)| *held == identity)
    {
        return Err(Error::SequenceRequiredByColumn {
            sequence_name: sequence.sequence_name().to_string(),
            table_name: TableAttribute::table(column.as_ref()).table_name().to_string(),
            column_name: column.column_name().to_string(),
        });
    }
    Ok(())
}

/// The longest name PostgreSQL keeps, in bytes.
const NAME_BYTES: usize = 63;

/// The longest prefix of `name` that fits in `bytes` without splitting a
/// character.
fn clipped(name: &str, bytes: usize) -> &str {
    let mut end = bytes.min(name.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Joins a table name, a column name and a label the way PostgreSQL's
/// `makeObjectName` does, cutting the longer of the two names a byte at a
/// time until the whole fits in [`NAME_BYTES`].
fn object_name(table: &str, column: &str, label: &str) -> String {
    let available = NAME_BYTES.saturating_sub(label.len() + 2);
    let (mut table_bytes, mut column_bytes) = (table.len(), column.len());
    while table_bytes + column_bytes > available {
        if table_bytes > column_bytes {
            table_bytes -= 1;
        } else {
            column_bytes -= 1;
        }
    }
    format!("{}_{}_{label}", clipped(table, table_bytes), clipped(column, column_bytes))
}

/// A sequence PostgreSQL creates for a column a statement declares, named
/// before the statement runs.
pub(super) struct ColumnSequence {
    /// The column as the statement declared it.
    column: Ident,
    /// Whether a serial type or an identity calls for the sequence.
    origin: SequenceOrigin,
    /// The `AS` type and the identity's own options.
    options: Vec<SequenceOptions>,
    /// The name chosen for it.
    name: String,
}

/// What a column declaration calls for, if PostgreSQL creates a sequence for
/// it: an identity, or else a serial type.
fn column_sequence_request(column: &ColumnDef) -> Option<(SequenceOrigin, Vec<SequenceOptions>)> {
    let identity = column.options.iter().find_map(|option| {
        match &option.option {
            ColumnOption::Generated { generation_expr: None, sequence_options, .. } => {
                Some(sequence_options)
            }
            _ => None,
        }
    });
    if let Some(sequence_options) = identity {
        let mut options = alloc::vec![SequenceOptions::DataType(column.data_type.clone())];
        options.extend(sequence_options.iter().flatten().cloned());
        return Some((SequenceOrigin::Identity, options));
    }
    let data_type = serial_sequence_type(&column.data_type)?;
    Some((SequenceOrigin::Serial, alloc::vec![SequenceOptions::DataType(data_type)]))
}

/// Names the sequences PostgreSQL creates for the columns a statement declares
/// on `table`, a `serial` type or an identity each calling for one.
///
/// PostgreSQL names each `<table>_<column>_seq`, cut to fit, and numbers the
/// label when a relation of the schema already holds the name. It names them
/// all before creating any, so two columns of one statement may arrive at the
/// same name, which [`add_column_sequences`] then refuses. Only PostgreSQL
/// creates such sequences.
pub(super) fn plan_column_sequences<'column>(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    columns: impl IntoIterator<Item = &'column ColumnDef>,
) -> Vec<ColumnSequence> {
    if *builder.dialect() != SqlparserDialect::PostgreSql {
        return Vec::new();
    }
    let schema = table.schema.as_deref().map(|schema| (schema, table.schema_quoted));
    let table_name = normalize_identifier(&table.name, table.name_quoted);
    columns
        .into_iter()
        .filter_map(|column| {
            let (origin, options) = column_sequence_request(column)?;
            let column_name =
                normalize_identifier(&column.name.value, column.name.quote_style.is_some());
            let table_name = clipped(&table_name, NAME_BYTES);
            let column_name = clipped(&column_name, NAME_BYTES);
            // Each pass that finds the name taken found a relation holding it,
            // so the passes end before the relations do.
            let name = (0..=builder.relation_count())
                .map(|pass| {
                    let label =
                        if pass == 0 { "seq".to_string() } else { alloc::format!("seq{pass}") };
                    object_name(table_name, column_name, &label)
                })
                .find(|candidate| {
                    relation_name_holder(
                        builder,
                        &Ident::with_quote('"', candidate.as_str()),
                        schema,
                    )
                    .is_none()
                })?;
            Some(ColumnSequence { column: column.name.clone(), origin, options, name })
        })
        .collect()
}

/// Records the sequences [`plan_column_sequences`] named, each owned by its
/// column of `table`, which holds the columns by now.
///
/// # Errors
///
/// Returns [`Error::RelationNameAlreadyTaken`] when a relation took a planned
/// name meanwhile, another sequence of the same statement included.
pub(super) fn add_column_sequences(
    mut builder: ParserDBBuilder,
    table: &StoredTable,
    planned: Vec<ColumnSequence>,
) -> Result<ParserDBBuilder, Error> {
    let case = builder.identifier_case();
    let schema = table.schema.as_ref().map(|schema| (schema.clone(), table.schema_quoted));
    for ColumnSequence { column, origin, options, name } in planned {
        let qualifier = schema.as_ref().map(|(schema, quoted)| (schema.as_str(), *quoted));
        if let Some(conflicting_kind) =
            relation_name_holder(&builder, &Ident::with_quote('"', name.as_str()), qualifier)
        {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Sequence,
                conflicting_kind,
                object_name: name,
            });
        }
        let owner = builder.columns().iter().find_map(|(candidate, _, identity)| {
            (table.matches(TableAttribute::table(candidate.as_ref()), case)
                && case.identifiers_match(
                    candidate.column_name(),
                    candidate.column_name_is_quoted(),
                    &column.value,
                    column.quote_style.is_some(),
                ))
            .then_some(StoredSequenceOwner::Column(*identity))
        });
        let quoted = normalize_identifier(&name, false) != name.as_str();
        let sequence = Sequence::new((name, quoted), schema.clone(), options, owner, origin);
        builder = builder.add_sequence(Arc::new(sequence), ());
    }
    Ok(builder)
}

/// Gives every identity sequence the type its column holds now, which
/// PostgreSQL carries over whenever the column's type changes.
///
/// A serial sequence keeps the type it was created with.
pub(super) fn follow_identity_types(builder: &mut ParserDBBuilder) {
    if builder
        .sequences()
        .iter()
        .all(|(sequence, ())| sequence.origin() != SequenceOrigin::Identity)
    {
        return;
    }
    let types: BTreeMap<_, _> = builder
        .columns()
        .iter()
        .map(|(column, _, identity)| (*identity, &column.attribute().data_type))
        .collect();
    let changed: Vec<(usize, DataType)> = builder
        .sequences()
        .iter()
        .enumerate()
        .filter_map(|(position, (sequence, ()))| {
            let (SequenceOrigin::Identity, Some(StoredSequenceOwner::Column(identity))) =
                (sequence.origin(), sequence.stored_owner())
            else {
                return None;
            };
            let data_type = *types.get(&identity)?;
            // An identity sequence is created with its `AS` type first.
            let current = matches!(
                sequence.options().first(),
                Some(SequenceOptions::DataType(current)) if current == data_type
            );
            (!current).then(|| (position, data_type.clone()))
        })
        .collect();
    for (position, data_type) in changed {
        Arc::make_mut(&mut builder.sequences_mut()[position].0).set_data_type(data_type);
    }
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
            if let Some(GrantObjects::Sequences(names) | GrantObjects::Tables(names)) =
                &grant.objects
                && names.iter().any(names_doomed)
                && let Some(GrantObjects::Sequences(names) | GrantObjects::Tables(names)) =
                    &mut Arc::make_mut(grant).objects
            {
                names.retain(|name| !names_doomed(name));
                return !names.is_empty();
            }
            true
        });
    };
    forget_in(builder.table_grants_mut());
    forget_in(builder.column_grants_mut());
}
