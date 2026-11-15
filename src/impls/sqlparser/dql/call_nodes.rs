//! Every node of an expression or a statement written as a function call,
//! borrowed for as long as the node it was found in.

use alloc::vec::Vec;

use sqlparser::ast::{
    Delete, DoUpdate, Expr, FromTable, Insert, Merge, MergeAction, MergeClause, MergeInsertExpr,
    MergeInsertKind, MergeUpdateExpr, MergeUpdateKind, OnConflict, OnConflictAction, OnInsert,
    OutputClause, Query, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins, Update,
    UpdateTableFromKind,
};

use super::{NestedNodes, NestedQueryWalker, scoped_roots::ScopedRoot};
use crate::errors::LookupError;

/// A node written as a function call.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CallNode<'node> {
    /// A function expression, `Expr::Function`.
    Expression(&'node Expr),
    /// A `FROM` item carrying an argument list.
    Relation(&'node TableFactor),
}

/// Every node `root` writes as a function call, nested queries and the
/// clauses of a data-modifying statement included, in the order a walk
/// meets them. A statement that is no query and modifies no data holds
/// none, except the arguments of a `CALL`.
pub(crate) fn call_nodes(root: ScopedRoot<'_>) -> Vec<CallNode<'_>> {
    let mut finder = CallFinder { calls: Vec::new() };
    // The walker fails only when a hook does, and these never do.
    let _ = match root {
        ScopedRoot::Expression(expression) => finder.walker().expression(expression),
        ScopedRoot::Query(query) => finder.query(query),
        ScopedRoot::Statement(statement) => finder.statement(statement),
    };
    finder.calls
}

/// Gathers the call nodes of the SQL it walks.
struct CallFinder<'node> {
    calls: Vec<CallNode<'node>>,
}

impl<'node> NestedNodes<'node> for &mut CallFinder<'node> {
    fn query(&mut self, query: &'node Query) -> Result<(), LookupError> {
        CallFinder::query(self, query)
    }

    fn expression(&mut self, expression: &'node Expr) -> Result<(), LookupError> {
        if let Expr::Function(_) = expression {
            self.calls.push(CallNode::Expression(expression));
        }
        Ok(())
    }
}

impl<'node> CallFinder<'node> {
    fn walker(&mut self) -> NestedQueryWalker<&mut Self> {
        NestedQueryWalker { query: self }
    }

    fn statement(&mut self, statement: &'node Statement) -> Result<(), LookupError> {
        match statement {
            Statement::Query(query) => self.query(query),
            Statement::Insert(insert) => self.insert(insert),
            Statement::Update(update) => self.update(update),
            Statement::Delete(delete) => self.delete(delete),
            Statement::Merge(merge) => self.merge(merge),
            Statement::Call(function) => self.walker().function(function),
            _ => Ok(()),
        }
    }

    fn query(&mut self, query: &'node Query) -> Result<(), LookupError> {
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                self.query(&cte.query)?;
            }
        }
        self.set_expression(&query.body)?;
        self.walker().query_expressions(query)
    }

    fn set_expression(&mut self, body: &'node SetExpr) -> Result<(), LookupError> {
        match body {
            SetExpr::Select(select) => {
                for table in &select.from {
                    self.table_with_joins(table)?;
                }
                self.walker().select(select)
            }
            SetExpr::Query(query) => self.query(query),
            SetExpr::SetOperation { left, right, .. } => {
                self.set_expression(left)?;
                self.set_expression(right)
            }
            SetExpr::Values(values) => self.walker().values(values),
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => self.statement(statement),
            SetExpr::Table(_) => Ok(()),
        }
    }

    fn tables(&mut self, tables: &'node [TableWithJoins]) -> Result<(), LookupError> {
        tables.iter().try_for_each(|table| self.table_with_joins(table))
    }

    fn table_with_joins(&mut self, table: &'node TableWithJoins) -> Result<(), LookupError> {
        self.factor(&table.relation)?;
        for join in &table.joins {
            self.factor(&join.relation)?;
            self.walker().join_operator(&join.join_operator)?;
        }
        Ok(())
    }

    /// The calls of `factor` and of the factors it nests, with the
    /// expressions they write.
    fn factor(&mut self, factor: &'node TableFactor) -> Result<(), LookupError> {
        let mut walker = self.walker();
        walker.factor_relation_inputs(factor)?;
        walker.factor_visible_expressions(factor)?;
        self.nested_factors(factor)
    }

    /// The calls `factor` and the factors it nests are written as, and the
    /// calls of the subqueries they derive from, whose expressions
    /// [`Self::factor`] leaves to this walk.
    fn nested_factors(&mut self, factor: &'node TableFactor) -> Result<(), LookupError> {
        match factor {
            TableFactor::Function { .. } | TableFactor::Table { args: Some(_), .. } => {
                self.calls.push(CallNode::Relation(factor));
                Ok(())
            }
            TableFactor::Derived { subquery, .. } => self.query(subquery),
            TableFactor::NestedJoin { table_with_joins, .. } => {
                self.nested_factors(&table_with_joins.relation)?;
                table_with_joins
                    .joins
                    .iter()
                    .try_for_each(|join| self.nested_factors(&join.relation))
            }
            TableFactor::Pivot { table, .. }
            | TableFactor::Unpivot { table, .. }
            | TableFactor::MatchRecognize { table, .. } => self.nested_factors(table),
            _ => Ok(()),
        }
    }

    fn returning(&mut self, items: Option<&'node [SelectItem]>) -> Result<(), LookupError> {
        let mut walker = self.walker();
        items.unwrap_or_default().iter().try_for_each(|item| walker.select_item(item))
    }

    fn insert(&mut self, insert: &'node Insert) -> Result<(), LookupError> {
        let Insert { source, on, returning, .. } = insert;
        if let Some(source) = source {
            self.query(source)?;
        }
        if let Some(OnInsert::OnConflict(OnConflict {
            action: OnConflictAction::DoUpdate(DoUpdate { assignments, selection }),
            ..
        })) = on
        {
            let mut walker = self.walker();
            for assignment in assignments {
                walker.expression(&assignment.value)?;
            }
            walker.optional_expression(selection.as_ref())?;
        }
        self.returning(returning.as_deref())
    }

    fn update(&mut self, update: &'node Update) -> Result<(), LookupError> {
        let Update { table, from, assignments, selection, returning, .. } = update;
        self.table_with_joins(table)?;
        if let Some(UpdateTableFromKind::BeforeSet(from) | UpdateTableFromKind::AfterSet(from)) =
            from
        {
            self.tables(from)?;
        }
        let mut walker = self.walker();
        for assignment in assignments {
            walker.expression(&assignment.value)?;
        }
        walker.optional_expression(selection.as_ref())?;
        self.returning(returning.as_deref())
    }

    fn delete(&mut self, delete: &'node Delete) -> Result<(), LookupError> {
        let Delete { from, using, selection, returning, .. } = delete;
        let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = from;
        self.tables(from)?;
        self.tables(using.as_deref().unwrap_or_default())?;
        self.walker().optional_expression(selection.as_ref())?;
        self.returning(returning.as_deref())
    }

    fn merge(&mut self, merge: &'node Merge) -> Result<(), LookupError> {
        let Merge { table, source, on, clauses, output, .. } = merge;
        self.factor(table)?;
        self.factor(source)?;
        self.walker().expression(on)?;
        for MergeClause { predicate, action, .. } in clauses {
            let mut walker = self.walker();
            walker.optional_expression(predicate.as_ref())?;
            match action {
                MergeAction::Insert(MergeInsertExpr {
                    kind: MergeInsertKind::Values(values),
                    ..
                }) => walker.values(values)?,
                MergeAction::Update(MergeUpdateExpr {
                    kind,
                    update_predicate,
                    delete_predicate,
                    ..
                }) => {
                    if let MergeUpdateKind::Set(assignments) = kind {
                        for assignment in assignments {
                            walker.expression(&assignment.value)?;
                        }
                    }
                    walker.optional_expression(update_predicate.as_ref())?;
                    walker.optional_expression(delete_predicate.as_ref())?;
                }
                _ => {}
            }
        }
        let returning = match output {
            Some(OutputClause::Returning { select_items, .. }) => Some(select_items.as_slice()),
            _ => None,
        };
        self.returning(returning)
    }
}
