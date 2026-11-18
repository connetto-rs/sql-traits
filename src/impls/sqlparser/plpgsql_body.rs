//! Walking the SQL a PL/pgSQL function body embeds, where its blocks,
//! branches, loops and exception handlers run it.
//!
//! A `SET` or `RESET` of `search_path` on the straight line of the body
//! reaches the statements after it, as in a SQL body. One in a branch may or
//! may not have run when a later statement runs, and one in a loop runs again
//! before the loop's earlier statements on the next iteration, so where it
//! may reach an unqualified name cannot be resolved. A handler runs once
//! PostgreSQL has rolled back what its block did, `SET` included, so it
//! starts under the path the block started with, and a statement after the
//! block cannot know whether the block finished or a handler ran.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::{
    ast::{Query, Statement},
    dialect::PostgreSqlDialect,
    parser::Parser,
};
use sqlparser_plpgsql::embedded::{
    Assignment, Block, BoundCursor, Branch, DeclarationKind, Dynamic, Fetch, Indirection, Loop,
    LoopControl, Node, NodeKind, Open, RaiseOption, ReturnQuery,
};

use super::data_statement::{
    BodyPath, BodyWalk, runs_code_outside, sets_search_path_at_run_time, unresolvable,
    walk_expression, walk_statement,
};
use crate::{errors::LookupError, structs::EmbeddedSql, utils::object_name::search_path_change};

/// Hands every statement the PL/pgSQL body `block` of `function` runs to
/// `walker`, in body order, together with the path each runs under.
///
/// # Errors
///
/// Returns the errors `walker` answers for SQL the body builds at run time,
/// and the errors of [`walk_statement`].
pub(crate) fn walk_block<'s>(
    function: &str,
    block: &'s Block,
    walker: &mut impl BodyWalk<'s>,
    hex_strings_may_be_integers: bool,
) -> Result<(), LookupError> {
    BlockWalk { function, walker, hex_strings_may_be_integers }.block(block)
}

/// A walk over a PL/pgSQL body, keeping the path `walker` holds to the one in
/// force where each statement runs.
struct BlockWalk<'w, W> {
    function: &'w str,
    walker: &'w mut W,
    hex_strings_may_be_integers: bool,
}

impl<'s, W: BodyWalk<'s>> BlockWalk<'_, W> {
    fn statement(&mut self, statement: &'s Statement) -> Result<(), LookupError> {
        walk_statement(self.function, statement, self.walker, self.hex_strings_may_be_integers)
    }

    fn expression(&mut self, query: &'s Query) -> Result<(), LookupError> {
        walk_expression(self.function, query, self.walker)
    }

    fn nodes(&mut self, nodes: &'s [Node]) -> Result<(), LookupError> {
        nodes.iter().try_for_each(|node| self.node(node))
    }

    fn node(&mut self, node: &'s Node) -> Result<(), LookupError> {
        match &node.kind {
            NodeKind::Statement { statement, .. }
            | NodeKind::Perform { query: statement }
            | NodeKind::ReturnQuery(ReturnQuery::Query(statement)) => self.statement(statement),
            NodeKind::Assignment(assignment) => {
                subscripts(assignment).try_for_each(|subscript| self.expression(subscript))?;
                self.expression(&assignment.value)
            }
            NodeKind::Dynamic { dynamic, .. }
            | NodeKind::Open(Open::Dynamic { dynamic, .. })
            | NodeKind::ReturnQuery(ReturnQuery::Dynamic(dynamic)) => {
                self.walker.dynamic(self.function, dynamic)
            }
            NodeKind::Open(Open::Bound(cursor)) => self.opened(cursor),
            NodeKind::Open(Open::Query { query, .. }) => self.statement(query),
            NodeKind::Fetch(Fetch { direction, .. }) | NodeKind::Move { direction, .. } => {
                direction.expression().map_or(Ok(()), |count| self.expression(count))
            }
            NodeKind::Close { .. }
            | NodeKind::Commit { .. }
            | NodeKind::Rollback { .. }
            | NodeKind::Diagnostics(_) => Ok(()),
            NodeKind::Block(block) => self.block(block),
            NodeKind::If(choice) => self.branches(&choice.branches, &choice.otherwise),
            NodeKind::Case(choice) => {
                if let Some(subject) = &choice.subject {
                    self.expression(subject)?;
                }
                // A missing `ELSE` raises `case_not_found` where no `WHEN`
                // holds, so it visits no statement.
                self.branches(&choice.branches, choice.otherwise.as_deref().unwrap_or_default())
            }
            NodeKind::Loop(repeated) => self.repeated(repeated),
            NodeKind::Exit(_)
            | NodeKind::Return { .. }
            | NodeKind::ReturnNext { .. }
            | NodeKind::Raise(_)
            | NodeKind::Assert(_) => {
                transfer_expressions(node).try_for_each(|query| self.expression(query))
            }
        }
    }

    /// Walks the arguments of `cursor`, then the query it opens, which
    /// PostgreSQL plans when the cursor opens.
    fn opened(&mut self, cursor: &'s BoundCursor) -> Result<(), LookupError> {
        cursor.arguments.iter().try_for_each(|argument| self.expression(argument))?;
        self.statement(&cursor.query)
    }

    /// Walks a block, starting each handler under the path the block's
    /// statements started with.
    fn block(&mut self, block: &'s Block) -> Result<(), LookupError> {
        // Run on entry, before the block's handlers take effect.
        defaults(block).try_for_each(|default| self.expression(default))?;
        let mut change = None;
        if !block.handlers.is_empty() {
            change = self.first_change(&block.statements)?;
            for handler in &block.handlers {
                if change.is_none() {
                    change = self.first_change(&handler.statements)?;
                }
            }
        }
        // Only a path some part of the block changes needs restoring.
        let entry = change.map(|_| self.walker.save_path());
        self.nodes(&block.statements)?;
        for handler in &block.handlers {
            if let Some(entry) = &entry {
                self.walker.restore_path(entry.clone());
            }
            self.nodes(&handler.statements)?;
        }
        if let Some(change) = change {
            self.forget(change);
        }
        Ok(())
    }

    /// Walks the branches of an `IF` or `CASE`, each condition tried after the
    /// one before it failed, so under the path the earlier branches left
    /// alone.
    fn branches(
        &mut self,
        branches: &'s [Branch],
        otherwise: &'s [Node],
    ) -> Result<(), LookupError> {
        let mut change = None;
        for branch in branches {
            self.expression(&branch.condition)?;
            let Some(found) = self.first_change(&branch.statements)? else {
                self.nodes(&branch.statements)?;
                continue;
            };
            let entry = self.walker.save_path();
            self.nodes(&branch.statements)?;
            self.walker.restore_path(entry);
            change.get_or_insert(found);
        }
        self.nodes(otherwise)?;
        if let Some(found) = change.or(self.first_change(otherwise)?) {
            self.forget(found);
        }
        Ok(())
    }

    /// Walks a loop, whose statements may run after a path change any of
    /// them makes.
    fn repeated(&mut self, repeated: &'s Loop) -> Result<(), LookupError> {
        let change = self.first_change(&repeated.statements)?;
        match &repeated.control {
            LoopControl::Unconditional => {}
            // Evaluated again before every iteration after the first.
            LoopControl::While(condition) => {
                if let Some(found) = change {
                    self.forget(found);
                }
                self.expression(condition)?;
            }
            LoopControl::Range { lower, upper, step, .. } => {
                self.expression(lower)?;
                self.expression(upper)?;
                if let Some(step) = step {
                    self.expression(step)?;
                }
            }
            LoopControl::Query { query, .. } => self.statement(query)?,
            LoopControl::Array { array, .. } => self.expression(array)?,
            LoopControl::Cursor { cursor, .. } => self.opened(cursor)?,
            LoopControl::Dynamic { dynamic, .. } => self.walker.dynamic(self.function, dynamic)?,
        }
        if let Some(found) = change {
            self.forget(found);
            self.nodes(&repeated.statements)?;
            // The loop may also have run no iteration.
            self.forget(found);
            return Ok(());
        }
        self.nodes(&repeated.statements)
    }

    /// Makes every later unqualified name answer that `sql` hides it.
    fn forget(&mut self, sql: EmbeddedSql<'_>) {
        self.walker.set_path(BodyPath::Unknown(unresolvable(self.function, &sql)));
    }

    /// The first statement or expression in `nodes`, nested ones included,
    /// that changes the path or may. Such SQL sets the path, runs code
    /// outside the body, calls a function that may set the path for its
    /// caller, or builds SQL at run time. Constant SQL text counts only when
    /// a statement it parses into would, or when PostgreSQL would refuse it
    /// once it runs, as the straight line reads it.
    fn first_change(&self, nodes: &'s [Node]) -> Result<Option<EmbeddedSql<'s>>, LookupError> {
        let may_change = |sql: EmbeddedSql<'_>| -> Result<bool, LookupError> {
            let changes = match sql {
                EmbeddedSql::Statement(statement) => {
                    runs_code_outside(statement)
                        || search_path_change(statement, self.hex_strings_may_be_integers)?
                            .is_some()
                }
                EmbeddedSql::Expression(_) => false,
            };
            Ok(changes || sets_search_path_at_run_time(&sql) || self.walker.calls_path_changer(sql))
        };
        let found = each_sql(nodes, &mut |sql, command| {
            let changed = match command {
                Some(dynamic) => {
                    Ok(constant_command(dynamic).is_none_or(|statements| {
                        statements
                            .iter()
                            .any(|run| may_change(EmbeddedSql::Statement(run)).unwrap_or(true))
                    }))
                }
                None => may_change(sql),
            };
            match changed {
                Ok(false) => ControlFlow::Continue(()),
                Ok(true) => ControlFlow::Break(Ok(sql)),
                Err(error) => ControlFlow::Break(Err(error)),
            }
        });
        found.break_value().transpose()
    }
}

/// The statements the command of `dynamic` runs when it is constant text
/// that parses, or `None` when only run time knows them.
pub(crate) fn constant_command(dynamic: &Dynamic) -> Option<Vec<Statement>> {
    Parser::parse_sql(&PostgreSqlDialect {}, dynamic.constant_command()?).ok()
}

/// Hands `visit` every statement and expression `nodes` run, nested ones
/// included, in body order, until it breaks, with the dynamic site whose
/// command an expression evaluates.
fn each_sql<'s, B>(
    nodes: &'s [Node],
    visit: &mut impl FnMut(EmbeddedSql<'s>, Option<&'s Dynamic>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    for node in nodes {
        match &node.kind {
            NodeKind::Statement { statement, .. }
            | NodeKind::Perform { query: statement }
            | NodeKind::ReturnQuery(ReturnQuery::Query(statement)) => {
                visit(EmbeddedSql::Statement(statement), None)?;
            }
            NodeKind::Assignment(assignment) => {
                subscripts(assignment)
                    .try_for_each(|subscript| visit(EmbeddedSql::Expression(subscript), None))?;
                visit(EmbeddedSql::Expression(&assignment.value), None)?;
            }
            NodeKind::Dynamic { dynamic, .. }
            | NodeKind::Open(Open::Dynamic { dynamic, .. })
            | NodeKind::ReturnQuery(ReturnQuery::Dynamic(dynamic)) => {
                each_dynamic_sql(dynamic, visit)?;
            }
            NodeKind::Open(Open::Bound(cursor)) => each_cursor_sql(cursor, visit)?,
            NodeKind::Open(Open::Query { query, .. }) => {
                visit(EmbeddedSql::Statement(query), None)?;
            }
            NodeKind::Fetch(Fetch { direction, .. }) | NodeKind::Move { direction, .. } => {
                if let Some(count) = direction.expression() {
                    visit(EmbeddedSql::Expression(count), None)?;
                }
            }
            NodeKind::Close { .. }
            | NodeKind::Commit { .. }
            | NodeKind::Rollback { .. }
            | NodeKind::Diagnostics(_) => {}
            NodeKind::Exit(_)
            | NodeKind::Return { .. }
            | NodeKind::ReturnNext { .. }
            | NodeKind::Raise(_)
            | NodeKind::Assert(_) => {
                transfer_expressions(node)
                    .try_for_each(|query| visit(EmbeddedSql::Expression(query), None))?;
            }
            NodeKind::Block(block) => each_block_sql(block, visit)?,
            NodeKind::If(choice) => each_branch_sql(&choice.branches, &choice.otherwise, visit)?,
            NodeKind::Case(choice) => {
                if let Some(subject) = &choice.subject {
                    visit(EmbeddedSql::Expression(subject), None)?;
                }
                // A missing `ELSE` raises `case_not_found` where no `WHEN`
                // holds, so it visits no SQL.
                each_branch_sql(
                    &choice.branches,
                    choice.otherwise.as_deref().unwrap_or_default(),
                    visit,
                )?;
            }
            NodeKind::Loop(repeated) => {
                match &repeated.control {
                    LoopControl::Unconditional => {}
                    LoopControl::While(query) | LoopControl::Array { array: query, .. } => {
                        visit(EmbeddedSql::Expression(query), None)?;
                    }
                    LoopControl::Query { query, .. } => {
                        visit(EmbeddedSql::Statement(query), None)?;
                    }
                    LoopControl::Range { lower, upper, step, .. } => {
                        visit(EmbeddedSql::Expression(lower), None)?;
                        visit(EmbeddedSql::Expression(upper), None)?;
                        if let Some(step) = step {
                            visit(EmbeddedSql::Expression(step), None)?;
                        }
                    }
                    LoopControl::Cursor { cursor, .. } => each_cursor_sql(cursor, visit)?,
                    LoopControl::Dynamic { dynamic, .. } => each_dynamic_sql(dynamic, visit)?,
                }
                each_sql(&repeated.statements, visit)?;
            }
        }
    }
    ControlFlow::Continue(())
}

/// Hands `visit` every statement and expression `block` runs as
/// [`each_sql`] does, the defaults its `DECLARE` section evaluates first and
/// its handlers last.
pub(crate) fn each_block_sql<'s, B>(
    block: &'s Block,
    visit: &mut impl FnMut(EmbeddedSql<'s>, Option<&'s Dynamic>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    defaults(block).try_for_each(|default| visit(EmbeddedSql::Expression(default), None))?;
    each_sql(&block.statements, visit)?;
    block.handlers.iter().try_for_each(|handler| each_sql(&handler.statements, visit))
}

/// The `SELECT` evaluating each default the `DECLARE` section of `block`
/// writes, in order.
fn defaults(block: &Block) -> impl Iterator<Item = &Query> {
    block.declarations.iter().filter_map(|declaration| {
        match &declaration.kind {
            DeclarationKind::Variable(variable) => variable.default.as_deref(),
            DeclarationKind::Alias(_) | DeclarationKind::Cursor(_) => None,
        }
    })
}

/// The `SELECT` evaluating each subscript and slice bound the target of
/// `assignment` writes, in the order written.
fn subscripts(assignment: &Assignment) -> impl Iterator<Item = &Query> {
    assignment.target.indirection.iter().flat_map(|step| {
        match step {
            Indirection::Field(_) => [None, None],
            Indirection::Index(index) => [Some(&**index), None],
            Indirection::Slice { lower, upper } => [lower.as_deref(), upper.as_deref()],
        }
        .into_iter()
        .flatten()
    })
}

/// The `SELECT` evaluating each expression an `EXIT`, `CONTINUE`, `RETURN`,
/// `RETURN NEXT`, `RAISE` or `ASSERT` evaluates, in the order written, and
/// none for any other node.
pub(crate) fn transfer_expressions(node: &Node) -> impl Iterator<Item = &Query> {
    let (first, arguments, options, last): (_, &[Query], &[RaiseOption], _) = match &node.kind {
        NodeKind::Exit(exit) => (exit.condition.as_deref(), &[], &[], None),
        NodeKind::Return { value } | NodeKind::ReturnNext { value } => {
            (value.as_deref(), &[], &[], None)
        }
        NodeKind::Raise(raise) => (None, &raise.arguments, &raise.options, None),
        NodeKind::Assert(assert) => (Some(&*assert.condition), &[], &[], assert.message.as_deref()),
        _ => (None, &[], &[], None),
    };
    first.into_iter().chain(arguments).chain(options.iter().map(|option| &option.value)).chain(last)
}

fn each_branch_sql<'s, B>(
    branches: &'s [Branch],
    otherwise: &'s [Node],
    visit: &mut impl FnMut(EmbeddedSql<'s>, Option<&'s Dynamic>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    for branch in branches {
        visit(EmbeddedSql::Expression(&branch.condition), None)?;
        each_sql(&branch.statements, visit)?;
    }
    each_sql(otherwise, visit)
}

fn each_dynamic_sql<'s, B>(
    dynamic: &'s Dynamic,
    visit: &mut impl FnMut(EmbeddedSql<'s>, Option<&'s Dynamic>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    visit(EmbeddedSql::Expression(&dynamic.command), Some(dynamic))?;
    dynamic.arguments.iter().try_for_each(|argument| visit(EmbeddedSql::Expression(argument), None))
}

fn each_cursor_sql<'s, B>(
    cursor: &'s BoundCursor,
    visit: &mut impl FnMut(EmbeddedSql<'s>, Option<&'s Dynamic>) -> ControlFlow<B>,
) -> ControlFlow<B> {
    cursor
        .arguments
        .iter()
        .try_for_each(|argument| visit(EmbeddedSql::Expression(argument), None))?;
    visit(EmbeddedSql::Statement(&cursor.query), None)
}
