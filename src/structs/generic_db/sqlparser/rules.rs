//! Recording rewrite rules and keeping them on the table or plain view they
//! are defined on.
//!
//! A rule depends on its relation and on the columns it reads through `NEW`
//! and `OLD`, so those references follow renames and drops. Relations the
//! action names stay as written, although PostgreSQL also tracks them.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CreateRule, CreateRuleEvent, Expr, Ident, ObjectNamePart, Visit, VisitMut, Visitor, VisitorMut,
};

use super::{
    NamedColumn, ParserDBBuilder, bind_reference, case_idents_match,
    object_name_matches_resolved_identity, require_named, stored_qualifier, stored_view_qualifier,
    target_tables_match,
};
use crate::{
    errors::{Error, ObjectKind},
    traits::ViewLike,
    utils::{identifier_resolution::identifiers_match, last_str},
};

/// Records a `CREATE RULE`, bound to the table or view its target reaches.
///
/// # Errors
///
/// Returns [`Error::RelationNotFoundForRule`] when no table or view holds the
/// target, [`Error::RuleOnUnsupportedRelation`] when a foreign table or a
/// materialized view does, [`Error::SelectRuleUnsupported`] for an `ON SELECT`
/// rule, and [`Error::RuleAlreadyExists`] when the relation carries a rule of
/// the same name and the statement wrote no `OR REPLACE`.
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
    let unsupported = |rule: CreateRule, relation_kind| {
        Err(Error::RuleOnUnsupportedRelation {
            relation_name: relation_name(&rule),
            rule_name: rule.name.value,
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

/// Points the rules on a renamed relation at its new name, which stays in the
/// same schema.
pub(super) fn follow_relation_rename(
    builder: &mut ParserDBBuilder,
    host: &RuleHost,
    new_name: &Ident,
) {
    for (rule, ()) in builder.rules_mut() {
        if host.carries(rule)
            && let Some(last) = Arc::make_mut(rule).table_name.0.last_mut()
        {
            *last = ObjectNamePart::Identifier(new_name.clone());
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
    let mut mentioned = RowColumnMentioned { column };
    rule.condition.visit(&mut mentioned).is_break() || rule.action.visit(&mut mentioned).is_break()
}

/// Renames every `NEW` and `OLD` reference to `from` in the rule's condition
/// and action.
pub(super) fn rename_row_column(rule: &mut CreateRule, from: &NamedColumn, to: &Ident) {
    let mut renamer = RowColumnRenamer { column: from, to };
    let _: ControlFlow<()> = VisitMut::visit(&mut rule.condition, &mut renamer);
    let _: ControlFlow<()> = VisitMut::visit(&mut rule.action, &mut renamer);
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
