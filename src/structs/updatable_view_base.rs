//! The relation a write through an automatically updatable view lands on.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::{
    ast::{
        Expr, Function, GroupByExpr, OrderByKind, Query, Select, SelectItem,
        SelectItemQualifiedWildcardKind, SetExpr, TableFactor, Visitor, WildcardAdditionalOptions,
    },
    dialect::SQLiteDialect,
};

use crate::{
    errors::LookupError,
    structs::{CallResolution, ColumnScope, ExpressionContext, FunctionKind, QueryOutput},
    traits::{DatabaseLike, DialectLike, FunctionLike, Relation, TableLike, ViewLike},
    utils::{object_name::resolve_bound_relation_object_name, walk::walk},
};

/// The relation PostgreSQL rewrites an `INSERT`, `UPDATE`, `DELETE` or
/// `MERGE` targeting an automatically updatable view onto, with what each
/// output column of the view writes there.
///
/// Each column list holds one entry per output column of the view, in output
/// order, and `None` for a column PostgreSQL refuses to write because it is
/// no simple reference to a column of the relation.
#[derive(Debug)]
pub enum UpdatableViewBase<'db, DB: DatabaseLike> {
    /// A table, whose own triggers, rules and row-level security policies
    /// then apply to the write.
    Table {
        /// The table written.
        table: &'db DB::Table,
        /// The column of the table each output column writes.
        columns: Vec<Option<&'db DB::Column>>,
    },
    /// A foreign table, whose foreign data wrapper takes the write.
    ForeignTable {
        /// The foreign table written.
        table: &'db DB::Table,
        /// The column of the table each output column writes.
        columns: Vec<Option<&'db DB::Column>>,
    },
    /// Another view, which takes the write as it takes one written to it
    /// directly, through an `INSTEAD OF` trigger or an `INSTEAD` rule on it,
    /// or else as its own [`ViewLike::updatable_base`] answers.
    View {
        /// The view written.
        view: &'db DB::View,
        /// The position among that view's output columns, from zero, each
        /// output column writes.
        columns: Vec<Option<usize>>,
    },
}

/// The one relation a view definition reads, as its `FROM` list names it.
struct Source<'query, 'db, DB: DatabaseLike> {
    factor: &'query TableFactor,
    relation: Relation<'db, DB>,
}

impl<'db, DB: DatabaseLike> UpdatableViewBase<'db, DB> {
    /// The relation PostgreSQL writes through a plain view defined by
    /// `definition`, or `None` when it makes the view not automatically
    /// updatable or `database` models SQLite.
    ///
    /// # Errors
    ///
    /// Returns the lookup errors resolving the definition's relation and
    /// column names runs into.
    pub(crate) fn of_definition(
        definition: &Query,
        database: &'db DB,
    ) -> Result<Option<Self>, LookupError> {
        if database.dialect().sqlparser_dialect().is::<SQLiteDialect>() {
            return Ok(None);
        }
        let mut ordering = Vec::new();
        let Some(select) = updatable_select(definition, &mut ordering) else { return Ok(None) };
        let Some(source) = single_source(select, database)? else { return Ok(None) };
        let context = ExpressionContext::for_stored_definition(definition, database)?;
        let top_level = select
            .projection
            .iter()
            .filter_map(|item| {
                match item {
                    SelectItem::UnnamedExpr(expression)
                    | SelectItem::ExprWithAlias { expr: expression, .. } => Some(expression),
                    _ => None,
                }
            })
            .chain(ordering);
        for expression in top_level {
            if computes_per_query(expression, &context, source.factor, database)? {
                return Ok(None);
            }
        }
        let Some(positions) = output_positions(select, &source, &context, database)? else {
            return Ok(None);
        };
        Ok(Some(match source.relation {
            Relation::Table(table) => {
                Self::Table { table, columns: table_columns(table, &positions, database)? }
            }
            Relation::ForeignTable(table) => {
                Self::ForeignTable { table, columns: table_columns(table, &positions, database)? }
            }
            Relation::View(view) => Self::View { view, columns: positions },
            Relation::MaterializedView(_) | Relation::Sequence(_) => return Ok(None),
        }))
    }
}

/// The select of `query` when no clause around it makes a view over it not
/// automatically updatable, collecting the `ORDER BY` keys of every level it
/// passes into `ordering`.
fn updatable_select<'query>(
    query: &'query Query,
    ordering: &mut Vec<&'query Expr>,
) -> Option<&'query Select> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.pipe_operators.is_empty()
    {
        return None;
    }
    if let Some(OrderByKind::Expressions(keys)) = query.order_by.as_ref().map(|order| &order.kind) {
        ordering.extend(keys.iter().map(|key| &key.expr));
    }
    let select = match query.body.as_ref() {
        SetExpr::Select(select) => select,
        SetExpr::Query(inner) => return updatable_select(inner, ordering),
        _ => return None,
    };
    let grouped = match &select.group_by {
        GroupByExpr::All(_) => true,
        GroupByExpr::Expressions(keys, modifiers) => !keys.is_empty() || !modifiers.is_empty(),
    };
    let reshaped = select.distinct.is_some()
        || grouped
        || select.having.is_some()
        || select.qualify.is_some()
        || !select.connect_by.is_empty()
        || !select.lateral_views.is_empty();
    (!reshaped).then_some(select.as_ref())
}

/// The relation `select` reads when its `FROM` list holds exactly one table,
/// foreign table or plain view, read without `TABLESAMPLE`, as `database`
/// bound its name when the view was created.
fn single_source<'query, 'db, DB: DatabaseLike>(
    select: &'query Select,
    database: &'db DB,
) -> Result<Option<Source<'query, 'db, DB>>, LookupError> {
    let [item] = select.from.as_slice() else { return Ok(None) };
    if !item.joins.is_empty() {
        return Ok(None);
    }
    let factor = &item.relation;
    let TableFactor::Table {
        name, args: None, sample: None, version: None, json_path: None, ..
    } = factor
    else {
        return Ok(None);
    };
    Ok(resolve_bound_relation_object_name(name, database)?.and_then(|relation| {
        matches!(relation, Relation::Table(_) | Relation::ForeignTable(_) | Relation::View(_))
            .then_some(Source { factor, relation })
    }))
}

/// Whether `expression`, a select-list item or `ORDER BY` key of the view's
/// query, holds an aggregate, a window function or a set-returning function
/// of that query, which PostgreSQL computes over its rows rather than per row
/// of the relation `factor` reads.
///
/// An aggregate inside a subquery belongs to the view's query when every
/// column its arguments reference outside the subqueries they hold is one of
/// `factor`'s, as PostgreSQL's `agglevelsup` places it.
fn computes_per_query<DB: DatabaseLike>(
    expression: &Expr,
    context: &ExpressionContext<'_, '_, DB>,
    factor: &TableFactor,
    database: &DB,
) -> Result<bool, LookupError> {
    let mut calls = LevelCalls { context, factor, database, depth: 0 };
    match walk(expression, &mut calls) {
        ControlFlow::Continue(()) => Ok(false),
        ControlFlow::Break(found) => found,
    }
}

/// Walks one expression of the view's query for the calls that make it not
/// automatically updatable.
struct LevelCalls<'context, 'query, 'db, DB: DatabaseLike> {
    context: &'context ExpressionContext<'query, 'db, DB>,
    factor: &'context TableFactor,
    database: &'db DB,
    /// How many subqueries of the view's query the walk is inside.
    depth: usize,
}

impl<DB: DatabaseLike> Visitor for LevelCalls<'_, '_, '_, DB> {
    type Break = Result<bool, LookupError>;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        let Expr::Function(function) = expression else { return ControlFlow::Continue(()) };
        let found = if self.depth == 0 {
            if function.over.is_some() {
                return ControlFlow::Break(Ok(true));
            }
            self.selected(expression).map(|selected| {
                selected.is_some_and(|(kind, returns_set)| {
                    kind == FunctionKind::Aggregate || returns_set
                })
            })
        } else {
            self.selected(expression).and_then(|selected| {
                if selected.is_some_and(|(kind, _)| kind == FunctionKind::Aggregate) {
                    self.aggregates_outer_rows(function)
                } else {
                    Ok(false)
                }
            })
        };
        match found {
            Ok(false) => ControlFlow::Continue(()),
            found => ControlFlow::Break(found),
        }
    }
}

impl<DB: DatabaseLike> LevelCalls<'_, '_, '_, DB> {
    /// The kind of the function the call `expression` runs and whether it
    /// returns a set, or `None` for a call selecting no single function.
    fn selected(&self, expression: &Expr) -> Result<Option<(FunctionKind, bool)>, LookupError> {
        let Some(call) = self.context.call(expression)? else { return Ok(None) };
        let CallResolution::Selected(selected) = call.resolution() else { return Ok(None) };
        let function = selected.function();
        Ok(Some((function.kind(self.database)?, function.returns_set())))
    }

    /// Whether the aggregate `function`, written in a subquery, aggregates
    /// rows of the view's query: some column its arguments reference is one
    /// of the view's relation and none belongs to a query between.
    fn aggregates_outer_rows(&self, function: &Function) -> Result<bool, LookupError> {
        let mut held = HeldFactors(Vec::new());
        let _ = walk(function, &mut held);
        let mut levels = ArgumentLevels {
            context: self.context,
            factor: self.factor,
            held: held.0,
            outer: false,
        };
        match walk(function, &mut levels) {
            ControlFlow::Continue(()) => Ok(levels.outer),
            ControlFlow::Break(between) => between.map(|()| false),
        }
    }
}

/// Collects the `FROM` items a node holds.
struct HeldFactors(Vec<*const TableFactor>);

impl Visitor for HeldFactors {
    type Break = ();

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        self.0.push(factor);
        ControlFlow::Continue(())
    }
}

/// Walks the arguments of an aggregate written in a subquery for the query
/// levels their column references reach, breaking at one of a query between
/// the view's and the aggregate's own.
struct ArgumentLevels<'walk, 'query, 'db, DB: DatabaseLike> {
    context: &'walk ExpressionContext<'query, 'db, DB>,
    /// The `FROM` item of the view's query.
    factor: &'walk TableFactor,
    /// The `FROM` items of the subqueries the arguments hold, whose columns
    /// leave the aggregate's level alone.
    held: Vec<*const TableFactor>,
    /// Whether some reference reaches the view's `FROM` item.
    outer: bool,
}

impl<DB: DatabaseLike> Visitor for ArgumentLevels<'_, '_, '_, DB> {
    type Break = Result<(), LookupError>;

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if !matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            return ControlFlow::Continue(());
        }
        let entry = match self.context.reference(expression) {
            Ok(bound) => bound.and_then(|(_, binding)| binding.entry()),
            Err(error) => return ControlFlow::Break(Err(error)),
        };
        let Some(entry) = entry else { return ControlFlow::Continue(()) };
        let factor = entry.factor();
        if core::ptr::eq(factor, self.factor) {
            self.outer = true;
        } else if !self.held.iter().any(|held| core::ptr::eq(*held, factor)) {
            return ControlFlow::Break(Ok(()));
        }
        ControlFlow::Continue(())
    }
}

/// The position among the columns of `source` each output column of `select`
/// simply references, or `None` when a wildcard reaches columns that cannot
/// be enumerated.
fn output_positions<DB: DatabaseLike>(
    select: &Select,
    source: &Source<'_, '_, DB>,
    context: &ExpressionContext<'_, '_, DB>,
    database: &DB,
) -> Result<Option<Vec<Option<usize>>>, LookupError> {
    let mut positions = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(expression)
            | SelectItem::ExprWithAlias { expr: expression, .. } => {
                positions.push(referenced_position(expression, source.factor, context)?);
            }
            SelectItem::Wildcard(options)
            | SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(_),
                options,
            ) if *options == WildcardAdditionalOptions::default() => {
                let Some(count) = source_width(source.relation, database)? else {
                    return Ok(None);
                };
                positions.extend((0..count).map(Some));
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(positions))
}

/// The position among `factor`'s columns of the column `expression` is a
/// simple reference to, or `None` for any other expression.
fn referenced_position<DB: DatabaseLike>(
    expression: &Expr,
    factor: &TableFactor,
    context: &ExpressionContext<'_, '_, DB>,
) -> Result<Option<usize>, LookupError> {
    if let Expr::Nested(inner) = expression {
        return referenced_position(inner, factor, context);
    }
    Ok(context
        .reference(expression)?
        .and_then(|(_, binding)| binding.entry())
        .filter(|entry| core::ptr::eq(entry.factor(), factor))
        .map(|entry| entry.ordinal()))
}

/// How many columns a wildcard over `relation` reaches, or `None` when the
/// output of a view cannot be enumerated.
fn source_width<DB: DatabaseLike>(
    relation: Relation<'_, DB>,
    database: &DB,
) -> Result<Option<usize>, LookupError> {
    match relation {
        Relation::Table(table) | Relation::ForeignTable(table) => {
            Ok(Some(table.columns(database)?.count()))
        }
        Relation::View(view) => {
            let definition = view.definition();
            let scope = ColumnScope::for_stored_definition(definition, database)?;
            let Some(outputs) = scope.scope_for_query(definition).map(|query| query.outputs())
            else {
                return Ok(None);
            };
            let mut count = 0;
            for output in outputs {
                if matches!(output, QueryOutput::Unexpanded) {
                    return Ok(None);
                }
                count += 1;
            }
            Ok(Some(count))
        }
        Relation::MaterializedView(_) | Relation::Sequence(_) => Ok(None),
    }
}

/// The column of `table` at each of `positions`.
fn table_columns<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    positions: &[Option<usize>],
    database: &'db DB,
) -> Result<Vec<Option<&'db DB::Column>>, LookupError> {
    let columns: Vec<&'db DB::Column> = table.columns(database)?.collect();
    Ok(positions
        .iter()
        .map(|position| position.and_then(|position| columns.get(position).copied()))
        .collect())
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{Query, Statement},
        dialect::PostgreSqlDialect,
        parser::Parser,
    };

    use super::UpdatableViewBase;
    use crate::{structs::ParserDB, traits::ColumnLike};

    fn definition(sql: &str) -> Query {
        match Parser::parse_sql(&PostgreSqlDialect {}, sql).expect("query parses").remove(0) {
            Statement::Query(query) => *query,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    /// A definition that still holds its wildcards, as a catalog other than
    /// [`ParserDB`] may store it, writes every column the wildcard reaches.
    #[test]
    fn a_wildcard_writes_every_column_it_reaches() {
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE t (id INT, x INT);
             CREATE VIEW inner_view AS SELECT x, id, id + 1 AS next FROM t;",
        )
        .expect("schema parses");
        let starred = definition("SELECT *, x AS again FROM t");
        let Some(UpdatableViewBase::Table { columns, .. }) =
            UpdatableViewBase::of_definition(&starred, &db).expect("resolves")
        else {
            panic!("the definition writes a table");
        };
        let names: Vec<_> =
            columns.iter().map(|column| column.map(ColumnLike::column_name)).collect();
        assert_eq!(names, [Some("id"), Some("x"), Some("x")]);
        let qualified = definition("SELECT v.* FROM inner_view AS v");
        let Some(UpdatableViewBase::View { columns, .. }) =
            UpdatableViewBase::of_definition(&qualified, &db).expect("resolves")
        else {
            panic!("the definition writes a view");
        };
        assert_eq!(columns, [Some(0), Some(1), Some(2)]);
    }
}
