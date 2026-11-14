//! The objects of the schema holding a stored function, which PostgreSQL
//! names when it refuses to drop the function.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{ColumnOption, CreateFunction, CreateTrigger, Visit};

use super::{
    ParserDBBuilder, calls_in_expression::CallScope, constraint_names, function_argument_types,
    partition_keys,
};
use crate::{
    errors::{Error, ObjectKind},
    structs::{
        IdentifierCase, TableAttribute, generic_db::builtin_functions::Builtins,
        metadata::ExpressionCalls,
    },
    traits::{DomainLike, IndexLike, UserDefinedTypeLike},
    utils::{call_sites::for_each_call, last_str, object_name::stored_view_key},
};

/// An object holding a function, by its kind and the name a refusal gives
/// it.
pub(super) type Dependent = (ObjectKind, String);

/// Whether `calls` holds the function `dropped`, compared by node.
fn calls_function(calls: &ExpressionCalls<CreateFunction>, dropped: &CreateFunction) -> bool {
    calls.functions().any(|function| core::ptr::eq(function, dropped))
}

impl ParserDBBuilder {
    /// The first object, but the functions in `dropping` the same statement
    /// drops, that still calls or runs the stored function `dropped`.
    ///
    /// Every call PostgreSQL binds holds the function it reached when it was
    /// written, so the comparison is by node, and a body calling its own
    /// function does not hold it in place. A trigger holds the name of the
    /// function it runs, which reaches a function taking no arguments.
    pub(super) fn function_dependent(
        &self,
        dropped: &Arc<CreateFunction>,
        dropping: &[Arc<CreateFunction>],
    ) -> Option<Dependent> {
        self.table_dependent(dropped).or_else(|| self.object_dependent(dropped, dropping))
    }

    /// The first part of a table calling `dropped`, among its checks, its
    /// columns' defaults and generation expressions, its indexes, its
    /// exclusion constraints and its partition key.
    fn table_dependent(&self, dropped: &CreateFunction) -> Option<Dependent> {
        for (check, metadata) in self.check_constraints() {
            if calls_function(metadata.calls(), dropped) {
                let table = TableAttribute::table(check.as_ref());
                // A dialect giving unnamed constraints no name leaves the
                // check known by how it is written.
                let name = constraint_names::check_name(self, table, check.attribute())
                    .unwrap_or_else(|| check.attribute().to_string());
                return Some((
                    ObjectKind::CheckConstraint,
                    alloc::format!("{}.{name}", table.name),
                ));
            }
        }
        for (column, metadata, _) in self.columns() {
            let mut sites = metadata.calls().sites();
            for option in &column.attribute().options {
                let held = sites.by_ref().take(call_count(&option.option)).any(|reached| {
                    reached.iter().any(|function| core::ptr::eq(Arc::as_ptr(function), dropped))
                });
                // A column's check is a constraint of its own.
                let kind = match option.option {
                    ColumnOption::Check(_) => continue,
                    ColumnOption::Default(_) => ObjectKind::ColumnDefault,
                    _ => ObjectKind::Column,
                };
                if held {
                    let table = TableAttribute::table(column.as_ref());
                    let name = alloc::format!("{}.{}", table.name, column.attribute().name);
                    return Some((kind, name));
                }
            }
        }
        for (index, metadata) in self.indices() {
            if calls_function(metadata.calls(), dropped) {
                let name = IndexLike::name(index.as_ref())
                    .map_or_else(|| index.attribute().to_string(), ToString::to_string);
                return Some((ObjectKind::Index, name));
            }
        }
        for (table, metadata, _) in self.tables() {
            let exclusions = constraint_names::exclusions(table);
            if let Some((exclusion, _)) = exclusions
                .zip(metadata.exclusion_calls())
                .find(|(_, calls)| calls_function(calls, dropped))
            {
                // An exclusion constraint is backed by an index of its name.
                let name = constraint_names::exclusion_name(self, table, exclusion)
                    .unwrap_or_else(|| exclusion.to_string());
                return Some((ObjectKind::Index, name));
            }
            if partition_keys::key_entries(table).is_some()
                && calls_function(metadata.partition_key_calls(), dropped)
            {
                return Some((ObjectKind::Table, table.name.to_string()));
            }
        }
        None
    }

    /// The first object outside a table's own parts calling or running
    /// `dropped`, but the functions in `dropping`.
    fn object_dependent(
        &self,
        dropped: &CreateFunction,
        dropping: &[Arc<CreateFunction>],
    ) -> Option<Dependent> {
        let calls_dropped =
            |calls: &ExpressionCalls<CreateFunction>| calls_function(calls, dropped);
        if let Some((view, _, _)) =
            self.views().iter().find(|(_, metadata, _)| calls_dropped(metadata.calls()))
        {
            let name = stored_view_key(view.as_ref(), IdentifierCase::AsWritten).name;
            return Some((ObjectKind::View, name));
        }
        if let Some((view, _, _)) = self
            .materialized_views()
            .iter()
            .find(|(_, metadata, _)| calls_dropped(metadata.calls()))
        {
            let name = stored_view_key(view.as_ref(), IdentifierCase::AsWritten).name;
            return Some((ObjectKind::MaterializedView, name));
        }
        if let Some((policy, _)) = self.policies().iter().find(|(_, metadata)| {
            calls_dropped(metadata.using_calls()) || calls_dropped(metadata.check_calls())
        }) {
            return Some((ObjectKind::Policy, policy.name.value.clone()));
        }
        if let Some((rule, _)) = self.rules().iter().find(|(_, calls)| calls_dropped(calls)) {
            return Some((ObjectKind::Rule, rule.name.value.clone()));
        }
        // A bound name left bare is one in the default schema.
        let trigger_scope = CallScope {
            functions: &[],
            builtins: &Builtins::none(),
            search_path: &[("public", false)],
            case: self.identifier_case(),
        };
        let runs_dropped = |trigger: &CreateTrigger| {
            trigger.exec_body.as_ref().is_some_and(|body| {
                trigger_scope.reaches(dropped, &body.func_desc.name, 0)
                    && function_argument_types(dropped.args.as_deref()).is_empty()
            })
        };
        if let Some((trigger, _)) = self
            .triggers()
            .iter()
            .find(|(trigger, calls)| calls_dropped(calls) || runs_dropped(trigger))
        {
            return Some((ObjectKind::Trigger, last_str(&trigger.name).to_string()));
        }
        if let Some((caller, _)) = self.functions().iter().find(|(caller, metadata)| {
            !core::ptr::eq(caller.as_ref(), dropped)
                && !dropping.iter().any(|other| Arc::ptr_eq(other, caller))
                && calls_dropped(metadata.body_calls())
        }) {
            return Some((ObjectKind::Function, last_str(&caller.name).to_string()));
        }
        if let Some((domain, _)) =
            self.domains().iter().find(|(_, metadata)| calls_dropped(metadata.calls()))
        {
            return Some((ObjectKind::Domain, domain.name().to_string()));
        }
        self.user_defined_types()
            .iter()
            .find(|(_, calls)| calls_dropped(calls))
            .map(|(type_, _)| (ObjectKind::Type, type_.name().to_string()))
    }
}

/// The refusal to drop `function`, which `dependent` still holds.
pub(super) fn referenced(function: &CreateFunction, (kind, name): Dependent) -> Error {
    Error::FunctionReferenced {
        function_name: last_str(&function.name).to_string(),
        dependent_kind: kind,
        dependent_name: name,
    }
}

/// How many calls a walk of `node` meets, which is how many sites of the
/// record of the node holding it `node` takes.
fn call_count<N: Visit + ?Sized>(node: &N) -> usize {
    let mut count = 0;
    let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |_| {
        count += 1;
        ControlFlow::Continue(())
    });
    count
}
