//! PostgreSQL foreign-key equality compatibility.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnDef, CreateTable, Expr, ForeignKeyConstraint,
    Ident, ObjectName, Statement,
};

use super::{ParserDB, ParserDBBuilder, StoredTable, inheritance};
use crate::{
    errors::Error,
    structs::{TableAttribute, TypeKind},
    utils::{
        coercion::{Proof, base, can_coerce, canonical, facts},
        object_name::table_matches_object_name,
        postgres_types::PgType,
    },
};

/// A column captured by foreign-key resolution.
#[derive(Clone, Debug)]
pub(super) struct BoundColumn {
    pub(super) name: Ident,
    pub(super) data_type: PgType,
}

impl BoundColumn {
    /// Captures the column's declared identity and type.
    pub(super) fn of(column: &ColumnDef) -> Self {
        Self { name: column.name.clone(), data_type: PgType::of(&column.data_type) }
    }
}

/// The columns compared by one foreign-key equality operator.
#[derive(Clone, Debug)]
pub(super) struct ColumnPair {
    pub(super) host: BoundColumn,
    pub(super) referenced: BoundColumn,
}

/// The resolved relations and columns of a PostgreSQL foreign key.
#[derive(Clone, Debug)]
pub(super) struct BoundForeignKey {
    pub(super) key: Arc<TableAttribute<CreateTable, ForeignKeyConstraint>>,
    pub(super) target: Arc<CreateTable>,
    pub(super) columns: Vec<ColumnPair>,
}

/// A resolved PostgreSQL foreign key awaiting its installation name.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedForeignKey {
    pub(super) binding: BoundForeignKey,
    pub(super) position: super::constraint_names::Position,
    pub(super) rendering: String,
}

/// A PostgreSQL foreign key with resolved columns and an installed name.
#[derive(Clone, Debug)]
pub(crate) struct InstalledForeignKey {
    pub(super) binding: BoundForeignKey,
    pub(super) name: Box<str>,
}

impl ResolvedForeignKey {
    /// Installs the resolved key under its assigned name.
    pub(super) fn install(self, name: String) -> InstalledForeignKey {
        InstalledForeignKey { binding: self.binding, name: name.into_boxed_str() }
    }
}

/// Carries incoming column bindings through replacement of their resolved
/// target.
pub(super) fn follow_target_replacement(
    builder: &mut ParserDBBuilder,
    previous: &Arc<CreateTable>,
    replacement: &Arc<CreateTable>,
) {
    let case = builder.identifier_case();
    for installed in builder
        .installed_foreign_keys_mut()
        .iter_mut()
        .filter(|installed| Arc::ptr_eq(&installed.binding.target, previous))
    {
        let binding = &mut Arc::make_mut(installed).binding;
        binding.target = replacement.clone();
        for column in &replacement.columns {
            for pair in binding
                .columns
                .iter_mut()
                .filter(|pair| super::case_idents_match(&pair.referenced.name, &column.name, case))
            {
                pair.referenced = BoundColumn::of(column);
            }
        }
    }
}

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
        .installed_foreign_keys()
        .iter()
        .skip(first_foreign_key)
        .any(|key| affected(builder, &key.binding.key, changed_tables))
}

/// Whether a matching unique index names an operator class the model cannot
/// resolve.
fn explicit_operator_class(
    builder: &ParserDBBuilder,
    target: &CreateTable,
    foreign_key: &ForeignKeyConstraint,
) -> bool {
    builder.indices().iter().any(|(index, metadata)| {
        let index = index.attribute();
        index.unique
            && metadata.is_valid()
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

/// Refuses installed foreign keys whose columns lack compatible equality.
pub(super) fn validate(
    database: &ParserDB,
    builder: &ParserDBBuilder,
    changed_tables: &[StoredTable],
    first_foreign_key: usize,
) -> Result<(), Error> {
    for installed in builder.installed_foreign_keys().iter().skip(first_foreign_key) {
        let binding = &installed.binding;
        let foreign_key = &binding.key;
        if !affected(builder, foreign_key, changed_tables)
            || explicit_operator_class(builder, &binding.target, foreign_key.attribute())
        {
            continue;
        }
        for pair in &binding.columns {
            let host_type = canonical(database, &pair.host.data_type);
            let referenced_type = canonical(database, &pair.referenced.data_type);
            if compatible(database, &host_type, &referenced_type) == Proof::No {
                return Err(Error::ForeignKeyCannotBeImplemented {
                    constraint_name: installed.name.clone(),
                    host_table: foreign_key.table().name.to_string().into_boxed_str(),
                    host_column: pair.host.name.value.as_str().into(),
                    referenced_table: binding.target.name.to_string().into_boxed_str(),
                    referenced_column: pair.referenced.name.value.as_str().into(),
                });
            }
        }
    }
    Ok(())
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
