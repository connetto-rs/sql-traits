//! What the statements of a PL/pgSQL body do with the rows they read.

use alloc::vec::Vec;
use core::ptr;

use sqlparser::ast::{Expr, ObjectName, Query, SelectItem, SetExpr};
use sqlparser_plpgsql::embedded::{
    AliasTarget, Block, DeclarationKind, LoopControl, Node, NodeKind, RaiseLevel, Target,
};

use crate::structs::EmbeddedSql;

/// The statements and expressions of a PL/pgSQL body whose rows decide
/// nothing, and what the body does with `NEW`.
#[derive(Default)]
pub(super) struct Kinds {
    /// The exception messages whose evaluation no handler can distinguish.
    quiet_expressions: Vec<*const Query>,
    /// Whether the body reads `GET DIAGNOSTICS`, which the rows its
    /// statements find decide.
    pub(super) diagnostics: bool,
    /// Whether any block can distinguish evaluation errors from raised errors.
    handlers: bool,
    /// Whether the body writes `NEW` or one of its fields.
    pub(super) writes_new: bool,
    /// Whether every `RETURN` of the body returns `NEW`.
    pub(super) returns_new: bool,
}

impl Kinds {
    /// The kinds of the statements of `block`.
    pub(super) fn of(block: &Block) -> Self {
        let mut kinds = Self { returns_new: true, ..Self::default() };
        kinds.block(block);
        if kinds.handlers {
            kinds.quiet_expressions.clear();
        }
        kinds
    }

    /// Whether the rows `sql` reads decide a branch, a raise, a value the
    /// body assigns or returns, or `FOUND`, the body reading `FOUND` when
    /// `found` says.
    pub(super) fn decides(&self, sql: EmbeddedSql<'_>, found: bool) -> bool {
        if found || self.diagnostics {
            return true;
        }
        let EmbeddedSql::Expression(query) = sql else { return true };
        !self.quiet_expressions.iter().any(|quiet| ptr::eq(*quiet, query))
    }

    /// Visits declarations, statements and exception handlers of a block.
    fn block(&mut self, block: &Block) {
        self.handlers |= !block.handlers.is_empty();
        for declaration in &block.declarations {
            self.writes_new |= declaration.name.eq_ignore_ascii_case("new")
                || matches!(&declaration.kind,
                    DeclarationKind::Alias(AliasTarget::Name(name)) if names_new(name));
        }
        self.nodes(&block.statements);
        for handler in &block.handlers {
            self.nodes(&handler.statements);
        }
    }

    /// Visits a sequence of body statements.
    fn nodes(&mut self, nodes: &[Node]) {
        for node in nodes {
            self.node(node);
        }
    }

    /// Records writes to a trigger row target.
    fn target(&mut self, target: &Target) {
        self.writes_new |= names_new(&target.name);
    }

    /// Classifies the effects of one body statement.
    fn node(&mut self, node: &Node) {
        match &node.kind {
            NodeKind::Statement { into, .. } => {
                if let Some(into) = into {
                    into.targets.iter().for_each(|target| self.target(target));
                }
            }
            NodeKind::Assignment(assignment) => self.target(&assignment.target),
            NodeKind::Dynamic { into, .. } => {
                if let Some(into) = into {
                    into.targets.iter().for_each(|target| self.target(target));
                }
            }
            NodeKind::Fetch(fetch) => fetch.targets.iter().for_each(|target| self.target(target)),
            NodeKind::Diagnostics(diagnostics) => {
                self.diagnostics = true;
                diagnostics.items.iter().for_each(|item| self.target(&item.target));
            }
            NodeKind::Block(block) => self.block(block),
            NodeKind::If(choice) => {
                for branch in &choice.branches {
                    self.nodes(&branch.statements);
                }
                self.nodes(&choice.otherwise);
            }
            NodeKind::Case(choice) => {
                for branch in &choice.branches {
                    self.nodes(&branch.statements);
                }
                if let Some(otherwise) = &choice.otherwise {
                    self.nodes(otherwise);
                }
            }
            NodeKind::Loop(repeated) => {
                match &repeated.control {
                    LoopControl::Query { targets, .. }
                    | LoopControl::Array { targets, .. }
                    | LoopControl::Dynamic { targets, .. } => {
                        for target in targets {
                            self.target(target);
                        }
                    }
                    LoopControl::Range { variable, .. } | LoopControl::Cursor { variable, .. } => {
                        self.writes_new |= variable.eq_ignore_ascii_case("new");
                    }
                    LoopControl::Unconditional | LoopControl::While(_) => {}
                }
                self.nodes(&repeated.statements);
            }
            NodeKind::Return { value } => {
                self.returns_new &= value.as_deref().is_some_and(is_new);
            }
            NodeKind::Raise(raise) if raise.level == RaiseLevel::Exception => {
                self.quiet_expressions.extend(raise.arguments.iter().map(ptr::from_ref));
                self.quiet_expressions
                    .extend(raise.options.iter().map(|option| ptr::from_ref(&option.value)));
            }
            NodeKind::Assert(assert) => {
                if let Some(message) = &assert.message {
                    self.quiet_expressions.push(ptr::from_ref(message.as_ref()));
                }
            }
            NodeKind::Raise(_)
            | NodeKind::Perform { .. }
            | NodeKind::Open(_)
            | NodeKind::Move { .. }
            | NodeKind::Close { .. }
            | NodeKind::Commit { .. }
            | NodeKind::Rollback { .. }
            | NodeKind::Exit(_)
            | NodeKind::ReturnNext { .. }
            | NodeKind::ReturnQuery(_) => {}
        }
    }
}

/// Whether `name`, a variable a statement writes, is `NEW` or a field of it.
fn names_new(name: &ObjectName) -> bool {
    name.0.iter().take(2).filter_map(|part| part.as_ident()).any(|ident| {
        if ident.quote_style.is_some() {
            ident.value == "new"
        } else {
            ident.value.eq_ignore_ascii_case("new")
        }
    })
}

/// Whether `value`, the expression a `RETURN` evaluates, is `NEW` itself.
fn is_new(value: &Query) -> bool {
    let SetExpr::Select(select) = value.body.as_ref() else {
        return false;
    };
    matches!(
        select.projection.as_slice(),
        [SelectItem::UnnamedExpr(Expr::Identifier(ident))]
            if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("new")
    ) && select.from.is_empty()
        && select.selection.is_none()
}
