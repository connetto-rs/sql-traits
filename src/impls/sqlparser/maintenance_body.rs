//! The `NEW` columns a trigger's PL/pgSQL body assigns before returning `NEW`,
//! each with the one expression it holds when the body returns, and the fields
//! of `NEW` and `OLD` it names that the trigger's table does not declare.

use alloc::{string::ToString, vec::Vec};
use core::{borrow::Borrow, ops::ControlFlow};

use sqlparser::ast::{Expr, Ident, ObjectNamePart, Query};
use sqlparser_plpgsql::{
    embedded::{Branch, Node, NodeKind},
    embedded_sql,
    namespace::{NameKind, Namespace, TriggerVariable},
    new_row::{FieldValue, NotFolded},
};

use super::dynamic_sites::BodyNames;
use crate::{
    errors::LookupError,
    structs::{BodyExcerpt, IdentifierCase, MaintenanceRefusal, TriggerRow, UndeclaredRowField},
    traits::{ColumnLike, DatabaseLike, FunctionLike, Relation, TableLike, TriggerLike},
    utils::{
        identifier_resolution::{name_parts, stored_identifier},
        walk::walk_expressions,
    },
};

/// What a maintenance body leaves in the row it returns, and the fields of the
/// rows it names that the trigger's table does not declare.
pub(crate) struct MaintenanceBody<'db, DB: DatabaseLike> {
    /// The columns the body assigns, each with the expression it holds when
    /// the body returns, in the order the body first assigns them.
    pub(crate) assignments: Vec<(&'db DB::Column, Expr)>,
    /// Each naming of a field of `NEW` or `OLD` the table does not declare,
    /// in body order.
    pub(crate) undeclared: Vec<UndeclaredRowField>,
}

/// The maintenance body `trigger` runs, or what makes its body no
/// maintenance body.
///
/// # Errors
///
/// Returns the [`LookupError`] that resolving the relation of `trigger`, or
/// reading its function's `search_path` setting or the columns of the table
/// from `database`, raises.
pub(crate) fn maintenance_body<'db, T: TriggerLike>(
    trigger: &'db T,
    database: &'db T::DB,
) -> Result<Result<MaintenanceBody<'db, T::DB>, MaintenanceRefusal>, LookupError> {
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
    read_body(function, body, table, database)
}

/// The maintenance body `body` is, the PL/pgSQL body of `function` run by a
/// trigger on `table`, or what makes it no maintenance body.
///
/// A maintenance body is one whose fields of `NEW`
/// [`Block::fields_of_new`](sqlparser_plpgsql::embedded::Block::fields_of_new)
/// folds. Every expression reads `NEW` as it was before the body ran. A field
/// the table does not declare is no assignment, and PostgreSQL fails each run
/// reaching a statement that names one, so each naming is listed instead.
///
/// # Errors
///
/// Returns the [`LookupError`] that reading the function's own `search_path`
/// setting or the columns of `table` from `database` raises.
fn read_body<'db, DB: DatabaseLike>(
    function: &'db DB::Function,
    body: &str,
    table: &'db DB::Table,
    database: &'db DB,
) -> Result<Result<MaintenanceBody<'db, DB>, MaintenanceRefusal>, LookupError> {
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
    let reader = Reader { columns, names: &names, namespace, case: database.identifier_case() };
    let assignments = fields
        .into_iter()
        .filter_map(|FieldValue { field, value }| Some((reader.column(&field)?, value)))
        .collect();
    let mut undeclared = Vec::new();
    reader.undeclared_in(&block.statements, &mut undeclared);
    Ok(Ok(MaintenanceBody { assignments, undeclared }))
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
    case: IdentifierCase,
}

impl<'db, DB: DatabaseLike> Reader<'_, 'db, DB> {
    /// The column `name` denotes under the catalog's case.
    fn column(&self, name: &Ident) -> Option<&'db DB::Column> {
        self.columns.iter().copied().find(|column| {
            self.case.identifiers_match(
                column.column_name(),
                column.column_name_is_quoted(),
                &name.value,
                name.quote_style.is_some(),
            )
        })
    }

    /// Pushes onto `found` each field the table does not declare that
    /// `statements`, those of a folded body, name, in the order PostgreSQL
    /// raises for them, an assignment's target before its value and an
    /// `IF`'s conditions and branches as written.
    fn undeclared_in(&self, statements: &[Node], found: &mut Vec<UndeclaredRowField>) {
        for statement in statements {
            match &statement.kind {
                NodeKind::Assignment(assignment) => {
                    let target: Option<Vec<&Ident>> =
                        assignment.target.name.0.iter().map(ObjectNamePart::as_ident).collect();
                    found.extend(target.and_then(|parts| self.undeclared(&parts, true)));
                    self.undeclared_reads(&assignment.value, found);
                }
                NodeKind::If(conditional) => {
                    for Branch { condition, statements } in &conditional.branches {
                        self.undeclared_reads(condition, found);
                        self.undeclared_in(statements, found);
                    }
                    self.undeclared_in(&conditional.otherwise, found);
                }
                _ => {}
            }
        }
    }

    /// Pushes onto `found` each field the table does not declare that
    /// `query`, an expression of the body, reads, leftmost first.
    fn undeclared_reads(&self, query: &Query, found: &mut Vec<UndeclaredRowField>) {
        let _: ControlFlow<()> = walk_expressions(query, |read| {
            found.extend(name_parts(read).and_then(|parts| self.undeclared(parts, false)));
            ControlFlow::Continue(())
        });
    }

    /// The field of `NEW` or `OLD` the name `parts` writes, when the table
    /// does not declare it, which the body assigns when `assigned`.
    fn undeclared<P: Borrow<Ident>>(
        &self,
        parts: &[P],
        assigned: bool,
    ) -> Option<UndeclaredRowField> {
        let (row, field) = self.row_field(parts)?;
        self.column(field).is_none().then(|| UndeclaredRowField::new(row, field.clone(), assigned))
    }

    /// The row and the field of it the name `parts` writes, or [`None`] when
    /// it names no field of `NEW` or `OLD`.
    ///
    /// PL/pgSQL reads a field only from a name that ends one part after the
    /// row, and reads `NEW.a.x` as column `x` of a relation `a`.
    fn row_field<'parts, P: Borrow<Ident>>(
        &self,
        parts: &'parts [P],
    ) -> Option<(TriggerRow, &'parts Ident)> {
        let stored: Vec<_> = parts
            .iter()
            .map(|part| {
                stored_identifier(&part.borrow().value, part.borrow().quote_style.is_some())
            })
            .collect();
        let (name, consumed) =
            self.namespace.as_ref()?.lookup(&stored, |name| self.names.holds_row(name))?;
        let row = match name.kind() {
            NameKind::Trigger(TriggerVariable::New) => TriggerRow::New,
            NameKind::Trigger(TriggerVariable::Old) => TriggerRow::Old,
            _ => return None,
        };
        match parts.get(consumed..)? {
            [field] => Some((row, field.borrow())),
            _ => None,
        }
    }
}
