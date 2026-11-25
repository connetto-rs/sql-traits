//! What a PL/pgSQL body does with each expression it evaluates.

use alloc::collections::BTreeMap;
use core::ptr;

use sqlparser::ast::Query;
use sqlparser_plpgsql::embedded::{
    Block, Case, DeclarationKind, ExitKind, LoopControl, Node, NodeKind,
};

use crate::structs::PlPgSqlSiteRole;

/// The role of an expression, and the `CASE` without `ELSE` whose end it
/// stands for.
pub(crate) type Role<'b> = (PlPgSqlSiteRole, Option<&'b Case>);

/// The role of every expression of `block` whose role is not
/// [`PlPgSqlSiteRole::Expression`], by the address of its `SELECT`.
pub(crate) fn site_roles(block: &Block) -> BTreeMap<usize, Role<'_>> {
    let mut roles = BTreeMap::new();
    of_block(block, &mut roles);
    roles
}

/// Records the role of `query`.
fn insert<'b>(roles: &mut BTreeMap<usize, Role<'b>>, query: &'b Query, role: Role<'b>) {
    roles.insert(ptr::from_ref(query).addr(), role);
}

/// Records the roles of the declarations, statements and handlers of
/// `block`.
fn of_block<'b>(block: &'b Block, roles: &mut BTreeMap<usize, Role<'b>>) {
    for declaration in &block.declarations {
        if let DeclarationKind::Variable(variable) = &declaration.kind
            && let Some(default) = &variable.default
        {
            insert(roles, default, (PlPgSqlSiteRole::Default, None));
        }
    }
    of_nodes(&block.statements, roles);
    for handler in &block.handlers {
        of_nodes(&handler.statements, roles);
    }
}

/// Records the roles of `nodes` and of the statements nested in them.
fn of_nodes<'b>(nodes: &'b [Node], roles: &mut BTreeMap<usize, Role<'b>>) {
    for node in nodes {
        match &node.kind {
            NodeKind::Assignment(assignment) => {
                insert(roles, &assignment.value, (PlPgSqlSiteRole::Assigned, None));
            }
            NodeKind::Block(block) => of_block(block, roles),
            NodeKind::If(conditional) => {
                let last = conditional.branches.len().saturating_sub(1);
                for (index, branch) in conditional.branches.iter().enumerate() {
                    let followed = index < last || !conditional.otherwise.is_empty();
                    let role = if index == 0 {
                        PlPgSqlSiteRole::IfCondition { followed }
                    } else {
                        PlPgSqlSiteRole::ElsifCondition { followed }
                    };
                    insert(roles, &branch.condition, (role, None));
                    of_nodes(&branch.statements, roles);
                }
                of_nodes(&conditional.otherwise, roles);
            }
            NodeKind::Case(case) => of_case(case, roles),
            NodeKind::Loop(repeated) => {
                if let LoopControl::While(condition) = &repeated.control {
                    insert(roles, condition, (PlPgSqlSiteRole::WhileCondition, None));
                }
                of_nodes(&repeated.statements, roles);
            }
            NodeKind::Exit(exit) => {
                if let Some(condition) = &exit.condition {
                    let role = match exit.kind {
                        ExitKind::Exit => PlPgSqlSiteRole::ExitWhen,
                        ExitKind::Continue => PlPgSqlSiteRole::ContinueWhen,
                    };
                    insert(roles, condition, (role, None));
                }
            }
            NodeKind::Assert(assert) => {
                insert(roles, &assert.condition, (PlPgSqlSiteRole::AssertCondition, None));
            }
            _ => {}
        }
    }
}

/// Records the roles of the subject, the `WHEN` clauses and the statements
/// of `case`, the subject of a simple one or the first `WHEN` of a searched
/// one standing for its end when it has no `ELSE`.
fn of_case<'b>(case: &'b Case, roles: &mut BTreeMap<usize, Role<'b>>) {
    let mut unmatched = case.otherwise.is_none().then_some(case);
    if let Some(subject) = &case.subject {
        insert(roles, subject, (PlPgSqlSiteRole::CaseSubject, unmatched.take()));
    }
    let last = case.branches.len().saturating_sub(1);
    for (index, branch) in case.branches.iter().enumerate() {
        let role = if case.subject.is_some() {
            PlPgSqlSiteRole::CaseValue
        } else {
            PlPgSqlSiteRole::CaseWhen { followed: index < last || case.otherwise.is_some() }
        };
        insert(roles, &branch.condition, (role, unmatched.take()));
        of_nodes(&branch.statements, roles);
    }
    if let Some(otherwise) = &case.otherwise {
        of_nodes(otherwise, roles);
    }
}
