//! Refuses a subquery in an expression PostgreSQL stores and evaluates without
//! running a query, as `transformSubLink` does for every expression kind but
//! policies, rules and the queries of views.

use core::ops::ControlFlow;

use sqlparser::ast::{
    AlterColumnOperation, ColumnDef, ColumnOption, CreateDomain, CreateIndex, CreateTable,
    ForValues, PartitionBoundValue, Query, TableConstraint, Visit, Visitor,
};

use super::ParserDBBuilder;
use crate::{
    errors::{Error, ExpressionPlace},
    impls::SqlparserDialect,
    utils::walk::walk,
};

/// Finds the first query of a node.
struct Subquery;

impl Visitor for Subquery {
    type Break = ();

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        ControlFlow::Break(())
    }
}

/// Whether `builder` models PostgreSQL, the engine whose refusals these are.
fn postgres(builder: &ParserDBBuilder) -> bool {
    matches!(builder.dialect(), SqlparserDialect::PostgreSql)
}

/// Refuses `node`, an expression stored at `place`, when it holds a subquery.
fn refuse<N: Visit + ?Sized>(node: &N, place: ExpressionPlace) -> Result<(), Error> {
    match walk(node, &mut Subquery) {
        ControlFlow::Break(()) => Err(Error::SubqueryInExpression { place }),
        ControlFlow::Continue(()) => Ok(()),
    }
}

/// Refuses a subquery in the default, generation expression or checks of
/// `column`.
fn column_options(column: &ColumnDef) -> Result<(), Error> {
    for option in &column.options {
        match &option.option {
            ColumnOption::Default(default) => refuse(default, ExpressionPlace::Default)?,
            ColumnOption::Generated { generation_expr: Some(generation), .. } => {
                refuse(generation, ExpressionPlace::Generation)?;
            }
            ColumnOption::Check(check) => refuse(&check.expr, ExpressionPlace::CheckConstraint)?,
            _ => {}
        }
    }
    Ok(())
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

/// Refuses a subquery in an expression `create_table` stores: a column's
/// default, generation expression or check, a table constraint, the
/// partition key or the partition bound.
pub(super) fn refuse_in_table(
    builder: &ParserDBBuilder,
    create_table: &CreateTable,
) -> Result<(), Error> {
    if !postgres(builder) {
        return Ok(());
    }
    create_table.columns.iter().try_for_each(column_options)?;
    create_table.constraints.iter().try_for_each(constraint)?;
    refuse(&create_table.partition_by, ExpressionPlace::PartitionKey)?;
    create_table.for_values.as_ref().map_or(Ok(()), bound)
}

/// Refuses a subquery in the default, generation expression or checks of
/// `column`, a column `ALTER TABLE` adds.
pub(super) fn refuse_in_column(builder: &ParserDBBuilder, column: &ColumnDef) -> Result<(), Error> {
    if postgres(builder) { column_options(column) } else { Ok(()) }
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
    for column in &create_index.columns {
        refuse(&column.column.expr, ExpressionPlace::IndexExpression)?;
    }
    refuse(&create_index.predicate, ExpressionPlace::IndexPredicate)
}

/// Refuses a subquery in the default or the checks of `create_domain`.
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
/// sets on a column.
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
