//! `ALTER TABLE ... REPLICA IDENTITY`, its index named as PostgreSQL stores the
//! name and followed through every rename and drop of that index.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{
    ColumnOption, ColumnOptionDef, ConstraintCharacteristics, CreateIndex, CreateTable, Expr,
    Ident, IndexColumn, ObjectName, PrimaryKeyConstraint, ReplicaIdentity, TableConstraint,
    UniqueConstraint,
};

use super::{
    ParserDB, ParserDBBuilder, StoredTable, case_idents_match,
    constraint_names::{self, Slot},
    constraint_validity, object_name_last_identifier, plain_column_names, relation_name_holder,
    requires_value, schema_qualifiers_match, states_not_null, table_not_null,
    table_schema_qualifier,
};
use crate::{
    errors::{Error, ObjectKind, ReplicaIdentityRefusal, RequiredValue},
    structs::{
        ConstraintDeferral, IdentifierCase, StoredReplicaIdentity, TableAttribute, TableMetadata,
        metadata::RecordedConstraintName,
    },
    traits::TableLike,
    utils::identifier_resolution::stored_ident_name,
};

/// Applies one `REPLICA IDENTITY` subcommand of `ALTER TABLE` to the table
/// `table_name` reaches.
///
/// # Errors
///
/// Returns [`Error::AlterTableNotFound`] for an absent table without `IF
/// EXISTS`, and for `USING INDEX` the errors of [`usable_index`].
pub(super) fn set(
    mut builder: ParserDBBuilder,
    table_name: &ObjectName,
    if_exists: bool,
    identity: &ReplicaIdentity,
) -> Result<ParserDBBuilder, Error> {
    constraint_names::name_pending(&mut builder, constraint_names::CreationOrder::AlterTable)?;
    let setting = match identity {
        ReplicaIdentity::Default => StoredReplicaIdentity::Default,
        ReplicaIdentity::Nothing => StoredReplicaIdentity::Nothing,
        ReplicaIdentity::Full => StoredReplicaIdentity::Full,
        ReplicaIdentity::Index(index) => {
            // An absent table is reported below, or skipped under `IF EXISTS`.
            let named = match builder.resolve_table_object_name(table_name)? {
                Some(table) => Some(usable_index(&builder, table, index)?),
                None => None,
            };
            StoredReplicaIdentity::Index(named)
        }
    };
    ParserDB::alter_table_metadata(builder, table_name, if_exists, |_, metadata| {
        metadata.set_replica_identity(setting);
    })
}

/// The stored name of the index `index` names in the schema of `table`, once
/// PostgreSQL would take it as the table's replica identity.
///
/// # Errors
///
/// Returns the error PostgreSQL raises for a name holding no usable index of
/// `table`.
fn usable_index(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    index: &Ident,
) -> Result<String, Error> {
    let wanted = stored_ident_name(index).into_owned();
    let case = builder.identifier_case();
    let schema = table_schema_qualifier(table);
    let namespaces = builder.namespaces();
    let in_schema = |other: &CreateTable| {
        schema_qualifiers_match(namespaces, table_schema_qualifier(other), schema, case)
    };
    let owner = StoredTable::of(table);
    let table_name =
        builder.dialect().stored_spelling(table.table_name(), table.table_name_is_quoted());
    let other_table = |index_name: String| {
        Error::ReplicaIdentityIndexOfOtherTable { table_name: table_name.clone(), index_name }
    };

    let created = builder.indices().iter().map(|(created, _)| created.as_ref()).find(|created| {
        in_schema(TableAttribute::table(created))
            && created
                .attribute()
                .name
                .as_ref()
                .and_then(object_name_last_identifier)
                .is_some_and(|held| case_idents_match(held, index, case))
    });
    let checked = if let Some(created) = created {
        let held =
            created_index_name(created.attribute()).map_or_else(|| wanted.clone(), Cow::into_owned);
        if !owner.matches(TableAttribute::table(created), case) {
            return Err(other_table(held));
        }
        Some((held, refuse_created_index(builder, table, created.attribute())))
    } else {
        let key = builder.tables().iter().filter(|(other, _, _)| in_schema(other)).find_map(
            |(other, metadata, _)| {
                constraint_names::names(other, metadata.recorded_constraint_names(), case)
                    .into_iter()
                    .find(|(slot, name)| {
                        constraint_names::backed_by_index(*slot)
                            && name.as_deref() == Some(wanted.as_str())
                    })
                    .map(|(slot, _)| (other, slot))
            },
        );
        match key {
            Some((other, _)) if !owner.matches(other, case) => return Err(other_table(wanted)),
            Some((_, slot)) => Some((wanted.clone(), refuse_key(builder, table, slot))),
            None => None,
        }
    };
    match checked {
        Some((held, Ok(()))) => Ok(held),
        Some((held, Err(reason))) => {
            Err(Error::ReplicaIdentityIndexRefused { table_name, index_name: held, reason })
        }
        None => {
            match relation_name_holder(builder, index, schema, None) {
                // An index on a materialized view.
                Some(ObjectKind::Index | ObjectKind::UniqueIndex) => Err(other_table(wanted)),
                Some(actual_kind) => {
                    Err(Error::RelationKindMismatch {
                        object_name: wanted,
                        expected_kind: ObjectKind::Index,
                        actual_kind,
                        context: crate::errors::RelationKindContext::Statement,
                    })
                }
                None => Err(Error::ReplicaIdentityIndexNotFound { table_name, index_name: wanted }),
            }
        }
    }
}

/// The name a `CREATE INDEX` index is stored under.
pub(super) fn created_index_name(index: &CreateIndex) -> Option<Cow<'_, str>> {
    index.name.as_ref().and_then(object_name_last_identifier).map(stored_ident_name)
}

/// Why PostgreSQL refuses the `CREATE INDEX` index `index` of `table` as its
/// replica identity, in the order it checks.
fn refuse_created_index(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    index: &CreateIndex,
) -> Result<(), ReplicaIdentityRefusal> {
    if !index.unique {
        return Err(ReplicaIdentityRefusal::NotUnique);
    }
    let columns = key_columns(&index.columns).ok_or(ReplicaIdentityRefusal::Expression)?;
    if index.predicate.is_some() {
        return Err(ReplicaIdentityRefusal::Partial);
    }
    refuse_nullable(builder, table, &columns)
}

/// Why PostgreSQL refuses the index behind the key in `slot` of `table` as its
/// replica identity, in the order it checks.
fn refuse_key(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    slot: Slot<'_>,
) -> Result<(), ReplicaIdentityRefusal> {
    let refuse_immediate = |characteristics: Option<ConstraintCharacteristics>| {
        if ConstraintDeferral::of(characteristics).is_deferrable() {
            Err(ReplicaIdentityRefusal::NotImmediate)
        } else {
            Ok(())
        }
    };
    match slot {
        Slot::Column {
            def,
            held:
                ColumnOptionDef {
                    option:
                        ColumnOption::PrimaryKey(PrimaryKeyConstraint { characteristics, .. })
                        | ColumnOption::Unique(UniqueConstraint { characteristics, .. }),
                    ..
                },
            ..
        } => {
            refuse_immediate(*characteristics)?;
            refuse_nullable(builder, table, core::slice::from_ref(&def.name))
        }
        Slot::Table(
            _,
            TableConstraint::PrimaryKey(PrimaryKeyConstraint { characteristics, columns, .. })
            | TableConstraint::Unique(UniqueConstraint { characteristics, columns, .. }),
        ) => {
            refuse_immediate(*characteristics)?;
            refuse_nullable(builder, table, &plain_column_names(columns))
        }
        Slot::Column { .. } | Slot::Table(..) => Err(ReplicaIdentityRefusal::NotUnique),
    }
}

/// The column of each key of an index, a parenthesized or collated column
/// included, or [`None`] when a key is an expression.
fn key_columns(keys: &[IndexColumn]) -> Option<Vec<Ident>> {
    keys.iter()
        .map(|key| {
            let mut expression = &key.column.expr;
            while let Expr::Nested(inner) | Expr::Collate { expr: inner, .. } = expression {
                expression = inner;
            }
            match expression {
                Expr::Identifier(column) => Some(column.clone()),
                _ => None,
            }
        })
        .collect()
}

/// Refuses the first of `columns` that `table` lets hold nothing or requires
/// a value of only through a `NOT VALID` constraint, checking each column in
/// that order as PostgreSQL does.
fn refuse_nullable(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    columns: &[Ident],
) -> Result<(), ReplicaIdentityRefusal> {
    let case = builder.identifier_case();
    let owner = StoredTable::of(table);
    for column in columns {
        let name = || stored_ident_name(column).into_owned();
        if !requires_value(table, column, case) {
            return Err(ReplicaIdentityRefusal::NullableColumn(name()));
        }
        let stated = table.columns.iter().any(|declared| {
            case_idents_match(&declared.name, column, case) && states_not_null(declared)
        });
        let unvalidated = table_not_null(table, column, case).is_some_and(|held| {
            !constraint_validity::is_validated(
                builder,
                &owner,
                &TableConstraint::NotNull(held.clone()),
            )
        });
        if !stated && unvalidated {
            return Err(ReplicaIdentityRefusal::UnvalidatedNotNull(name()));
        }
    }
    Ok(())
}

/// Whether `column` is a key column of the index `table` uses as its replica
/// identity, which then may not stop requiring a value.
fn covers_column(builder: &ParserDBBuilder, table: &CreateTable, column: &Ident) -> bool {
    let case = builder.identifier_case();
    let owner = StoredTable::of(table);
    let Some((metadata, wanted)) =
        builder.tables().iter().find(|(held, _, _)| owner.matches(held, case)).and_then(
            |(_, metadata, _)| Some((metadata, metadata.replica_identity().index_name()?)),
        )
    else {
        return false;
    };
    let created = builder
        .indices()
        .iter()
        .map(|(created, _)| created.as_ref())
        .filter(|created| owner.matches(TableAttribute::table(created), case))
        .find(|created| created_index_name(created.attribute()).as_deref() == Some(wanted));
    let columns = match created {
        Some(created) => key_columns(&created.attribute().columns).unwrap_or_default(),
        None => {
            constraint_names::names(table, metadata.recorded_constraint_names(), case)
                .into_iter()
                .find(|(_, name)| name.as_deref() == Some(wanted))
                .map(|(slot, _)| key_slot_columns(slot))
                .unwrap_or_default()
        }
    };
    columns.iter().any(|key| case_idents_match(key, column, case))
}

/// The key columns of the primary key or unique constraint in `slot`.
fn key_slot_columns(slot: Slot<'_>) -> Vec<Ident> {
    match slot {
        Slot::Column { def, .. } => alloc::vec![def.name.clone()],
        Slot::Table(
            _,
            TableConstraint::PrimaryKey(PrimaryKeyConstraint { columns, .. })
            | TableConstraint::Unique(UniqueConstraint { columns, .. }),
        ) => plain_column_names(columns),
        Slot::Table(..) => Vec::new(),
    }
}

/// Refuses `table_name` dropping the value requirement of `column` while
/// `column` is a key column of the index `node` uses as its replica identity.
///
/// # Errors
///
/// Returns [`Error::RequiredValueNotDroppable`] for such a column.
pub(super) fn refuse_losing_requirement(
    builder: &ParserDBBuilder,
    stored: &StoredTable,
    node: &CreateTable,
    column: &Ident,
) -> Result<(), Error> {
    if covers_column(builder, node, column) {
        return Err(Error::RequiredValueNotDroppable {
            table_name: builder.dialect().stored_spelling(&stored.name, stored.name_quoted),
            column_name: builder.dialect().stored_name(column),
            reason: RequiredValue::InReplicaIdentity,
        });
    }
    Ok(())
}

/// Refuses the table `stored` below a dropped requirement having lost the
/// value requirement of `column` while `column` is in its replica identity.
///
/// # Errors
///
/// Returns [`Error::RequiredValueNotDroppable`] for such a column, and
/// `ObjectKind::Table.not_in_database` when the stored entry is missing.
pub(super) fn refuse_lost_requirement(
    builder: &ParserDBBuilder,
    stored: &StoredTable,
    column: &Ident,
) -> Result<(), Error> {
    let node = ParserDB::stored_node(builder, stored)?;
    if requires_value(node, column, builder.identifier_case()) {
        return Ok(());
    }
    refuse_losing_requirement(builder, stored, node, column)
}

/// Points the setting of `metadata` at `new` where it named the index `old`,
/// both stored names, which renaming that index does.
pub(super) fn follow_rename(metadata: &mut TableMetadata<CreateTable>, old: &str, new: &str) {
    if metadata.replica_identity().index_name() == Some(old) {
        metadata.set_replica_identity(StoredReplicaIdentity::Index(Some(new.to_string())));
    }
}

/// Leaves the setting of `metadata` naming no index where it named `name`,
/// which dropping that index does.
pub(super) fn forget(metadata: &mut TableMetadata<CreateTable>, name: &str) {
    if metadata.replica_identity().index_name() == Some(name) {
        metadata.set_replica_identity(StoredReplicaIdentity::Index(None));
    }
}

/// Leaves the setting of `metadata` naming no index where it named a key of
/// `previous` that `replacement`, named by `metadata`, no longer holds, its
/// columns named under `case`.
pub(super) fn follow_rebuild(
    metadata: &mut TableMetadata<CreateTable>,
    previous: &CreateTable,
    previous_names: &[RecordedConstraintName],
    replacement: &CreateTable,
    case: IdentifierCase,
) {
    let Some(name) = metadata.replica_identity().index_name() else {
        return;
    };
    let key_named = |node: &CreateTable, names: &[RecordedConstraintName]| {
        constraint_names::names(node, names, case).into_iter().any(|(slot, held)| {
            constraint_names::backed_by_index(slot) && held.as_deref() == Some(name)
        })
    };
    if key_named(previous, previous_names)
        && !key_named(replacement, metadata.recorded_constraint_names())
    {
        metadata.set_replica_identity(StoredReplicaIdentity::Index(None));
    }
}
