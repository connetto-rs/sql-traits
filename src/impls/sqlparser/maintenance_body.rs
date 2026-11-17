//! The `NEW` columns a trigger's PL/pgSQL body assigns before returning `NEW`,
//! each folded into the one expression it holds when the body returns.

use alloc::{boxed::Box, vec, vec::Vec};
use core::{borrow::Borrow, ops::ControlFlow, slice};

use sqlparser::ast::{
    CaseWhen, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName,
    ObjectNamePart, Select, SelectItem, SelectItemQualifiedWildcardKind, Visitor,
    helpers::attached_token::AttachedToken,
};
use sqlparser_plpgsql::{
    embedded::{Assignment, Branch, If, Node, expression},
    embedded_sql,
    namespace::{NameKind, Namespace, TriggerVariable},
};

use super::dynamic_sites::BodyNames;
use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::{ColumnLike, DatabaseLike, FunctionLike, TableLike},
    utils::{
        identifier_resolution::stored_identifier,
        walk::{walk, walk_expressions_mut},
    },
};

/// The columns a maintenance body assigns, each with the expression it holds
/// when the body returns, in the order the body first assigns them.
pub(crate) type MaintenanceAssignments<'db, DB> = Vec<(&'db <DB as DatabaseLike>::Column, Expr)>;

/// The assignments of the PL/pgSQL body of `function`, run by a trigger on
/// `table`, or [`None`] when it is no maintenance body.
///
/// A maintenance body is one block without declarations or handlers whose
/// statements assign columns of `NEW`, alone or under `IF`, and whose last
/// statement is `RETURN NEW`, the body's only `RETURN`. `NEW` is any name
/// that reaches the trigger's `NEW` row, the function's name qualifying it
/// included. Each column it assigns yields the expression it holds when the
/// body returns, an `IF` folded into a `CASE` over its conditions in order
/// whose `ELSE` is the column's value before the `IF`. A column read through
/// `NEW` after an assignment to it reads the assigned expression, as
/// PostgreSQL runs the statements in order, so every expression reads `NEW`
/// as it was before the body ran. Reading the whole row `NEW` after an
/// assignment cannot be written that way and refuses the body, and so does
/// reading a field of `NEW` the table does not declare.
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
    if !block.declarations.is_empty() || !block.handlers.is_empty() {
        return Ok(None);
    }
    let Some((Node::Return { value: Some(returned) }, statements)) = block.statements.split_last()
    else {
        return Ok(None);
    };
    let columns = table.columns(database)?.collect();
    let names = BodyNames::new(function, database)?;
    // A maintenance body declares nothing and holds no loop or nested block,
    // so the names `RETURN` reads, the last site, are those of every statement.
    Ok(block.sites(&names.signature(), |name| names.holds_row(name)).ok().and_then(|mut sites| {
        let reader = Reader { columns, names: &names, namespace: sites.pop()?.namespace };
        reader.is_whole_new(&expression(returned)).then_some(())?;
        let mut state = Vec::new();
        reader.run(statements, &mut state)?;
        (!state.is_empty()).then_some(state)
    }))
}

/// Reads the statements of a maintenance body against the trigger's table.
///
/// Every method answers [`None`] for a body that is no maintenance body.
struct Reader<'names, 'db, DB: DatabaseLike> {
    /// The columns of the trigger's table.
    columns: Vec<&'db DB::Column>,
    /// The names of the trigger function's body.
    names: &'names BodyNames<'db, DB>,
    /// The names every statement of the body reaches.
    namespace: Namespace,
}

impl<'db, DB: DatabaseLike> Reader<'_, 'db, DB> {
    /// The parts of the name `parts` writes after those that reach `NEW`, or
    /// [`None`] when it does not start with `NEW`.
    fn after_new<'parts, Part: Borrow<Ident>>(
        &self,
        parts: &'parts [Part],
    ) -> Option<&'parts [Part]> {
        let stored: Vec<_> = parts
            .iter()
            .map(|part| {
                let part = part.borrow();
                stored_identifier(&part.value, part.quote_style.is_some())
            })
            .collect();
        let (name, consumed) = self.namespace.lookup(&stored, |name| self.names.holds_row(name))?;
        if matches!(name.kind(), NameKind::Trigger(TriggerVariable::New)) {
            parts.get(consumed..)
        } else {
            None
        }
    }

    /// Whether `read` is the whole row `NEW`.
    fn is_whole_new(&self, read: &Expr) -> bool {
        name_parts(read).and_then(|parts| self.after_new(parts)).is_some_and(<[Ident]>::is_empty)
    }

    /// Runs `statements` over `state`, the value each column assigned so far
    /// holds.
    fn run(&self, statements: &[Node], state: &mut MaintenanceAssignments<'db, DB>) -> Option<()> {
        for statement in statements {
            match statement {
                Node::Assignment(assignment) => self.assign(assignment, state)?,
                Node::If(conditional) => self.branch(conditional, state)?,
                _ => return None,
            }
        }
        Some(())
    }

    /// Runs `assignment` over `state`, refusing one to anything but a column
    /// of `NEW` the table declares.
    fn assign(
        &self,
        assignment: &Assignment,
        state: &mut MaintenanceAssignments<'db, DB>,
    ) -> Option<()> {
        let target = &assignment.target;
        if !target.indirection.is_empty() {
            return None;
        }
        let parts =
            target.name.0.iter().map(ObjectNamePart::as_ident).collect::<Option<Vec<_>>>()?;
        let [field] = self.after_new(&parts)? else {
            return None;
        };
        let column = self.column(field)?;
        let value = self.read(expression(&assignment.value).into_owned(), state)?;
        set(state, column, value);
        Some(())
    }

    /// Runs `conditional` over `state`, folding each column a branch assigns
    /// into a `CASE` over the conditions of the branches.
    fn branch(&self, conditional: &If, state: &mut MaintenanceAssignments<'db, DB>) -> Option<()> {
        let mut arms = Vec::with_capacity(conditional.branches.len());
        for Branch { condition, statements } in &conditional.branches {
            let condition = self.read(expression(condition).into_owned(), state)?;
            let mut arm = state.clone();
            self.run(statements, &mut arm)?;
            arms.push((condition, arm));
        }
        let mut otherwise = state.clone();
        self.run(&conditional.otherwise, &mut otherwise)?;

        let mut assigned: Vec<&'db DB::Column> = Vec::new();
        for (column, _) in arms.iter().map(|(_, arm)| arm).chain([&otherwise]).flatten() {
            if !assigned.iter().any(|seen| core::ptr::eq(*seen, *column)) {
                assigned.push(column);
            }
        }
        for column in assigned {
            let before = value_of(state, column);
            let after = |arm: &MaintenanceAssignments<'db, DB>| value_of(arm, column);
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
        Some(())
    }

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

    /// `expression` with every column of `NEW` it reads replaced by the value
    /// `state` holds for it, refusing one that reads `NEW.*` or `NEW.field.*`
    /// after an assignment changed what it expands to.
    fn read(&self, mut expression: Expr, state: &MaintenanceAssignments<'db, DB>) -> Option<Expr> {
        if walk(&expression, &mut Wildcards { reader: self, state }).is_break() {
            return None;
        }
        match walk_expressions_mut(&mut expression, |read| {
            self.substitute(read, state).map_or(ControlFlow::Break(()), ControlFlow::Continue)
        }) {
            ControlFlow::Continue(()) => Some(expression),
            ControlFlow::Break(()) => None,
        }
    }

    /// Whether the wildcard `prefix.*` expands to what it did before the body
    /// ran, given the columns `state` assigns.
    fn expands_unchanged(
        &self,
        prefix: &ObjectName,
        state: &MaintenanceAssignments<'db, DB>,
    ) -> bool {
        prefix.0.iter().map(ObjectNamePart::as_ident).collect::<Option<Vec<_>>>().is_none_or(
            |parts| {
                match self.after_new(&parts) {
                    None => true,
                    Some([]) => state.is_empty(),
                    Some([field, ..]) => {
                        self.column(field).is_some_and(|column| {
                            !state.iter().any(|(assigned, _)| core::ptr::eq(*assigned, column))
                        })
                    }
                }
            },
        )
    }

    /// Replaces `read`, once its subexpressions are read, by the value
    /// `state` holds when it reads a column of `NEW` assigned so far.
    fn substitute(&self, read: &mut Expr, state: &MaintenanceAssignments<'db, DB>) -> Option<()> {
        let Some(fields) = name_parts(read).and_then(|parts| self.after_new(parts)) else {
            return Some(());
        };
        let [field, rest @ ..] = fields else {
            return state.is_empty().then_some(());
        };
        let column = self.column(field)?;
        let Some((_, value)) = state.iter().find(|(assigned, _)| core::ptr::eq(*assigned, column))
        else {
            return Some(());
        };
        if !rest.is_empty() {
            return None;
        }
        *read = parenthesized(value.clone());
        Some(())
    }
}

/// Breaks on a wildcard over `NEW` whose expansion an assignment in `state`
/// changed, in `ROW(...)`, a function's arguments or a query's select list.
struct Wildcards<'reader, 'names, 'db, DB: DatabaseLike> {
    /// Resolves the wildcard's prefix.
    reader: &'reader Reader<'names, 'db, DB>,
    /// The value each column assigned so far holds.
    state: &'reader MaintenanceAssignments<'db, DB>,
}

impl<DB: DatabaseLike> Wildcards<'_, '_, '_, DB> {
    /// Breaks unless `prefix.*` expands as it did before the body ran.
    fn check(&self, prefix: &ObjectName) -> ControlFlow<()> {
        if self.reader.expands_unchanged(prefix, self.state) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }
}

impl<DB: DatabaseLike> Visitor for Wildcards<'_, '_, '_, DB> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        match expr {
            Expr::QualifiedWildcard(prefix, _) => self.check(prefix),
            Expr::Function(Function { args: FunctionArguments::List(list), .. }) => {
                list.args.iter().try_for_each(|arg| {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(prefix))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::QualifiedWildcard(prefix),
                            ..
                        }
                        | FunctionArg::ExprNamed {
                            arg: FunctionArgExpr::QualifiedWildcard(prefix),
                            ..
                        } => self.check(prefix),
                        _ => ControlFlow::Continue(()),
                    }
                })
            }
            _ => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<()> {
        select.projection.iter().try_for_each(|item| {
            match item {
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(prefix),
                    _,
                ) => self.check(prefix),
                _ => ControlFlow::Continue(()),
            }
        })
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
