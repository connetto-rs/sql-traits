//! The `NEW` columns a trigger's PL/pgSQL body assigns before returning `NEW`,
//! each with the one expression it holds when the body returns.

use alloc::{string::ToString, vec::Vec};
use core::{ops::ControlFlow, slice};

use sqlparser::ast::{Expr, Ident};
use sqlparser_plpgsql::{
    embedded::Node,
    embedded_sql,
    namespace::{NameKind, Namespace, TriggerVariable},
    new_row::{FieldValue, NotFolded},
};

use super::dynamic_sites::BodyNames;
use crate::{
    errors::LookupError,
    structs::{BodyExcerpt, IdentifierCase, MaintenanceRefusal},
    traits::{ColumnLike, DatabaseLike, FunctionLike, Relation, TableLike, TriggerLike},
    utils::{identifier_resolution::stored_identifier, walk::walk_expressions},
};

/// The columns a maintenance body assigns, each with the expression it holds
/// when the body returns, in the order the body first assigns them.
pub(crate) type MaintenanceAssignments<'db, DB> = Vec<(&'db <DB as DatabaseLike>::Column, Expr)>;

/// The assignments of the PL/pgSQL body `trigger` runs, or what makes it no
/// maintenance body.
///
/// # Errors
///
/// Returns the [`LookupError`] that resolving the relation of `trigger`, or
/// reading its function's `search_path` setting or the columns of the table
/// from `database`, raises.
pub(crate) fn maintenance_assignments<'db, T: TriggerLike>(
    trigger: &'db T,
    database: &'db T::DB,
) -> Result<Result<MaintenanceAssignments<'db, T::DB>, MaintenanceRefusal>, LookupError> {
    let Some((function, body)) =
        trigger.function(database).and_then(|function| Some((function, function.body()?)))
    else {
        return Ok(Err(MaintenanceRefusal::NoBody));
    };
    let table = match trigger.relation(database)? {
        Relation::Table(table) | Relation::ForeignTable(table) => table,
        Relation::View(_) | Relation::MaterializedView(_) | Relation::Sequence(_) => {
            return Ok(Err(MaintenanceRefusal::OnView));
        }
    };
    body_assignments(function, body, table, database)
}

/// The assignments of `body`, the PL/pgSQL body of `function` run by a
/// trigger on `table`, or what makes it no maintenance body.
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
fn body_assignments<'db, DB: DatabaseLike>(
    function: &'db DB::Function,
    body: &str,
    table: &'db DB::Table,
    database: &'db DB,
) -> Result<Result<MaintenanceAssignments<'db, DB>, MaintenanceRefusal>, LookupError> {
    let block = match embedded_sql(body) {
        Ok(block) => block,
        Err(error) => {
            return Ok(Err(MaintenanceRefusal::UnparsableBody { reason: error.to_string() }));
        }
    };
    let names = BodyNames::new(function, database)?;
    let signature = names.signature();
    let fields = match block.fields_of_new(&signature) {
        Ok(fields) => fields,
        Err(refused) => return Ok(Err(refusal(body, refused))),
    };
    // A folded body has no block or loop, so the last site's names are every
    // statement's.
    let namespace = block
        .sites(&signature, |name| names.holds_row(name))
        .ok()
        .and_then(|mut sites| sites.pop())
        .map(|site| site.namespace);
    let columns: Vec<&'db DB::Column> = table.columns(database)?.collect();
    let reader = Reader { columns, names: &names, namespace };
    Ok(fields
        .into_iter()
        .map(|FieldValue { field, value }| {
            let Some(column) = reader.column(&field) else {
                return Err(MaintenanceRefusal::UndeclaredColumn { field });
            };
            match reader.undeclared_read(&value) {
                Some(field) => Err(MaintenanceRefusal::UndeclaredColumn { field }),
                None => Ok((column, value)),
            }
        })
        .collect())
}

/// The refusal `refused` names, its statements quoted from `body`.
fn refusal(body: &str, refused: NotFolded<'_>) -> MaintenanceRefusal {
    let excerpt = |node: &Node| BodyExcerpt::new(body, node.span);
    match refused {
        NotFolded::Declaration(declaration) => {
            MaintenanceRefusal::Declaration(declaration.name.clone())
        }
        NotFolded::Handler(_) => MaintenanceRefusal::Handler,
        NotFolded::Statement(node) => MaintenanceRefusal::Statement(excerpt(node)),
        NotFolded::EarlyReturn(node) => MaintenanceRefusal::EarlyReturn(excerpt(node)),
        NotFolded::NoReturnOfNew(node) => MaintenanceRefusal::NoReturnOfNew(excerpt(node)),
        NotFolded::NoStatement => MaintenanceRefusal::NoStatement,
        NotFolded::Read { statement, read } => {
            MaintenanceRefusal::ReadAfterAssignment { read, statement: excerpt(statement) }
        }
        NotFolded::NothingAssigned => MaintenanceRefusal::NothingAssigned,
    }
}

/// Resolves the names a folded body reads against the trigger's table.
struct Reader<'names, 'db, DB: DatabaseLike> {
    /// The columns of the trigger's table.
    columns: Vec<&'db DB::Column>,
    /// The names of the trigger function's body.
    names: &'names BodyNames<'db, DB>,
    /// The names every statement of the body reaches, [`None`] for a body
    /// with no statement.
    namespace: Option<Namespace>,
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

    /// The first field of `NEW` that `value` reads and the table does not
    /// declare.
    fn undeclared_read(&self, value: &Expr) -> Option<Ident> {
        walk_expressions(value, |read| {
            match name_parts(read).and_then(|parts| self.after_new(parts)) {
                Some([field, ..]) if self.column(field).is_none() => {
                    ControlFlow::Break(field.clone())
                }
                _ => ControlFlow::Continue(()),
            }
        })
        .break_value()
    }

    /// The parts of the name `parts` writes after those that reach `NEW`, or
    /// [`None`] when it does not start with `NEW`.
    fn after_new<'parts>(&self, parts: &'parts [Ident]) -> Option<&'parts [Ident]> {
        let stored: Vec<_> = parts
            .iter()
            .map(|part| stored_identifier(&part.value, part.quote_style.is_some()))
            .collect();
        let (name, consumed) =
            self.namespace.as_ref()?.lookup(&stored, |name| self.names.holds_row(name))?;
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
