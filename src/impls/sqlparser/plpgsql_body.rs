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

use core::ops::ControlFlow;

use sqlparser::ast::{SelectItem, SetExpr, Statement};
use sqlparser_plpgsql::embedded::{Block, Branch, Dynamic, Loop, LoopControl, Node};

use super::data_statement::{
    BodyPath, BodyWalk, sets_search_path_at_run_time, unresolvable, walk_statement,
};
use crate::{errors::LookupError, utils::object_name::search_path_change};

/// Hands every statement the PL/pgSQL body `block` of `function` runs to
/// `walker`, in body order, together with the path each runs under.
///
/// # Errors
///
/// Returns [`LookupError::FunctionBodyUnresolvable`] for SQL the body builds
/// at run time, and the errors of [`walk_statement`].
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

    fn nodes(&mut self, nodes: &'s [Node]) -> Result<(), LookupError> {
        nodes.iter().try_for_each(|node| self.node(node))
    }

    fn node(&mut self, node: &'s Node) -> Result<(), LookupError> {
        match node {
            Node::Statement(statement) => self.statement(statement),
            Node::Dynamic(dynamic) => Err(self.dynamic(dynamic)),
            Node::Block(block) => self.block(block),
            Node::If(choice) => self.branches(&choice.branches, &choice.otherwise),
            Node::Case(choice) => {
                if let Some(subject) = &choice.subject {
                    self.statement(subject)?;
                }
                self.branches(&choice.branches, &choice.otherwise)
            }
            Node::Loop(repeated) => self.repeated(repeated),
        }
    }

    /// Walks a block, starting each handler under the path the block's
    /// statements started with.
    fn block(&mut self, block: &'s Block) -> Result<(), LookupError> {
        // Run on entry, before the block's handlers take effect.
        block.declarations.iter().try_for_each(|declaration| self.statement(declaration))?;
        let mut change = None;
        if !block.handlers.is_empty() {
            change = self.first_change(&block.statements)?;
            for handler in &block.handlers {
                if change.is_none() {
                    change = self.first_change(handler)?;
                }
            }
        }
        // Only a path some part of the block changes needs restoring.
        let entry = change.and_then(|_| self.walker.path().cloned());
        self.nodes(&block.statements)?;
        for handler in &block.handlers {
            if let Some(entry) = &entry {
                self.walker.set_path(entry.clone());
            }
            self.nodes(handler)?;
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
            self.statement(&branch.condition)?;
            let Some(found) = self.first_change(&branch.statements)? else {
                self.nodes(&branch.statements)?;
                continue;
            };
            let entry = self.walker.path().cloned();
            self.nodes(&branch.statements)?;
            if let Some(entry) = entry {
                self.walker.set_path(entry);
            }
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
                self.statement(condition)?;
            }
            LoopControl::Range { lower, upper, step } => {
                self.statement(lower)?;
                self.statement(upper)?;
                if let Some(step) = step {
                    self.statement(step)?;
                }
            }
            LoopControl::Query(query) | LoopControl::Array(query) => self.statement(query)?,
            LoopControl::Cursor { arguments, query } => {
                arguments.iter().try_for_each(|argument| self.statement(argument))?;
                self.statement(query)?;
            }
            LoopControl::Dynamic(dynamic) => return Err(self.dynamic(dynamic)),
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

    /// Makes every later unqualified name answer that `statement` hides it.
    fn forget(&mut self, statement: &Statement) {
        self.walker.set_path(BodyPath::Unknown(unresolvable(self.function, statement)));
    }

    /// The first statement in `nodes`, nested ones included, that changes
    /// the path.
    fn first_change(&self, nodes: &'s [Node]) -> Result<Option<&'s Statement>, LookupError> {
        let hex_strings_may_be_integers = self.hex_strings_may_be_integers;
        let found = each_statement(nodes, &mut |statement| {
            match search_path_change(statement, hex_strings_may_be_integers) {
                Ok(None) if !sets_search_path_at_run_time(statement) => ControlFlow::Continue(()),
                Ok(_) => ControlFlow::Break(Ok(statement)),
                Err(error) => ControlFlow::Break(Err(error)),
            }
        });
        found.break_value().transpose()
    }

    /// The error SQL built at run time answers, naming the `EXECUTE` by the
    /// expression that builds its command.
    fn dynamic(&self, dynamic: &Dynamic) -> LookupError {
        let command = &*dynamic.command;
        if let Statement::Query(query) = command
            && let SetExpr::Select(select) = &*query.body
            && select.from.is_empty()
            && let [SelectItem::UnnamedExpr(expression)] = select.projection.as_slice()
        {
            return unresolvable(self.function, &format_args!("EXECUTE {expression}"));
        }
        unresolvable(self.function, &format_args!("EXECUTE ({command})"))
    }
}

/// Hands `visit` every statement `nodes` run, nested ones included, in body
/// order, until it breaks.
fn each_statement<'s, B>(
    nodes: &'s [Node],
    visit: &mut impl FnMut(&'s Statement) -> ControlFlow<B>,
) -> ControlFlow<B> {
    for node in nodes {
        match node {
            Node::Statement(statement) => visit(statement)?,
            Node::Dynamic(dynamic) => each_dynamic_statement(dynamic, visit)?,
            Node::Block(block) => {
                block.declarations.iter().try_for_each(&mut *visit)?;
                each_statement(&block.statements, visit)?;
                for handler in &block.handlers {
                    each_statement(handler, visit)?;
                }
            }
            Node::If(choice) => each_branch_statement(&choice.branches, &choice.otherwise, visit)?,
            Node::Case(choice) => {
                if let Some(subject) = &choice.subject {
                    visit(subject)?;
                }
                each_branch_statement(&choice.branches, &choice.otherwise, visit)?;
            }
            Node::Loop(repeated) => {
                match &repeated.control {
                    LoopControl::Unconditional => {}
                    LoopControl::While(statement)
                    | LoopControl::Query(statement)
                    | LoopControl::Array(statement) => visit(statement)?,
                    LoopControl::Range { lower, upper, step } => {
                        visit(lower)?;
                        visit(upper)?;
                        if let Some(step) = step {
                            visit(step)?;
                        }
                    }
                    LoopControl::Cursor { arguments, query } => {
                        arguments.iter().try_for_each(&mut *visit)?;
                        visit(query)?;
                    }
                    LoopControl::Dynamic(dynamic) => each_dynamic_statement(dynamic, visit)?,
                }
                each_statement(&repeated.statements, visit)?;
            }
        }
    }
    ControlFlow::Continue(())
}

fn each_branch_statement<'s, B>(
    branches: &'s [Branch],
    otherwise: &'s [Node],
    visit: &mut impl FnMut(&'s Statement) -> ControlFlow<B>,
) -> ControlFlow<B> {
    for branch in branches {
        visit(&branch.condition)?;
        each_statement(&branch.statements, visit)?;
    }
    each_statement(otherwise, visit)
}

fn each_dynamic_statement<'s, B>(
    dynamic: &'s Dynamic,
    visit: &mut impl FnMut(&'s Statement) -> ControlFlow<B>,
) -> ControlFlow<B> {
    visit(&dynamic.command)?;
    dynamic.arguments.iter().try_for_each(visit)
}
