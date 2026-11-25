//! Whether a filter implies the conditions of row-level-security policies,
//! compared as written once the columns each tests go by one name.

use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{convert::Infallible, mem, ops::ControlFlow, slice};

use sqlparser::ast::{
    AccessExpr, BinaryOperator, Distinct, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentList, FunctionArguments, Ident, InTarget, Query, Select, SelectItem, SetExpr,
    TableAlias, TableFactor, Value, Visit, Visitor, VisitorMut, WildcardAdditionalOptions,
};

use super::{
    identity::stored_name,
    terms::{
        conjuncts, disjuncts, is_column_reference, is_tautology, literal, unnested, unwrapped,
    },
};
use crate::{
    errors::LookupError,
    structs::{GateTerm, RowGate},
    traits::DatabaseLike,
    utils::{
        identifier_resolution::normalize_identifier,
        walk::{walk, walk_expressions, walk_expressions_mut, walk_mut},
    },
};

/// What a name a filter writes like a column's stands for.
pub(super) enum Term {
    /// The column of this stored name.
    Column(String),
    /// The input parameter at this position, counted from zero, of the
    /// function whose body holds the filter.
    Parameter(usize),
}

/// The values an input parameter of an enum type may hold.
pub(super) struct ParameterValues {
    /// Each label of the enum, as a string literal.
    pub(super) labels: Vec<Expr>,
    /// Whether the parameter may also be `NULL`, as in a function that runs
    /// on `NULL` input.
    pub(super) null: bool,
}

/// One value of the row a policy condition tests against a set.
pub(super) enum Operand<'e> {
    /// The condition columns this column names.
    Tested(String),
    /// A value that reads no column, met as written.
    Value(&'e Expr),
}

/// How the columns a policy condition tests map onto those a filter tests.
pub(super) trait Columns {
    /// Whether every value the set expression `narrower` yields is one
    /// `wider` yields.
    ///
    /// # Errors
    ///
    /// Returns the lookup error reading what either expression yields raised.
    fn is_set_within(&self, narrower: &Expr, wider: &Expr) -> Result<bool, LookupError>;

    /// The condition columns `column` names, when it names one the filter can
    /// test.
    fn tested(&self, column: &Expr) -> Option<String>;

    /// Whether the filter column `column` is one of `tested`.
    fn tests(&self, column: &Expr, tested: &str) -> bool;

    /// The values the input parameter at `ordinal` may hold, `None` for one
    /// whose type is no enum.
    fn parameter_values(&self, ordinal: usize) -> Option<&ParameterValues>;

    /// Whether every function `call` may reach returns `NULL` when its
    /// positional argument at `index`, counted from zero, is `NULL`.
    fn returns_null_on_null_argument(&self, call: &Function, index: usize) -> bool;
}

/// A policy condition as a filter would state it.
pub(super) struct Condition<'e> {
    /// The condition with each column it tests named as the filter names it,
    /// when the filter names every one.
    renamed: Option<Expr>,
    /// The row the condition tests against a set, value by value, and the
    /// expression yielding the members of the set.
    pub(super) membership: Option<(Vec<Operand<'e>>, Cow<'e, Expr>)>,
    /// The conjuncts of an `AND`, each implied for the whole.
    pub(super) parts: Vec<Self>,
    /// The arms of an `OR`, one implied for the whole.
    arms: Vec<Self>,
}

impl<'e> Condition<'e> {
    /// `condition` as a filter naming columns as `columns` does would state
    /// it, split into its conjuncts and arms.
    pub(super) fn new<C: Columns>(columns: &C, condition: &'e Expr) -> Self {
        let condition = unnested(condition);
        let split = |operands: Vec<&'e Expr>| {
            if operands.len() > 1 {
                operands.into_iter().map(|operand| Self::new(columns, operand)).collect()
            } else {
                Vec::new()
            }
        };
        Self {
            renamed: renamed(condition, |column| {
                stored_name(column).map(|name| Term::Column(name.into_owned()))
            }),
            membership: membership(condition).and_then(|(row, members)| {
                let operands = row
                    .iter()
                    .map(|operand| {
                        if is_column(operand) {
                            columns.tested(operand).map(Operand::Tested)
                        } else {
                            Some(Operand::Value(operand))
                        }
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some((operands, members))
            }),
            parts: split(conjuncts(condition)),
            arms: split(disjuncts(condition)),
        }
    }
}

/// Whether every row `clause` admits meets `condition` given `context`, through
/// matching predicates, set containment and Boolean implication.
///
/// # Errors
///
/// Returns the lookup error [`Columns::is_set_within`] raised.
pub(super) fn clause_implies<C: Columns>(
    columns: &C,
    clause: &Expr,
    condition: &Condition<'_>,
    context: &[&Expr],
) -> Result<bool, LookupError> {
    let clause = unnested(clause);
    if let Some(stated) = &condition.renamed
        && renamed(clause, |column| stored_name(column).map(|name| Term::Column(name.into_owned())))
            .as_ref()
            == Some(stated)
    {
        return Ok(true);
    }
    if let Some((operands, wider)) = &condition.membership
        && let Some((row, members)) = membership(clause)
        && row.len() == operands.len()
        && row_meets(columns, row, operands, context)
        && is_within(columns, &members, wider)?
    {
        return Ok(true);
    }
    let arms = disjuncts(clause);
    if arms.len() > 1 && every(&arms, |arm| clause_implies(columns, arm, condition, context))? {
        return Ok(true);
    }
    if let Expr::In { expr, target: InTarget::List(list), negated: false, .. } = clause {
        let arms: Vec<Expr> = list
            .iter()
            .map(|value| {
                Expr::BinaryOp {
                    left: expr.clone(),
                    op: BinaryOperator::Eq,
                    right: Box::new(value.clone()),
                }
            })
            .collect();
        if every(&arms, |arm| clause_implies(columns, arm, condition, context))? {
            return Ok(true);
        }
    }
    if !condition.parts.is_empty()
        && every(&condition.parts, |part| clause_implies(columns, clause, part, context))?
    {
        return Ok(true);
    }
    let parts = conjuncts(clause);
    if parts.len() > 1 {
        for (index, part) in parts.iter().enumerate() {
            let mut beside = context.to_vec();
            beside.extend(
                parts
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != index)
                    .flat_map(|(_, other)| conjuncts(other)),
            );
            if clause_implies(columns, part, condition, &beside)? {
                return Ok(true);
            }
        }
    }
    for arm in &condition.arms {
        if clause_implies(columns, clause, arm, context)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `holds` accepts every one of `items`, stopping at the first it
/// refuses or the first error it raises.
fn every<T, E>(items: &[T], mut holds: impl FnMut(&T) -> Result<bool, E>) -> Result<bool, E> {
    for item in items {
        if !holds(item)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The policies of a gate no accepted condition implies.
pub(super) enum Refused<'p, P> {
    /// A restrictive term not implied.
    Restrictive(&'p P),
    /// The permissive terms, none implied, empty when the gate has none.
    Permissive(Vec<&'p P>),
}

/// What keeps `gates` from admitting every row of `table` that meets each
/// condition `implied` accepts, or `None` when they admit them all, which
/// holds when every restrictive term and one permissive term of each gate is
/// a tautology or accepted.
///
/// # Errors
///
/// Returns the lookup error reading a policy or `implied` raised.
pub(super) fn admits<'p, DB: DatabaseLike, C: Columns>(
    database: &DB,
    table: &DB::Table,
    gates: &[RowGate<'p, DB::Policy>],
    columns: &C,
    implied: impl Fn(&Condition<'_>) -> Result<bool, LookupError>,
) -> Result<Option<Refused<'p, DB::Policy>>, LookupError> {
    let holds = |condition: &Expr| -> Result<bool, LookupError> {
        Ok(is_tautology(database, table, condition)?
            || implied(&Condition::new(columns, condition))?)
    };
    for gate in gates {
        for term in gate.restrictive() {
            if !holds(term.expression().expression())? {
                return Ok(Some(Refused::Restrictive(term.policy())));
            }
        }
        let mut admitted = false;
        for term in gate.permissive() {
            if holds(term.expression().expression())? {
                admitted = true;
                break;
            }
        }
        if !admitted {
            return Ok(Some(Refused::Permissive(
                gate.permissive().iter().map(GateTerm::policy).collect(),
            )));
        }
    }
    Ok(None)
}

/// Whether `expression` is a column reference, bare or qualified.
pub(super) fn is_column(expression: &Expr) -> bool {
    is_column_reference(unwrapped(expression))
}

/// Whether `node` mentions a column, which in a policy can be one of the
/// row's.
fn references_column<V: Visit>(node: &V) -> bool {
    walk_expressions(node, |expression| {
        if is_column(expression) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    })
    .is_break()
}

/// Whether `expression` mentions a column or reads an input parameter as
/// `$n`, either of which may hold another value where it is read again.
pub(super) fn reads_column_or_parameter(expression: &Expr) -> bool {
    walk_expressions(expression, |expression| {
        if is_column(expression) || parameter_ordinal(expression).is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

/// The numbered placeholder `$n` PostgreSQL reads as the input parameter at
/// `ordinal`, counted from zero.
pub(super) fn parameter(ordinal: usize) -> Expr {
    Expr::Value(Value::Placeholder(format!("${}", ordinal + 1)).into())
}

/// The position, counted from zero, of the input parameter `expression`
/// reads as `$n`.
pub(super) fn parameter_ordinal(expression: &Expr) -> Option<usize> {
    let Expr::Value(value) = expression else {
        return None;
    };
    let Value::Placeholder(placeholder) = &value.value else {
        return None;
    };
    let digits = placeholder.strip_prefix('$')?;
    digits.parse::<usize>().ok()?.checked_sub(1)
}

/// `expression` with every column reference named, quoted, by the column
/// `rename` resolves it to and every parameter reference written as `$n`,
/// and every scalar subquery that reads no relation, such as
/// `(SELECT auth.uid())`, replaced by the value it projects. `rename` is
/// called on the references of `expression` itself. `None` when `rename`
/// resolves a reference to nothing or a subquery reads a column, which may
/// resolve to another table's.
pub(super) fn renamed(expression: &Expr, rename: impl Fn(&Expr) -> Option<Term>) -> Option<Expr> {
    /// Resolves each reference, in the order [`Renamer`] meets them.
    struct Resolver<F> {
        /// Resolves a column reference to its term.
        rename: F,
        /// The number of enclosing queries.
        depth: usize,
        /// The references resolved in visitation order.
        resolved: Vec<Expr>,
    }

    impl<F: Fn(&Expr) -> Option<Term>> Visitor for Resolver<F> {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }

        fn post_visit_expr(&mut self, expression: &Expr) -> ControlFlow<()> {
            if !is_column_reference(expression) {
                return ControlFlow::Continue(());
            }
            let resolved = match (self.rename)(expression) {
                Some(Term::Column(name)) if self.depth == 0 => {
                    Expr::Identifier(Ident::with_quote('"', name))
                }
                Some(Term::Parameter(ordinal)) => parameter(ordinal),
                _ => return ControlFlow::Break(()),
            };
            self.resolved.push(resolved);
            ControlFlow::Continue(())
        }
    }

    /// Writes the resolved references and inlines the scalar subqueries.
    struct Renamer<I> {
        /// The references to write in visitation order.
        resolved: I,
    }

    impl<I: Iterator<Item = Expr>> VisitorMut for Renamer<I> {
        type Break = Infallible;

        fn post_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Infallible> {
            if let Expr::Subquery(query) = expression
                && sole_item(query).is_some()
                && let SetExpr::Select(select) = query.body.as_mut()
                && let [
                    SelectItem::UnnamedExpr(value) | SelectItem::ExprWithAlias { expr: value, .. },
                ] = select.projection.as_mut_slice()
            {
                *expression = mem::replace(value, Expr::Value(Value::Null.into()));
            } else if is_column_reference(expression)
                && let Some(resolved) = self.resolved.next()
            {
                *expression = resolved;
            }
            ControlFlow::Continue(())
        }
    }

    let mut resolver = Resolver { rename, depth: 0, resolved: Vec::new() };
    if walk(expression, &mut resolver).is_break() {
        return None;
    }
    let mut renamed = expression.clone();
    let ControlFlow::Continue(()) =
        walk_mut(&mut renamed, &mut Renamer { resolved: resolver.resolved.into_iter() });
    Some(renamed)
}

/// Whether `row`, tested against a set by a clause, holds the values each
/// of `operands` asks of a condition's row, a column matching a column the
/// condition tests, a value matching it as written, and a column `context`
/// fixes to a literal matching that literal either way.
fn row_meets<C: Columns>(
    columns: &C,
    row: &[Expr],
    operands: &[Operand<'_>],
    context: &[&Expr],
) -> bool {
    row.iter().zip(operands).all(|(value, operand)| {
        match operand {
            Operand::Tested(tested) => {
                if is_column(value) {
                    columns.tests(value, tested)
                } else {
                    literal(value).is_some()
                        && pinned(context, |column| columns.tests(column, tested), value)
                }
            }
            Operand::Value(stated) => {
                unnested(value) == unnested(stated)
                    || is_column(value)
                        && columns.tested(value).is_some_and(|tested| {
                            literal(stated).is_some()
                                && pinned(context, |column| columns.tests(column, &tested), stated)
                        })
            }
        }
    })
}

/// Whether a conjunct of `context` fixes a column `tested` accepts to the
/// literal `fixed` with `=`, written on either side.
fn pinned(context: &[&Expr], tested: impl Fn(&Expr) -> bool, fixed: &Expr) -> bool {
    context.iter().any(|conjunct| {
        let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = unnested(conjunct) else {
            return false;
        };
        [(left, right), (right, left)].into_iter().any(|(column, value)| {
            is_column(column) && tested(column) && unnested(value) == unnested(fixed)
        })
    })
}

/// Whether every value `narrower` yields is one `wider` yields, whatever
/// value each input parameter it reads holds. A parameter of an enum type
/// takes each label in turn, and a `NULL` one passes only to a call returning
/// `NULL` on `NULL` input, which yields no value. A parameter of any other
/// type proves nothing.
fn is_within<C: Columns>(columns: &C, narrower: &Expr, wider: &Expr) -> Result<bool, LookupError> {
    let mut ordinals = Vec::new();
    let _ = walk_expressions(narrower, |expression| {
        if let Some(ordinal) = parameter_ordinal(expression)
            && !ordinals.contains(&ordinal)
        {
            ordinals.push(ordinal);
        }
        ControlFlow::<()>::Continue(())
    });
    if ordinals.is_empty() {
        return columns.is_set_within(narrower, wider);
    }
    let Some(values) = ordinals
        .iter()
        .map(|&ordinal| columns.parameter_values(ordinal))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(false);
    };
    if ordinals
        .iter()
        .zip(&values)
        .any(|(&ordinal, values)| values.null && !nothing_on_null(columns, narrower, ordinal))
    {
        return Ok(false);
    }
    let mut choice = vec![0; ordinals.len()];
    loop {
        let mut expanded = narrower.clone();
        let _ = walk_expressions_mut(&mut expanded, |expression| {
            if let Some(ordinal) = parameter_ordinal(expression)
                && let Some(position) = ordinals.iter().position(|&seen| seen == ordinal)
                && let Some(label) = values[position].labels.get(choice[position])
            {
                *expression = label.clone();
            }
            ControlFlow::<()>::Continue(())
        });
        if !columns.is_set_within(&expanded, wider)? {
            return Ok(false);
        }
        let Some(position) = (0..choice.len())
            .find(|&position| choice[position] + 1 < values[position].labels.len())
        else {
            return Ok(true);
        };
        choice[position] += 1;
        choice[..position].fill(0);
    }
}

/// Whether `narrower` yields no value while the input parameter at
/// `ordinal` is `NULL`, as a call does that takes it, through parentheses
/// and casts, as a positional argument it returns `NULL` for.
fn nothing_on_null<C: Columns>(columns: &C, narrower: &Expr, ordinal: usize) -> bool {
    let Expr::Function(call @ Function { args: FunctionArguments::List(list), .. }) =
        unwrapped(narrower)
    else {
        return false;
    };
    list.args.iter().enumerate().any(|(index, argument)| {
        matches!(
            argument,
            FunctionArg::Unnamed(FunctionArgExpr::Expr(argument))
                if parameter_ordinal(unwrapped(argument)) == Some(ordinal)
        ) && columns.returns_null_on_null_argument(call, index)
    })
}

/// The expression yielding the values `value` holds as the operand of `=`,
/// when it reads no column.
fn value_members(value: &Expr) -> Option<Cow<'_, Expr>> {
    match unnested(value) {
        Expr::Subquery(query) => query_members(query),
        value => (!references_column(value)).then_some(Cow::Borrowed(value)),
    }
}

/// The expression yielding the rows of `query`, when the query projects it
/// alone, selects every column of one call or one column of one call, and
/// filters and limits nothing.
pub(super) fn query_members(query: &Query) -> Option<Cow<'_, Expr>> {
    match sole_item(query) {
        Some(value) => value_members(value),
        None => sole_call(query).or_else(|| sole_column(query)).map(Cow::Owned),
    }
}

/// The `SELECT` of `query`, when it filters, limits and draws on nothing
/// and keeps every distinct row, so it yields every row its `FROM` list
/// does.
fn unfiltered(query: &Query) -> Option<&Select> {
    if let SetExpr::Select(select) = query.body.as_ref()
        && query.with.is_none()
        && query.limit_clause.is_none()
        && query.fetch.is_none()
        && query.pipe_operators.is_empty()
        && !matches!(select.distinct, Some(Distinct::On(_)))
        && select.top.is_none()
        && select.prewhere.is_none()
        && select.selection.is_none()
        && select.connect_by.is_empty()
        && select.having.is_none()
        && select.qualify.is_none()
    {
        Some(select)
    } else {
        None
    }
}

/// The one expression `query` projects, when it reads, filters and limits
/// nothing, so it yields that value in one row.
fn sole_item(query: &Query) -> Option<&Expr> {
    let select = unfiltered(query)?;
    if let [SelectItem::UnnamedExpr(value) | SelectItem::ExprWithAlias { expr: value, .. }] =
        select.projection.as_slice()
        && select.from.is_empty()
    {
        Some(value)
    } else {
        None
    }
}

/// The call `query` selects every column of, written as an expression, when
/// the query reads that call alone, without ordinality, and filters and
/// limits nothing, so it yields every row of the call.
fn sole_call(query: &Query) -> Option<Expr> {
    let select = unfiltered(query)?;
    let [SelectItem::Wildcard(options)] = select.projection.as_slice() else {
        return None;
    };
    if *options != WildcardAdditionalOptions::default() {
        return None;
    }
    sole_relation_call(select).map(|(call, _)| call)
}

/// The column of one call `query` projects, written `(call).column`, when
/// the query reads that call alone under an alias renaming no column, names
/// the column through the alias, and filters and limits nothing, so it
/// yields every value of the column. The call reads no column or parameter,
/// which could stand for another row's value.
fn sole_column(query: &Query) -> Option<Expr> {
    let select = unfiltered(query)?;
    let [
        SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts))
        | SelectItem::ExprWithAlias { expr: Expr::CompoundIdentifier(parts), .. },
    ] = select.projection.as_slice()
    else {
        return None;
    };
    let [relation, column] = parts.as_slice() else {
        return None;
    };
    let (call, Some(alias)) = sole_relation_call(select)? else {
        return None;
    };
    if !alias.columns.is_empty()
        || normalize_identifier(&alias.name.value, alias.name.quote_style.is_some())
            != normalize_identifier(&relation.value, relation.quote_style.is_some())
        || reads_column_or_parameter(&call)
    {
        return None;
    }
    Some(column_of(call, &normalize_identifier(&column.value, column.quote_style.is_some())))
}

/// The values the column stored as `column` takes among the rows `call`
/// yields, written `(call).column`, the column quoted unless its stored name
/// is folded.
pub(super) fn column_of(call: Expr, column: &str) -> Expr {
    let field = if normalize_identifier(column, false) == column {
        Ident::new(column)
    } else {
        Ident::with_quote('"', column)
    };
    Expr::CompoundFieldAccess {
        root: Box::new(Expr::Nested(Box::new(call))),
        access_chain: vec![AccessExpr::Dot(Expr::Identifier(field))],
    }
}

/// The call `select` reads alone, without ordinality, written as an
/// expression, with the alias it reads the call under.
fn sole_relation_call(select: &Select) -> Option<(Expr, Option<&TableAlias>)> {
    let [from] = select.from.as_slice() else {
        return None;
    };
    if !from.joins.is_empty() {
        return None;
    }
    let (name, args, alias) = match &from.relation {
        TableFactor::Table {
            name,
            args: Some(args),
            with_ordinality: false,
            sample: None,
            alias,
            ..
        } if args.settings.is_none() => (name, &args.args, alias),
        TableFactor::Function { name, args, with_ordinality: false, alias, .. } => {
            (name, args, alias)
        }
        _ => return None,
    };
    let call = Expr::Function(Function {
        name: name.clone(),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args.clone(),
            clauses: Vec::new(),
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: Vec::new(),
    });
    Some((call, alias.as_ref()))
}

/// The expression yielding the elements of `array` as the operand of
/// `= ANY`, when it reads no column.
fn array_members(array: &Expr) -> Option<Cow<'_, Expr>> {
    let array = unnested(array);
    if let Expr::Function(function) = array
        && function.name.to_string().eq_ignore_ascii_case("array")
        && let FunctionArguments::Subquery(query) = &function.args
    {
        return query_members(query);
    }
    (!references_column(array)).then_some(Cow::Borrowed(array))
}

/// The row `condition` tests against a set with `=`, `= ANY` or `IN`, one
/// column or a row value of columns and values reading none, and the
/// expression yielding the members of the set. Column types keep one
/// expression from yielding a value in one spelling and an array in another.
pub(super) fn membership(condition: &Expr) -> Option<(&[Expr], Cow<'_, Expr>)> {
    match unwrapped(condition) {
        Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
            [(left, right), (right, left)].into_iter().find_map(|(column, value)| {
                if is_column(column) {
                    Some((slice::from_ref(column.as_ref()), value_members(value)?))
                } else {
                    None
                }
            })
        }
        Expr::AnyOp { left, compare_op: BinaryOperator::Eq, right, .. } if is_column(left) => {
            Some((slice::from_ref(left.as_ref()), array_members(right)?))
        }
        Expr::In { expr, target: InTarget::Subquery(query), negated: false, .. } => {
            let row = match unwrapped(expr) {
                Expr::Tuple(values)
                    if values.iter().all(|value| is_column(value) || !references_column(value)) =>
                {
                    values.as_slice()
                }
                _ if is_column(expr) => slice::from_ref(expr.as_ref()),
                _ => return None,
            };
            Some((row, query_members(query)?))
        }
        _ => None,
    }
}
