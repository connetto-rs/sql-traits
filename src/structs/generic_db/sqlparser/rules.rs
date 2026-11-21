//! Recording rewrite rules and keeping them on the table or plain view they
//! are defined on.
//!
//! A rule depends on its relation and on the columns it reads through `NEW`
//! and `OLD`, so those references follow renames and drops. The relations
//! the action and condition name bind under the search path in effect at
//! creation, the way a `BEGIN ATOMIC` function body does, and follow their
//! renames and drops.

use alloc::{sync::Arc, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CreateRule, CreateRuleAction, CreateRuleEvent, Expr, Ident, ObjectName, Statement, Visitor,
    VisitorMut,
};

use super::{
    NamedColumn, ParserDBBuilder, bind_reference, case_idents_match, function_bodies,
    require_named, stored_qualifier, stored_view_qualifier, target_tables_match,
    views::{self, ViewHost, relations_read_in, statements_read},
};
use crate::{
    errors::{Error, ObjectKind},
    impls::SqlparserDialect,
    structs::IdentifierCase,
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{Namespaces, RelationKey, target_key, target_name_of_object_name},
        walk::{walk, walk_mut},
    },
};

/// Records a `CREATE RULE`, bound to the table or view its target reaches.
///
/// # Errors
///
/// Returns [`Error::RelationNotFoundForRule`] when no relation holds the
/// target, [`Error::RuleOnUnsupportedRelation`] when a foreign table, a
/// materialized view or a sequence does, [`Error::SelectRuleUnsupported`] for
/// an `ON SELECT` rule, and [`Error::RuleAlreadyExists`] when the relation
/// carries a rule of the same name and the statement wrote no `OR REPLACE`.
pub(super) fn create_rule(
    mut builder: ParserDBBuilder,
    mut rule: CreateRule,
) -> Result<ParserDBBuilder, Error> {
    if rule.name.value.is_empty() {
        return Err(Error::UnnamedObject { object_kind: ObjectKind::Rule });
    }
    require_named(&rule.table_name, ObjectKind::Rule)?;

    // Bound to the qualifier of the relation reached, as the other objects
    // naming a relation are.
    let unsupported = |rule: CreateRule, relation_kind| {
        Err(Error::RuleOnUnsupportedRelation {
            relation_name: builder.dialect().stored_object_name_last(&rule.table_name),
            rule_name: builder.dialect().stored_name(&rule.name),
            relation_kind,
        })
    };
    let qualifier = if let Some(table) = builder.resolve_table_object_name(&rule.table_name)? {
        if builder.is_foreign_table(table) {
            return unsupported(rule, ObjectKind::ForeignTable);
        }
        stored_qualifier(&table.name)
    } else if let Some(view) = builder.resolve_view_object_name(&rule.table_name)? {
        stored_view_qualifier(view)
    } else if builder.resolve_materialized_view_object_name(&rule.table_name)?.is_some() {
        return unsupported(rule, ObjectKind::MaterializedView);
    } else if super::sequences::holds_sequence(&builder, &rule.table_name) {
        return unsupported(rule, ObjectKind::Sequence);
    } else {
        return Err(Error::RelationNotFoundForRule {
            relation_name: builder.dialect().stored_object_name(&rule.table_name),
            rule_name: builder.dialect().stored_name(&rule.name),
        });
    };
    if rule.event == CreateRuleEvent::Select {
        return Err(Error::SelectRuleUnsupported {
            relation_name: builder.dialect().stored_object_name_last(&rule.table_name),
            rule_name: builder.dialect().stored_name(&rule.name),
        });
    }
    bind_reference(&mut rule.table_name, qualifier);

    // The action and condition bind the relations they name the way a
    // `BEGIN ATOMIC` body does, under the search path in effect now.
    let rule_name = builder.dialect().stored_name(&rule.name);
    let statements = action_statements_mut(&mut rule.action);
    super::view_rewrites::normalize_block_references(&builder, statements)?;
    let too_wide = || Error::RuleActionInsertTooWide { rule_name: rule_name.clone() };
    for statement in statements.iter_mut() {
        let ambiguous = |column: Ident| {
            Error::RuleActionReferenceAmbiguous {
                column_name: builder.dialect().stored_name(&column),
                rule_name: rule_name.clone(),
            }
        };
        super::view_rewrites::refuse_ambiguous_block_reference(&builder, statement, ambiguous)?;
        super::view_rewrites::spell_block_statement(&builder, statement, &too_wide)?;
    }
    let missing = |name: &sqlparser::ast::ObjectName| {
        Error::RelationNotFoundForRuleAction {
            relation_name: builder.dialect().stored_object_name(name),
            rule_name: rule_name.clone(),
        }
    };
    function_bodies::bind_rule_parts(&builder, statements, rule.condition.as_mut(), missing)?;

    let case = builder.identifier_case();
    let existing = builder.rules().iter().position(|(existing, _)| {
        case_idents_match(&existing.name, &rule.name, case)
            && target_tables_match(&existing.table_name, &rule.table_name, case)
    });
    match (existing, rule.or_replace) {
        (Some(_), false) => {
            return Err(Error::RuleAlreadyExists {
                relation_name: builder.dialect().stored_object_name_last(&rule.table_name),
                rule_name: builder.dialect().stored_name(&rule.name),
            });
        }
        (Some(position), true) => {
            builder.rules_mut().remove(position);
        }
        (None, _) => {}
    }

    let calls = builder.calls_in(&rule);
    Ok(builder.add_rule(Arc::new(rule), calls))
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
/// empty for `DO NOTHING`.
pub(super) fn action_statements_mut(action: &mut CreateRuleAction) -> &mut [Statement] {
    match action {
        CreateRuleAction::Nothing => &mut [],
        CreateRuleAction::Statement(statement) => core::slice::from_mut(statement.as_mut()),
        CreateRuleAction::Statements(statements) => statements,
    }
}

/// Writes respelled statements back into a rule's action, one for each
/// statement it holds.
pub(super) fn write_action_statements(action: &mut CreateRuleAction, statements: Vec<Statement>) {
    for (held, statement) in action_statements_mut(action).iter_mut().zip(statements) {
        *held = statement;
    }
}

/// The key the rule's relation is stored under, which a rename or drop of
/// that relation or one of its columns must reach the rule by.
pub(super) fn relation_key(
    namespaces: Namespaces,
    rule: &CreateRule,
    case: IdentifierCase,
) -> RelationKey {
    target_key(namespaces, &target_name_of_object_name(&rule.table_name), case)
}

/// The rule as a dependency walk reaches it, by its stored name on its
/// relation keyed under `case`.
pub(super) fn dependent(
    namespaces: Namespaces,
    dialect: SqlparserDialect,
    rule: &CreateRule,
    case: IdentifierCase,
) -> views::Dependent {
    views::Dependent::Rule(relation_key(namespaces, rule, case), dialect.stored_name(&rule.name))
}

/// The normalized identity of every relation the rule's action and condition
/// read, which is the read set a dependency walk closes over.
pub(super) fn rules_reading(builder: &ParserDBBuilder, rule: &CreateRule) -> Vec<RelationKey> {
    let mut read = statements_read(builder, action_statements(&rule.action));
    if let Some(condition) = rule.condition.as_ref() {
        read.extend(relations_read_in(
            builder.namespaces(),
            &function_bodies::definition(condition),
            builder.identifier_case(),
        ));
    }
    read
}

/// Drops the rules defined on a view that is going away.
pub(super) fn drop_rules_on(builder: &mut ParserDBBuilder, host: &ViewHost) {
    builder.rules_mut().retain(|(rule, _)| !host.named_by(&rule.table_name));
}

/// Points the rules on a renamed or moved view at its stored name
/// `new_name`.
pub(super) fn follow_relation_rename(
    builder: &mut ParserDBBuilder,
    host: &ViewHost,
    new_name: &ObjectName,
) {
    for (rule, _) in builder.rules_mut() {
        if host.named_by(&rule.table_name) {
            Arc::make_mut(rule).table_name = new_name.clone();
        }
    }
}

/// Rewrites the `NEW` and `OLD` references to a renamed column in the rules
/// on its view.
pub(super) fn follow_column_rename(
    builder: &mut ParserDBBuilder,
    host: &ViewHost,
    from: &NamedColumn,
    to: &Ident,
) {
    for (rule, _) in builder.rules_mut() {
        if host.named_by(&rule.table_name) {
            rename_row_column(Arc::make_mut(rule), from, to);
        }
    }
}

/// Whether the rule's condition or action reads `column` of its relation
/// through `NEW` or `OLD`.
pub(super) fn reads_row_column(rule: &CreateRule, column: &NamedColumn) -> bool {
    rule.condition.as_ref().is_some_and(|condition| expression_reads_row_column(condition, column))
        || walk(&rule.action, &mut RowColumnMentioned { column }).is_break()
}

/// Renames every `NEW` and `OLD` reference to `from` in the rule's condition
/// and action.
pub(super) fn rename_row_column(rule: &mut CreateRule, from: &NamedColumn, to: &Ident) {
    if let Some(condition) = rule.condition.as_mut() {
        rename_row_column_in_expression(condition, from, to);
    }
    let mut renamer = RowColumnRenamer { column: from, to };
    let _: ControlFlow<()> = walk_mut(&mut rule.action, &mut renamer);
}

/// Whether the expression reads the relation's row column through `NEW` or
/// `OLD`, which a trigger's `WHEN` condition does as well.
pub(super) fn expression_reads_row_column(expression: &Expr, column: &NamedColumn) -> bool {
    let mut mentioned = RowColumnMentioned { column };
    walk(expression, &mut mentioned).is_break()
}

/// Renames every `NEW` and `OLD` reference to `from` in the expression.
pub(super) fn rename_row_column_in_expression(
    expression: &mut Expr,
    from: &NamedColumn,
    to: &Ident,
) {
    let mut renamer = RowColumnRenamer { column: from, to };
    let _: ControlFlow<()> = walk_mut(expression, &mut renamer);
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
