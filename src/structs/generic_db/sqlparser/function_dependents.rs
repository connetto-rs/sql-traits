//! The objects of the schema holding a stored function, which PostgreSQL
//! names when it refuses to drop the function and takes along under
//! `CASCADE`.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::{convert::Infallible, ops::ControlFlow};

use sqlparser::ast::{ColumnOption, CreateFunction, CreatePolicy, CreateTrigger, Ident, Visit};

use super::{
    ParserDBBuilder, StoredTable,
    calls_in_expression::CallScope,
    constraint_names::{self, Position},
    function_argument_types, inheritance, partition_keys, rules, views,
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

/// Where an object holding a function sits, which is what taking it along
/// under `CASCADE` needs.
pub(super) enum Held {
    /// The default of the column of the table.
    ColumnDefault(StoredTable, Ident),
    /// The stored generated column of the table, declared by the table
    /// itself.
    GeneratedColumn(StoredTable, Ident),
    /// The check at the position in the table, its own rather than one a
    /// parent passes down.
    Check(StoredTable, Position),
    /// The index at the position among the stored indexes.
    Index(usize),
    /// The exclusion constraint at the position of the table's constraint
    /// list.
    Exclusion(StoredTable, usize),
    /// The table whose partition key calls the function.
    PartitionedTable(StoredTable),
    /// A view, a materialized view, a rule or a calling function, which goes
    /// with what reads it.
    Reader(views::Dependent),
    /// The policy.
    Policy(Arc<CreatePolicy>),
    /// The trigger.
    Trigger(Arc<CreateTrigger>),
    /// The check at the position of the constraint list of the domain at the
    /// position among the stored domains.
    DomainCheck(usize, usize),
    /// The domain at the position among the stored domains, whose default
    /// calls the function.
    Domain(usize),
    /// The user-defined type at the position, which runs the function.
    Type(usize),
}

/// The first object holding a function, with the name a refusal gives it.
pub(super) struct Holder {
    /// How a refusal names the object.
    pub(super) named: Dependent,
    /// Where the object sits.
    pub(super) held: Held,
}

impl Holder {
    /// The holder `held`, named `name` of `kind`.
    const fn new(kind: ObjectKind, name: String, held: Held) -> Self {
        Self { named: (kind, name), held }
    }
}

/// Whether `calls` holds the function `dropped`, compared by node.
fn calls_function(calls: &ExpressionCalls<CreateFunction>, dropped: &CreateFunction) -> bool {
    calls.functions().any(|function| core::ptr::eq(function, dropped))
}

/// Whether one of the next `count` sites of `sites` reaches `dropped`, every
/// one of them taken off `sites`.
fn sites_reach<'a>(
    sites: &mut impl Iterator<Item = &'a [Arc<CreateFunction>]>,
    count: usize,
    dropped: &CreateFunction,
) -> bool {
    let mut held = false;
    for reached in sites.by_ref().take(count) {
        held |= reached.iter().any(|function| core::ptr::eq(Arc::as_ptr(function), dropped));
    }
    held
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
        dropped: &CreateFunction,
        dropping: &[Arc<CreateFunction>],
    ) -> Option<Dependent> {
        self.function_holder(dropped, dropping).map(|holder| holder.named)
    }

    /// The first object, but the functions in `dropping`, that still calls
    /// or runs the stored function `dropped`, with where it sits.
    pub(super) fn function_holder(
        &self,
        dropped: &CreateFunction,
        dropping: &[Arc<CreateFunction>],
    ) -> Option<Holder> {
        self.table_holder(dropped).or_else(|| self.object_holder(dropped, dropping))
    }

    /// The first part of a table calling `dropped`, among its checks, its
    /// columns' defaults and generation expressions, its indexes, its
    /// exclusion constraints and its partition key.
    ///
    /// A check or a generated column a parent passes down goes with the
    /// parent's, so only a table's own are holders.
    fn table_holder(&self, dropped: &CreateFunction) -> Option<Holder> {
        for (check, metadata) in self.check_constraints() {
            let table = TableAttribute::table(check.as_ref());
            let own = calls_function(metadata.calls(), dropped)
                .then(|| constraint_names::check_slot(table, check.attribute()))
                .flatten()
                .filter(|slot| !constraint_names::receives_check(self, table, *slot));
            let Some(slot) = own else {
                continue;
            };
            // A dialect giving unnamed constraints no name leaves the check
            // known by how it is written.
            let name = constraint_names::check_name(self, table, check.attribute())
                .unwrap_or_else(|| check.attribute().to_string());
            return Some(Holder::new(
                ObjectKind::CheckConstraint,
                alloc::format!("{}.{name}", table.name),
                Held::Check(StoredTable::of(table), slot.position()),
            ));
        }
        for (column, metadata, _) in self.columns() {
            let table = TableAttribute::table(column.as_ref());
            let declared = column.attribute();
            let mut sites = metadata.calls().sites();
            for option in &declared.options {
                let held = sites_reach(&mut sites, call_count(&option.option), dropped);
                let name = || alloc::format!("{}.{}", table.name, declared.name);
                let holder = match option.option {
                    // A column's check is a constraint of its own.
                    ColumnOption::Check(_) => continue,
                    ColumnOption::Default(_) => {
                        Holder::new(
                            ObjectKind::ColumnDefault,
                            name(),
                            Held::ColumnDefault(StoredTable::of(table), declared.name.clone()),
                        )
                    }
                    _ if inheritance::is_inherited_column(self, table, &declared.name) => continue,
                    _ => {
                        Holder::new(
                            ObjectKind::Column,
                            name(),
                            Held::GeneratedColumn(StoredTable::of(table), declared.name.clone()),
                        )
                    }
                };
                if held {
                    return Some(holder);
                }
            }
        }
        for (position, (index, metadata)) in self.indices().iter().enumerate() {
            if calls_function(metadata.calls(), dropped) {
                let name = IndexLike::name(index.as_ref())
                    .map_or_else(|| index.attribute().to_string(), ToString::to_string);
                return Some(Holder::new(ObjectKind::Index, name, Held::Index(position)));
            }
        }
        for (table, metadata, _) in self.tables() {
            let positions = table
                .constraints
                .iter()
                .enumerate()
                .filter(|(_, constraint)| {
                    matches!(constraint, sqlparser::ast::TableConstraint::Exclude(_))
                })
                .map(|(position, _)| position);
            if let Some(((exclusion, position), _)) = constraint_names::exclusions(table)
                .zip(positions)
                .zip(metadata.exclusion_calls())
                .find(|(_, calls)| calls_function(calls, dropped))
            {
                // An exclusion constraint is backed by an index of its name.
                let name = constraint_names::exclusion_name(self, table, exclusion)
                    .unwrap_or_else(|| exclusion.to_string());
                return Some(Holder::new(
                    ObjectKind::Index,
                    name,
                    Held::Exclusion(StoredTable::of(table), position),
                ));
            }
            if partition_keys::key_entries(table).is_some()
                && calls_function(metadata.partition_key_calls(), dropped)
            {
                return Some(Holder::new(
                    ObjectKind::Table,
                    table.name.to_string(),
                    Held::PartitionedTable(StoredTable::of(table)),
                ));
            }
        }
        None
    }

    /// The first object outside a table's own parts calling or running
    /// `dropped`, but the functions in `dropping`.
    fn object_holder(
        &self,
        dropped: &CreateFunction,
        dropping: &[Arc<CreateFunction>],
    ) -> Option<Holder> {
        let calls_dropped =
            |calls: &ExpressionCalls<CreateFunction>| calls_function(calls, dropped);
        if let Some((view, _, _)) =
            self.views().iter().find(|(_, metadata, _)| calls_dropped(metadata.calls()))
        {
            let key = stored_view_key(view.as_ref(), IdentifierCase::AsWritten);
            return Some(Holder::new(
                ObjectKind::View,
                key.name.clone(),
                Held::Reader(views::Dependent::Reader(ObjectKind::View, key)),
            ));
        }
        if let Some((view, _, _)) = self
            .materialized_views()
            .iter()
            .find(|(_, metadata, _)| calls_dropped(metadata.calls()))
        {
            let key = stored_view_key(view.as_ref(), IdentifierCase::AsWritten);
            return Some(Holder::new(
                ObjectKind::MaterializedView,
                key.name.clone(),
                Held::Reader(views::Dependent::Reader(ObjectKind::MaterializedView, key)),
            ));
        }
        if let Some((policy, _)) = self.policies().iter().find(|(_, metadata)| {
            calls_dropped(metadata.using_calls()) || calls_dropped(metadata.check_calls())
        }) {
            return Some(Holder::new(
                ObjectKind::Policy,
                policy.name.value.clone(),
                Held::Policy(Arc::clone(policy)),
            ));
        }
        if let Some((rule, _)) = self.rules().iter().find(|(_, calls)| calls_dropped(calls)) {
            return Some(Holder::new(
                ObjectKind::Rule,
                rule.name.value.clone(),
                Held::Reader(rules::dependent(rule)),
            ));
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
            return Some(Holder::new(
                ObjectKind::Trigger,
                last_str(&trigger.name).to_string(),
                Held::Trigger(Arc::clone(trigger)),
            ));
        }
        if let Some((caller, _)) = self.functions().iter().find(|(caller, metadata)| {
            !core::ptr::eq(caller.as_ref(), dropped)
                && !dropping.iter().any(|other| Arc::ptr_eq(other, caller))
                && calls_dropped(metadata.body_calls())
        }) {
            return Some(Holder::new(
                ObjectKind::Function,
                last_str(&caller.name).to_string(),
                Held::Reader(views::Dependent::Function(Arc::clone(caller))),
            ));
        }
        for (position, (domain, metadata)) in self.domains().iter().enumerate() {
            let mut sites = metadata.calls().sites();
            let held = if sites_reach(&mut sites, call_count(&domain.default), dropped) {
                Held::Domain(position)
            } else if let Some(check) = domain
                .constraints
                .iter()
                .position(|check| sites_reach(&mut sites, call_count(check), dropped))
            {
                Held::DomainCheck(position, check)
            } else {
                continue;
            };
            return Some(Holder::new(ObjectKind::Domain, domain.name().to_string(), held));
        }
        self.user_defined_types()
            .iter()
            .enumerate()
            .find(|(_, (_, calls))| calls_dropped(calls))
            .map(|(position, (type_, _))| {
                Holder::new(ObjectKind::Type, type_.name().to_string(), Held::Type(position))
            })
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
pub(super) fn call_count<N: Visit + ?Sized>(node: &N) -> usize {
    let mut count = 0;
    let ControlFlow::<Infallible>::Continue(()) = for_each_call(node, |_| {
        count += 1;
        ControlFlow::Continue(())
    });
    count
}
