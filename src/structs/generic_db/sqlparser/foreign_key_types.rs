//! PostgreSQL foreign-key equality compatibility.

use alloc::{boxed::Box, string::ToString, vec::Vec};

use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, CreateTable, Expr, ForeignKeyConstraint, ObjectName,
    Statement,
};

use super::{ParserDB, ParserDBBuilder, StoredTable, inheritance};
use crate::{
    errors::Error,
    structs::{TableAttribute, TypeKind},
    utils::{
        coercion::{Proof, base, can_coerce, canonical_owned, facts},
        object_name::table_matches_object_name,
        postgres_types::PgType,
    },
};

/// Returns the table whose foreign-key types a statement can change.
pub(super) fn changed_table(statement: &Statement) -> Option<&ObjectName> {
    match statement {
        Statement::CreateTable(table)
            if super::foreign_keys_of(table).next().is_some() || table.partition_of.is_some() =>
        {
            Some(&table.name)
        }
        Statement::AlterTable(table)
            if table.operations.iter().any(|operation| {
                matches!(
                    operation,
                    AlterTableOperation::AddConstraint { .. }
                        | AlterTableOperation::AddColumn { .. }
                        | AlterTableOperation::AlterColumn {
                            op: AlterColumnOperation::SetDataType { .. },
                            ..
                        }
                        | AlterTableOperation::AttachPartitionOf { .. }
                )
            }) =>
        {
            Some(&table.name)
        }
        _ => None,
    }
}

/// The changed table and every descendant that takes its column types.
pub(super) fn changed_tables(builder: &ParserDBBuilder, name: &ObjectName) -> Vec<StoredTable> {
    let Some((table, _, _)) = builder.tables().iter().find(|(table, _, _)| {
        table_matches_object_name(builder.namespaces(), table.as_ref(), name)
    }) else {
        return Vec::new();
    };
    let table = StoredTable::of(table);
    let mut tables = inheritance::descendants(builder, &table);
    tables.push(table);
    tables
}

/// Whether a statement's table is either side of a foreign key.
fn affected(
    builder: &ParserDBBuilder,
    foreign_key: &TableAttribute<CreateTable, ForeignKeyConstraint>,
    changed_tables: &[StoredTable],
) -> bool {
    changed_tables.iter().any(|changed| {
        changed.matches(foreign_key.table(), builder.identifier_case())
            || changed.named_by(&foreign_key.attribute().foreign_table)
    })
}

/// Whether a table change reaches a foreign key in the builder.
pub(super) fn needs_validation(
    builder: &ParserDBBuilder,
    changed_tables: &[StoredTable],
    first_foreign_key: usize,
) -> bool {
    builder
        .foreign_keys()
        .iter()
        .skip(first_foreign_key)
        .any(|(key, ())| affected(builder, key, changed_tables))
}

/// Whether a matching unique index names an operator class the model cannot
/// resolve.
fn explicit_operator_class(
    builder: &ParserDBBuilder,
    target: &CreateTable,
    foreign_key: &ForeignKeyConstraint,
) -> bool {
    builder.indices().iter().any(|(index, _)| {
        let index = index.attribute();
        index.unique
            && table_matches_object_name(builder.namespaces(), target, &index.table_name)
            && index.columns.len() == foreign_key.referred_columns.len()
            && index.columns.iter().any(|column| column.operator_class.is_some())
            && index.columns.iter().all(|column| {
                matches!(&column.column.expr, Expr::Identifier(name)
                if foreign_key.referred_columns.iter().any(|referred| {
                    super::case_idents_match(name, referred, builder.identifier_case())
                }))
            })
    })
}

/// Refuses foreign keys whose columns have no compatible equality operator.
pub(super) fn validate(
    database: &ParserDB,
    builder: &ParserDBBuilder,
    changed_tables: &[StoredTable],
    first_foreign_key: usize,
) -> Result<(), Error> {
    for (foreign_key, ()) in builder.foreign_keys().iter().skip(first_foreign_key) {
        if !affected(builder, foreign_key, changed_tables) {
            continue;
        }
        let target = builder
            .tables()
            .iter()
            .find(|(table, _, _)| {
                table_matches_object_name(
                    builder.namespaces(),
                    table.as_ref(),
                    &foreign_key.attribute().foreign_table,
                )
            })
            .map(|(table, _, _)| table.as_ref())
            .ok_or_else(|| {
                Error::ReferencedTableNotFoundForForeignKey {
                    referenced_table: foreign_key.attribute().foreign_table.to_string(),
                    host_table: foreign_key.table().name.to_string(),
                }
            })?;
        if explicit_operator_class(builder, target, foreign_key.attribute()) {
            continue;
        }
        for (host_name, referenced_name) in
            foreign_key.attribute().columns.iter().zip(&foreign_key.attribute().referred_columns)
        {
            let host = foreign_key
                .table()
                .columns
                .iter()
                .find(|column| {
                    super::case_idents_match(&column.name, host_name, builder.identifier_case())
                })
                .ok_or_else(|| {
                    Error::HostColumnNotFoundForForeignKey {
                        host_column: host_name.value.clone(),
                        host_table: foreign_key.table().name.to_string(),
                    }
                })?;
            let referenced = target
                .columns
                .iter()
                .find(|column| {
                    super::case_idents_match(
                        &column.name,
                        referenced_name,
                        builder.identifier_case(),
                    )
                })
                .ok_or_else(|| {
                    Error::ReferencedColumnNotFoundForForeignKey {
                        referenced_column: referenced_name.value.clone(),
                        referenced_table: target.name.to_string(),
                        host_table: foreign_key.table().name.to_string(),
                    }
                })?;
            let host_type = canonical_owned(database, PgType::of(&host.data_type));
            let referenced_type = canonical_owned(database, PgType::of(&referenced.data_type));
            if compatible(database, &host_type, &referenced_type) == Proof::No {
                return Err(Error::ForeignKeyCannotBeImplemented {
                    constraint_name: constraint_name(builder, foreign_key)?,
                    host_table: foreign_key.table().name.to_string().into_boxed_str(),
                    host_column: host.name.value.as_str().into(),
                    referenced_table: target.name.to_string().into_boxed_str(),
                    referenced_column: referenced.name.value.as_str().into(),
                });
            }
        }
    }
    Ok(())
}

/// Resolves the name recorded for a stored foreign key.
fn constraint_name(
    builder: &ParserDBBuilder,
    foreign_key: &TableAttribute<CreateTable, ForeignKeyConstraint>,
) -> Result<Box<str>, Error> {
    builder
        .tables()
        .iter()
        .find(|(table, _, _)| core::ptr::eq(table.as_ref(), foreign_key.table()))
        .and_then(|(table, metadata, _)| {
            let position =
                metadata.foreign_keys().position(|stored| core::ptr::eq(stored, foreign_key))?;
            super::constraint_names::names(
                table,
                metadata.recorded_constraint_names(),
                builder.identifier_case(),
            )
            .into_iter()
            .filter(|(slot, _)| slot.is_foreign_key())
            .nth(position)
            .and_then(|(_, name)| name)
        })
        .map(|name| name.into_owned().into_boxed_str())
        .ok_or_else(|| {
            Error::ForeignKeyNameMissing {
                host_table: foreign_key.table().name.to_string().into_boxed_str(),
            }
        })
}
/// Proves equality support after flattening domain types.
fn compatible(database: &ParserDB, host: &PgType, referenced: &PgType) -> Proof {
    let (Ok(host), Ok(referenced)) = (base(database, host), base(database, referenced)) else {
        return Proof::Unknown;
    };
    if facts(database, &host).is_none() {
        return Proof::Unknown;
    }
    let Some(referenced_facts) = facts(database, &referenced) else {
        return Proof::Unknown;
    };
    if matches!(referenced.as_ref(), PgType::Array(_))
        || matches!(
            referenced_facts.kind(),
            Some(TypeKind::Enum | TypeKind::Range | TypeKind::Multirange)
        )
    {
        return if host == referenced { Proof::Yes } else { Proof::No };
    }
    let PgType::Catalog(referenced_name) = referenced.as_ref() else {
        return Proof::Unknown;
    };
    let input = match *referenced_name {
        "varchar" => "text",
        "cidr" => "inet",
        "int2" | "int4" | "int8" | "numeric" | "float4" | "float8" | "text" | "bpchar" | "name"
        | "uuid" | "date" | "timestamp" | "timestamptz" | "bool" | "bit" | "varbit" | "bytea"
        | "char" | "inet" | "interval" | "jsonb" | "macaddr" | "macaddr8" | "money" | "oid"
        | "oidvector" | "pg_lsn" | "tid" | "time" | "timetz" | "tsquery" | "tsvector" | "xid8" => {
            referenced_name
        }
        _ => return Proof::Unknown,
    };
    if let PgType::Catalog(host_name) = host.as_ref()
        && (host_name == &input || same_family(host_name, input))
    {
        return Proof::Yes;
    }
    can_coerce(database, &host, &PgType::Catalog(input))
}

/// Identifies the built-in btree families with cross-type equality operators.
fn same_family(host: &str, referenced: &str) -> bool {
    [
        &["int2", "int4", "int8"][..],
        &["float4", "float8"][..],
        &["date", "timestamp", "timestamptz"][..],
        &["name", "text"][..],
    ]
    .iter()
    .any(|family| family.contains(&host) && family.contains(&referenced))
}
