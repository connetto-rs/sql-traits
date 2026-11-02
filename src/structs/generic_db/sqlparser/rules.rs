//! Recording rewrite rules and keeping them on the table or plain view they
//! are defined on.
//!
//! A rule depends on its relation and on the columns it reads through `NEW`
//! and `OLD`, so those references follow renames and drops. The relations
//! the action and condition name bind under the search path in effect at
//! creation, the way a `BEGIN ATOMIC` function body does, and follow their
//! renames and drops.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CreateRule, CreateRuleAction, CreateRuleEvent, Expr, Ident, ObjectName, Statement, Visit,
    VisitMut, Visitor, VisitorMut,
};

use super::{
    NamedColumn, ParserDBBuilder, bind_reference, case_idents_match, function_bodies,
    object_name_matches_resolved_identity, require_named, stored_qualifier, stored_view_qualifier,
    target_tables_match,
    views::{self, relations_read_in, statements_read},
};
use crate::{
    errors::{Error, ObjectKind},
    structs::IdentifierCase,
    traits::ViewLike,
    utils::{
        identifier_resolution::identifiers_match,
        last_str,
        object_name::{RelationKey, target_key, target_name_of_object_name},
    },
};

/// Records a `CREATE RULE`, bound to the table or view its target reaches.
///
/// # Errors
///
/// Returns [`Error::RelationNotFoundForRule`] when no relation holds the
/// target, [`Error::RuleOnMaterializedView`] when a materialized view does,
/// [`Error::RuleOnSequence`] when a sequence does,
/// [`Error::SelectRuleUnsupported`] for an `ON SELECT` rule, and
/// [`Error::RuleAlreadyExists`] when the relation carries a rule of the same
/// name and the statement wrote no `OR REPLACE`.
pub(super) fn create_rule(
    mut builder: ParserDBBuilder,
    mut rule: CreateRule,
) -> Result<ParserDBBuilder, Error> {
    if rule.name.value.is_empty() {
        return Err(Error::UnnamedObject { object_kind: ObjectKind::Rule });
    }
    require_named(&rule.table_name, ObjectKind::Rule)?;
    let relation_name = |rule: &CreateRule| last_str(&rule.table_name).to_string();

    // Bound to the qualifier of the relation reached, as the other objects
    // naming a relation are.
    let qualifier = if let Some(table) = builder.resolve_table_object_name(&rule.table_name)? {
        stored_qualifier(&table.name)
    } else if let Some(view) = builder.resolve_view_object_name(&rule.table_name)? {
        stored_view_qualifier(view)
    } else if builder.resolve_materialized_view_object_name(&rule.table_name)?.is_some() {
        return Err(Error::RuleOnMaterializedView {
            view_name: relation_name(&rule),
            rule_name: rule.name.value,
        });
    } else if super::sequences::holds_sequence(&builder, &rule.table_name) {
        return Err(Error::RuleOnSequence {
            sequence_name: relation_name(&rule),
            rule_name: rule.name.value,
        });
    } else {
        return Err(Error::RelationNotFoundForRule {
            relation_name: relation_name(&rule),
            rule_name: rule.name.value,
        });
    };
    if rule.event == CreateRuleEvent::Select {
        return Err(Error::SelectRuleUnsupported {
            relation_name: relation_name(&rule),
            rule_name: rule.name.value,
        });
    }
    bind_reference(&mut rule.table_name, qualifier);

    // The action and condition bind the relations they name the way a
    // `BEGIN ATOMIC` body does, under the search path in effect now.
    let rule_name = rule.name.value.clone();
    let statements: &mut [Statement] = match action_statements_mut(&mut rule.action) {
        Some(statements) => {
            super::view_rewrites::normalize_block_references(&builder, statements)?;
            let too_wide = || Error::RuleActionInsertTooWide { rule_name: rule_name.clone() };
            for statement in statements.iter_mut() {
                let ambiguous = |column: Ident| {
                    Error::RuleActionReferenceAmbiguous {
                        column_name: column.value,
                        rule_name: rule_name.clone(),
                    }
                };
                super::view_rewrites::refuse_ambiguous_block_reference(
                    &builder, statement, ambiguous,
                )?;
                super::view_rewrites::spell_block_statement(&builder, statement, &too_wide)?;
            }
            statements
        }
        None => &mut [],
    };
    let missing = |name: &sqlparser::ast::ObjectName| {
        Error::RelationNotFoundForRuleAction {
            relation_name: name.to_string(),
            rule_name: rule_name.clone(),
        }
    };
    function_bodies::bind_rule_parts(&builder, statements, rule.condition.as_mut(), missing)?;

    let case = builder.identifier_case();
    let existing = builder.rules().iter().position(|(existing, ())| {
        case_idents_match(&existing.name, &rule.name, case)
            && target_tables_match(&existing.table_name, &rule.table_name, case)
    });
    match (existing, rule.or_replace) {
        (Some(_), false) => {
            return Err(Error::RuleAlreadyExists {
                relation_name: relation_name(&rule),
                rule_name: rule.name.value,
            });
        }
        (Some(position), true) => {
            builder.rules_mut().remove(position);
        }
        (None, _) => {}
    }

    Ok(builder.add_rule(Arc::new(rule), ()))
}

/// The statements a rule's action holds, which is empty for `DO NOTHING`.
pub(super) fn action_statements(action: &CreateRuleAction) -> &[Statement] {
    match action {
        CreateRuleAction::Nothing => &[],
        CreateRuleAction::Statement(statement) => core::slice::from_ref(statement),
        CreateRuleAction::Statements(statements) => statements,
    }
}

/// The statements a rule's action holds, held for their rewrites, which is
/// `None` for `DO NOTHING`.
pub(super) fn action_statements_mut(action: &mut CreateRuleAction) -> Option<&mut [Statement]> {
    match action {
        CreateRuleAction::Nothing => None,
        CreateRuleAction::Statement(statement) => {
            let inner: &mut Statement = statement;
            Some(core::slice::from_mut(inner))
        }
        CreateRuleAction::Statements(statements) => Some(statements),
    }
}

/// Writes respelled statements back into a rule's action, one for each
/// statement it holds.
pub(super) fn write_action_statements(action: &mut CreateRuleAction, statements: Vec<Statement>) {
    if let Some(held) = action_statements_mut(action) {
        for (held, statement) in held.iter_mut().zip(statements) {
            *held = statement;
        }
    }
}

/// Writes one respelled statement back into a rule's action at `position`.
pub(super) fn write_action_statement(
    action: &mut CreateRuleAction,
    position: usize,
    statement: Statement,
) {
    if let Some(held) = action_statements_mut(action).and_then(|held| held.get_mut(position)) {
        *held = statement;
    }
}

/// The key the rule's relation is stored under, which a rename or drop of
/// that relation or one of its columns must reach the rule by.
pub(super) fn relation_key(rule: &CreateRule) -> RelationKey {
    target_key(&target_name_of_object_name(&rule.table_name), IdentifierCase::AsWritten)
}

/// The rule as a dependency walk reaches it, by its name on its relation.
pub(super) fn dependent(rule: &CreateRule) -> views::Dependent {
    let name = IdentifierCase::AsWritten
        .compared_form(&rule.name.value, rule.name.quote_style.is_some())
        .into_owned();
    views::Dependent::Rule(relation_key(rule), name)
}

/// The normalized identity of every relation the rule's action and condition
/// read, which is the read set a dependency walk closes over.
pub(super) fn rules_reading(builder: &ParserDBBuilder, rule: &CreateRule) -> Vec<RelationKey> {
    let mut read = statements_read(builder, action_statements(&rule.action));
    if let Some(condition) = rule.condition.as_ref() {
        read.extend(relations_read_in(&function_bodies::definition(condition)));
    }
    read
}

/// The stored identity of a view whose rules a statement drops or rewrites,
/// owned so it survives the edits made to the view store.
pub(super) struct RuleHost {
    name: String,
    name_quoted: bool,
    schema: Option<String>,
    schema_quoted: bool,
}

impl RuleHost {
    pub(super) fn of_view<V: ViewLike>(view: &V) -> Self {
        Self {
            name: view.view_name().to_string(),
            name_quoted: view.view_name_is_quoted(),
            schema: view.view_schema().map(ToString::to_string),
            schema_quoted: view.view_schema_is_quoted(),
        }
    }

    /// Whether the rule is defined on this relation.
    fn carries(&self, rule: &CreateRule) -> bool {
        object_name_matches_resolved_identity(
            &rule.table_name,
            &self.name,
            self.name_quoted,
            self.schema.as_deref(),
            self.schema_quoted,
        )
    }
}

/// Drops the rules defined on a relation that is going away.
pub(super) fn drop_rules_on(builder: &mut ParserDBBuilder, host: &RuleHost) {
    builder.rules_mut().retain(|(rule, ())| !host.carries(rule));
}

/// Points the rules on a renamed or moved relation at its stored name
/// `new_name`.
pub(super) fn follow_relation_rename(
    builder: &mut ParserDBBuilder,
    host: &RuleHost,
    new_name: &ObjectName,
) {
    for (rule, ()) in builder.rules_mut() {
        if host.carries(rule) {
            Arc::make_mut(rule).table_name = new_name.clone();
        }
    }
}

/// Rewrites the `NEW` and `OLD` references to a renamed column in the rules
/// on its relation.
pub(super) fn follow_column_rename(
    builder: &mut ParserDBBuilder,
    host: &RuleHost,
    from: &Ident,
    to: &Ident,
) {
    let from = NamedColumn::of(from, builder.identifier_case());
    for (rule, ()) in builder.rules_mut() {
        if host.carries(rule) {
            rename_row_column(Arc::make_mut(rule), &from, to);
        }
    }
}

/// Whether the rule's condition or action reads `column` of its relation
/// through `NEW` or `OLD`.
pub(super) fn reads_row_column(rule: &CreateRule, column: &NamedColumn) -> bool {
    rule.condition.as_ref().is_some_and(|condition| expression_reads_row_column(condition, column))
        || rule.action.visit(&mut RowColumnMentioned { column }).is_break()
}

/// Renames every `NEW` and `OLD` reference to `from` in the rule's condition
/// and action.
pub(super) fn rename_row_column(rule: &mut CreateRule, from: &NamedColumn, to: &Ident) {
    if let Some(condition) = rule.condition.as_mut() {
        rename_row_column_in_expression(condition, from, to);
    }
    let mut renamer = RowColumnRenamer { column: from, to };
    let _: ControlFlow<()> = VisitMut::visit(&mut rule.action, &mut renamer);
}

/// Whether the expression reads the relation's row column through `NEW` or
/// `OLD`, which a trigger's `WHEN` condition does as well.
pub(super) fn expression_reads_row_column(expression: &Expr, column: &NamedColumn) -> bool {
    let mut mentioned = RowColumnMentioned { column };
    expression.visit(&mut mentioned).is_break()
}

/// Renames every `NEW` and `OLD` reference to `from` in the expression.
pub(super) fn rename_row_column_in_expression(
    expression: &mut Expr,
    from: &NamedColumn,
    to: &Ident,
) {
    let mut renamer = RowColumnRenamer { column: from, to };
    let _: ControlFlow<()> = VisitMut::visit(expression, &mut renamer);
}

/// The column a `NEW.column` or `OLD.column` expression names, if the
/// expression is one.
fn row_column(expr: &Expr) -> Option<&Ident> {
    let Expr::CompoundIdentifier(parts) = expr else {
        return None;
    };
    let [row, column] = parts.as_slice() else {
        return None;
    };
    let quoted = row.quote_style.is_some();
    (identifiers_match(&row.value, quoted, "new", false)
        || identifiers_match(&row.value, quoted, "old", false))
    .then_some(column)
}

/// Breaks on the first `NEW` or `OLD` reference to the column.
struct RowColumnMentioned<'a> {
    column: &'a NamedColumn,
}

impl Visitor for RowColumnMentioned<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        match row_column(expr) {
            Some(column) if self.column.matches(column) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    }
}

/// Rewrites every `NEW` and `OLD` reference to the column.
struct RowColumnRenamer<'a> {
    column: &'a NamedColumn,
    to: &'a Ident,
}

impl VisitorMut for RowColumnRenamer<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        if row_column(expr).is_some_and(|column| self.column.matches(column))
            && let Expr::CompoundIdentifier(parts) = expr
            && let Some(last) = parts.last_mut()
        {
            *last = self.to.clone();
        }
        ControlFlow::Continue(())
    }
}
