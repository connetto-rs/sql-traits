//! The `NEW` columns a trigger's PL/pgSQL body assigns before returning `NEW`,
//! each folded into the one expression it holds when the body returns.

use alloc::{boxed::Box, vec, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CaseWhen, Expr, Ident, ObjectNamePart, helpers::attached_token::AttachedToken,
};
use sqlparser_plpgsql::{
    embedded::{Assignment, Branch, If, Node, expression},
    embedded_sql,
};

use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{ColumnLike, DatabaseLike, TableLike},
    utils::walk::walk_expressions_mut,
};

/// The columns a maintenance body assigns, each with the expression it holds
/// when the body returns, in the order the body first assigns them.
pub(crate) type MaintenanceAssignments<'db, T> =
    Vec<(&'db <<T as TableLike>::DB as DatabaseLike>::Column, Expr)>;

/// Why a body yields no maintenance assignments.
enum Refusal {
    /// The body is not a maintenance body.
    NotMaintenance,
    /// Resolving a column against the database failed.
    Lookup(LookupError),
}

impl From<LookupError> for Refusal {
    fn from(error: LookupError) -> Self {
        Self::Lookup(error)
    }
}

/// The assignments of the PL/pgSQL `body` of a trigger on `table`, or
/// [`None`] when `body` is no maintenance body.
///
/// A maintenance body is one block without declarations or handlers whose
/// statements assign columns of `NEW`, alone or under `IF`, and whose last
/// statement is `RETURN NEW`, the body's only `RETURN`. Each column it assigns
/// yields the expression it holds when the body returns, an `IF` folded into
/// a `CASE` over its conditions in order whose `ELSE` is the column's value
/// before the `IF`. A column read through `NEW` after an assignment to it
/// reads the assigned expression, as PostgreSQL runs the statements in order,
/// so every expression reads `NEW` as it was before the body ran. Reading the
/// whole row `NEW` after an assignment cannot be written that way and refuses
/// the body.
///
/// # Errors
///
/// Returns the [`LookupError`] that reading the columns of `table` from
/// `database` raises.
pub(crate) fn maintenance_assignments<'db, T: TableLike>(
    body: &str,
    table: &'db T,
    database: &'db T::DB,
) -> Result<Option<MaintenanceAssignments<'db, T>>, LookupError> {
    let Ok(block) = embedded_sql(body) else {
        return Ok(None);
    };
    if !block.declarations.is_empty() || !block.handlers.is_empty() {
        return Ok(None);
    }
    let Some((Node::Return { value: Some(returned) }, statements)) = block.statements.split_last()
    else {
        return Ok(None);
    };
    if !matches!(&*expression(returned), Expr::Identifier(name) if is_new(name)) {
        return Ok(None);
    }
    let reader = Reader { table, database };
    let mut state = Vec::new();
    match reader.run(statements, &mut state) {
        Ok(()) if !state.is_empty() => Ok(Some(state)),
        Ok(()) | Err(Refusal::NotMaintenance) => Ok(None),
        Err(Refusal::Lookup(error)) => Err(error),
    }
}

/// Whether `name` denotes the trigger's `NEW` row.
fn is_new(name: &Ident) -> bool {
    if name.quote_style.is_some() {
        name.value == "new"
    } else {
        name.value.eq_ignore_ascii_case("new")
    }
}

/// Reads the statements of a maintenance body against the trigger's table.
struct Reader<'db, T: TableLike> {
    /// The trigger's table.
    table: &'db T,
    /// The database holding `table`.
    database: &'db T::DB,
}

impl<'db, T: TableLike> Reader<'db, T> {
    /// Runs `statements` over `state`, the value each column assigned so far
    /// holds.
    fn run(
        &self,
        statements: &[Node],
        state: &mut MaintenanceAssignments<'db, T>,
    ) -> Result<(), Refusal> {
        for statement in statements {
            match statement {
                Node::Assignment(assignment) => self.assign(assignment, state)?,
                Node::If(conditional) => self.branch(conditional, state)?,
                _ => return Err(Refusal::NotMaintenance),
            }
        }
        Ok(())
    }

    /// Runs `assignment` over `state`, refusing one to anything but a column
    /// of `NEW` the table declares.
    fn assign(
        &self,
        assignment: &Assignment,
        state: &mut MaintenanceAssignments<'db, T>,
    ) -> Result<(), Refusal> {
        let target = &assignment.target;
        let [ObjectNamePart::Identifier(record), ObjectNamePart::Identifier(field)] =
            target.name.0.as_slice()
        else {
            return Err(Refusal::NotMaintenance);
        };
        if !target.indirection.is_empty() || !is_new(record) {
            return Err(Refusal::NotMaintenance);
        }
        let column = self.column(field)?.ok_or(Refusal::NotMaintenance)?;
        let value = self.read(expression(&assignment.value).into_owned(), state)?;
        set(state, column, value);
        Ok(())
    }

    /// Runs `conditional` over `state`, folding each column a branch assigns
    /// into a `CASE` over the conditions of the branches.
    fn branch(
        &self,
        conditional: &If,
        state: &mut MaintenanceAssignments<'db, T>,
    ) -> Result<(), Refusal> {
        let mut arms = Vec::with_capacity(conditional.branches.len());
        for Branch { condition, statements } in &conditional.branches {
            let condition = self.read(expression(condition).into_owned(), state)?;
            let mut arm = state.clone();
            self.run(statements, &mut arm)?;
            arms.push((condition, arm));
        }
        let mut otherwise = state.clone();
        self.run(&conditional.otherwise, &mut otherwise)?;

        let mut assigned: Vec<&'db <T::DB as DatabaseLike>::Column> = Vec::new();
        for (column, _) in arms.iter().map(|(_, arm)| arm).chain([&otherwise]).flatten() {
            if !assigned.iter().any(|seen| core::ptr::eq(*seen, *column)) {
                assigned.push(column);
            }
        }
        for column in assigned {
            let before = value_of(state, column);
            let after = |arm: &MaintenanceAssignments<'db, T>| value_of(arm, column);
            if after(&otherwise) == before && arms.iter().all(|(_, arm)| after(arm) == before) {
                continue;
            }
            let conditions = arms
                .iter()
                .map(|(condition, arm)| {
                    CaseWhen { condition: condition.clone(), result: after(arm) }
                })
                .collect();
            let folded = Expr::Case {
                case_token: AttachedToken::empty(),
                end_token: AttachedToken::empty(),
                operand: None,
                conditions,
                else_result: Some(Box::new(after(&otherwise))),
            };
            set(state, column, folded);
        }
        Ok(())
    }

    /// The column of the table `name` denotes, as PostgreSQL compares
    /// identifiers.
    fn column(
        &self,
        name: &Ident,
    ) -> Result<Option<&'db <T::DB as DatabaseLike>::Column>, LookupError> {
        Ok(self.table.columns(self.database)?.find(|column| {
            IdentifierCase::AsWritten.identifiers_match(
                column.column_name(),
                column.column_name_is_quoted(),
                &name.value,
                name.quote_style.is_some(),
            )
        }))
    }

    /// `expression` with every column of `NEW` it reads replaced by the value
    /// `state` holds for it.
    fn read(
        &self,
        mut expression: Expr,
        state: &MaintenanceAssignments<'db, T>,
    ) -> Result<Expr, Refusal> {
        match walk_expressions_mut(&mut expression, |read| {
            self.substitute(read, state).map_or_else(ControlFlow::Break, ControlFlow::Continue)
        }) {
            ControlFlow::Continue(()) => Ok(expression),
            ControlFlow::Break(refusal) => Err(refusal),
        }
    }

    /// Replaces `read`, once its subexpressions are read, by the value
    /// `state` holds when it reads a column of `NEW` assigned so far.
    fn substitute(
        &self,
        read: &mut Expr,
        state: &MaintenanceAssignments<'db, T>,
    ) -> Result<(), Refusal> {
        let value = match read {
            Expr::Identifier(name) if is_new(name) && !state.is_empty() => {
                return Err(Refusal::NotMaintenance);
            }
            Expr::CompoundIdentifier(parts) => {
                let [record, field, rest @ ..] = parts.as_slice() else {
                    return Ok(());
                };
                if !is_new(record) {
                    return Ok(());
                }
                let Some(column) = self.column(field)? else {
                    return Ok(());
                };
                let Some((_, value)) =
                    state.iter().find(|(assigned, _)| core::ptr::eq(*assigned, column))
                else {
                    return Ok(());
                };
                if !rest.is_empty() {
                    return Err(Refusal::NotMaintenance);
                }
                value.clone()
            }
            _ => return Ok(()),
        };
        *read = parenthesized(value);
        Ok(())
    }
}

/// `value` in parentheses unless it delimits itself, so that it binds as one
/// operand where it replaces a column.
fn parenthesized(value: Expr) -> Expr {
    match value {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Value(_)
        | Expr::Function(_)
        | Expr::Nested(_)
        | Expr::Case { .. } => value,
        _ => Expr::Nested(Box::new(value)),
    }
}

/// The value `column` holds in `state`, `NEW.column` when nothing assigned
/// it.
fn value_of<C: ColumnLike>(state: &[(&C, Expr)], column: &C) -> Expr {
    state.iter().find(|(assigned, _)| core::ptr::eq(*assigned, column)).map_or_else(
        || {
            let name = if column.column_name_is_quoted() {
                Ident::with_quote('"', column.column_name())
            } else {
                Ident::new(column.column_name())
            };
            Expr::CompoundIdentifier(vec![Ident::new("NEW"), name])
        },
        |(_, value)| value.clone(),
    )
}

/// Records that `column` holds `value` in `state`.
fn set<'db, C>(state: &mut Vec<(&'db C, Expr)>, column: &'db C, value: Expr) {
    match state.iter_mut().find(|(assigned, _)| core::ptr::eq(*assigned, column)) {
        Some((_, held)) => *held = value,
        None => state.push((column, value)),
    }
}
