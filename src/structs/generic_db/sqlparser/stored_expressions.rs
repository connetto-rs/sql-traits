//! Refuses a subquery in an expression PostgreSQL stores and evaluates without
//! running a query, as `transformSubLink` does for every expression kind but
//! policies, rules and the queries of views, and a column reference in a
//! default, as `transformColumnRef` does.

use alloc::string::ToString;
use core::ops::ControlFlow;

use sqlparser::ast::{
    AlterColumnOperation, ColumnOption, CreateDomain, CreateIndex, CreateTable, Expr, ForValues,
    PartitionBoundValue, Query, TableConstraint, Visit, Visitor,
};

use super::ParserDBBuilder;
use crate::{
    errors::{Error, ExpressionPlace},
    impls::SqlparserDialect,
    utils::{columns_in_expression::Labels, walk::walk},
};

/// Finds the first query of an expression stored at `place`, or in a default
/// the first column reference, whichever PostgreSQL meets first.
struct Refused {
    place: ExpressionPlace,
    labels: Labels,
}

impl Visitor for Refused {
    type Break = Error;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        ControlFlow::Break(Error::SubqueryInExpression { place: self.place })
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.place != ExpressionPlace::Default || self.labels.holds(expr) {
            return ControlFlow::Continue(());
        }
        self.labels.enter(expr);
        match expr {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::QualifiedWildcard(..) => {
                ControlFlow::Break(Error::ColumnReferenceInDefault { reference: expr.to_string() })
            }
            _ => ControlFlow::Continue(()),
        }
    }
}

/// Whether `builder` models PostgreSQL, the engine whose refusals these are.
fn postgres(builder: &ParserDBBuilder) -> bool {
    matches!(builder.dialect(), SqlparserDialect::PostgreSql)
}

/// Refuses `node`, an expression stored at `place`, when it holds a subquery
/// or, as a default, a column reference.
fn refuse<N: Visit + ?Sized>(node: &N, place: ExpressionPlace) -> Result<(), Error> {
    match walk(node, &mut Refused { place, labels: Labels::default() }) {
        ControlFlow::Break(error) => Err(error),
        ControlFlow::Continue(()) => Ok(()),
    }
}

/// Refuses expressions forbidden in a stored column option.
fn column_option(option: &ColumnOption) -> Result<(), Error> {
    refuse_column_value(option)?;
    if let ColumnOption::Check(check) = option {
        refuse(&check.expr, ExpressionPlace::CheckConstraint)?;
    }
    Ok(())
}

/// Refuses subqueries and forbidden references in a column's value expression.
pub(super) fn refuse_column_value(option: &ColumnOption) -> Result<(), Error> {
    match option {
        ColumnOption::Default(default) => refuse(default, ExpressionPlace::Default),
        ColumnOption::Generated { generation_expr: Some(generation), .. } => {
            refuse(generation, ExpressionPlace::Generation)
        }
        _ => Ok(()),
    }
}

/// Refuses subqueries in the checks and partition key of a new table.
pub(super) fn refuse_table_checks(create_table: &CreateTable) -> Result<(), Error> {
    for column in &create_table.columns {
        for option in &column.options {
            if let ColumnOption::Check(check) = &option.option {
                refuse(&check.expr, ExpressionPlace::CheckConstraint)?;
            }
        }
    }
    create_table.constraints.iter().try_for_each(constraint)?;
    refuse(&create_table.partition_by, ExpressionPlace::PartitionKey)
}

/// Refuses a subquery in `constraint`, a check or an exclusion constraint.
fn constraint(constraint: &TableConstraint) -> Result<(), Error> {
    match constraint {
        TableConstraint::Check(check) => refuse(&check.expr, ExpressionPlace::CheckConstraint),
        TableConstraint::Exclude(exclusion) => {
            for element in &exclusion.elements {
                refuse(&element.column.column.expr, ExpressionPlace::IndexExpression)?;
            }
            refuse(&exclusion.where_clause, ExpressionPlace::IndexPredicate)
        }
        _ => Ok(()),
    }
}

/// Refuses a subquery in the values bounding a partition.
fn bound(bound: &ForValues) -> Result<(), Error> {
    let values = match bound {
        ForValues::In(values) => return refuse(values, ExpressionPlace::PartitionBound),
        ForValues::From { from, to } => from.iter().chain(to),
        ForValues::With { .. } | ForValues::Default => return Ok(()),
    };
    for value in values {
        if let PartitionBoundValue::Expr(value) = value {
            refuse(value, ExpressionPlace::PartitionBound)?;
        }
    }
    Ok(())
}

/// Refuses expressions forbidden in a delayed inline constraint.
pub(super) fn refuse_in_column_option(
    builder: &ParserDBBuilder,
    option: &ColumnOption,
) -> Result<(), Error> {
    if postgres(builder) { column_option(option) } else { Ok(()) }
}

/// Refuses a subquery in `added`, a constraint `ALTER TABLE` adds.
pub(super) fn refuse_in_constraint(
    builder: &ParserDBBuilder,
    added: &TableConstraint,
) -> Result<(), Error> {
    if postgres(builder) { constraint(added) } else { Ok(()) }
}

/// Refuses a subquery in `for_values`, the bound `ATTACH PARTITION` gives.
pub(super) fn refuse_in_bound(
    builder: &ParserDBBuilder,
    for_values: &ForValues,
) -> Result<(), Error> {
    if postgres(builder) { bound(for_values) } else { Ok(()) }
}

/// Refuses a subquery in the expressions or the predicate of `create_index`.
pub(super) fn refuse_in_index(
    builder: &ParserDBBuilder,
    create_index: &CreateIndex,
) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    if let Some(predicate) = &create_index.predicate {
        refuse(predicate, ExpressionPlace::IndexPredicate)?;
        super::column_references::refuse_in_expressions(
            builder,
            &create_index.table_name,
            &[None],
            core::iter::once(predicate),
        )?;
    }
    for column in &create_index.columns {
        let expression = &column.column.expr;
        refuse(expression, ExpressionPlace::IndexExpression)?;
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

/// Refuses a subquery in the default or the checks of `create_domain`, and a
/// column reference, `VALUE` included, in its default.
pub(super) fn refuse_in_domain(
    builder: &ParserDBBuilder,
    create_domain: &CreateDomain,
) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    refuse(&create_domain.default, ExpressionPlace::Default)?;
    create_domain.constraints.iter().try_for_each(constraint)
}

/// Refuses a subquery in the default or the `USING` expression `operation`
/// sets on a column, and a column reference in the default.
pub(super) fn refuse_in_column_change(
    builder: &ParserDBBuilder,
    operation: &AlterColumnOperation,
) -> Result<(), Error> {
    match operation {
        _ if !postgres(builder) => Ok(()),
        AlterColumnOperation::SetDefault { value } => refuse(value, ExpressionPlace::Default),
        AlterColumnOperation::SetDataType { using, .. } => {
            refuse(using, ExpressionPlace::Transform)
        }
        _ => Ok(()),
    }
}

/// Refuses a subquery in `condition`, the `WHEN` condition of a trigger.
pub(super) fn refuse_in_trigger_condition<N: Visit + ?Sized>(
    builder: &ParserDBBuilder,
    condition: &N,
) -> Result<(), Error> {
    if postgres(builder) { refuse(condition, ExpressionPlace::TriggerWhen) } else { Ok(()) }
}
