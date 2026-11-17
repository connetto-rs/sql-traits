//! The `NEW` columns a trigger's PL/pgSQL body assigns before returning `NEW`,
//! each with the one expression it holds when the body returns.

use alloc::vec::Vec;
use core::{ops::ControlFlow, slice};

use sqlparser::ast::{Expr, Ident};
use sqlparser_plpgsql::{
    embedded_sql,
    namespace::{NameKind, Namespace, TriggerVariable},
    new_row::FieldValue,
};

use super::dynamic_sites::BodyNames;
use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{ColumnLike, DatabaseLike, FunctionLike, TableLike},
    utils::{identifier_resolution::stored_identifier, walk::walk_expressions},
};

/// The columns a maintenance body assigns, each with the expression it holds
/// when the body returns, in the order the body first assigns them.
pub(crate) type MaintenanceAssignments<'db, DB> = Vec<(&'db <DB as DatabaseLike>::Column, Expr)>;

/// The assignments of the PL/pgSQL body of `function`, run by a trigger on
/// `table`, or [`None`] when it is no maintenance body.
///
/// A maintenance body is one whose fields of `NEW`
/// [`Block::fields_of_new`](sqlparser_plpgsql::embedded::Block::fields_of_new)
/// folds, each field a column of `table`. Every expression reads `NEW` as it
/// was before the body ran, and reading a field of `NEW` the table does not
/// declare refuses the body.
///
/// # Errors
///
/// Returns the [`LookupError`] that reading the function's own `search_path`
/// setting or the columns of `table` from `database` raises.
pub(crate) fn maintenance_assignments<'db, DB: DatabaseLike>(
    function: &'db DB::Function,
    table: &'db DB::Table,
    database: &'db DB,
) -> Result<Option<MaintenanceAssignments<'db, DB>>, LookupError> {
    let Some(Ok(block)) = function.body().map(embedded_sql) else {
        return Ok(None);
    };
    let names = BodyNames::new(function, database)?;
    let signature = names.signature();
    let Some(fields) = block.fields_of_new(&signature) else {
        return Ok(None);
    };
    let columns: Vec<&'db DB::Column> = table.columns(database)?.collect();
    // A body `fields_of_new` folds declares nothing and holds no loop or
    // nested block, so the names `RETURN` reads, the last site, are those of
    // every statement.
    let Some(namespace) = block
        .sites(&signature, |name| names.holds_row(name))
        .ok()
        .and_then(|mut sites| sites.pop())
        .map(|site| site.namespace)
    else {
        return Ok(None);
    };
    let reader = Reader { columns, names: &names, namespace };
    Ok(fields
        .into_iter()
        .map(|FieldValue { field, value }| {
            let column = reader.column(&field)?;
            reader.reads_columns(&value).then_some((column, value))
        })
        .collect())
}

/// Resolves the names a folded body reads against the trigger's table.
struct Reader<'names, 'db, DB: DatabaseLike> {
    /// The columns of the trigger's table.
    columns: Vec<&'db DB::Column>,
    /// The names of the trigger function's body.
    names: &'names BodyNames<'db, DB>,
    /// The names every statement of the body reaches.
    namespace: Namespace,
}

impl<'db, DB: DatabaseLike> Reader<'_, 'db, DB> {
    /// The column of the table `name` denotes, as PostgreSQL compares
    /// identifiers.
    fn column(&self, name: &Ident) -> Option<&'db DB::Column> {
        self.columns.iter().copied().find(|column| {
            IdentifierCase::AsWritten.identifiers_match(
                column.column_name(),
                column.column_name_is_quoted(),
                &name.value,
                name.quote_style.is_some(),
            )
        })
    }

    /// Whether every field of `NEW` that `value` reads is a column of the
    /// table.
    fn reads_columns(&self, value: &Expr) -> bool {
        walk_expressions(value, |read| {
            match name_parts(read).and_then(|parts| self.after_new(parts)) {
                Some([field, ..]) if self.column(field).is_none() => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        })
        .is_continue()
    }

    /// The parts of the name `parts` writes after those that reach `NEW`, or
    /// [`None`] when it does not start with `NEW`.
    fn after_new<'parts>(&self, parts: &'parts [Ident]) -> Option<&'parts [Ident]> {
        let stored: Vec<_> = parts
            .iter()
            .map(|part| stored_identifier(&part.value, part.quote_style.is_some()))
            .collect();
        let (name, consumed) = self.namespace.lookup(&stored, |name| self.names.holds_row(name))?;
        if matches!(name.kind(), NameKind::Trigger(TriggerVariable::New)) {
            parts.get(consumed..)
        } else {
            None
        }
    }
}

/// The parts of the name `read` writes, when it is a name.
fn name_parts(read: &Expr) -> Option<&[Ident]> {
    match read {
        Expr::Identifier(name) => Some(slice::from_ref(name)),
        Expr::CompoundIdentifier(parts) => Some(parts),
        _ => None,
    }
}
