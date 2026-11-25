//! Whether one call of a set-returning `LANGUAGE sql` function yields only
//! values another call of it yields, read from how its body compares an
//! input parameter of an enum type.

use alloc::{
    borrow::ToOwned,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::{cell::RefCell, iter, ops::ControlFlow, ptr};

use sqlparser::ast::{
    ArgMode, BinaryOperator, Distinct, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, FunctionBehavior, GroupByExpr, InTarget, JoinConstraint, JoinOperator,
    Query, Select, SelectFlavor, SelectItem, SetExpr, SetOperator, Statement, TableFactor,
    TableFunctionArgs, TableWithJoins, Visit, Visitor,
};

use super::{
    implication::{parameter, parameter_ordinal},
    terms::{
        literal, sets_only_search_path, sql_query_body, text_literal, unnested, unwrapped,
        user_type_named,
    },
};
use crate::{
    errors::LookupError,
    structs::{
        ColumnDefinition, ExpressionContext, ExpressionScope, IdentifierCase, OperandType,
        WrittenCall,
    },
    traits::{DatabaseLike, Enumeration, FunctionLike, UserDefinedTypeLike},
    utils::{identifier_resolution::normalize_identifier, walk::walk},
};

/// An input parameter of a function, by the function's address and the
/// parameter's position among its inputs.
type Position<DB> = (*const <DB as DatabaseLike>::Function, usize);

/// Whether functions descend in input parameters of an enum type, each read
/// once: a function descends in one when a call passing a later label
/// yields only values a call passing an earlier one yields, the other
/// arguments alike.
pub(super) struct Ladders<'a, DB: DatabaseLike> {
    /// The schema declaring the functions and enum types.
    database: &'a DB,
    /// The descent results already read for each parameter.
    read: RefCell<Vec<(Position<DB>, bool)>>,
    /// The parameters being read, so a cycle proves nothing.
    reading: RefCell<Vec<Position<DB>>>,
}

impl<'a, DB: DatabaseLike> Ladders<'a, DB> {
    /// Reads descending parameters from `database`.
    pub(super) fn new(database: &'a DB) -> Self {
        Self { database, read: RefCell::default(), reading: RefCell::default() }
    }

    /// Whether every value the call `narrower` yields is one the call
    /// `wider` yields: both call one function, passing literals that agree
    /// except at input parameters of an enum type it descends in, where
    /// `narrower` passes a label the type orders at or after the one
    /// `wider` passes.
    ///
    /// # Errors
    ///
    /// Returns the lookup error reading either call or the function raised.
    pub(super) fn within(&self, narrower: &Expr, wider: &Expr) -> Result<bool, LookupError> {
        let (Expr::Function(narrower), Expr::Function(wider)) =
            (unnested(narrower), unnested(wider))
        else {
            return Ok(false);
        };
        let Some(function) = called(self.database, narrower)? else {
            return Ok(false);
        };
        if !called(self.database, wider)?.is_some_and(|other| ptr::eq(other, function)) {
            return Ok(false);
        }
        let (Some(lower), Some(upper)) =
            (positional_arguments(narrower, function), positional_arguments(wider, function))
        else {
            return Ok(false);
        };
        for (position, (low, high)) in lower.into_iter().zip(upper).enumerate() {
            if literal(low).is_none() || literal(high).is_none() {
                return Ok(false);
            }
            if low == high {
                continue;
            }
            let Some(labeled) = enumeration(self.database, function, position)? else {
                return Ok(false);
            };
            let labels: Vec<String> = labeled
                .enumeration()
                .into_iter()
                .flat_map(Enumeration::into_members)
                .map(|member| member.label().to_owned())
                .collect();
            let rank = |argument: &Expr| {
                text_literal(argument)
                    .and_then(|label| labels.iter().position(|known| known == label))
            };
            let (Some(low), Some(high)) = (rank(low), rank(high)) else {
                return Ok(false);
            };
            if low == high {
                continue;
            }
            if low < high || !self.descends(function, position, labeled)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `function` descends in its input parameter at `position`, of
    /// the enum type `ordered`.
    fn descends(
        &self,
        function: &'a DB::Function,
        position: usize,
        ordered: &'a DB::UserDefinedType,
    ) -> Result<bool, LookupError> {
        let key = (ptr::from_ref(function), position);
        if let Some(&(_, known)) = self.read.borrow().iter().find(|(read, _)| *read == key) {
            return Ok(known);
        }
        if self.reading.borrow().contains(&key) {
            return Ok(false);
        }
        self.reading.borrow_mut().push(key);
        let descends = self.read_descent(function, position, ordered);
        self.reading.borrow_mut().pop();
        let descends = descends?;
        self.read.borrow_mut().push((key, descends));
        Ok(descends)
    }

    /// [`Self::descends`] read from the body of `function`, one query that
    /// may not read a later snapshot or change a setting its comparisons
    /// read.
    fn read_descent(
        &self,
        function: &'a DB::Function,
        position: usize,
        ordered: &'a DB::UserDefinedType,
    ) -> Result<bool, LookupError> {
        if function.volatility() == FunctionBehavior::Volatile
            || !function.returns_set()
            || !sets_only_search_path(function)
        {
            return Ok(false);
        }
        let Some(query) = sql_query_body(self.database, function)? else {
            return Ok(false);
        };
        let statement = Statement::Query(Box::new(query.into_owned()));
        let scope = ExpressionScope::FunctionBody(function);
        let (Statement::Query(query), Ok(context)) = (
            &statement,
            ExpressionContext::for_statement(
                &statement,
                &scope,
                self.database,
                IdentifierCase::AsWritten,
            ),
        ) else {
            return Ok(false);
        };
        Descent { ladders: self, context: &context, position, ordered, items: RefCell::default() }
            .query(query)
    }
}

/// The enum type of the input parameter of `function` at `position`.
fn enumeration<'db, DB: DatabaseLike>(
    database: &'db DB,
    function: &DB::Function,
    position: usize,
) -> Result<Option<&'db DB::UserDefinedType>, LookupError> {
    let placeholder = parameter(position);
    let scope = ExpressionScope::FunctionBody(function);
    let Ok(context) = ExpressionContext::for_expression(
        &placeholder,
        &scope,
        database,
        IdentifierCase::AsWritten,
    ) else {
        return Ok(None);
    };
    let OperandType::Known { type_name, .. } = context.type_of(&placeholder)? else {
        return Ok(None);
    };
    Ok(user_type_named(database, &type_name).filter(|user_type| user_type.enumeration().is_some()))
}

/// Whether a body yields fewer values as its input parameter at `position`
/// takes later labels of `ordered`: each place reading the parameter, or a
/// `WITH` item that reads it, only drops rows as the parameter grows.
struct Descent<'l, 'c, 'q, 'a, DB: DatabaseLike> {
    /// The function descent cache.
    ladders: &'l Ladders<'a, DB>,
    /// The references and types of this body.
    context: &'c ExpressionContext<'q, 'a, DB>,
    /// The input parameter's position, counted from zero.
    position: usize,
    /// The enum type ordering the parameter's values.
    ordered: &'a DB::UserDefinedType,
    /// The normalized names of the `WITH` items reading the parameter.
    items: RefCell<Vec<String>>,
}

impl<'a, DB: DatabaseLike> Descent<'_, '_, '_, 'a, DB> {
    /// Whether `node` reads the parameter or a `WITH` item reading it.
    fn touches<N: Visit + ?Sized>(&self, node: &N) -> bool {
        walk(node, &mut Touch(self)).is_break()
    }

    /// Whether `expression` names the parameter, or a name that may.
    fn is_parameter(&self, expression: &Expr) -> bool {
        if parameter_ordinal(expression) == Some(self.position) {
            return true;
        }
        if !matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            return false;
        }
        match self.context.reference(expression) {
            Ok(Some((ColumnDefinition::Parameter { ordinal, .. }, _))) => ordinal == self.position,
            Ok(_) => false,
            Err(_) => true,
        }
    }

    /// Whether `factor` reads a `WITH` item reading the parameter.
    fn names_item(&self, factor: &TableFactor) -> bool {
        let TableFactor::Table { name, args: None, .. } = factor else {
            return false;
        };
        let [part] = name.0.as_slice() else {
            return false;
        };
        part.as_ident().is_some_and(|name| {
            self.items
                .borrow()
                .iter()
                .any(|item| *item == normalize_identifier(&name.value, name.quote_style.is_some()))
        })
    }

    /// Whether `query` only drops rows as the parameter grows.
    fn query(&self, query: &Query) -> Result<bool, LookupError> {
        for item in query.with.iter().flat_map(|with| &with.cte_tables) {
            if self.touches(item.query.as_ref()) {
                let name = &item.alias.name;
                self.items.borrow_mut().push(
                    normalize_identifier(&name.value, name.quote_style.is_some()).into_owned(),
                );
                if !self.query(&item.query)? {
                    return Ok(false);
                }
            }
        }
        if !self.touches(query.body.as_ref()) {
            return Ok(true);
        }
        Ok(query.limit_clause.is_none()
            && query.fetch.is_none()
            && query.locks.is_empty()
            && query.for_clause.is_none()
            && query.pipe_operators.is_empty()
            && self.body(&query.body)?)
    }

    /// Whether `body` only drops rows as the parameter grows.
    fn body(&self, body: &SetExpr) -> Result<bool, LookupError> {
        if !self.touches(body) {
            return Ok(true);
        }
        match body {
            SetExpr::Select(select) => self.select(select),
            SetExpr::Query(query) => self.query(query),
            SetExpr::SetOperation {
                op: SetOperator::Union | SetOperator::Intersect,
                left,
                right,
                ..
            } => Ok(self.body(left)? && self.body(right)?),
            SetExpr::SetOperation {
                op: SetOperator::Except | SetOperator::Minus,
                left,
                right,
                ..
            } => Ok(!self.touches(right.as_ref()) && self.body(left)?),
            _ => Ok(false),
        }
    }

    /// Whether `select` yields one row per row its `FROM` and `WHERE` leave,
    /// each of which only drops rows as the parameter grows.
    fn select(&self, select: &Select) -> Result<bool, LookupError> {
        if !plain(select) {
            return Ok(false);
        }
        for item in &select.projection {
            let kept = match item {
                SelectItem::UnnamedExpr(projected)
                | SelectItem::ExprWithAlias { expr: projected, .. } => {
                    match projected {
                        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => {
                            !self.is_parameter(projected)
                        }
                        Expr::Function(call) => self.call(call)?,
                        _ => false,
                    }
                }
                other => {
                    matches!(other, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..))
                }
            };
            if !kept {
                return Ok(false);
            }
        }
        for from in &select.from {
            if !self.from(from)? {
                return Ok(false);
            }
        }
        match &select.selection {
            Some(filter) => self.condition(filter),
            None => Ok(true),
        }
    }

    /// Whether the joined relations only drop rows as the parameter grows.
    fn from(&self, from: &TableWithJoins) -> Result<bool, LookupError> {
        if !self.touches(from) {
            return Ok(true);
        }
        for join in &from.joins {
            let (JoinOperator::Join(constraint)
            | JoinOperator::Inner(constraint)
            | JoinOperator::CrossJoin(constraint)) = &join.join_operator
            else {
                return Ok(false);
            };
            if let JoinConstraint::On(condition) = constraint
                && !self.condition(condition)?
            {
                return Ok(false);
            }
        }
        for factor in iter::once(&from.relation).chain(from.joins.iter().map(|join| &join.relation))
        {
            if !self.factor(factor)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `factor` only drops rows as the parameter grows.
    fn factor(&self, factor: &TableFactor) -> Result<bool, LookupError> {
        if !self.touches(factor) {
            return Ok(true);
        }
        if let Some(call) = WrittenCall::relation(factor) {
            let Some(callee) = called(self.ladders.database, call)? else {
                return Ok(false);
            };
            return match relation_arguments(factor, callee) {
                Some(arguments) => self.passes(callee, &arguments),
                None => Ok(false),
            };
        }
        match factor {
            TableFactor::Table { args: None, sample: None, .. } => Ok(true),
            TableFactor::Derived { subquery, .. } => self.query(subquery),
            TableFactor::NestedJoin { table_with_joins, .. } => self.from(table_with_joins),
            _ => Ok(false),
        }
    }

    /// Whether the set-returning call `call`, whose rows the result holds,
    /// only drops rows as the parameter grows.
    fn call(&self, call: &Function) -> Result<bool, LookupError> {
        if !self.touches(call) {
            return Ok(false);
        }
        let Some(callee) = called(self.ladders.database, call)? else {
            return Ok(false);
        };
        match positional_arguments(call, callee) {
            Some(arguments) if call.over.is_none() && call.filter.is_none() => {
                self.passes(callee, &arguments)
            }
            _ => Ok(false),
        }
    }

    /// Whether `callee`, passed `arguments`, descends in each position whose
    /// argument reads the parameter, which each passes as it stands.
    fn passes(&self, callee: &'a DB::Function, arguments: &[&Expr]) -> Result<bool, LookupError> {
        for (position, argument) in arguments.iter().enumerate() {
            if !self.touches(*argument) {
                continue;
            }
            let mut passed = *argument;
            while let Expr::Nested(inner) = passed {
                passed = inner;
            }
            if !self.is_parameter(passed)
                || !enumeration(self.ladders.database, callee, position)?
                    .is_some_and(|labeled| ptr::eq(labeled, self.ordered))
                || !self.ladders.descends(callee, position, self.ordered)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `condition` only drops rows as the parameter grows.
    fn condition(&self, condition: &Expr) -> Result<bool, LookupError> {
        if !self.touches(condition) {
            return Ok(true);
        }
        match condition {
            Expr::Nested(inner) => self.condition(inner),
            Expr::BinaryOp { left, op: BinaryOperator::And | BinaryOperator::Or, right } => {
                Ok(self.condition(left)? && self.condition(right)?)
            }
            Expr::BinaryOp { left, op: BinaryOperator::GtEq | BinaryOperator::Gt, right } => {
                self.bounds(left, right)
            }
            Expr::BinaryOp { left, op: BinaryOperator::LtEq | BinaryOperator::Lt, right } => {
                self.bounds(right, left)
            }
            Expr::In { expr, target: InTarget::Subquery(subquery), negated: false, .. } => {
                Ok(!self.touches(expr.as_ref()) && self.query(subquery)?)
            }
            Expr::Exists { subquery, negated: false } => self.query(subquery),
            Expr::AnyOp { left, compare_op: BinaryOperator::Eq, right, .. } => {
                match unwrapped(right) {
                    Expr::Function(array)
                        if array.name.to_string().eq_ignore_ascii_case("array")
                            && !self.touches(left.as_ref()) =>
                    {
                        match &array.args {
                            FunctionArguments::Subquery(subquery) => self.query(subquery),
                            _ => Ok(false),
                        }
                    }
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    /// Whether `column` is a column of the parameter's enum type and `bound`
    /// the parameter, so `column >= bound` drops rows as the bound grows.
    fn bounds(&self, column: &Expr, mut bound: &Expr) -> Result<bool, LookupError> {
        while let Expr::Nested(inner) = bound {
            bound = inner;
        }
        if !self.is_parameter(bound)
            || !matches!(column, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
            || self.is_parameter(column)
        {
            return Ok(false);
        }
        let OperandType::Known { type_name, .. } = self.context.type_of(column)? else {
            return Ok(false);
        };
        Ok(user_type_named(self.ladders.database, &type_name)
            .is_some_and(|user_type| ptr::eq(user_type, self.ordered)))
    }
}

/// Finds a read of the parameter or of a `WITH` item reading it.
struct Touch<'d, 'l, 'c, 'q, 'a, DB: DatabaseLike>(&'d Descent<'l, 'c, 'q, 'a, DB>);

impl<DB: DatabaseLike> Visitor for Touch<'_, '_, '_, '_, '_, DB> {
    type Break = ();

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<()> {
        if self.0.is_parameter(expression) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        if self.0.names_item(factor) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }
}

/// The one function of the schema `call` reaches.
pub(super) fn called<'db, 'c, DB: DatabaseLike>(
    database: &'db DB,
    call: impl Into<WrittenCall<'c>>,
) -> Result<Option<&'db DB::Function>, LookupError> {
    let mut reached = database.functions_reached_by(call, IdentifierCase::Folded)?;
    Ok(match (reached.next(), reached.next()) {
        (Some(function), None) => Some(function),
        _ => None,
    })
}

/// The arguments `call` passes `function`, each positionally, one for each
/// input parameter in the order `$n` counts them, leaving none to a default
/// and none to a `VARIADIC` parameter.
pub(super) fn positional_arguments<'c, F: FunctionLike>(
    call: &'c Function,
    function: &F,
) -> Option<Vec<&'c Expr>> {
    let inputs = inputs(function)?;
    listed_arguments(call).filter(|arguments| arguments.len() == inputs)
}

/// [`positional_arguments`] for `relation`, a set-returning call in a
/// `FROM` list that numbers no row.
pub(super) fn relation_arguments<'r, F: FunctionLike>(
    relation: &'r TableFactor,
    function: &F,
) -> Option<Vec<&'r Expr>> {
    let (TableFactor::Table {
        args: Some(TableFunctionArgs { args: arguments, .. }),
        with_ordinality: false,
        sample: None,
        ..
    }
    | TableFactor::Function { args: arguments, with_ordinality: false, .. }) = relation
    else {
        return None;
    };
    let inputs = inputs(function)?;
    unnamed(arguments).filter(|arguments| arguments.len() == inputs)
}

/// How many input parameters `function` takes, or `None` when one is
/// `VARIADIC`.
fn inputs<F: FunctionLike>(function: &F) -> Option<usize> {
    let mut inputs = 0;
    for mode in function.argument_modes() {
        match mode {
            ArgMode::In | ArgMode::InOut => inputs += 1,
            ArgMode::Out => {}
            ArgMode::Variadic => return None,
        }
    }
    Some(inputs)
}

/// The arguments `call` passes, when it passes each positionally, with no
/// `DISTINCT`, `ORDER BY` or other clause among them.
pub(super) fn listed_arguments(call: &Function) -> Option<Vec<&Expr>> {
    let FunctionArguments::List(list) = &call.args else {
        return None;
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return None;
    }
    unnamed(&list.args)
}

/// Each of `arguments`, when every one is passed positionally.
fn unnamed(arguments: &[FunctionArg]) -> Option<Vec<&Expr>> {
    arguments
        .iter()
        .map(|argument| {
            match argument {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(value)) => Some(value),
                _ => None,
            }
        })
        .collect()
}

/// Whether each row the `FROM` and filters of `select` leave yields at
/// most one row, with no grouping or `DISTINCT ON` choosing among them.
pub(super) fn plain(select: &Select) -> bool {
    matches!(select.distinct, None | Some(Distinct::All | Distinct::Distinct))
        && select.select_modifiers.is_none()
        && select.top.is_none()
        && select.exclude.is_none()
        && select.into.is_none()
        && select.lateral_views.is_empty()
        && select.prewhere.is_none()
        && select.connect_by.is_empty()
        && matches!(&select.group_by, GroupByExpr::Expressions(grouped, modifiers) if grouped.is_empty() && modifiers.is_empty())
        && select.cluster_by.is_empty()
        && select.distribute_by.is_empty()
        && select.sort_by.is_empty()
        && select.having.is_none()
        && select.named_window.is_empty()
        && select.qualify.is_none()
        && select.value_table_mode.is_none()
        && select.flavor == SelectFlavor::Standard
}

#[cfg(test)]
mod tests {
    use alloc::{
        borrow::ToOwned,
        string::{String, ToString},
        vec,
        vec::Vec,
    };

    use sqlparser::{ast::Expr, dialect::PostgreSqlDialect, parser::Parser};

    use super::listed_arguments;

    /// The arguments [`listed_arguments`] reads from `call`, written for
    /// PostgreSQL, each as written.
    #[expect(clippy::expect_used, reason = "The test supplies valid expressions.")]
    #[expect(clippy::panic, reason = "The test supplies function calls.")]
    fn listed(call: &str) -> Option<Vec<String>> {
        let call = Parser::new(&PostgreSqlDialect {})
            .try_with_sql(call)
            .and_then(|mut parser| parser.parse_expr())
            .expect("call parses");
        let Expr::Function(call) = call else {
            panic!("{call} is not a call");
        };
        listed_arguments(&call).map(|arguments| arguments.iter().map(ToString::to_string).collect())
    }

    /// Only plain positional arguments are read.
    #[test]
    fn only_a_plain_positional_argument_list_is_read() {
        assert_eq!(
            listed("granted(1, 'writer')"),
            Some(vec!["1".to_owned(), "'writer'".to_owned()])
        );
        for refused in [
            "CURRENT_TIMESTAMP",
            "granted(DISTINCT 'writer')",
            "granted('writer' ORDER BY 1)",
            "granted(min_role => 'writer')",
        ] {
            assert_eq!(listed(refused), None, "{refused}");
        }
    }
}
