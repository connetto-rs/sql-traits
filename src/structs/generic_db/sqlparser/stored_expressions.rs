//! Refuses what PostgreSQL refuses while transforming an expression it stores
//! and evaluates one row at a time, in the order it transforms the nodes.
//! Those are a subquery, as `transformSubLink` refuses one for every
//! expression kind but policies, rules and the queries of views, a column
//! reference in a default, as `transformColumnRef` refuses one, a system
//! column in a generation expression, and, once its arguments are
//! transformed, a call of an aggregate, a window or a set-returning
//! function, as `check_agglevels_and_constraints`, `transformWindowFuncCall`
//! and `check_srf_call_placement` refuse one.

use alloc::{string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    AlterColumnOperation, ColumnOption, CreateDomain, CreateIndex, CreateTable, Expr, ForValues,
    Function, FunctionArg, FunctionArguments, PartitionBoundValue, Query, TableConstraint, Visit,
    Visitor,
};

use super::{ParserDBBuilder, generated_expressions};
use crate::{
    errors::{Error, ExpressionPlace, RefusedCall},
    impls::SqlparserDialect,
    structs::{FunctionKind, places_call},
    traits::FunctionLike,
    utils::{
        call_sites::{argument_count, call_reaches, expression_form},
        columns_in_expression::Labels,
        object_name::object_name_last_part,
        walk::walk,
    },
};

/// Finds the first node of an expression stored at `place` that PostgreSQL
/// refuses while transforming it.
struct Refused<'b> {
    builder: &'b ParserDBBuilder,
    place: ExpressionPlace,
    labels: Labels,
    /// The system columns a generation expression reads, by the address of
    /// the name reading each.
    system_columns: Vec<(usize, &'static str)>,
    /// The call whose clauses the walk is passing over, by its address.
    ///
    /// PostgreSQL transforms `FILTER`, an aggregate's `ORDER BY` and `WITHIN
    /// GROUP` as expressions of their own kinds, which may run a subquery or
    /// read any column, and never transforms the window `OVER` names here.
    passing: Option<usize>,
}

impl<'b> Refused<'b> {
    fn new(builder: &'b ParserDBBuilder, place: ExpressionPlace) -> Self {
        Self {
            builder,
            place,
            labels: Labels::default(),
            system_columns: Vec::new(),
            passing: None,
        }
    }

    /// Walks the arguments `function` passes, then refuses the call when
    /// PostgreSQL does, and passes over the rest of it.
    fn call(&mut self, expr: &Expr, function: &Function) -> ControlFlow<Error> {
        walk(&function.parameters, self)?;
        match &function.args {
            FunctionArguments::List(list) => {
                for argument in &list.args {
                    match argument {
                        FunctionArg::Named { arg, .. }
                        | FunctionArg::ExprNamed { arg, .. }
                        | FunctionArg::Unnamed(arg) => walk(arg, self)?,
                    }
                }
            }
            arguments => walk(arguments, self)?,
        }
        self.passing = Some(core::ptr::from_ref(expr).addr());
        match self.refused_call(function) {
            Some(call) => ControlFlow::Break(Error::CallInExpression { call, place: self.place }),
            None => ControlFlow::Continue(()),
        }
    }

    /// What `function` computes that PostgreSQL refuses here, judged by every
    /// function the call reaches by name and argument count, or `None` while
    /// those disagree or the call writes a clause PostgreSQL refuses first.
    fn refused_call(&self, function: &Function) -> Option<RefusedCall> {
        if expression_form(&function.name).is_some() {
            return None;
        }
        let (name, _) = object_name_last_part(&function.name)?;
        let arguments = argument_count(function);
        let path = || self.builder.search_path();
        let case = self.builder.identifier_case();
        let mut refused = None;
        for (called, metadata) in
            self.builder.functions().iter().chain(self.builder.builtins().named(name))
        {
            if !call_reaches(called.as_ref(), &function.name, arguments, path, case) {
                continue;
            }
            let kind = metadata.kind();
            if !places_call(function, called.as_ref(), kind, metadata.aggregate_kind()) {
                return None;
            }
            let call = if function.over.is_some() {
                RefusedCall::Window
            } else if called.returns_set() {
                RefusedCall::SetReturning
            } else if kind == FunctionKind::Aggregate {
                RefusedCall::Aggregate
            } else {
                return None;
            };
            if refused.replace(call).is_some_and(|earlier| earlier != call) {
                return None;
            }
        }
        refused
    }

    /// The system column a generation expression may not read that `expr`
    /// names.
    fn system_column(&self, expr: &Expr) -> Option<&'static str> {
        let name = match expr {
            Expr::Identifier(name) => name,
            Expr::CompoundIdentifier(parts) => parts.last()?,
            _ => return None,
        };
        let address = core::ptr::from_ref(name).addr();
        self.system_columns.iter().find(|(read, _)| *read == address).map(|(_, system)| *system)
    }
}

impl Visitor for Refused<'_> {
    type Break = Error;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        if self.passing.is_some() {
            return ControlFlow::Continue(());
        }
        ControlFlow::Break(Error::SubqueryInExpression { place: self.place })
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.passing.is_some() || self.labels.holds(expr) {
            return ControlFlow::Continue(());
        }
        self.labels.enter(expr);
        if let Expr::Function(function) = expr {
            return self.call(expr, function);
        }
        if self.place == ExpressionPlace::Default
            && matches!(
                expr,
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::QualifiedWildcard(..)
            )
        {
            return ControlFlow::Break(Error::ColumnReferenceInDefault {
                reference: expr.to_string(),
            });
        }
        match self.system_column(expr) {
            Some(system) => {
                ControlFlow::Break(Error::GeneratedSystemColumnReference {
                    column: system.to_string(),
                })
            }
            None => ControlFlow::Continue(()),
        }
    }

    fn post_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.passing == Some(core::ptr::from_ref(expr).addr()) {
            self.passing = None;
        }
        ControlFlow::Continue(())
    }
}

/// Whether `builder` models PostgreSQL, the engine whose refusals these are.
fn postgres(builder: &ParserDBBuilder) -> bool {
    matches!(builder.dialect(), SqlparserDialect::PostgreSql)
}

/// Refuses `node` with the first refusal `refused` meets in it.
fn refuse_with<N: Visit + ?Sized>(node: &N, mut refused: Refused<'_>) -> Result<(), Error> {
    match walk(node, &mut refused) {
        ControlFlow::Break(error) => Err(error),
        ControlFlow::Continue(()) => Ok(()),
    }
}

/// Refuses `node`, an expression stored at `place`, when PostgreSQL refuses
/// a node of it while transforming it.
fn refuse<N: Visit + ?Sized>(
    builder: &ParserDBBuilder,
    node: &N,
    place: ExpressionPlace,
) -> Result<(), Error> {
    refuse_with(node, Refused::new(builder, place))
}

/// Refuses expressions forbidden in a stored column option of `table`.
fn column_option(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    option: &ColumnOption,
) -> Result<(), Error> {
    refuse_column_value(builder, table, option)?;
    if let ColumnOption::Check(check) = option {
        refuse(builder, &check.expr, ExpressionPlace::CheckConstraint)?;
    }
    Ok(())
}

/// Refuses what PostgreSQL refuses in the value expression of a column of
/// `table`.
pub(super) fn refuse_column_value(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    option: &ColumnOption,
) -> Result<(), Error> {
    match option {
        ColumnOption::Default(default) => refuse(builder, default, ExpressionPlace::Default),
        ColumnOption::Generated { generation_expr: Some(generation), .. } => {
            let mut refused = Refused::new(builder, ExpressionPlace::Generation);
            refused.system_columns =
                generated_expressions::system_columns(builder, table, generation);
            refuse_with(generation, refused)
        }
        _ => Ok(()),
    }
}

/// Refuses what PostgreSQL refuses in the checks and partition key of a new
/// table.
pub(super) fn refuse_table_checks(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
) -> Result<(), Error> {
    for column in &create_table.columns {
        for option in &column.options {
            if let ColumnOption::Check(check) = &option.option {
                refuse(builder, &check.expr, ExpressionPlace::CheckConstraint)?;
            }
        }
    }
    for added in &create_table.constraints {
        constraint(builder, added)?;
    }
    refuse(builder, &create_table.partition_by, ExpressionPlace::PartitionKey)
}

/// Refuses what PostgreSQL refuses in `constraint`, a check or an exclusion
/// constraint.
fn constraint(builder: &ParserDBBuilder, constraint: &TableConstraint) -> Result<(), Error> {
    match constraint {
        TableConstraint::Check(check) => {
            refuse(builder, &check.expr, ExpressionPlace::CheckConstraint)
        }
        TableConstraint::Exclude(exclusion) => {
            for element in &exclusion.elements {
                refuse(builder, &element.column.column.expr, ExpressionPlace::IndexExpression)?;
            }
            refuse(builder, &exclusion.where_clause, ExpressionPlace::IndexPredicate)
        }
        _ => Ok(()),
    }
}

/// Refuses what PostgreSQL refuses in the values bounding a partition.
fn bound(builder: &ParserDBBuilder, bound: &ForValues) -> Result<(), Error> {
    let values = match bound {
        ForValues::In(values) => return refuse(builder, values, ExpressionPlace::PartitionBound),
        ForValues::From { from, to } => from.iter().chain(to),
        ForValues::With { .. } | ForValues::Default => return Ok(()),
    };
    for value in values {
        if let PartitionBoundValue::Expr(value) = value {
            refuse(builder, value, ExpressionPlace::PartitionBound)?;
        }
    }
    Ok(())
}

/// Refuses expressions forbidden in a delayed inline constraint of a column
/// added to `table`.
pub(super) fn refuse_in_column_option(
    builder: &ParserDBBuilder,
    table: &CreateTable,
    option: &ColumnOption,
) -> Result<(), Error> {
    if postgres(builder) { column_option(builder, table, option) } else { Ok(()) }
}

/// Refuses what PostgreSQL refuses in `added`, a constraint `ALTER TABLE`
/// adds.
pub(super) fn refuse_in_constraint(
    builder: &ParserDBBuilder,
    added: &TableConstraint,
) -> Result<(), Error> {
    if postgres(builder) { constraint(builder, added) } else { Ok(()) }
}

/// Refuses what PostgreSQL refuses in `for_values`, the bound `ATTACH
/// PARTITION` gives.
pub(super) fn refuse_in_bound(
    builder: &ParserDBBuilder,
    for_values: &ForValues,
) -> Result<(), Error> {
    if postgres(builder) { bound(builder, for_values) } else { Ok(()) }
}

/// Refuses what PostgreSQL refuses in the expressions or the predicate of
/// `create_index`.
pub(super) fn refuse_in_index(
    builder: &ParserDBBuilder,
    create_index: &CreateIndex,
) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    if let Some(predicate) = &create_index.predicate {
        refuse(builder, predicate, ExpressionPlace::IndexPredicate)?;
        super::column_references::refuse_in_expressions(
            builder,
            &create_index.table_name,
            &[None],
            core::iter::once(predicate),
        )?;
    }
    for column in &create_index.columns {
        let expression = &column.column.expr;
        refuse(builder, expression, ExpressionPlace::IndexExpression)?;
        if !matches!(expression, Expr::Identifier(_)) {
            super::column_references::refuse_in_expressions(
                builder,
                &create_index.table_name,
                &[None],
                core::iter::once(expression),
            )?;
        }
    }
    Ok(())
}

/// Refuses what PostgreSQL refuses in the default or the checks of
/// `create_domain`, a column reference, `VALUE` included, in its default
/// among them.
pub(super) fn refuse_in_domain(
    builder: &ParserDBBuilder,
    create_domain: &CreateDomain,
) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    refuse(builder, &create_domain.default, ExpressionPlace::Default)?;
    for check in &create_domain.constraints {
        constraint(builder, check)?;
    }
    Ok(())
}

/// Refuses what PostgreSQL refuses in the default or the `USING` expression
/// `operation` sets on a column, a column reference in the default among
/// them.
pub(super) fn refuse_in_column_change(
    builder: &ParserDBBuilder,
    operation: &AlterColumnOperation,
) -> Result<(), Error> {
    match operation {
        _ if !postgres(builder) => Ok(()),
        AlterColumnOperation::SetDefault { value } => {
            refuse(builder, value, ExpressionPlace::Default)
        }
        AlterColumnOperation::SetDataType { using, .. } => {
            refuse(builder, using, ExpressionPlace::Transform)
        }
        _ => Ok(()),
    }
}

/// Refuses what PostgreSQL refuses in `condition`, the `WHEN` condition of
/// a trigger.
pub(super) fn refuse_in_trigger_condition<N: Visit + ?Sized>(
    builder: &ParserDBBuilder,
    condition: &N,
) -> Result<(), Error> {
    if postgres(builder) {
        refuse(builder, condition, ExpressionPlace::TriggerWhen)
    } else {
        Ok(())
    }
}
