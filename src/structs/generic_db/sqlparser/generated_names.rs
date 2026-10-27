//! The names PostgreSQL gives indexes it is not told a name for, following
//! `ChooseIndexColumnNames`, `ChooseIndexNameAddition`, `makeObjectName` and
//! `ChooseRelationName`.

use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::ast::{CreateIndex, CreateTable, Ident, IndexColumn};

use super::{ParserDBBuilder, relation_name_holder, table_schema_qualifier};
use crate::{
    traits::TableLike,
    utils::{
        figured_names::{Figure, figure},
        identifier_resolution::normalize_identifier,
    },
};

/// The most bytes PostgreSQL keeps of a name, `NAMEDATALEN` less one.
const MAX_NAME_BYTES: usize = 63;

/// The longest prefix of `name` within `limit` bytes ending on a character
/// boundary, as `pg_mbcliplen` cuts.
fn clipped(name: &str, limit: usize) -> &str {
    let mut end = limit.min(name.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// The name PostgreSQL stores for a written identifier, cut as its scanner cuts
/// it.
fn stored(value: &str, quoted: bool) -> String {
    let mut name = normalize_identifier(value, quoted).into_owned();
    name.truncate(clipped(&name, MAX_NAME_BYTES).len());
    name
}

/// An identifier reaching exactly the stored name `value`, bare only when
/// folding leaves it unchanged.
fn spelled(value: String) -> Ident {
    // Never a keyword bare, since a generated name ends in `_` and a label.
    let mut bytes = value.bytes();
    let bare = bytes.next().is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if bare { Ident::new(value) } else { Ident::with_quote('"', value) }
}

/// The column part of the name of an index over `columns` and `include`, or
/// [`None`] when a key column is an expression whose name is not modeled.
pub(super) fn index_columns_part(columns: &[IndexColumn], include: &[Ident]) -> Option<String> {
    let mut names: Vec<String> = Vec::with_capacity(columns.len() + include.len());
    let figured = columns.iter().map(|column| {
        match figure(&column.column.expr) {
            Figure::Strong(name, quoted) | Figure::Weak(name, quoted) => {
                Some(stored(&name, quoted))
            }
            Figure::Unnamed => Some("expr".to_owned()),
            Figure::Unknown => None,
        }
    });
    let included =
        include.iter().map(|ident| Some(stored(&ident.value, ident.quote_style.is_some())));
    for name in figured.chain(included) {
        let name = name?;
        let mut renamed: Option<String> = None;
        let mut counter = 0u32;
        while names.iter().any(|held| *held == renamed.as_deref().unwrap_or(&name)) {
            counter += 1;
            let suffix = counter.to_string();
            renamed = Some(format!("{}{suffix}", clipped(&name, MAX_NAME_BYTES - suffix.len())));
        }
        names.push(renamed.unwrap_or(name));
    }

    let mut part = String::new();
    for name in &names {
        if !part.is_empty() {
            part.push('_');
        }
        part.push_str(name);
        // The rest would be cut anyway.
        if part.len() > MAX_NAME_BYTES {
            break;
        }
    }
    Some(part)
}

/// The column part of the name of the index behind a key one column declares
/// inline.
pub(super) fn column_part(column: &Ident) -> String {
    stored(&column.value, column.quote_style.is_some())
}

/// `name1`, the optional `name2` and `label` joined by `_`, the longer name
/// giving up a byte at a time until the whole fits.
fn object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let overhead = usize::from(name2.is_some()) + 1 + label.len();
    let available = MAX_NAME_BYTES.saturating_sub(overhead);
    let mut name1_bytes = name1.len();
    let mut name2_bytes = name2.map_or(0, str::len);
    while name1_bytes + name2_bytes > available {
        if name1_bytes > name2_bytes {
            name1_bytes -= 1;
        } else {
            name2_bytes -= 1;
        }
    }

    let mut name = String::with_capacity(MAX_NAME_BYTES);
    name.push_str(clipped(name1, name1_bytes));
    if let Some(name2) = name2 {
        name.push('_');
        name.push_str(clipped(name2, name2_bytes));
    }
    name.push('_');
    name.push_str(label);
    name
}

/// The first name for an index on `table` that no relation in its schema holds
/// and `also_taken` does not report.
pub(super) fn index_name_on(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    columns: Option<&str>,
    label: &str,
    mut also_taken: impl FnMut(&Ident) -> bool,
) -> Ident {
    let table_name = stored(table.table_name(), table.table_name_is_quoted());
    let schema = table_schema_qualifier(table);
    let mut pass = 0u32;
    loop {
        let candidate = if pass == 0 {
            spelled(object_name(&table_name, columns, label))
        } else {
            spelled(object_name(&table_name, columns, &format!("{label}{pass}")))
        };
        if relation_name_holder(builder, &candidate, schema).is_none() && !also_taken(&candidate) {
            return candidate;
        }
        pass += 1;
    }
}

/// The name of the index `create_index` creates on `table` without naming it,
/// or [`None`] when an index expression's name is not modeled.
pub(super) fn create_index_name(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    create_index: &CreateIndex,
) -> Option<Ident> {
    let columns = index_columns_part(&create_index.columns, &create_index.include)?;
    Some(index_name_on(builder, table, Some(&columns), "idx", |_| false))
}
