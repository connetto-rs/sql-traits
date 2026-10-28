//! The columns a table copies from another with `LIKE`.
//!
//! `CREATE TABLE copy (LIKE original)` duplicates the original's columns at
//! the point the statement runs and links nothing, so the copy is an ordinary
//! table afterwards and a later change to the original does not reach it.
//! That is what separates it from `INHERITS`, which keeps a lasting edge.
//!
//! Measured against PostgreSQL 18.4 rather than read from documentation: the
//! copy receives each column's name, type, collation and `NOT NULL`, and
//! nothing else. A default arrives only under `INCLUDING DEFAULTS`. Primary
//! keys, unique constraints, checks, foreign keys, identities, stored
//! generated expressions, indexes and comments all stay with the original,
//! while the `NOT NULL` that a primary key or an identity implied is kept.
//! A serial column arrives as its integer type, drawing from the original's
//! sequence only under `INCLUDING DEFAULTS`.

use alloc::{string::ToString, vec::Vec};

use sqlparser::ast::{
    ColumnDef, ColumnOption, CreateTable, CreateTableLikeDefaults, CreateTableLikeKind, ObjectName,
};

use super::{ParserDBBuilder, column_copy::copy_column, sequences};
use crate::errors::Error;

/// The table a `LIKE` clause copies from, and whether it asks for defaults.
fn source(create_table: &CreateTable) -> Option<(&ObjectName, bool)> {
    let (CreateTableLikeKind::Parenthesized(like) | CreateTableLikeKind::Plain(like)) =
        create_table.like.as_ref()?;
    Some((&like.name, matches!(like.defaults, Some(CreateTableLikeDefaults::Including))))
}

/// Whether the option describes the column itself rather than a constraint,
/// a sequence or a comment the copy does not receive.
///
/// An allow list rather than a deny list, because a copy keeps very little
/// and an unfamiliar option is far more likely to be something the original
/// owns than something the copy should carry.
fn is_copied_option(option: &ColumnOption, with_defaults: bool) -> bool {
    match option {
        ColumnOption::NotNull | ColumnOption::Collation(_) | ColumnOption::CharacterSet(_) => true,
        ColumnOption::Default(_) => with_defaults,
        _ => false,
    }
}

/// Gives the node the columns it copies from the table its `LIKE` names.
///
/// Runs while the `CREATE TABLE` statement is applied, which is when
/// PostgreSQL performs the copy. The copied columns become the table's own,
/// so a `LIKE` inside a table that also inherits contributes to the columns
/// the child declares rather than to the ones it receives from a parent.
///
/// # Errors
///
/// Returns [`Error::CopiedTableNotFound`] when the `LIKE` names a table the
/// input never created, which PostgreSQL refuses too.
pub(super) fn apply_like(
    builder: &ParserDBBuilder,
    create_table: &mut CreateTable,
) -> Result<(), Error> {
    let Some((source_name, with_defaults)) = source(create_table) else {
        return Ok(());
    };

    let Some(copied) = builder.resolve_table_object_name(source_name)? else {
        return Err(Error::CopiedTableNotFound {
            copied_table: source_name.to_string(),
            table_name: create_table.name.to_string(),
        });
    };

    let mut columns: Vec<ColumnDef> = copied
        .columns
        .iter()
        .map(|column| {
            let mut copy = copy_column(column, |option| is_copied_option(option, with_defaults));
            if let Some(drawn) = sequences::serial_default(builder, copied, column) {
                sequences::spell_out_serial(&mut copy, with_defaults.then_some(drawn));
            }
            copy
        })
        .collect();

    // The clause sits among the column list, so anything written beside it
    // keeps its place relative to the copy. Only the parenthesized spelling
    // can share the list, and upstream does not parse that combination yet,
    // so in practice this appends to nothing.
    columns.append(&mut create_table.columns);
    create_table.columns = columns;

    Ok(())
}
