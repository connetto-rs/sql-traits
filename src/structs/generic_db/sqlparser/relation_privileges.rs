//! `GRANT` and `REVOKE` on tables, views, materialized views and sequences.
//!
//! PostgreSQL keeps one access list per relation. A statement naming several
//! relations, or sweeping schemas with `ALL TABLES IN SCHEMA` or `ALL
//! SEQUENCES IN SCHEMA`, edits the list of each relation it reaches when it
//! runs, and remembers nothing about the statement itself. So each such
//! statement is recorded here as one grant per relation, which leaves a
//! relation created later out, and lets a `DROP`, a rename or a `REVOKE`
//! touching one relation leave the others as they stood.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{Grant, GrantObjects, Ident, ObjectName, ObjectNamePart, Privileges, Revoke};

use super::{ParserDBBuilder, apply_revoke_to_grant_store, table_owner};
use crate::{
    errors::Error,
    impls::{
        check_granted_columns, object_names_match, schema_matches_sequence, schema_matches_table,
        schema_matches_view, split_by_column_scope,
    },
    traits::{SequenceLike, TableLike, ViewLike},
};

/// Records `grant`, one grant per relation it reaches.
///
/// The names `grant` carries must already be bound, as a recorded name keeps
/// reaching the relation it reached when the grant ran.
///
/// # Errors
///
/// Returns [`Error::ColumnNotFoundForGrant`] when a table a blanket form
/// reaches lacks a column the privileges list.
pub(super) fn grant(builder: &mut ParserDBBuilder, mut grant: Grant) -> Result<(), Error> {
    let parts = match &grant.objects {
        Some(objects) => per_relation(builder, objects, &grant.privileges)?,
        None => None,
    };
    let Some(parts) = parts else {
        record(builder, grant);
        return Ok(());
    };
    grant.objects = None;
    for part in parts {
        let mut single = grant.clone();
        single.objects = Some(part);
        record(builder, single);
    }
    Ok(())
}

/// Applies `revoke` to every grant on each relation it reaches.
///
/// The names `revoke` carries must already be bound, so that each one meets
/// the grants recorded on the relation it reaches.
///
/// # Errors
///
/// Returns [`Error::ColumnNotFoundForGrant`] when a table a blanket form
/// reaches lacks a column the privileges list, and
/// [`Error::UnsupportedRevoke`] when the revoke leaves a grant this model
/// cannot represent.
pub(super) fn revoke(builder: &mut ParserDBBuilder, mut revoke: Revoke) -> Result<(), Error> {
    let parts = match &revoke.objects {
        Some(objects) => per_relation(builder, objects, &revoke.privileges)?,
        None => None,
    };
    let Some(parts) = parts else {
        return take(builder, &revoke).map_err(|reason| unsupported(&revoke, reason));
    };
    let written = revoke.objects.take();
    for part in parts {
        revoke.objects = Some(part);
        if let Err(reason) = take(builder, &revoke) {
            revoke.objects = written;
            return Err(unsupported(&revoke, reason));
        }
    }
    Ok(())
}

/// Stores one grant, its whole-table privileges among the table grants and
/// those written with a column list among the column grants.
fn record(builder: &mut ParserDBBuilder, grant: Grant) {
    let (relation_part, column_part) = split_by_column_scope(grant);
    if let Some(part) = relation_part {
        builder.table_grants_mut().push((Arc::new(part), ()));
    }
    if let Some(part) = column_part {
        builder.column_grants_mut().push((Arc::new(part), ()));
    }
}

/// Takes what `revoke` names from the grants and from the owner's entry,
/// returning why the result cannot be represented when it cannot.
fn take(builder: &mut ParserDBBuilder, revoke: &Revoke) -> Result<(), &'static str> {
    if let Some(reason) = apply_revoke_to_grant_store(builder.table_grants_mut(), revoke)
        .or_else(|| apply_revoke_to_grant_store(builder.column_grants_mut(), revoke))
    {
        return Err(reason);
    }
    table_owner::revoke(builder, revoke);
    Ok(())
}

/// The error for a revoke this model cannot represent, quoting the statement
/// as written.
fn unsupported(revoke: &Revoke, reason: &str) -> Error {
    Error::UnsupportedRevoke { statement: revoke.to_string(), reason: reason.to_string() }
}

/// Splits `objects` into one object list per relation it reaches, or returns
/// `None` for objects naming no relation.
///
/// A listed name is kept as bound, once however often it is written. A
/// blanket form reaches the relations its schemas hold now, under the names
/// they are stored by, and an empty schema reaches none.
///
/// # Errors
///
/// Returns [`Error::ColumnNotFoundForGrant`] when a table a blanket form
/// reaches lacks a column `privileges` lists, which PostgreSQL refuses for
/// the whole statement.
fn per_relation(
    builder: &ParserDBBuilder,
    objects: &GrantObjects,
    privileges: &Privileges,
) -> Result<Option<Vec<GrantObjects>>, Error> {
    let parts = match objects {
        GrantObjects::Tables(names) => one_each(names, GrantObjects::Tables),
        GrantObjects::Sequences(names) => one_each(names, GrantObjects::Sequences),
        GrantObjects::AllTablesInSchema { schemas } => {
            let mut reached = Vec::new();
            for (table, _, _) in builder.tables() {
                let table = table.as_ref();
                if schemas.iter().any(|schema| schema_matches_table(schema, table)) {
                    check_granted_columns(privileges, table, builder.identifier_case())?;
                    reached.push(stored_name(
                        table.table_schema().map(|schema| (schema, table.table_schema_is_quoted())),
                        (table.table_name(), table.table_name_is_quoted()),
                    ));
                }
            }
            let views =
                builder.views().iter().map(|(view, _, _)| view_name(view.as_ref(), schemas));
            let materialized = builder
                .materialized_views()
                .iter()
                .map(|(view, _, _)| view_name(view.as_ref(), schemas));
            reached.extend(views.chain(materialized).flatten());
            reached.into_iter().map(|name| GrantObjects::Tables(alloc::vec![name])).collect()
        }
        GrantObjects::AllSequencesInSchema { schemas } => {
            builder
                .sequences()
                .iter()
                .map(|(sequence, ())| sequence.as_ref())
                .filter(|sequence| {
                    schemas.iter().any(|schema| schema_matches_sequence(schema, *sequence))
                })
                .map(|sequence| {
                    GrantObjects::Sequences(alloc::vec![stored_name(
                        sequence
                            .sequence_schema()
                            .map(|schema| (schema, sequence.sequence_schema_is_quoted())),
                        (sequence.sequence_name(), sequence.sequence_name_is_quoted()),
                    )])
                })
                .collect()
        }
        _ => return Ok(None),
    };
    Ok(Some(parts))
}

/// One object list per distinct name in `names`, in written order.
fn one_each(names: &[ObjectName], form: fn(Vec<ObjectName>) -> GrantObjects) -> Vec<GrantObjects> {
    let mut distinct: Vec<&ObjectName> = Vec::with_capacity(names.len());
    for name in names {
        if !distinct.iter().any(|kept| object_names_match(kept, name)) {
            distinct.push(name);
        }
    }
    distinct.into_iter().map(|name| form(alloc::vec![name.clone()])).collect()
}

/// The name a view is stored by, when one of `schemas` holds it.
fn view_name<V: ViewLike>(view: &V, schemas: &[ObjectName]) -> Option<ObjectName> {
    schemas.iter().any(|schema| schema_matches_view(schema, view)).then(|| {
        stored_name(
            view.view_schema().map(|schema| (schema, view.view_schema_is_quoted())),
            (view.view_name(), view.view_name_is_quoted()),
        )
    })
}

/// The bound name of a relation stored under `schema`, bare for one in the
/// default schema.
fn stored_name(schema: Option<(&str, bool)>, (name, quoted): (&str, bool)) -> ObjectName {
    let ident = |value: &str, quoted: bool| {
        ObjectNamePart::Identifier(if quoted {
            Ident::with_quote('"', value)
        } else {
            Ident::new(value)
        })
    };
    ObjectName(
        schema
            .map(|(schema, quoted)| ident(schema, quoted))
            .into_iter()
            .chain(core::iter::once(ident(name, quoted)))
            .collect(),
    )
}
