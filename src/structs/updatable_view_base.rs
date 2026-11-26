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
    utils::{
        object_name::resolve_bound_relation_object_name,
        walk::{walk, walk_expressions},
    },
};

/// The relation PostgreSQL rewrites an `INSERT`, `UPDATE`, `DELETE` or
/// `MERGE` targeting an automatically updatable view onto, with what each
/// output column of the view writes there and the condition the view puts on
/// the write.
#[derive(Debug)]
pub struct UpdatableViewBase<'db, DB: DatabaseLike> {
    /// The relation written, with what the view's columns and condition
    /// reach in it.
    pub relation: WrittenRelation<'db, DB>,
    /// The `WHERE` of the definition's one `SELECT`, as written, or `None`
    /// when it has none.
    ///
    /// PostgreSQL adds it to an `UPDATE`, `DELETE` or `MERGE` through the
    /// view, so the write reaches only the rows of the relation it admits.
    /// A view with a check option also tests each row a write through it
    /// inserts or updates against it.
    pub condition: Option<&'db Expr>,
}

/// The relation a write through an automatically updatable view lands on.
///
/// Each `columns` list holds one entry per output column of the view, in
/// output order, and `None` for a column PostgreSQL refuses to write because
/// it is no simple reference to a column of the relation.
///
/// Each `condition_columns` list holds one entry per column reference inside
/// [`UpdatableViewBase::condition`] that binds to a column of the relation,
/// in walk order. An entry pairs the reference's index in the pre-order walk
/// sqlparser's `visit_expressions` makes over the condition, counting every
/// expression it meets from zero, with the column the reference binds to.
/// The reference is an [`Expr::Identifier`] or an
/// [`Expr::CompoundIdentifier`], bound by the names the definition's `FROM`
/// item gives the relation and its columns, alias and column aliases
/// included. An identifier of either kind left out binds to no column of the
/// relation. It reads a relation a subquery in the condition reads, names the
/// whole row of the relation, such as `r` in `row_to_json(r)`, or names no
/// column at all, such as the field `amount` in `(r.payload).amount`.
#[derive(Debug)]
pub enum WrittenRelation<'db, DB: DatabaseLike> {
    /// A table, whose own triggers, rules and row-level security policies
    /// then apply to the write.
    Table {
        /// The table written.
        table: &'db DB::Table,
        /// The column of the table each output column writes.
        columns: Vec<Option<&'db DB::Column>>,
        /// The column of the table each reference in the condition reads.
        condition_columns: Vec<(usize, &'db DB::Column)>,
    },
    /// A foreign table, whose foreign data wrapper takes the write.
    ForeignTable {
        /// The foreign table written.
        table: &'db DB::Table,
        /// The column of the table each output column writes.
        columns: Vec<Option<&'db DB::Column>>,
        /// The column of the table each reference in the condition reads.
        condition_columns: Vec<(usize, &'db DB::Column)>,
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
        /// The position among that view's output columns, from zero, each
        /// reference in the condition reads.
        condition_columns: Vec<(usize, usize)>,
    },
}

/// The one relation a view definition reads, as its `FROM` list names it.
struct Source<'query, 'db, DB: DatabaseLike> {
    factor: &'query TableFactor,
    relation: SourceRelation<'db, DB>,
}

/// A relation of a kind a write through a view lands on.
enum SourceRelation<'db, DB: DatabaseLike> {
    Table(&'db DB::Table),
    ForeignTable(&'db DB::Table),
    View(&'db DB::View),
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
        definition: &'db Query,
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
        let condition = select.selection.as_ref();
        let read = match condition {
            Some(condition) => condition_positions(condition, source.factor, &context)?,
            None => Vec::new(),
        };
        let relation = match source.relation {
            SourceRelation::Table(table) => {
                let (columns, condition_columns) =
                    table_columns(table, &positions, &read, database)?;
                WrittenRelation::Table { table, columns, condition_columns }
            }
            SourceRelation::ForeignTable(table) => {
                let (columns, condition_columns) =
                    table_columns(table, &positions, &read, database)?;
                WrittenRelation::ForeignTable { table, columns, condition_columns }
            }
            SourceRelation::View(view) => {
                WrittenRelation::View { view, columns: positions, condition_columns: read }
            }
        };
        Ok(Some(Self { relation, condition }))
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
    let relation = match resolve_bound_relation_object_name(name, database)? {
        Some(Relation::Table(table)) => SourceRelation::Table(table),
        Some(Relation::ForeignTable(table)) => SourceRelation::ForeignTable(table),
        Some(Relation::View(view)) => SourceRelation::View(view),
        Some(Relation::MaterializedView(_) | Relation::Sequence(_)) | None => return Ok(None),
    };
    Ok(Some(Source { factor, relation }))
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
                let Some(count) = source_width(&source.relation, database)? else {
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

/// Each column reference inside `condition` that binds to a column of
/// `factor`, as its index in the pre-order walk over `condition` paired with
/// the position of that column among `factor`'s.
fn condition_positions<DB: DatabaseLike>(
    condition: &Expr,
    factor: &TableFactor,
    context: &ExpressionContext<'_, '_, DB>,
) -> Result<Vec<(usize, usize)>, LookupError> {
    let mut read = Vec::new();
    let mut index = 0;
    let walked = walk_expressions(condition, |expression| {
        let at = index;
        index += 1;
        if !matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            return ControlFlow::Continue(());
        }
        match referenced_position(expression, factor, context) {
            Ok(position) => {
                read.extend(position.map(|position| (at, position)));
                ControlFlow::Continue(())
            }
            Err(error) => ControlFlow::Break(error),
        }
    });
    match walked {
        ControlFlow::Continue(()) => Ok(read),
        ControlFlow::Break(error) => Err(error),
    }
}

/// How many columns a wildcard over `relation` reaches, or `None` when the
/// output of a view cannot be enumerated.
fn source_width<DB: DatabaseLike>(
    relation: &SourceRelation<'_, DB>,
    database: &DB,
) -> Result<Option<usize>, LookupError> {
    match relation {
        SourceRelation::Table(table) | SourceRelation::ForeignTable(table) => {
            Ok(Some(table.columns(database)?.count()))
        }
        SourceRelation::View(view) => {
            let definition = view.definition();
            let scope = ColumnScope::for_stored_definition(definition, database)?;
            Ok(scope.scope_for_query(definition).and_then(|query| {
                query.outputs().try_fold(0, |count, output| {
                    (!matches!(output, QueryOutput::Unexpanded { .. })).then_some(count + 1)
                })
            }))
        }
    }
}

/// The column of `table` at each of `positions`, and at the position each
/// reference of `read` reads, paired with that reference's index.
fn table_columns<'db, DB: DatabaseLike>(
    table: &'db DB::Table,
    positions: &[Option<usize>],
    read: &[(usize, usize)],
    database: &'db DB,
) -> Result<TableColumns<'db, DB>, LookupError> {
    let columns: Vec<&'db DB::Column> = table.columns(database)?.collect();
    let written = positions
        .iter()
        .map(|position| position.and_then(|position| columns.get(position).copied()))
        .collect();
    let condition_columns = read
        .iter()
        .filter_map(|(index, position)| columns.get(*position).map(|column| (*index, *column)))
        .collect();
    Ok((written, condition_columns))
}

/// The columns of a table each output column writes, and those the
/// references of the condition read.
type TableColumns<'db, DB> = (
    Vec<Option<&'db <DB as DatabaseLike>::Column>>,
    Vec<(usize, &'db <DB as DatabaseLike>::Column)>,
);

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, vec::Vec};

    use sqlparser::{
        ast::Query,
        dialect::{GenericDialect, PostgreSqlDialect},
        parser::Parser,
    };

    use super::{UpdatableViewBase, WrittenRelation};
    use crate::{
        structs::ParserDB,
        traits::{ColumnLike, TableLike},
    };

    const SCHEMA: &str = "
        CREATE TABLE t (id INT, x INT);
        CREATE TABLE u (id INT);
        CREATE VIEW inner_view AS SELECT x, id, id + 1 AS next FROM t;
        CREATE VIEW whole_table AS TABLE t;
        CREATE VIEW opaque AS SELECT * FROM t, generate_series(1, 2) AS g;
    ";

    fn definition(sql: &str) -> Box<Query> {
        Parser::new(&GenericDialect {})
            .try_with_sql(sql)
            .and_then(|mut parser| parser.parse_query())
            .expect("query parses")
    }

    fn relation<'db>(
        definition: &'db Query,
        db: &'db ParserDB,
    ) -> Option<WrittenRelation<'db, ParserDB>> {
        UpdatableViewBase::of_definition(definition, db)
            .expect("resolves")
            .map(|base| base.relation)
    }

    fn names<'db, C: ColumnLike + 'db>(columns: &[Option<&'db C>]) -> Vec<Option<&'db str>> {
        columns.iter().map(|column| column.map(ColumnLike::column_name)).collect()
    }

    /// A definition that still holds its wildcards, as a catalog other than
    /// [`ParserDB`] may store it, writes every column the wildcard reaches.
    #[test]
    fn a_wildcard_writes_every_column_it_reaches() {
        let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses");
        let starred = definition("SELECT *, x AS again FROM t");
        let starred = relation(&starred, &db);
        assert!(
            matches!(
                &starred,
                Some(WrittenRelation::Table { table, columns, .. })
                    if table.table_name() == "t"
                        && names(columns) == [Some("id"), Some("x"), Some("x")]
            ),
            "{starred:?}"
        );
        let qualified = definition("SELECT v.* FROM inner_view AS v");
        let qualified = relation(&qualified, &db);
        assert!(
            matches!(
                &qualified,
                Some(WrittenRelation::View { columns, .. })
                    if columns == &[Some(0), Some(1), Some(2)]
            ),
            "{qualified:?}"
        );
        let whole = definition("SELECT * FROM whole_table");
        let whole = relation(&whole, &db);
        assert!(
            matches!(
                &whole,
                Some(WrittenRelation::View { columns, .. }) if columns == &[Some(0), Some(1)]
            ),
            "{whole:?}"
        );
    }

    /// A wildcard over a view whose output width is unknown leaves the
    /// columns after it without a position, so no column list answers.
    #[test]
    fn a_wildcard_over_columns_of_unknown_width_answers_nothing() {
        let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses");
        assert!(relation(&definition("SELECT * FROM opaque"), &db).is_none());
    }

    /// A wildcard modifier another dialect accepts, such as `EXCLUDE`, drops
    /// or reorders the columns the wildcard reaches, so no column list
    /// answers.
    #[test]
    fn a_modified_wildcard_answers_nothing() {
        let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses");
        assert!(relation(&definition("SELECT * EXCLUDE (x) FROM t"), &db).is_none());
    }

    /// `GROUP BY ALL`, which PostgreSQL does not accept and other dialects
    /// read as grouping by every output without an aggregate, collapses rows
    /// like any grouping.
    #[test]
    fn grouping_by_all_outputs_makes_a_view_read_only() {
        let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses");
        assert!(relation(&definition("SELECT id FROM t GROUP BY ALL"), &db).is_none());
    }

    /// A reference in an aggregate's arguments that the definition cannot
    /// resolve is a lookup error, never a guess at the aggregate's level,
    /// even where only the walk over those arguments meets it.
    #[test]
    fn an_unresolvable_aggregate_argument_is_an_error() {
        let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses");
        let ambiguous = definition(
            "SELECT (SELECT max((SELECT 1 FROM u, u AS u2 WHERE id = 1)) FROM u) AS m FROM t",
        );
        let resolved = UpdatableViewBase::of_definition(&ambiguous, &db);
        assert!(resolved.is_err(), "{resolved:?}");
    }

    /// A reference in the condition that the definition cannot resolve is a
    /// lookup error, never a reference left out of the condition's columns.
    #[test]
    fn an_unresolvable_condition_reference_is_an_error() {
        let db = ParserDB::parse::<PostgreSqlDialect>(SCHEMA).expect("schema parses");
        let ambiguous =
            definition("SELECT x FROM t WHERE EXISTS (SELECT 1 FROM u, u AS u2 WHERE id = 1)");
        let resolved = UpdatableViewBase::of_definition(&ambiguous, &db);
        assert!(resolved.is_err(), "{resolved:?}");
    }
}
