//! The columns and checks a table takes from a parent.
//!
//! PostgreSQL gives an `INHERITS` child every column of every parent, and a
//! `PARTITION OF` child every column of the table it partitions, before the
//! columns the child declares itself. Both spellings record the same kind of
//! edge, so both resolve here.
//!
//! Resolution runs while the `CREATE TABLE` statement is applied, and every
//! later change to a parent is walked down to its descendants one edge at a
//! time. Parents are complete before children, which makes a grandchild pick
//! up an already complete parent.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{
    CharacterLength, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, DataType,
    ExactNumberInfo, Expr, Ident, IndexColumn, ObjectName, TableConstraint, TimezoneInfo,
};

use super::{ParserDBBuilder, StoredTable, column_copy::copy_column};
use crate::{errors::Error, traits::TableLike, utils::is_identity};

/// Which spelling links a child to a table it takes its shape from.
///
/// PostgreSQL records both in `pg_inherits`, but a partition and its root are
/// one table where keys are concerned, so the two pass down different amounts
/// of the parent's declaration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ParentKind {
    /// `INHERITS (parent)`, which passes down columns and checks.
    Inherits,
    /// `PARTITION OF root`, which passes down the whole shape.
    PartitionOf,
}

/// The tables a table takes its shape from, in the order they are written,
/// each under the spelling that links it.
///
/// `INHERITS` names any number of parents and `PARTITION OF` names exactly
/// one.
fn parents_with_kind(
    create_table: &CreateTable,
) -> impl Iterator<Item = (ParentKind, &ObjectName)> {
    create_table
        .inherits
        .iter()
        .flatten()
        .map(|name| (ParentKind::Inherits, name))
        .chain(create_table.partition_of.iter().map(|name| (ParentKind::PartitionOf, name)))
}

/// The tables a table takes its shape from, in the order they are written.
pub(super) fn parent_names(create_table: &CreateTable) -> impl Iterator<Item = &ObjectName> {
    parents_with_kind(create_table).map(|(_, name)| name)
}

/// The tables a table takes its shape from, for a node being rewritten.
fn parent_names_mut(create_table: &mut CreateTable) -> impl Iterator<Item = &mut ObjectName> {
    create_table.inherits.iter_mut().flatten().chain(create_table.partition_of.iter_mut())
}

/// Whether the table names `renamed` as a parent.
pub(super) fn names_parent(node: &CreateTable, renamed: &StoredTable) -> bool {
    parent_names(node).any(|name| renamed.named_by(name))
}

/// Rewrites the parent names a table lists when they name the renamed table.
///
/// PostgreSQL keeps the edge across a rename because it records the parent by
/// identity rather than by spelling, so the spelling follows the parent here.
pub(super) fn rewrite_parent_names(
    node: &mut CreateTable,
    renamed: &StoredTable,
    target: &super::RenameTarget,
) {
    for name in parent_names_mut(node) {
        if renamed.named_by(name) {
            target.rewrite(name);
        }
    }
}

/// Every table that inherits from `parent`, directly or through another.
///
/// Ordered children first so that dropping them in order never leaves a child
/// naming a parent that has already gone.
pub(super) fn descendants(builder: &ParserDBBuilder, parent: &StoredTable) -> Vec<StoredTable> {
    let mut found: Vec<StoredTable> = Vec::new();
    let mut frontier: Vec<StoredTable> = alloc::vec![parent.clone()];

    while let Some(current) = frontier.pop() {
        for (table, _) in builder.tables() {
            if !names_parent(table, &current) {
                continue;
            }
            let child = StoredTable::of(table);
            if found.contains(&child) || child == *parent {
                continue;
            }
            found.push(child.clone());
            frontier.push(child);
        }
    }

    found
}

/// The tables linked directly to `parent`, each under the spelling that links
/// it.
///
/// A change to a parent is walked down one edge at a time rather than over the
/// whole descendant set, because how much of the change a table receives
/// depends on the spelling of the edge it arrives through.
pub(super) fn direct_children(
    builder: &ParserDBBuilder,
    parent: &StoredTable,
) -> Vec<(ParentKind, StoredTable)> {
    builder
        .tables()
        .iter()
        .filter_map(|(table, _)| {
            parents_with_kind(table)
                .find(|(_, name)| parent.named_by(name))
                .map(|(kind, _)| (kind, StoredTable::of(table)))
        })
        .collect()
}

/// Marks a column a table has just received from a parent as inherited.
pub(super) fn mark_inherited(builder: &mut ParserDBBuilder, table: &StoredTable, column: &str) {
    let case = builder.identifier_case();
    if let Some((_, metadata)) =
        builder.tables_mut().iter_mut().find(|(stored, _)| table.matches(stored, case))
    {
        let mut names = metadata.inherited_column_names().to_vec();
        names.push(column.to_string());
        metadata.set_inherited_column_names(names);
    }
}

/// Records that a column a table received is now its own, which is what an
/// `ONLY` drop on the parent leaves behind.
pub(super) fn unmark_inherited(builder: &mut ParserDBBuilder, table: &StoredTable, column: &str) {
    let case = builder.identifier_case();
    if let Some((_, metadata)) =
        builder.tables_mut().iter_mut().find(|(stored, _)| table.matches(stored, case))
    {
        let names: Vec<String> = metadata
            .inherited_column_names()
            .iter()
            .filter(|name| name.as_str() != column)
            .cloned()
            .collect();
        metadata.set_inherited_column_names(names);
    }
}

/// Follows a parent's column rename in the record of what a child inherits.
pub(super) fn rename_inherited(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    from: &str,
    to: &Ident,
) {
    let case = builder.identifier_case();
    if let Some((_, metadata)) =
        builder.tables_mut().iter_mut().find(|(stored, _)| table.matches(stored, case))
    {
        let names = metadata
            .inherited_column_names()
            .iter()
            .map(|name| if name == from { to.value.clone() } else { name.clone() })
            .collect();
        metadata.set_inherited_column_names(names);
    }
}

/// Whether an `INHERITS` child receives the option along with the column.
///
/// PostgreSQL passes down what describes the column itself and withholds what
/// would create a constraint or an index of its own: a primary key, a unique
/// constraint, a foreign key, and an identity all stay with the parent.
fn is_inherited_option(option: &ColumnOption) -> bool {
    !matches!(
        option,
        ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_) | ColumnOption::ForeignKey(_)
    ) && !is_identity(option)
}

/// Whether a parent linked by this spelling writes the option onto the copy.
///
/// A check written `NO INHERIT` stays with the table declaring it, which is
/// the attribute's entire meaning, so it never joins the copy.
pub(super) fn option_passes_down(kind: ParentKind, option: &ColumnOption) -> bool {
    if matches!(option, ColumnOption::Check(check) if check.no_inherit) {
        return false;
    }
    kind == ParentKind::PartitionOf || is_inherited_option(option)
}

/// The option with the name a partition's copy would have been given stripped
/// away.
///
/// A partition's copy of a key differs from the root's only in that name, for
/// the same reason a copy written as a table constraint does, so recognising it
/// has to look past it.
fn without_generated_option_name(option: &ColumnOptionDef) -> ColumnOptionDef {
    let mut bare = option.clone();
    if matches!(option.option, ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_)) {
        bare.name = None;
    }
    bare
}

/// Whether the option a table holds is its copy of the one a parent writes.
pub(super) fn is_copy_of_option(held: &ColumnOptionDef, written: &ColumnOptionDef) -> bool {
    without_generated_option_name(held) == without_generated_option_name(written)
}

/// Whether a parent still writes the same constraint on the same column.
///
/// Read from the parents rather than recorded, which is enough because a copy
/// written on a column always arrives with the column and so is never the
/// child's own. A table with two parents keeps its copy until the last of them
/// stops writing it.
pub(super) fn receives_column_constraint(
    builder: &ParserDBBuilder,
    child: &CreateTable,
    written_on: &Ident,
    option: &ColumnOptionDef,
) -> bool {
    parents_with_kind(child).any(|(kind, name)| {
        option_passes_down(kind, &option.option)
            && builder.resolve_bound_table_name(name).ok().flatten().is_some_and(|parent| {
                parent.columns.iter().any(|declared| {
                    same_column(&declared.name, written_on)
                        && declared.options.iter().any(|held| is_copy_of_option(option, held))
                })
            })
    })
}

/// Whether a parent requires the column to hold a value, so that only the
/// parent may lift the requirement.
///
/// Read from the parents, the way the other inherited facts are. A parent that
/// states it explicitly, one whose key covers the column, and one whose column
/// is an identity all require it, because each is recorded as a requirement in
/// its own right once the parent's node is stored.
pub(super) fn requires_a_value(
    builder: &ParserDBBuilder,
    child: &CreateTable,
    column: &str,
) -> bool {
    parents_with_kind(child).any(|(_, name)| {
        builder.resolve_bound_table_name(name).ok().flatten().is_some_and(|parent| {
            parent.columns.iter().any(|declared| {
                super::identifiers_match(
                    declared.name.value.as_str(),
                    declared.name.quote_style.is_some(),
                    column,
                    false,
                ) && declared
                    .options
                    .iter()
                    .any(|option| matches!(option.option, ColumnOption::NotNull))
            })
        })
    })
}

/// Rewrites a parent column into the column the child receives.
///
/// A partition withholds nothing, because PostgreSQL enforces the root's keys
/// across every partition, but a key written inline still needs a name of its
/// own for the same reason one written as a table constraint does. The
/// `NOT NULL` an option implies is added back either way, which is what the
/// catalog holds too: a partition of an identity column carries a not-null
/// constraint of its own.
fn inherited_column(
    builder: &ParserDBBuilder,
    kind: ParentKind,
    child: &CreateTable,
    column: &ColumnDef,
    taken: &mut Vec<Ident>,
) -> ColumnDef {
    let mut copy = copy_column(column, |option| option_passes_down(kind, option));

    if kind == ParentKind::PartitionOf {
        let unique_stem = format!("_{}_key", copy.name.value);
        for option in &mut copy.options {
            let stem = match option.option {
                ColumnOption::PrimaryKey(_) => Some("_pkey"),
                ColumnOption::Unique(_) => Some(unique_stem.as_str()),
                _ => None,
            };
            if let Some(stem) = stem {
                option.name = Some(generated_key_name(builder, child, stem, taken));
            }
        }
    }

    copy
}

/// The name PostgreSQL resolves the type to, or [`None`] when the spelling is
/// not one this crate can place.
///
/// Only used to refuse a child that redeclares an inherited column with a
/// different type. An unplaceable spelling answers [`None`] and is accepted,
/// because wrongly refusing a legal schema costs more than missing a conflict
/// the parser cannot see.
fn type_identity(data_type: &DataType) -> Option<String> {
    let exact = |name: &str, info: &ExactNumberInfo| {
        match info {
            ExactNumberInfo::None => name.to_string(),
            ExactNumberInfo::Precision(p) => format!("{name}({p})"),
            ExactNumberInfo::PrecisionAndScale(p, s) => format!("{name}({p},{s})"),
        }
    };
    let sized = |name: &str, size: Option<u64>| {
        size.map_or_else(|| name.to_string(), |size| format!("{name}({size})"))
    };
    let charlen = |name: &str, length: Option<CharacterLength>| {
        match length {
            None => name.to_string(),
            Some(CharacterLength::Max) => format!("{name}(max)"),
            Some(CharacterLength::IntegerLength { length, .. }) => format!("{name}({length})"),
        }
    };

    Some(match data_type {
        DataType::Int(size) | DataType::Integer(size) | DataType::Int4(size) => {
            sized("int4", *size)
        }
        DataType::SmallInt(size) | DataType::Int2(size) => sized("int2", *size),
        DataType::BigInt(size) | DataType::Int8(size) => sized("int8", *size),
        DataType::Bool | DataType::Boolean => "bool".to_string(),
        DataType::Real | DataType::Float4 => "float4".to_string(),
        DataType::DoublePrecision | DataType::Float8 => "float8".to_string(),
        DataType::Numeric(info) | DataType::Decimal(info) | DataType::Dec(info) => {
            exact("numeric", info)
        }
        DataType::Text => "text".to_string(),
        DataType::Uuid => "uuid".to_string(),
        DataType::JSON => "json".to_string(),
        DataType::JSONB => "jsonb".to_string(),
        DataType::Date => "date".to_string(),
        DataType::Bytea => "bytea".to_string(),
        DataType::Varchar(length)
        | DataType::CharacterVarying(length)
        | DataType::CharVarying(length) => charlen("varchar", *length),
        DataType::Char(length) | DataType::Character(length) => charlen("bpchar", *length),
        DataType::Timestamp(precision, timezone) => {
            let base = match timezone {
                TimezoneInfo::Tz | TimezoneInfo::WithTimeZone => "timestamptz",
                TimezoneInfo::None | TimezoneInfo::WithoutTimeZone => "timestamp",
            };
            sized(base, *precision)
        }
        _ => return None,
    })
}

/// Whether the two spellings name types PostgreSQL would refuse to merge.
///
/// Answers `false` whenever either spelling cannot be placed, so an unfamiliar
/// type never blocks a schema PostgreSQL accepts.
fn types_conflict(parent: &DataType, child: &DataType) -> bool {
    match (type_identity(parent), type_identity(child)) {
        (Some(parent), Some(child)) => parent != child,
        _ => false,
    }
}

/// Whether the two column names denote the same column.
fn same_column(left: &Ident, right: &Ident) -> bool {
    super::identifiers_match(
        left.value.as_str(),
        left.quote_style.is_some(),
        right.value.as_str(),
        right.quote_style.is_some(),
    )
}

/// Whether a parent linked by this spelling passes the constraint down.
///
/// A check passes down whichever way it is written, so one attached to a
/// column arrives with the column and one written on its own arrives here,
/// unless it is written `NO INHERIT`, which is the one spelling that keeps a
/// check with the table declaring it. A partition also receives the root's
/// keys, unique constraints and foreign keys, because PostgreSQL enforces
/// those across the whole hierarchy rather than one table at a time.
pub(super) fn passes_down(kind: ParentKind, constraint: &TableConstraint) -> bool {
    match constraint {
        TableConstraint::Check(check) => !check.no_inherit,
        _ => kind == ParentKind::PartitionOf,
    }
}

/// The columns a generated index name is built from, an unnamed expression
/// contributing the same `expr` PostgreSQL uses for one.
fn index_name_columns(columns: &[IndexColumn]) -> Vec<&str> {
    columns
        .iter()
        .map(|column| {
            match &column.column.expr {
                Expr::Identifier(ident) => ident.value.as_str(),
                _ => "expr",
            }
        })
        .collect()
}

/// The name PostgreSQL gives an index it creates on its own behalf, with a
/// counter appended until the schema has room for it.
///
/// A key is also an index, and two indexes in one schema cannot share a name,
/// which is why a partition's copy of the root's key cannot simply keep the
/// root's name.
fn generated_key_name(
    builder: &ParserDBBuilder,
    child: &CreateTable,
    stem: &str,
    taken: &mut Vec<Ident>,
) -> Ident {
    let table = super::last_str(&child.name);
    let base = format!("{table}{stem}");
    let schema = super::table_schema_qualifier(child);
    // A generated name follows the quoting of the table it is built from, so
    // one built off a quoted name stays reachable under the spelling it needs.
    let spell = |value: &str| {
        if child.table_name_is_quoted() { Ident::with_quote('"', value) } else { Ident::new(value) }
    };

    let mut candidate = spell(&base);
    let mut counter = 0u32;
    while super::relation_name_holder(builder, &candidate, schema).is_some()
        || taken.iter().any(|held| super::idents_match(held, &candidate))
    {
        counter += 1;
        candidate = spell(&format!("{base}{counter}"));
    }

    taken.push(candidate.clone());
    candidate
}

/// The copy of a parent's constraint the child holds, or [`None`] when the
/// spelling of the edge keeps it with the parent.
///
/// A partition's copy of a key is given a name of its own, because the name of
/// a key is the name of an index. A check and a foreign key take no index
/// name, so their copies keep the parent's, which is what makes one of them
/// recognisable across a whole hierarchy.
pub(super) fn received_constraint(
    builder: &ParserDBBuilder,
    kind: ParentKind,
    child: &CreateTable,
    constraint: &TableConstraint,
    taken: &mut Vec<Ident>,
) -> Option<TableConstraint> {
    if !passes_down(kind, constraint) {
        return None;
    }

    let mut copy = constraint.clone();
    match &mut copy {
        TableConstraint::PrimaryKey(primary_key) => {
            primary_key.name = Some(generated_key_name(builder, child, "_pkey", taken));
            primary_key.index_name = None;
        }
        TableConstraint::Unique(unique) => {
            let stem = format!("_{}_key", index_name_columns(&unique.columns).join("_"));
            unique.name = Some(generated_key_name(builder, child, &stem, taken));
            unique.index_name = None;
        }
        _ => {}
    }
    Some(copy)
}

/// The constraint with the name a copy would have been given stripped away.
///
/// A partition's copy of a key differs from the root's only in that name, so
/// recognising the copy has to look past it. Every other kind keeps the
/// parent's name and is compared whole.
fn without_generated_name(constraint: &TableConstraint) -> TableConstraint {
    let mut bare = constraint.clone();
    match &mut bare {
        TableConstraint::PrimaryKey(primary_key) => {
            primary_key.name = None;
            primary_key.index_name = None;
        }
        TableConstraint::Unique(unique) => {
            unique.name = None;
            unique.index_name = None;
        }
        _ => {}
    }
    bare
}

/// Whether the constraint a table holds is its copy of the one a parent passes
/// down.
pub(super) fn is_copy_of(held: &TableConstraint, passed: &TableConstraint) -> bool {
    without_generated_name(held) == without_generated_name(passed)
}

/// The name the constraint is declared under, when the kind writes one.
pub(super) fn declared_name(constraint: &TableConstraint) -> Option<&Ident> {
    match constraint {
        TableConstraint::Check(check) => check.name.as_ref(),
        TableConstraint::Unique(unique) => unique.name.as_ref(),
        TableConstraint::PrimaryKey(primary_key) => primary_key.name.as_ref(),
        TableConstraint::ForeignKey(foreign_key) => foreign_key.name.as_ref(),
        _ => None,
    }
}

/// Whether the table already carries its copy of the constraint.
fn holds_constraint(create_table: &CreateTable, candidate: &TableConstraint) -> bool {
    create_table.constraints.iter().any(|existing| is_copy_of(existing, candidate))
}

/// Whether a parent still passes the table its copy of the constraint.
///
/// Read from the parents rather than counted, so a table with two parents
/// keeps the constraint until the last of them has dropped it, and a table
/// that redeclared it is still refused the right to drop it.
pub(super) fn receives_constraint(
    builder: &ParserDBBuilder,
    child: &CreateTable,
    held: &TableConstraint,
) -> bool {
    parents_with_kind(child).any(|(kind, name)| {
        builder.resolve_bound_table_name(name).ok().flatten().is_some_and(|parent| {
            parent
                .constraints
                .iter()
                .any(|passed| passes_down(kind, passed) && is_copy_of(held, passed))
        })
    })
}

/// Records that a table has just received a constraint from a parent.
pub(super) fn mark_inherited_constraint(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    constraint: &TableConstraint,
) {
    let case = builder.identifier_case();
    if let Some((_, metadata)) =
        builder.tables_mut().iter_mut().find(|(stored, _)| table.matches(stored, case))
    {
        let mut held = metadata.inherited_constraints().to_vec();
        held.push(constraint.to_string());
        metadata.set_inherited_constraints(held);
    }
}

/// Records that a constraint a table received is now its own, which is what an
/// `ONLY` drop on the parent leaves behind.
pub(super) fn unmark_inherited_constraint(
    builder: &mut ParserDBBuilder,
    table: &StoredTable,
    constraint: &TableConstraint,
) {
    let case = builder.identifier_case();
    if let Some((_, metadata)) =
        builder.tables_mut().iter_mut().find(|(stored, _)| table.matches(stored, case))
    {
        let rendered = constraint.to_string();
        let held: Vec<String> = metadata
            .inherited_constraints()
            .iter()
            .filter(|recorded| **recorded != rendered)
            .cloned()
            .collect();
        metadata.set_inherited_constraints(held);
    }
}

/// Whether the table received the constraint from a parent rather than
/// declaring it.
pub(super) fn records_inherited_constraint(
    builder: &ParserDBBuilder,
    table: &StoredTable,
    constraint: &TableConstraint,
) -> bool {
    let rendered = constraint.to_string();
    builder
        .tables()
        .iter()
        .find(|(stored, _)| table.matches(stored, builder.identifier_case()))
        .is_some_and(|(_, metadata)| metadata.inherited_constraints().contains(&rendered))
}

/// Carries the record of which constraints a table received across a rebuild
/// of its node.
///
/// An edit that rewrites the list in place, which is what a column rename
/// reaching into a check expression does, keeps its length and its order, so
/// the record follows position. An edit that adds or removes one leaves the
/// survivors untouched, so those are recognised by their rendering.
pub(super) fn follow_constraint_rewrite(
    recorded: &[String],
    previous: &[TableConstraint],
    replacement: &[TableConstraint],
) -> Vec<String> {
    let inherited = |index: usize, constraint: &TableConstraint| {
        if previous.len() == replacement.len() {
            previous
                .get(index)
                .is_some_and(|before| recorded.iter().any(|held| *held == before.to_string()))
        } else {
            recorded.iter().any(|held| *held == constraint.to_string())
        }
    };

    replacement
        .iter()
        .enumerate()
        .filter(|(index, constraint)| inherited(*index, constraint))
        .map(|(_, constraint)| constraint.to_string())
        .collect()
}

fn inherited_column_metadata(
    builder: &ParserDBBuilder,
    parent: &CreateTable,
    source: &ColumnDef,
    copy: &ColumnDef,
) -> Option<super::PreservedColumnMetadata> {
    builder
        .columns()
        .iter()
        .find(|(stored, _)| {
            stored.table() == parent && same_column(&stored.attribute().name, &source.name)
        })
        .map(|(_, metadata)| {
            super::PreservedColumnMetadata {
                column_name: copy.name.value.clone(),
                column_name_quoted: copy.name.quote_style.is_some(),
                collation: super::stored_column_collation_name(copy).cloned(),
                metadata: metadata.clone(),
            }
        })
}

fn inherited_column_metadata_for<'a>(
    column: &Ident,
    metadata: &'a [super::PreservedColumnMetadata],
) -> Option<&'a super::PreservedColumnMetadata> {
    metadata.iter().find(|metadata| {
        super::identifiers_match(
            &metadata.column_name,
            metadata.column_name_quoted,
            &column.value,
            column.quote_style.is_some(),
        )
    })
}

struct ParentApplication<'a> {
    builder: &'a ParserDBBuilder,
    parents: &'a [(ParentKind, ObjectName)],
    child_name: &'a str,
    collations: &'a [super::CreatedCollationMetadata],
    catalog: &'a super::PostgresCatalog,
    search_path: &'a [(String, bool)],
}

impl<'a> ParentApplication<'a> {
    fn new(
        builder: &'a ParserDBBuilder,
        parents: &'a [(ParentKind, ObjectName)],
        child_name: &'a str,
        collations: &'a [super::CreatedCollationMetadata],
        catalog: &'a super::PostgresCatalog,
        search_path: &'a [(String, bool)],
    ) -> Self {
        Self { builder, parents, child_name, collations, catalog, search_path }
    }
}

fn local_collation_conflict(
    context: &ParentApplication<'_>,
    column: &ColumnDef,
    inherited_metadata: &super::PreservedColumnMetadata,
) -> Result<Option<Error>, Error> {
    let local_collation = super::stored_column_collation_name(column);
    let local_metadata = super::column_metadata_for_collations(
        column,
        context.collations,
        context.catalog,
        context.search_path,
        &[],
        super::should_validate_missing_collations(*context.builder.dialect()),
    )?;
    if inherited_metadata.metadata.postgres_collation_matches(&local_metadata) != Some(false) {
        return Ok(None);
    }
    Ok(Some(Error::InheritedColumnCollationConflict {
        column_name: column.name.value.clone().into_boxed_str(),
        child_table: context.child_name.to_string().into_boxed_str(),
        child_collation: local_collation
            .map_or_else(|| "default".to_string(), ObjectName::to_string)
            .into_boxed_str(),
        parent_table: parent_owning(context.builder, context.parents, &column.name)
            .unwrap_or_else(|| "a parent".to_string())
            .into_boxed_str(),
        parent_collation: inherited_metadata
            .collation
            .as_ref()
            .map_or_else(|| "default".to_string(), ObjectName::to_string)
            .into_boxed_str(),
    }))
}

fn inherited_collation_conflict(
    context: &ParentApplication<'_>,
    column: &Ident,
    inherited_metadata: &super::PreservedColumnMetadata,
    incoming_metadata: &super::PreservedColumnMetadata,
) -> Option<Error> {
    if inherited_metadata.metadata.postgres_collation_matches(&incoming_metadata.metadata)
        != Some(false)
    {
        return None;
    }
    Some(Error::InheritedColumnCollationConflict {
        column_name: column.value.clone().into_boxed_str(),
        child_table: context.child_name.to_string().into_boxed_str(),
        child_collation: incoming_metadata
            .collation
            .as_ref()
            .map_or_else(|| "default".to_string(), ObjectName::to_string)
            .into_boxed_str(),
        parent_table: parent_owning(context.builder, context.parents, column)
            .unwrap_or_else(|| "a parent".to_string())
            .into_boxed_str(),
        parent_collation: inherited_metadata
            .collation
            .as_ref()
            .map_or_else(|| "default".to_string(), ObjectName::to_string)
            .into_boxed_str(),
    })
}

fn merge_local_columns(
    context: &ParentApplication<'_>,
    local_columns: &[ColumnDef],
    columns: &mut Vec<ColumnDef>,
    column_metadata: &[super::PreservedColumnMetadata],
) -> Result<(), Error> {
    for column in local_columns {
        if let Some(position) =
            columns.iter().position(|held| same_column(&held.name, &column.name))
        {
            if types_conflict(&columns[position].data_type, &column.data_type) {
                return Err(Error::InheritedColumnTypeConflict {
                    column_name: column.name.value.clone(),
                    child_table: context.child_name.to_string(),
                    child_type: column.data_type.to_string(),
                    parent_table: parent_owning(context.builder, context.parents, &column.name)
                        .unwrap_or_else(|| "a parent".to_string()),
                    parent_type: columns[position].data_type.to_string(),
                });
            }
            if let Some(inherited_metadata) =
                inherited_column_metadata_for(&column.name, column_metadata)
                && let Some(error) = local_collation_conflict(context, column, inherited_metadata)?
            {
                return Err(error);
            }
            columns[position] = merge_declarations(&columns[position], column);
        } else {
            columns.push(column.clone());
        }
    }
    Ok(())
}

/// What a table took from its parents rather than declaring itself.
#[derive(Default)]
pub(super) struct Inherited {
    /// The names of the columns received.
    pub(super) columns: Vec<String>,
    /// The rendering of each table constraint received.
    pub(super) constraints: Vec<String>,
    /// Metadata copied with inherited columns.
    pub(super) column_metadata: Vec<super::PreservedColumnMetadata>,
}

/// Gives the node the columns and constraints its parents pass down, answering
/// what it took from a parent rather than declaring itself.
///
/// Runs while the `CREATE TABLE` statement is applied, which is when
/// PostgreSQL copies the parent's shape into the child. The parent's own node
/// is already complete by then, because a parent has to exist before the
/// table naming it, so a grandparent's columns arrive through the parent and
/// no separate walk up the chain is needed.
///
/// # Errors
///
/// Returns [`Error::ParentTableNotFound`] when a parent was never created and
/// [`Error::InheritedColumnTypeConflict`] when the child redeclares an
/// inherited column with a different type.
pub(super) fn apply_parents(
    builder: &ParserDBBuilder,
    create_table: &mut CreateTable,
    collations: &[super::CreatedCollationMetadata],
    catalog: &super::PostgresCatalog,
) -> Result<Inherited, Error> {
    let child_name = create_table.name.to_string();
    bind_parents(builder, create_table, &child_name)?;
    let parents: Vec<(ParentKind, ObjectName)> =
        parents_with_kind(create_table).map(|(kind, name)| (kind, name.clone())).collect();
    if parents.is_empty() {
        return Ok(Inherited::default());
    }

    let local_collations: Vec<Ident> = create_table
        .columns
        .iter()
        .filter(|column| super::stored_column_collation_name(column).is_some())
        .map(|column| column.name.clone())
        .collect();

    let local: Vec<Ident> = create_table.columns.iter().map(|column| column.name.clone()).collect();
    // The child is not in the stores yet, so the names it introduces itself
    // have to be spoken for before a generated one is built beside them.
    let mut taken: Vec<Ident> =
        super::relation_names_of(create_table).into_iter().map(|(_, name)| name.clone()).collect();
    let search_path: Vec<_> =
        builder.search_path().map(|(schema, quoted)| (schema.to_string(), quoted)).collect();

    let mut columns: Vec<ColumnDef> = Vec::new();
    let mut column_metadata: Vec<super::PreservedColumnMetadata> = Vec::new();
    let mut constraints: Vec<TableConstraint> = Vec::new();

    let context =
        ParentApplication::new(builder, &parents, &child_name, collations, catalog, &search_path);

    for (kind, parent_name) in &parents {
        let parent = resolve_parent(builder, parent_name, &child_name)?;

        for column in &parent.columns {
            // A second parent declaring the same column merges into the one
            // already taken, keeping the position the first parent gave it.
            if !columns.iter().any(|held| same_column(&held.name, &column.name)) {
                let inherited = inherited_column(builder, *kind, create_table, column, &mut taken);
                if let Some(metadata) =
                    inherited_column_metadata(builder, parent, column, &inherited)
                {
                    column_metadata.push(metadata);
                }
                columns.push(inherited);
            } else if let Some(inherited_metadata) =
                inherited_column_metadata_for(&column.name, &column_metadata)
                && let Some(incoming_metadata) =
                    inherited_column_metadata(builder, parent, column, column)
                && let Some(error) = inherited_collation_conflict(
                    &context,
                    &column.name,
                    inherited_metadata,
                    &incoming_metadata,
                )
            {
                return Err(error);
            }
        }

        for constraint in &parent.constraints {
            if holds_constraint(create_table, constraint)
                || constraints.iter().any(|held| is_copy_of(held, constraint))
            {
                continue;
            }
            let Some(copy) =
                received_constraint(builder, *kind, create_table, constraint, &mut taken)
            else {
                continue;
            };
            // A child holding an exact copy was merged above, so a name still
            // taken here belongs to a constraint the arriving one cannot merge
            // with, which PostgreSQL refuses.
            if let Some(name) = declared_name(&copy)
                && (create_table
                    .constraints
                    .iter()
                    .any(|held| super::table_constraint_has_name(held, name))
                    || constraints.iter().any(|held| super::table_constraint_has_name(held, name)))
            {
                return Err(Error::InheritedConstraintConflict {
                    table_name: child_name.clone(),
                    constraint_name: name.value.clone(),
                });
            }
            constraints.push(copy);
        }
    }

    merge_local_columns(&context, &create_table.columns, &mut columns, &column_metadata)?;

    column_metadata.retain(|metadata| {
        !local_collations.iter().any(|local| {
            super::identifiers_match(
                &metadata.column_name,
                metadata.column_name_quoted,
                &local.value,
                local.quote_style.is_some(),
            )
        })
    });

    // Which columns and constraints came from a parent is not spelled by the
    // node once they join it, so the caller records it beside them, the way
    // `pg_attribute.attislocal` and `pg_constraint.conislocal` do.
    let inherited = Inherited {
        columns: columns
            .iter()
            .map(|column| &column.name)
            .filter(|name| !local.iter().any(|declared| same_column(declared, name)))
            .map(|name| name.value.clone())
            .collect(),
        constraints: constraints.iter().map(TableConstraint::to_string).collect(),
        column_metadata,
    };

    create_table.columns = columns;
    create_table.constraints.extend(constraints);

    Ok(inherited)
}

/// Whether the option is one a column may state only once.
///
/// A check may be written any number of times, each becoming a constraint of
/// its own, so several stand together. Everything else describes the column and
/// a second one would contradict the first.
fn stated_once(option: &ColumnOption) -> bool {
    !matches!(option, ColumnOption::Check(_))
}

/// Merges the column a parent passes down with the child's redeclaration of it.
///
/// A redeclared column follows the same rule as an inherited one, because the
/// copy handed in here has already been filtered to what the link passes down.
/// The child's declaration wins on anything that can be stated only once, which
/// is what PostgreSQL leaves: a parent's default gives way to the child's,
/// while both checks stand under their own names.
fn merge_declarations(received: &ColumnDef, child: &ColumnDef) -> ColumnDef {
    let mut merged = child.clone();

    for option in &received.options {
        let already_stated = if stated_once(&option.option) {
            merged.options.iter().any(|held| {
                core::mem::discriminant(&held.option) == core::mem::discriminant(&option.option)
            })
        } else {
            merged.options.iter().any(|held| held.option == option.option)
        };
        if !already_stated {
            merged.options.push(option.clone());
        }
    }

    merged
}

/// Names the parent a column reaches the child from, for the conflict message.
fn parent_owning(
    builder: &ParserDBBuilder,
    parents: &[(ParentKind, ObjectName)],
    column: &Ident,
) -> Option<String> {
    parents
        .iter()
        .filter_map(|(_, name)| builder.resolve_bound_table_name(name).ok().flatten())
        .find(|parent| parent.columns.iter().any(|held| same_column(&held.name, column)))
        .map(|parent| parent.name.to_string())
}

/// Binds every parent a table being created names to the table it reaches
/// through the session's path, refusing an absent one.
fn bind_parents(
    builder: &ParserDBBuilder,
    create_table: &mut CreateTable,
    child_name: &str,
) -> Result<(), Error> {
    for parent_name in parent_names_mut(create_table) {
        let Some(parent) = builder.resolve_table_object_name(parent_name)? else {
            return Err(Error::ParentTableNotFound {
                parent_table: parent_name.to_string(),
                child_table: child_name.to_string(),
            });
        };
        let qualifier = super::stored_qualifier(&parent.name);
        super::bind_reference(parent_name, qualifier);
    }
    Ok(())
}

/// Resolves one parent name the child already bound.
fn resolve_parent<'builder>(
    builder: &'builder ParserDBBuilder,
    parent_name: &ObjectName,
    child_name: &str,
) -> Result<&'builder CreateTable, Error> {
    builder.resolve_bound_table_name(parent_name)?.ok_or_else(|| {
        Error::ParentTableNotFound {
            parent_table: parent_name.to_string(),
            child_table: child_name.to_string(),
        }
    })
}

/// Whether the table receives the column from a parent rather than declaring
/// it.
pub(super) fn is_inherited_column(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    column: &Ident,
) -> bool {
    parent_names(table)
        .filter_map(|name| builder.resolve_bound_table_name(name).ok().flatten())
        .any(|parent| parent.columns.iter().any(|held| same_column(&held.name, column)))
}
