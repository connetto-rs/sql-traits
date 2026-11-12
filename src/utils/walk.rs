//! Walks the syntax tree through one trait object, so sqlparser's traversal
//! compiles once per break type instead of once per visitor.

use core::ops::ControlFlow;

use sqlparser::ast::{
    Expr, GroupByExpr, Ident, ObjectName, OrderBy, OrderByExpr, Query, Select, Statement,
    TableFactor, ValueWithSpan, Visit, VisitMut, Visitor, VisitorMut,
};

/// Walks `node` with `visitor`.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place the traversal is instantiated, for the trait object"
)]
pub(crate) fn walk<N: Visit + ?Sized, B>(
    node: &N,
    visitor: &mut dyn Visitor<Break = B>,
) -> ControlFlow<B> {
    node.visit(&mut Dynamic(visitor))
}

/// Walks `node` with `visitor`, which may rewrite what it meets.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place the rewriting traversal is instantiated, for the trait object"
)]
pub(crate) fn walk_mut<N: VisitMut + ?Sized, B>(
    node: &mut N,
    visitor: &mut dyn VisitorMut<Break = B>,
) -> ControlFlow<B> {
    node.visit(&mut DynamicMut(visitor))
}

/// Hands every expression in `node` to `each` before its subexpressions,
/// until `each` breaks.
pub(crate) fn walk_expressions<N: Visit + ?Sized, B>(
    node: &N,
    mut each: impl FnMut(&Expr) -> ControlFlow<B>,
) -> ControlFlow<B> {
    walk(node, &mut Expressions(&mut each))
}

/// Hands every expression in `node` to `each` after its subexpressions, so
/// `each` may replace it, until `each` breaks.
pub(crate) fn walk_expressions_mut<N: VisitMut + ?Sized, B>(
    node: &mut N,
    mut each: impl FnMut(&mut Expr) -> ControlFlow<B>,
) -> ControlFlow<B> {
    walk_mut(node, &mut ExpressionsMut(&mut each))
}

/// Hands every relation name in `node` to `each`, until `each` breaks.
pub(crate) fn walk_relations<N: Visit + ?Sized, B>(
    node: &N,
    mut each: impl FnMut(&ObjectName) -> ControlFlow<B>,
) -> ControlFlow<B> {
    walk(node, &mut Relations(&mut each))
}

/// Hands each expression the walk meets to a borrowed closure.
struct Expressions<'f, B>(&'f mut dyn FnMut(&Expr) -> ControlFlow<B>);

impl<B> Visitor for Expressions<'_, B> {
    type Break = B;

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<B> {
        (self.0)(expr)
    }
}

/// Hands each expression the walk leaves to a borrowed closure.
struct ExpressionsMut<'f, B>(&'f mut dyn FnMut(&mut Expr) -> ControlFlow<B>);

impl<B> VisitorMut for ExpressionsMut<'_, B> {
    type Break = B;

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<B> {
        (self.0)(expr)
    }
}

/// Hands each relation name the walk meets to a borrowed closure.
struct Relations<'f, B>(&'f mut dyn FnMut(&ObjectName) -> ControlFlow<B>);

impl<B> Visitor for Relations<'_, B> {
    type Break = B;

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<B> {
        (self.0)(relation)
    }
}

/// Forwards every hook to a borrowed visitor. A hook left to its default here
/// would never reach the inner visitor, so each `Visitor` method needs an
/// override.
struct Dynamic<'v, B>(&'v mut dyn Visitor<Break = B>);

impl<B> Visitor for Dynamic<'_, B> {
    type Break = B;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<B> {
        self.0.pre_visit_query(query)
    }

    fn post_visit_query(&mut self, query: &Query) -> ControlFlow<B> {
        self.0.post_visit_query(query)
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<B> {
        self.0.pre_visit_select(select)
    }

    fn post_visit_select(&mut self, select: &Select) -> ControlFlow<B> {
        self.0.post_visit_select(select)
    }

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<B> {
        self.0.pre_visit_relation(relation)
    }

    fn post_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<B> {
        self.0.post_visit_relation(relation)
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<B> {
        self.0.pre_visit_table_factor(table_factor)
    }

    fn post_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<B> {
        self.0.post_visit_table_factor(table_factor)
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<B> {
        self.0.pre_visit_expr(expr)
    }

    fn post_visit_expr(&mut self, expr: &Expr) -> ControlFlow<B> {
        self.0.post_visit_expr(expr)
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<B> {
        self.0.pre_visit_statement(statement)
    }

    fn post_visit_statement(&mut self, statement: &Statement) -> ControlFlow<B> {
        self.0.post_visit_statement(statement)
    }

    fn pre_visit_value(&mut self, value: &ValueWithSpan) -> ControlFlow<B> {
        self.0.pre_visit_value(value)
    }

    fn post_visit_value(&mut self, value: &ValueWithSpan) -> ControlFlow<B> {
        self.0.post_visit_value(value)
    }

    fn pre_visit_ident(&mut self, ident: &Ident) -> ControlFlow<B> {
        self.0.pre_visit_ident(ident)
    }

    fn post_visit_ident(&mut self, ident: &Ident) -> ControlFlow<B> {
        self.0.post_visit_ident(ident)
    }

    fn pre_visit_order_by(&mut self, order_by: &OrderBy) -> ControlFlow<B> {
        self.0.pre_visit_order_by(order_by)
    }

    fn post_visit_order_by(&mut self, order_by: &OrderBy) -> ControlFlow<B> {
        self.0.post_visit_order_by(order_by)
    }

    fn pre_visit_order_by_expr(&mut self, order_by_expr: &OrderByExpr) -> ControlFlow<B> {
        self.0.pre_visit_order_by_expr(order_by_expr)
    }

    fn post_visit_order_by_expr(&mut self, order_by_expr: &OrderByExpr) -> ControlFlow<B> {
        self.0.post_visit_order_by_expr(order_by_expr)
    }

    fn pre_visit_group_by(&mut self, group_by: &GroupByExpr) -> ControlFlow<B> {
        self.0.pre_visit_group_by(group_by)
    }

    fn post_visit_group_by(&mut self, group_by: &GroupByExpr) -> ControlFlow<B> {
        self.0.post_visit_group_by(group_by)
    }
}

/// Forwards every hook to a borrowed rewriting visitor, overriding each
/// `VisitorMut` method for the reason [`Dynamic`] does.
struct DynamicMut<'v, B>(&'v mut dyn VisitorMut<Break = B>);

impl<B> VisitorMut for DynamicMut<'_, B> {
    type Break = B;

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<B> {
        self.0.pre_visit_query(query)
    }

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<B> {
        self.0.post_visit_query(query)
    }

    fn pre_visit_select(&mut self, select: &mut Select) -> ControlFlow<B> {
        self.0.pre_visit_select(select)
    }

    fn post_visit_select(&mut self, select: &mut Select) -> ControlFlow<B> {
        self.0.post_visit_select(select)
    }

    fn pre_visit_relation(&mut self, relation: &mut ObjectName) -> ControlFlow<B> {
        self.0.pre_visit_relation(relation)
    }

    fn post_visit_relation(&mut self, relation: &mut ObjectName) -> ControlFlow<B> {
        self.0.post_visit_relation(relation)
    }

    fn pre_visit_table_factor(&mut self, table_factor: &mut TableFactor) -> ControlFlow<B> {
        self.0.pre_visit_table_factor(table_factor)
    }

    fn post_visit_table_factor(&mut self, table_factor: &mut TableFactor) -> ControlFlow<B> {
        self.0.post_visit_table_factor(table_factor)
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<B> {
        self.0.pre_visit_expr(expr)
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<B> {
        self.0.post_visit_expr(expr)
    }

    fn pre_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<B> {
        self.0.pre_visit_statement(statement)
    }

    fn post_visit_statement(&mut self, statement: &mut Statement) -> ControlFlow<B> {
        self.0.post_visit_statement(statement)
    }

    fn pre_visit_value(&mut self, value: &mut ValueWithSpan) -> ControlFlow<B> {
        self.0.pre_visit_value(value)
    }

    fn post_visit_value(&mut self, value: &mut ValueWithSpan) -> ControlFlow<B> {
        self.0.post_visit_value(value)
    }

    fn pre_visit_ident(&mut self, ident: &mut Ident) -> ControlFlow<B> {
        self.0.pre_visit_ident(ident)
    }

    fn post_visit_ident(&mut self, ident: &mut Ident) -> ControlFlow<B> {
        self.0.post_visit_ident(ident)
    }

    fn pre_visit_order_by(&mut self, order_by: &mut OrderBy) -> ControlFlow<B> {
        self.0.pre_visit_order_by(order_by)
    }

    fn post_visit_order_by(&mut self, order_by: &mut OrderBy) -> ControlFlow<B> {
        self.0.post_visit_order_by(order_by)
    }

    fn pre_visit_order_by_expr(&mut self, order_by_expr: &mut OrderByExpr) -> ControlFlow<B> {
        self.0.pre_visit_order_by_expr(order_by_expr)
    }

    fn post_visit_order_by_expr(&mut self, order_by_expr: &mut OrderByExpr) -> ControlFlow<B> {
        self.0.post_visit_order_by_expr(order_by_expr)
    }

    fn pre_visit_group_by(&mut self, group_by: &mut GroupByExpr) -> ControlFlow<B> {
        self.0.pre_visit_group_by(group_by)
    }

    fn post_visit_group_by(&mut self, group_by: &mut GroupByExpr) -> ControlFlow<B> {
        self.0.post_visit_group_by(group_by)
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "the direct walks are the oracle the adaptors are checked against"
)]
mod tests {
    //! A hook the adaptors fail to forward is silently skipped, so a walk
    //! through them has to meet the same nodes, in the same order, as a
    //! direct walk does.

    use alloc::{
        format,
        string::{String, ToString},
        vec::Vec,
    };
    use core::ops::ControlFlow;

    use sqlparser::{
        ast::{
            Expr, GroupByExpr, Ident, ObjectName, OrderBy, OrderByExpr, Query, Select, Statement,
            TableFactor, ValueWithSpan, Visit, VisitMut, Visitor, VisitorMut,
        },
        dialect::GenericDialect,
        parser::Parser,
    };

    use super::{walk, walk_mut};

    /// A query reaching every hook, an `ORDER BY` and a `GROUP BY` included.
    const SQL: &str = "SELECT a + 1 FROM t JOIN u ON t.x = u.x WHERE b IN (SELECT c FROM v) \
                       GROUP BY a ORDER BY a DESC";

    /// Records each hook it is handed, with the node it meets.
    #[derive(Default)]
    struct Log(Vec<String>);

    impl Log {
        fn record(&mut self, hook: &str, node: &impl ToString) -> ControlFlow<()> {
            self.0.push(format!("{hook} {}", node.to_string()));
            ControlFlow::Continue(())
        }
    }

    /// Spells out one `Visitor` and one `VisitorMut` override per hook, all
    /// recording into the [`Log`].
    macro_rules! logging_hooks {
        ($($hook:ident: $node:ty),* $(,)?) => {
            impl Visitor for Log {
                type Break = ();
                $(fn $hook(&mut self, node: &$node) -> ControlFlow<()> {
                    self.record(stringify!($hook), node)
                })*
            }

            impl VisitorMut for Log {
                type Break = ();
                $(fn $hook(&mut self, node: &mut $node) -> ControlFlow<()> {
                    self.record(stringify!($hook), node)
                })*
            }
        };
    }

    logging_hooks! {
        pre_visit_query: Query,
        post_visit_query: Query,
        pre_visit_select: Select,
        post_visit_select: Select,
        pre_visit_relation: ObjectName,
        post_visit_relation: ObjectName,
        pre_visit_table_factor: TableFactor,
        post_visit_table_factor: TableFactor,
        pre_visit_expr: Expr,
        post_visit_expr: Expr,
        pre_visit_statement: Statement,
        post_visit_statement: Statement,
        pre_visit_value: ValueWithSpan,
        post_visit_value: ValueWithSpan,
        pre_visit_ident: Ident,
        post_visit_ident: Ident,
        pre_visit_order_by: OrderBy,
        post_visit_order_by: OrderBy,
        pre_visit_order_by_expr: OrderByExpr,
        post_visit_order_by_expr: OrderByExpr,
        pre_visit_group_by: GroupByExpr,
        post_visit_group_by: GroupByExpr,
    }

    /// The distinct hooks a log records.
    fn hooks(log: &Log) -> Vec<&str> {
        let mut hooks: Vec<&str> =
            log.0.iter().filter_map(|entry| entry.split(' ').next()).collect();
        hooks.sort_unstable();
        hooks.dedup();
        hooks
    }

    #[test]
    fn walk_meets_what_a_direct_walk_meets() {
        let statements = Parser::parse_sql(&GenericDialect {}, SQL).expect("the query parses");
        let mut direct = Log::default();
        let _ = Visit::visit(&statements, &mut direct);
        let mut walked = Log::default();
        let _ = walk(&statements, &mut walked);

        assert_eq!(hooks(&direct).len(), 22, "the query reaches every hook");
        assert_eq!(walked.0, direct.0);
    }

    #[test]
    fn walk_mut_meets_what_a_direct_walk_meets() {
        let mut statements = Parser::parse_sql(&GenericDialect {}, SQL).expect("the query parses");
        let mut direct = Log::default();
        let _ = VisitMut::visit(&mut statements, &mut direct);
        let mut walked = Log::default();
        let _ = walk_mut(&mut statements, &mut walked);

        assert_eq!(hooks(&direct).len(), 22, "the query reaches every hook");
        assert_eq!(walked.0, direct.0);
    }
}
