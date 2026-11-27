//! The columns of a table whose value every `INSERT` takes from its
//! `BEFORE INSERT` row triggers, whatever value the statement gives them.
//!
//! PostgreSQL fires the enabled `BEFORE INSERT … FOR EACH ROW` triggers of
//! the table a row lands in, in the byte order of their names, each handed
//! the `NEW` the previous one returned. A column is filled when one of them
//! without `WHEN` assigns it on every path of its body returning `NEW`, and
//! nothing before that assignment reads the value the statement gave,
//! whether an earlier trigger, an earlier `WHEN` condition or an earlier
//! part of the filling body, naming the field or the whole row.

use alloc::{borrow::ToOwned, collections::BTreeSet, string::String, vec::Vec};
use core::{convert::Infallible, ops::ControlFlow, slice};

use sqlparser::ast::{
    CreateRuleEvent, Expr, Ident, Statement, TriggerEvent, TriggerObject, TriggerObjectKind,
    TriggerPeriod, Value, ValueWithSpan, Visitor,
};
use sqlparser_expr_eval::Outcomes;
use sqlparser_plpgsql::{
    embedded::{NodeKind, expression},
    embedded_sql,
    namespace::{Name, NameKind, Namespace, Site, Sql, TriggerVariable},
};
use sqlparser_plpgsql_dataflow::{
    Catalog, Domain, Ending, Part, Return, Step, Test, Transactions, Write, walk,
};

use super::dynamic_sites::{BodyNames, SiteKey};
use crate::{
    errors::LookupError,
    structs::TriggerEnablement,
    traits::{ColumnLike, DatabaseLike, FunctionLike, RuleLike, TableLike, TriggerLike},
    utils::{
        identifier_resolution::{name_parts, stored_identifier},
        walk::walk as walk_syntax,
    },
};

/// The stored names of the columns of `table` whose value every `INSERT`
/// takes from its `BEFORE INSERT` row triggers.
///
/// A row inserted into a partitioned table lands in one of its partitions
/// and fires that partition's triggers, so a partitioned table with
/// partitions answers the columns every partition fills.
///
/// # Errors
///
/// Returns the [`LookupError`] that reading the table's triggers, rules,
/// columns or partitions, or a trigger function's `search_path` setting,
/// raises.
pub(crate) fn trigger_filled_columns<DB: DatabaseLike>(
    table: &DB::Table,
    database: &DB,
) -> Result<BTreeSet<String>, LookupError> {
    if table.is_partitioned() {
        let mut every: Option<BTreeSet<String>> = None;
        for partition in table.partitions(database)? {
            let filled = trigger_filled_columns(partition, database)?;
            match &mut every {
                Some(every) => every.retain(|column| filled.contains(column)),
                None => every = Some(filled),
            }
        }
        if let Some(every) = every {
            return Ok(every);
        }
    }
    // A rule rewrites the statement before any trigger sees the row, and
    // may read every value it gives.
    if table.rules(database)?.any(|rule| rule.event() == CreateRuleEvent::Insert) {
        return Ok(BTreeSet::new());
    }
    let columns: Vec<String> =
        table.columns(database)?.map(|column| column.stored_column_name().into_owned()).collect();
    let mut chain = Vec::new();
    for trigger in table.triggers(database)? {
        let fires = trigger.timing() == Some(TriggerPeriod::Before)
            && matches!(
                trigger.orientation(),
                Some(
                    TriggerObjectKind::For(TriggerObject::Row)
                        | TriggerObjectKind::ForEach(TriggerObject::Row)
                )
            )
            && trigger.events().contains(&TriggerEvent::Insert)
            && matches!(
                trigger.enablement(database)?,
                TriggerEnablement::Origin | TriggerEnablement::Always
            );
        if fires {
            chain.push(trigger);
        }
    }
    chain.sort_by(|left, right| left.stored_name().as_bytes().cmp(right.stored_name().as_bytes()));
    // `filled` holds the columns no longer holding the given value, and
    // `read` those whose given value something read first, which nothing
    // fills afterwards.
    let mut filled: BTreeSet<&str> = BTreeSet::new();
    let mut read: BTreeSet<&str> = BTreeSet::new();
    for trigger in chain {
        let mut reads = BTreeSet::new();
        if let Some(condition) = trigger.condition() {
            let condition_reads = RowReads::of(condition, |parts: &[Ident]| {
                let first = parts.first()?;
                (stored_identifier(&first.value, first.quote_style.is_some()) == "new").then_some(1)
            });
            reads.extend(condition_reads.fields_of(&columns, &Assigned::default()));
        }
        // A body this cannot read may read every field it is handed.
        let effect = body_effect(trigger, &columns, database)?.unwrap_or_else(|| {
            BodyEffect {
                reads: columns.iter().map(String::as_str).collect(),
                assigns: BTreeSet::new(),
            }
        });
        reads.extend(effect.reads);
        read.extend(reads.difference(&filled));
        if trigger.condition().is_none() {
            filled.extend(effect.assigns.difference(&read));
        }
    }
    Ok(filled.into_iter().map(str::to_owned).collect())
}

/// What a `BEFORE INSERT` row trigger's body does to the fields of `NEW`
/// that are columns of its table.
struct BodyEffect<'columns> {
    /// The columns it may read before assigning them.
    reads: BTreeSet<&'columns str>,
    /// The columns it assigns on every path returning `NEW`.
    assigns: BTreeSet<&'columns str>,
}

/// What the PL/pgSQL body `trigger` runs does to the fields of `NEW` named
/// `columns`, or [`None`] for a body this cannot read.
///
/// A body returning anything but `NEW` or `NULL` on some path assigns
/// nothing, and so does a body that never returns `NEW`.
///
/// # Errors
///
/// Returns the [`LookupError`] that reading the function's own
/// `search_path` setting raises.
fn body_effect<'columns, DB: DatabaseLike>(
    trigger: &DB::Trigger,
    columns: &'columns [String],
    database: &DB,
) -> Result<Option<BodyEffect<'columns>>, LookupError> {
    let Some(function) = trigger.function(database) else { return Ok(None) };
    if function.stored_language().as_deref() != Some("plpgsql") {
        return Ok(None);
    }
    let Some(Ok(block)) = function.body().map(embedded_sql) else { return Ok(None) };
    let names = BodyNames::new(function, database)?;
    let signature = names.signature();
    let Ok(sites) = block.sites(&signature, |name| names.holds_row(name)) else {
        return Ok(None);
    };
    let mut domain = NewAssignments::default();
    // A trigger function is a function returning `trigger`, which refuses
    // transaction control and raises when control runs off its end.
    let Ok(flow) = walk(
        &block,
        &signature,
        Transactions::Refused,
        Ending::Raises,
        &mut RowCatalog(&names),
        &mut domain,
    ) else {
        return Ok(None);
    };
    let namespace_of = |sql: Sql<'_>| {
        sites
            .iter()
            .find(|site| SiteKey::of(site.sql) == SiteKey::of(sql))
            .map(|site| &site.namespace)
    };
    let new_row = |namespace: &Namespace, parts: &[Ident]| {
        let stored: Vec<_> = parts
            .iter()
            .map(|part| stored_identifier(&part.value, part.quote_style.is_some()))
            .collect();
        let (name, consumed) = namespace.lookup(&stored, |name| names.holds_row(name))?;
        matches!(name.kind(), NameKind::Trigger(TriggerVariable::New)).then_some(consumed)
    };
    let mut returned_new = BTreeSet::new();
    let mut assigned: Option<Assigned> = None;
    let mut returns_other = false;
    for (point, state) in flow.returns() {
        let Return::Statement(node) = point else { continue };
        let NodeKind::Return { value: Some(value) } = &node.kind else {
            returns_other = true;
            continue;
        };
        let sql = Sql::Expression(value);
        match &*expression(value) {
            Expr::Value(ValueWithSpan { value: Value::Null, .. }) => {}
            returned
                if name_parts(returned).is_some_and(|parts| {
                    namespace_of(sql).and_then(|namespace| new_row(namespace, parts))
                        == Some(parts.len())
                }) =>
            {
                returned_new.insert(SiteKey::of(sql));
                match &mut assigned {
                    Some(assigned) => assigned.meet(state),
                    None => assigned = Some(state.clone()),
                }
            }
            _ => returns_other = true,
        }
    }
    let mut reads: BTreeSet<&str> = columns
        .iter()
        .map(String::as_str)
        .filter(|column| domain.partially_written.contains(*column))
        .collect();
    for Site { sql, namespace } in &sites {
        let Some(state) = flow.at(*sql) else { continue };
        if returned_new.contains(&SiteKey::of(*sql)) {
            continue;
        }
        let resolve = |parts: &[Ident]| new_row(namespace, parts);
        let site_reads = match sql {
            Sql::Statement(statement) => RowReads::of(*statement, resolve),
            Sql::Expression(query) => RowReads::of(*query, resolve),
            Sql::Dynamic(dynamic) => {
                let mut site_reads = RowReads::of(&*dynamic.command, resolve);
                for argument in &dynamic.arguments {
                    site_reads.extend(RowReads::of(argument, resolve));
                }
                site_reads
            }
        };
        reads.extend(site_reads.fields_of(columns, state));
    }
    let assigns = match assigned {
        Some(assigned) if !returns_other => {
            columns.iter().map(String::as_str).filter(|column| assigned.covers(column)).collect()
        }
        _ => BTreeSet::new(),
    };
    Ok(Some(BodyEffect { reads, assigns }))
}

/// What the catalog tells the walk of a trigger body.
///
/// A `CALL` writing back to a field of `NEW` is taken to write maybe, so
/// only an assignment the body itself makes fills a field.
struct RowCatalog<'names, 'db, DB: DatabaseLike>(&'names BodyNames<'db, DB>);

impl<DB: DatabaseLike> Catalog for RowCatalog<'_, '_, DB> {
    fn holds_row(&self, name: &Name) -> bool {
        self.0.holds_row(name)
    }

    fn written_back(&mut self, _call: &Statement) -> Option<Vec<bool>> {
        None
    }
}

/// The fields of `NEW` assigned on every path reaching a point of a body,
/// each as PostgreSQL stores its name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Assigned {
    /// Whether the whole row is assigned, which assigns every field.
    whole: bool,
    /// The fields assigned, empty when `whole` is.
    fields: BTreeSet<String>,
}

impl Assigned {
    /// Whether `field` is assigned.
    fn covers(&self, field: &str) -> bool {
        self.whole || self.fields.contains(field)
    }

    /// Keeps only what `other` assigns too, where two paths meet.
    fn meet(&mut self, other: &Self) {
        if other.whole {
            return;
        }
        if self.whole {
            self.clone_from(other);
            return;
        }
        self.fields.retain(|field| other.fields.contains(field));
    }
}

/// The [`Domain`] of the fields of `NEW` assigned on every path, which also
/// keeps each field a write changes part of before any path assigns it whole,
/// since such a write keeps the rest of the value it was handed.
#[derive(Debug, Default)]
struct NewAssignments {
    partially_written: BTreeSet<String>,
}

impl<'body> Domain<'body> for NewAssignments {
    type State = Assigned;
    type Error = Infallible;

    fn entry(&mut self) -> Result<Assigned, Infallible> {
        Ok(Assigned::default())
    }

    fn join(&self, into: &mut Assigned, from: &Assigned) {
        into.meet(from);
    }

    fn widen(&self, head: &Assigned, mut next: Assigned, _round: usize) -> Assigned {
        // Each round keeps fewer of finitely many fields.
        next.meet(head);
        next
    }

    fn write(
        &mut self,
        write: &Write<'body>,
        inputs: &Assigned,
        state: &mut Assigned,
    ) -> Result<(), Infallible> {
        if !matches!(write.variable.kind(), NameKind::Trigger(TriggerVariable::New)) {
            return Ok(());
        }
        match write.part.as_slice() {
            [] if write.surely => {
                state.whole = true;
                state.fields.clear();
            }
            [Part::Field(field)] if write.surely && !state.whole => {
                state.fields.insert(field.clone());
            }
            [Part::Field(field), _, ..] if !inputs.covers(field) => {
                self.partially_written.insert(field.clone());
            }
            _ => {}
        }
        Ok(())
    }

    fn assume(
        &mut self,
        _test: Test<'body>,
        _outcomes: Outcomes,
        state: Assigned,
    ) -> Result<Option<Assigned>, Infallible> {
        Ok(Some(state))
    }

    fn may_raise(&mut self, _step: Step<'body>, _state: &Assigned) -> Result<bool, Infallible> {
        Ok(true)
    }
}

/// The fields of `NEW` one piece of SQL reads, and whether it reads the
/// whole row.
#[derive(Debug, Default)]
struct RowReads {
    /// The fields read by name, as PostgreSQL stores each.
    fields: BTreeSet<String>,
    /// Whether the whole row is read, which any naming of `NEW` other than
    /// one of its fields is taken to do.
    whole: bool,
}

impl RowReads {
    /// What `node` reads of `NEW`, where `resolve` answers how many leading
    /// parts of a name name `NEW`, and [`None`] for a name not reaching it.
    fn of<N: sqlparser::ast::Visit + ?Sized>(
        node: &N,
        resolve: impl Fn(&[Ident]) -> Option<usize>,
    ) -> Self {
        let mut collector = Collector { resolve, named: BTreeSet::new(), reads: Self::default() };
        let _: ControlFlow<()> = walk_syntax(node, &mut collector);
        collector.reads
    }

    /// Adds what `other` reads.
    fn extend(&mut self, other: Self) {
        self.whole |= other.whole;
        self.fields.extend(other.fields);
    }

    /// The columns among `columns` read while `assigned` does not cover
    /// them.
    fn fields_of<'columns>(
        &self,
        columns: &'columns [String],
        assigned: &Assigned,
    ) -> impl Iterator<Item = &'columns str> {
        columns.iter().map(String::as_str).filter(|column| {
            !assigned.covers(column) && (self.whole || self.fields.contains(*column))
        })
    }
}

/// Gathers the [`RowReads`] of a walk.
struct Collector<F> {
    resolve: F,
    /// The addresses of the identifiers of names already read as a field.
    named: BTreeSet<usize>,
    reads: RowReads,
}

impl<F: Fn(&[Ident]) -> Option<usize>> Visitor for Collector<F> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        if let Some(parts) = name_parts(expr)
            && let Some(consumed) = (self.resolve)(parts)
        {
            match parts.get(consumed) {
                Some(field) => {
                    self.reads.fields.insert(
                        stored_identifier(&field.value, field.quote_style.is_some()).into_owned(),
                    );
                }
                None => self.reads.whole = true,
            }
            self.named.extend(parts.iter().map(|part| core::ptr::from_ref(part).addr()));
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_ident(&mut self, ident: &Ident) -> ControlFlow<()> {
        // `NEW.*`, `(NEW).x` and every other naming that is no field read
        // reads the whole row.
        if !self.named.contains(&core::ptr::from_ref(ident).addr())
            && (self.resolve)(slice::from_ref(ident)).is_some()
        {
            self.reads.whole = true;
        }
        ControlFlow::Continue(())
    }
}
