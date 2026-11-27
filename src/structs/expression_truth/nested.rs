//! What the queries an expression nests return and what the `LANGUAGE sql`
//! functions it calls answer, read over the values their names bind.

use alloc::{boxed::Box, rc::Rc, vec, vec::Vec};
use core::{
    mem,
    ops::{ControlFlow, Range},
    ptr,
};

use sqlparser::{
    ast::{
        Distinct, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
        FunctionSecurity, GroupByExpr, InTarget, JoinConstraint, JoinOperator, LimitClause,
        ObjectName, ObjectNamePart, OrderBy, OrderByKind, Query, Select, SelectFlavor, SelectItem,
        SetExpr, Statement, TableFactor, Value, ValueWithSpan, Values as ValueRows, Visit, Visitor,
        WildcardAdditionalOptions,
    },
    dialect::{GenericDialect, PostgreSqlDialect},
};
use sqlparser_expr_eval::{
    ArrayElement, CalleeKind, Context, Evaluation, Guarantees, NullInput, Outcomes,
    PostgreSqlOptions, QueryEvaluation, Rows, ScalarType, Subject, Unlisted, Values, evaluate,
    implied, sql_values::postgres::PgValue,
};

use super::{Engine, Key, Relation, Site, Source, TruthContext};
use crate::{
    errors::LookupError,
    impls::{body_expression, dql::scoped_roots::is_input, sql_query_body},
    structs::{
        ActingRole, CallResolution, ExpressionContext, ExpressionScope, IdentifierCase,
        PolicyClause, PolicyTarget, RowSecurity, RowStatement,
        expression_context::declared_type_named,
    },
    traits::{DataTypeLike, DatabaseLike, FunctionLike, PolicyLike, TableLike},
    utils::{
        call_sites::expression_form,
        postgres_types::PgType,
        walk::{walk, walk_expressions},
    },
};

/// How many bodies and policies a read follows inside one another.
const BODY_DEPTH: usize = 8;

/// The queries a read may meet, the bodies it read, and what each answered.
pub(super) struct Nested<'a, DB: DatabaseLike> {
    /// The queries planned, each by its address.
    queries: Vec<QueryPlan<'a, DB>>,
    /// The bodies planned, each by its function's address.
    bodies: Vec<BodyPlan<'a, DB>>,
    /// The policy clauses planned, each by its expression's address.
    policies: Vec<PolicyPlan<'a, DB>>,
    /// What a query answered under the guarantees and the role it was asked
    /// under.
    answers: Vec<(usize, usize, Guarantees<usize>, QueryEvaluation)>,
    /// What a body answered for the arguments and the role it was read with,
    /// `None` for a body the read does not follow.
    results: Vec<(usize, usize, Vec<Evaluation>, Option<QueryEvaluation>)>,
    /// Whether a policy clause may raise under a role, by the clause's
    /// address.
    raising: Vec<(usize, usize, bool)>,
    /// The addresses of the functions and policy clauses being read.
    running: Vec<usize>,
    /// The functions a call reached, whose addresses their callees carry.
    functions: Vec<&'a DB::Function>,
    /// The role the expression being read runs as.
    reader: Reader<'a, DB::Role>,
}

impl<DB: DatabaseLike> Default for Nested<'_, DB> {
    fn default() -> Self {
        Self {
            queries: Vec::new(),
            bodies: Vec::new(),
            policies: Vec::new(),
            answers: Vec::new(),
            results: Vec::new(),
            raising: Vec::new(),
            running: Vec::new(),
            functions: Vec::new(),
            reader: Reader::Caller,
        }
    }
}

impl<'a, DB: DatabaseLike> Nested<'a, DB> {
    /// Records `functions` as reached, so that a body read finds each by the
    /// token its callee carries.
    pub(super) fn know(&mut self, functions: &[&'a DB::Function]) {
        for function in functions {
            if !self.functions.iter().any(|known| ptr::eq(*known, *function)) {
                self.functions.push(function);
            }
        }
    }

    /// The role the read runs as, as the caches key it.
    fn role_key(&self) -> usize {
        match self.reader {
            Reader::Caller => 0,
            Reader::Unknown => 1,
            Reader::Role(role) => ptr::from_ref(role).addr(),
        }
    }
}

/// The role a read runs as.
enum Reader<'a, R> {
    /// The role running the expression the read starts from, each acting
    /// role of the assumed deployment, or one the read does not know where
    /// none is assumed.
    Caller,
    /// A role the database declares.
    Role(&'a R),
    /// A role the read does not know.
    Unknown,
}

impl<R> Clone for Reader<'_, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for Reader<'_, R> {}

/// A query the read may meet, its names bound before the read.
struct QueryPlan<'a, DB: DatabaseLike> {
    address: usize,
    /// The references and calls the query writes outside the queries it
    /// nests.
    site: Site<'a, DB>,
    /// The table reads the query's names made first, whose checks hold on
    /// the rows it reads.
    reads: Range<usize>,
    /// What each `FROM` item of the query reads, by the item's address.
    factors: Vec<(usize, Factor<'a, DB>)>,
    /// The context and scope the query's `FROM` items bind their columns
    /// in, `None` for a query reading none.
    scope: Option<(usize, usize)>,
    /// Whether the query is being read.
    active: bool,
}

/// What a `FROM` item reads.
enum Factor<'a, DB: DatabaseLike> {
    /// The stored rows of `table`.
    Table {
        /// The table read.
        table: &'a DB::Table,
        /// The read naming its columns, when the item yields only rows the
        /// table's constraints hold on.
        read: Option<usize>,
    },
    /// The rows of a table another catalog holds.
    Unheld,
    /// The rows a call of `function` returns, whose columns `relation`
    /// names.
    Call {
        /// The function called.
        function: &'a DB::Function,
        /// The relation the item is.
        relation: Relation,
    },
}

#[expect(clippy::expl_impl_clone_on_copy, reason = "derive would require DB: Clone")]
impl<DB: DatabaseLike> Clone for Factor<'_, DB> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<DB: DatabaseLike> Copy for Factor<'_, DB> {}

/// The body of a function, bound before it is read.
struct BodyPlan<'a, DB: DatabaseLike> {
    /// The address of the function.
    function: usize,
    /// The body, `None` for one the read does not follow.
    body: Option<Rc<Body>>,
    /// The references and calls a `RETURN` expression writes outside the
    /// queries it nests.
    site: Site<'a, DB>,
    /// What the domain checks guarantee of the values the body names.
    base: Guarantees<usize>,
}

/// A policy clause, bound before it is read over the rows of its table.
struct PolicyPlan<'a, DB: DatabaseLike> {
    /// The address of the clause.
    address: usize,
    /// The references and calls the clause writes outside the queries it
    /// nests, `None` for a clause whose names fail to bind.
    site: Option<Site<'a, DB>>,
    /// What the domain checks and the table's checks guarantee of the values
    /// the clause names.
    base: Guarantees<usize>,
}

/// A body the read follows.
#[expect(
    clippy::large_enum_variant,
    reason = "A body lives behind an `Rc`, which holds either variant once."
)]
enum Body {
    /// One query, as a statement.
    Query(Statement),
    /// The expression of `RETURN <expression>`.
    Return(Expr),
}

/// Where the expression being read was written, kept while another is read.
pub(super) struct Frame<'a, DB: DatabaseLike> {
    site: Option<Site<'a, DB>>,
    reading: Option<usize>,
    source: Source<'a, DB>,
    value: Option<usize>,
    current: Option<Guarantees<usize>>,
}

/// What a query returns, while it is read.
struct Read {
    rows: Rows,
    columns: Option<Vec<Evaluation>>,
    error: bool,
    /// What holds of the named values on the rows the query returns.
    guarantees: Guarantees<usize>,
}

impl Read {
    /// The query's answer. A query returning no row holds no value in any
    /// column.
    fn finish(self) -> QueryEvaluation {
        let Self { rows, columns, error, .. } = self;
        let columns = if rows.one || rows.many {
            columns
        } else {
            columns.map(|columns| {
                columns.iter().map(|column| Evaluation::impossible(column.ty().clone())).collect()
            })
        };
        QueryEvaluation::new(rows, columns, error)
    }
}

/// Hands every query a node nests to its context's planner.
struct Planner<'p, 'q, 'a, DB: DatabaseLike> {
    truth: &'p mut TruthContext<'a, DB>,
    context: &'p ExpressionContext<'q, 'a, DB>,
}

impl<DB: DatabaseLike> Visitor for Planner<'_, '_, '_, DB> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        self.truth.plan_query(self.context, query);
        ControlFlow::Continue(())
    }
}

/// Stops at the first query a node nests.
struct NestsQuery;

impl Visitor for NestsQuery {
    type Break = ();

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<()> {
        ControlFlow::Break(())
    }
}

impl<'a, DB: DatabaseLike> TruthContext<'a, DB> {
    /// Plans every query `root`, a node of `context`'s root, nests. A query
    /// whose names fail to bind stays unplanned, and reads as unknown.
    pub(super) fn plan<N: Visit + ?Sized>(
        &mut self,
        context: &ExpressionContext<'_, 'a, DB>,
        root: &N,
    ) {
        let _ = walk(root, &mut Planner { truth: self, context });
    }

    /// Plans the queries the `expressions`, written over the rows of
    /// `table`, nest, each in a context of its own reaching the caller's row.
    pub(super) fn plan_table<'e>(
        &mut self,
        table: &'a DB::Table,
        expressions: impl Iterator<Item = &'e Expr>,
    ) {
        let scope = ExpressionScope::Table(table);
        for expression in expressions {
            if walk(expression, &mut NestsQuery).is_continue() {
                continue;
            }
            let Ok(context) =
                ExpressionContext::for_expression(expression, &scope, self.database, self.case)
            else {
                continue;
            };
            self.roots += 1;
            let root = mem::replace(&mut self.root, self.roots);
            self.caller_scope = Some((self.roots, context.root_scope()));
            self.plan(&context, expression);
            self.root = root;
        }
    }

    /// Binds the names `query`, a node of `context`'s root, writes outside
    /// the queries it nests, and what each of its `FROM` items reads.
    fn plan_query(&mut self, context: &ExpressionContext<'_, 'a, DB>, query: &Query) {
        let address = ptr::from_ref(query).addr();
        if self.nested.queries.iter().any(|plan| plan.address == address) {
            return;
        }
        let start = self.tables.len();
        let mut factors = Vec::new();
        let mut scope = None;
        if let SetExpr::Select(select) = query.body.as_ref() {
            let items = select.from.iter().flat_map(|tree| {
                core::iter::once(&tree.relation).chain(tree.joins.iter().map(|join| &join.relation))
            });
            for factor in items {
                if scope.is_none() {
                    scope =
                        context.factor_relation(factor).map(|(slot, _)| (self.root, slot.scope));
                }
                match self.plan_factor(context, factor) {
                    Ok(Some(planned)) => factors.push((ptr::from_ref(factor).addr(), planned)),
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
        }
        let mut site = Site::empty();
        if self.bind_nodes(context, query, 1, &mut site).is_err() {
            return;
        }
        let reads = start..self.tables.len();
        self.nested.queries.push(QueryPlan { address, site, reads, factors, scope, active: false });
    }

    /// What the `FROM` item `factor` reads, `None` for an item the read does
    /// not follow, among them a sampled table, whose sampling arguments may
    /// raise.
    fn plan_factor(
        &mut self,
        context: &ExpressionContext<'_, 'a, DB>,
        factor: &TableFactor,
    ) -> Result<Option<Factor<'a, DB>>, LookupError> {
        if matches!(factor, TableFactor::Table { sample: Some(_), .. }) {
            return Ok(None);
        }
        let Some((slot, table)) = context.factor_relation(factor) else { return Ok(None) };
        let relation = Relation { root: self.root, slot };
        if let Some(table) = table {
            // A table of another catalog has privileges and policies the
            // read does not hold.
            let Some(table) = self.database.tables().find(|held| ptr::eq(*held, table)) else {
                return Ok(Some(Factor::Unheld));
            };
            let read = if slot.relation.constrained && !slot.relation.null_extended {
                Some(self.read_table(Some(relation), table, true)?)
            } else {
                None
            };
            return Ok(Some(Factor::Table { table, read }));
        }
        let Some(call) = context.relation_call(factor)? else { return Ok(None) };
        Ok(match call.resolution() {
            CallResolution::Selected(selected) => {
                Some(Factor::Call { function: selected.function(), relation })
            }
            _ => None,
        })
    }

    /// The fields that tell where the expression being read was written,
    /// taken out of the context.
    fn save(&mut self) -> Frame<'a, DB> {
        Frame {
            site: self.site.take(),
            reading: self.reading.take(),
            source: mem::replace(&mut self.source, Source::Site),
            value: self.value.take(),
            current: self.current.take(),
        }
    }

    /// Puts back the fields [`Self::save`] took.
    fn restore(&mut self, frame: Frame<'a, DB>) {
        self.site = frame.site;
        self.reading = frame.reading;
        self.source = frame.source;
        self.value = frame.value;
        self.current = frame.current;
    }

    /// Reads the names and calls `site` binds, under `current`, until
    /// [`Self::leave`] puts back the returned frame.
    fn enter(&mut self, site: Site<'a, DB>, current: Option<Guarantees<usize>>) -> Frame<'a, DB> {
        let frame = self.save();
        self.site = Some(site);
        self.current = current;
        frame
    }

    /// Puts back `frame`, answering the site [`Self::enter`] read.
    fn leave(&mut self, frame: Frame<'a, DB>) -> Site<'a, DB> {
        let site = self.site.take().unwrap_or_else(Site::empty);
        self.restore(frame);
        site
    }

    /// What `query`, nested in the expression being read, returns where the
    /// named values satisfy `outer`, unknown for a query the read did not
    /// plan or is reading.
    pub(super) fn answer(
        &mut self,
        query: &Query,
        outer: &Guarantees<usize>,
    ) -> Result<QueryEvaluation, LookupError> {
        let address = ptr::from_ref(query).addr();
        let role = self.nested.role_key();
        if let Some((.., answered)) = self
            .nested
            .answers
            .iter()
            .find(|(held, by, given, _)| *held == address && *by == role && given == outer)
        {
            return Ok(answered.clone());
        }
        let Some(index) =
            self.nested.queries.iter().position(|plan| plan.address == address && !plan.active)
        else {
            return Ok(QueryEvaluation::unknown());
        };
        let plan = &mut self.nested.queries[index];
        plan.active = true;
        let site = mem::replace(&mut plan.site, Site::empty());
        let frame = self.enter(site, None);
        let read = self.read_query(index, query, outer);
        let site = self.leave(frame);
        let plan = &mut self.nested.queries[index];
        plan.site = site;
        plan.active = false;
        let answered = read?.map_or_else(QueryEvaluation::unknown, Read::finish);
        self.nested.answers.push((address, role, outer.clone(), answered.clone()));
        Ok(answered)
    }

    /// What the planned query at `index`, `query`, returns under `outer`,
    /// `None` for a query written with a clause the read does not follow.
    fn read_query(
        &mut self,
        index: usize,
        query: &Query,
        outer: &Guarantees<usize>,
    ) -> Result<Option<Read>, LookupError> {
        if query.with.is_some()
            || query.fetch.is_some()
            || !query.locks.is_empty()
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return Ok(None);
        }
        let reads = self.nested.queries[index].reads.clone();
        let guarantees = self.seeded_here(outer.clone(), reads)?;
        self.reading = None;
        self.source = Source::Site;
        self.value = None;
        self.current = Some(guarantees.clone());
        let read = match query.body.as_ref() {
            SetExpr::Select(select) => {
                self.read_select(index, select, query.order_by.as_ref(), guarantees)?
            }
            SetExpr::Values(values) => self.read_values(values, guarantees)?,
            _ => None,
        };
        let Some(mut read) = read else { return Ok(None) };
        if let Some(order_by) = &query.order_by
            && !self.read_order_by(order_by, &mut read)?
        {
            return Ok(None);
        }
        if let Some(limit) = &query.limit_clause
            && !limited(limit, &mut read)
        {
            return Ok(None);
        }
        Ok(Some(read))
    }

    /// What `select`, the body of the planned query at `index` sorted by
    /// `order_by`, returns where the named values satisfy `scan`.
    ///
    /// PostgreSQL may test a condition on a row before the conditions of
    /// other joins drop it, so whether one raises is read under `scan`, which
    /// holds what the `FROM` items alone guarantee. Which rows remain and
    /// what they hold are read under what the conditions narrow.
    fn read_select(
        &mut self,
        index: usize,
        select: &Select,
        order_by: Option<&OrderBy>,
        mut scan: Guarantees<usize>,
    ) -> Result<Option<Read>, LookupError> {
        if !plain_select(select) {
            return Ok(None);
        }
        let mut error = false;
        // The named values `*` expands to, `None` once an item expands to
        // columns the read does not name.
        let mut outputs = Some(Vec::new());
        let mut reading =
            Reading { guarantees: &mut scan, error: &mut error, outputs: &mut outputs };
        let Some((mut rows, constraints)) = self.joined_rows(index, select, &mut reading)? else {
            return Ok(None);
        };
        let mut guarantees = scan.clone();
        for (constraint, kept) in constraints {
            match constraint {
                JoinConstraint::On(condition) => {
                    error |= self.evaluated(condition, &scan)?.may_raise();
                    if kept {
                        rows = filtered(rows, &self.evaluated(condition, &guarantees)?);
                        guarantees = self.narrowed(condition, &guarantees)?;
                    }
                }
                JoinConstraint::Using(..)
                | JoinConstraint::UsingAlias(..)
                | JoinConstraint::Natural => {
                    outputs = None;
                    if kept {
                        rows = Rows::new(true, rows.one || rows.many, rows.many);
                    }
                }
                JoinConstraint::None => {}
            }
        }
        if let Some(condition) = &select.selection {
            error |= self.evaluated(condition, &scan)?.may_raise();
            rows = filtered(rows, &self.evaluated(condition, &guarantees)?);
            guarantees = self.narrowed(condition, &guarantees)?;
        }
        if guarantees.is_impossible() {
            rows = Rows::NONE;
        }
        let keys = order_by.into_iter().flat_map(|order_by| {
            match &order_by.kind {
                OrderByKind::Expressions(keys) => keys.as_slice(),
                OrderByKind::All(_) => &[],
            }
        });
        let items = select.projection.iter().filter_map(|item| {
            match item {
                SelectItem::UnnamedExpr(expression)
                | SelectItem::ExprWithAlias { expr: expression, .. } => Some(expression),
                _ => None,
            }
        });
        let distinct = match &select.distinct {
            Some(Distinct::On(keys)) => keys.as_slice(),
            _ => &[],
        };
        let Some((aggregate, sets)) =
            self.query_calls(index, items.chain(distinct).chain(keys.map(|key| &key.expr)))?
        else {
            return Ok(None);
        };
        // Aggregating with no `GROUP BY` returns one row, rows or none, so
        // the conditions that filter the rows narrow nothing the row holds.
        if aggregate {
            rows = Rows::ONE;
            guarantees = scan;
        }
        self.current = Some(guarantees.clone());
        let mut read = Read { rows, columns: None, error, guarantees };
        self.read_projection(&select.projection, outputs.as_deref(), &mut read)?;
        if sets && (read.rows.one || read.rows.many) {
            read.rows = Rows::ANY;
        }
        match &select.distinct {
            None | Some(Distinct::All) => {}
            Some(Distinct::Distinct) => read.rows = merged(read.rows),
            Some(Distinct::On(keys)) => {
                for key in keys {
                    read.error |= self.evaluated(key, &read.guarantees)?.may_raise();
                }
                read.rows = merged(read.rows);
            }
        }
        Ok(Some(read))
    }

    /// The rows the `FROM` list of `select`, the planned query at `index`,
    /// joins before its join constraints filter them, with each join's
    /// constraint and whether its rows stay joined to the end, which a later
    /// right or full join undoes by null-extending them.
    fn joined_rows<'s>(
        &mut self,
        index: usize,
        select: &'s Select,
        reading: &mut Reading<'_>,
    ) -> Result<Option<Joined<'s>>, LookupError> {
        let mut rows = Rows::ONE;
        let mut constraints = Vec::new();
        for tree in &select.from {
            let Some(mut tree_rows) = self.factor_rows(index, &tree.relation, reading)? else {
                return Ok(None);
            };
            let extended = tree.joins.iter().rposition(|join| {
                matches!(
                    join.join_operator,
                    JoinOperator::Right(_)
                        | JoinOperator::RightOuter(_)
                        | JoinOperator::FullOuter(_)
                )
            });
            for (position, join) in tree.joins.iter().enumerate() {
                let (constraint, outer) = match &join.join_operator {
                    JoinOperator::Join(constraint)
                    | JoinOperator::Inner(constraint)
                    | JoinOperator::CrossJoin(constraint) => (constraint, false),
                    JoinOperator::Left(constraint)
                    | JoinOperator::LeftOuter(constraint)
                    | JoinOperator::Right(constraint)
                    | JoinOperator::RightOuter(constraint)
                    | JoinOperator::FullOuter(constraint) => (constraint, true),
                    _ => return Ok(None),
                };
                let Some(joined) = self.factor_rows(index, &join.relation, reading)? else {
                    return Ok(None);
                };
                if outer {
                    tree_rows = Rows::ANY;
                    *reading.outputs = None;
                } else {
                    tree_rows = product(tree_rows, joined);
                }
                let kept = !outer && extended.is_none_or(|last| position > last);
                constraints.push((constraint, kept));
            }
            rows = product(rows, tree_rows);
        }
        Ok(Some((rows, constraints)))
    }

    /// Whether `expressions`, the select list, `DISTINCT ON` keys and sort
    /// keys of the planned query at `index`, call an aggregate outside a
    /// window and whether they call a function returning a set, outside the
    /// queries they nest.
    /// `None` where a call runs no function the read knows, which may be an
    /// aggregate, or where an aggregate may belong to another query than
    /// the one it is written in, which PostgreSQL decides by the level of
    /// the columns it reads.
    fn query_calls<'e>(
        &mut self,
        index: usize,
        expressions: impl Iterator<Item = &'e Expr>,
    ) -> Result<Option<(bool, bool)>, LookupError> {
        let scope = self.nested.queries[index].scope;
        let (mut aggregate, mut sets, mut known) = (false, false, true);
        for expression in expressions {
            let mut visitor = Calls {
                queries: Vec::new(),
                each: |nested: Option<usize>, call: &Function| -> Result<(), LookupError> {
                    if !known {
                        return Ok(());
                    }
                    // An expression written as a call, such as `NULLIF`, runs
                    // no function and is neither an aggregate nor a set.
                    if expression_form(&call.name).is_some() {
                        return Ok(());
                    }
                    if let Some(address) = nested {
                        known = self.stays_nested(address, call)?;
                        return Ok(());
                    }
                    let callees = self.callees(call)?;
                    known = !callees.is_empty();
                    for callee in callees {
                        let aggregates =
                            call.over.is_none() && callee.kind == CalleeKind::Aggregate;
                        if aggregates && known {
                            known = self.reads_locally(call, scope)?;
                        }
                        aggregate |= aggregates;
                        sets |= callee.returns_set;
                    }
                    Ok(())
                },
            };
            if let ControlFlow::Break(error) = walk(expression, &mut visitor) {
                return Err(error);
            }
        }
        Ok(known.then_some((aggregate, sets)))
    }

    /// Whether `call`, written in the planned query at `address` nested in
    /// the query being read, runs no aggregate of an enclosing query.
    /// PostgreSQL makes an aggregate reading only columns of enclosing
    /// queries an aggregate of the nearest of them.
    fn stays_nested(&mut self, address: usize, call: &Function) -> Result<bool, LookupError> {
        if call.over.is_some() {
            return Ok(true);
        }
        let Some(index) =
            self.nested.queries.iter().position(|plan| plan.address == address && !plan.active)
        else {
            return Ok(false);
        };
        let scope = self.nested.queries[index].scope;
        let site = mem::replace(&mut self.nested.queries[index].site, Site::empty());
        let frame = self.enter(site, None);
        let stays = self.callees(call).and_then(|callees| {
            let plain = !callees.is_empty()
                && callees.iter().all(|callee| callee.kind != CalleeKind::Aggregate);
            if plain { Ok(true) } else { self.reads_locally(call, scope) }
        });
        self.nested.queries[index].site = self.leave(frame);
        stays
    }

    /// Whether every column the arguments, sort keys and filter of `call`
    /// read is one of the `FROM` items binding in `scope`, a context and a
    /// scope in it, so that the aggregate `call` belongs to the query of
    /// those items.
    fn reads_locally(
        &mut self,
        call: &Function,
        scope: Option<(usize, usize)>,
    ) -> Result<bool, LookupError> {
        let walked = walk_expressions(call, |node| {
            let local = match node {
                Expr::Subquery(_)
                | Expr::Exists { .. }
                | Expr::In { target: InTarget::Subquery(_), .. } => false,
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                    match self.named_by(node) {
                        Ok(named) => {
                            named.is_some_and(|named| local_key(&self.named[named].key, scope))
                        }
                        Err(error) => return ControlFlow::Break(Err(error)),
                    }
                }
                _ => true,
            };
            if local { ControlFlow::Continue(()) } else { ControlFlow::Break(Ok(())) }
        });
        match walked {
            ControlFlow::Continue(()) => Ok(true),
            ControlFlow::Break(Ok(())) => Ok(false),
            ControlFlow::Break(Err(error)) => Err(error),
        }
    }

    /// Reads the select list `items` into `read`, a `*` expanding to the
    /// named values `outputs`.
    fn read_projection(
        &mut self,
        items: &[SelectItem],
        outputs: Option<&[usize]>,
        read: &mut Read,
    ) -> Result<(), LookupError> {
        let mut columns = Some(Vec::with_capacity(items.len()));
        for item in items {
            match item {
                SelectItem::UnnamedExpr(expression)
                | SelectItem::ExprWithAlias { expr: expression, .. } => {
                    let evaluation = self.evaluated(expression, &read.guarantees)?;
                    read.error |= evaluation.may_raise();
                    if let Some(columns) = &mut columns {
                        columns.push(evaluation.with_error(false));
                    }
                }
                SelectItem::Wildcard(options) if plain_wildcard(options) => {
                    match (outputs, &mut columns) {
                        (Some(outputs), Some(columns)) => {
                            columns.extend(
                                outputs
                                    .iter()
                                    .map(|&named| self.held_value(named, &read.guarantees)),
                            );
                        }
                        _ => columns = None,
                    }
                }
                _ => columns = None,
            }
        }
        read.columns = columns;
        Ok(())
    }

    /// What the named value `named` holds under `guarantees`.
    fn held_value(&self, named: usize, guarantees: &Guarantees<usize>) -> Evaluation {
        let held = &self.named[named].evaluation;
        guarantees
            .reference(&named)
            .map_or_else(|| held.clone(), |guarantee| held.meet(guarantee))
            .with_error(false)
    }

    /// The rows the `FROM` item `factor` of the planned query at `index`
    /// returns, narrowing what `reading` holds of the names its columns
    /// bind, `None` for an item the read does not follow.
    fn factor_rows(
        &mut self,
        index: usize,
        factor: &TableFactor,
        reading: &mut Reading<'_>,
    ) -> Result<Option<Rows>, LookupError> {
        let address = ptr::from_ref(factor).addr();
        let Some(planned) = self.nested.queries[index]
            .factors
            .iter()
            .find(|(held, _)| *held == address)
            .map(|(_, planned)| *planned)
        else {
            return Ok(None);
        };
        match planned {
            Factor::Table { table, read } => {
                *reading.error |= self.table_read_raises(table)?;
                match (read, reading.outputs.as_mut()) {
                    (Some(read), Some(outputs)) => {
                        for column in table.columns(self.database)? {
                            outputs.extend(self.column_named(read, column));
                        }
                    }
                    _ => *reading.outputs = None,
                }
                Ok(Some(Rows::ANY))
            }
            Factor::Unheld => {
                *reading.error = true;
                *reading.outputs = None;
                Ok(Some(Rows::ANY))
            }
            Factor::Call { function, relation } => {
                self.call_rows(factor, function, relation, reading)
            }
        }
    }

    /// Whether scanning `table` may raise for the role the read runs as,
    /// through a missing `SELECT` privilege or a policy clause that may
    /// raise on a row it tests. The caller's read may raise where the read
    /// of any acting role of the assumed deployment may. SQLite checks no
    /// privilege and no policy.
    fn table_read_raises(&mut self, table: &'a DB::Table) -> Result<bool, LookupError> {
        if self.engine == Engine::Sqlite {
            return Ok(false);
        }
        let Reader::Caller = self.nested.reader else {
            return self.read_raises_as(table, self.nested.reader);
        };
        let Some(deployment) = self.deployment else {
            return self.read_raises_as(table, Reader::Unknown);
        };
        let database = self.database;
        for name in deployment.acting_roles() {
            let reader = database.role(name).map_or(Reader::Unknown, Reader::Role);
            if self.read_raises_as(table, reader)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// [`Self::table_read_raises`] for the read running as `reader`, a
    /// known role or an unknown one, which needs `PUBLIC` to hold `SELECT`
    /// and every policy clause a `SELECT` may test to be safe.
    fn read_raises_as(
        &mut self,
        table: &'a DB::Table,
        reader: Reader<'a, DB::Role>,
    ) -> Result<bool, LookupError> {
        let database = self.database;
        let privileges = match reader {
            Reader::Role(role) => table.privileges(role, database)?,
            Reader::Caller | Reader::Unknown => table.privileges(ActingRole::Public, database)?,
        };
        if !privileges.select.on_table() {
            return Ok(true);
        }
        let Some(clauses) = self.select_clauses(table, reader) else { return Ok(true) };
        let caller = mem::replace(&mut self.nested.reader, reader);
        let mut raises = Ok(false);
        for clause in clauses {
            raises = self.policy_raises(table, clause);
            if !matches!(raises, Ok(false)) {
                break;
            }
        }
        self.nested.reader = caller;
        raises
    }

    /// The policy clauses a `SELECT` running as `reader` tests on the rows
    /// of `table` it scans, those of every role for a role the read does not
    /// know. `None` where PostgreSQL refuses the read or the policies do not
    /// settle.
    fn select_clauses(
        &self,
        table: &'a DB::Table,
        reader: Reader<'a, DB::Role>,
    ) -> Option<Vec<&'a Expr>> {
        let database = self.database;
        if let Reader::Role(role) = reader {
            return match table.row_security(RowStatement::Select, role, database).ok()? {
                RowSecurity::Exempt(_) => Some(Vec::new()),
                RowSecurity::Gated(gates) => {
                    Some(
                        gates
                            .iter()
                            .flat_map(|gate| gate.permissive().iter().chain(gate.restrictive()))
                            .map(|term| term.expression().expression())
                            .collect(),
                    )
                }
                RowSecurity::InfiniteRecursion { .. } => None,
            };
        }
        if !table.has_row_level_security(database).ok()? {
            return Some(Vec::new());
        }
        let mut clauses = Vec::new();
        for policy in table.policies(database).ok()? {
            let analysis = policy.clause_analysis(database).ok()?;
            let tests =
                |clause| analysis.targets(clause).any(|target| target == PolicyTarget::Select);
            let (using, check) = (tests(PolicyClause::Using), tests(PolicyClause::WithCheck));
            let (using_clause, check_clause) = analysis.into_clauses();
            for (tested, clause) in [(using, using_clause), (check, check_clause)] {
                if tested && let Some(clause) = clause {
                    clauses.push(clause.expression());
                }
            }
        }
        Some(clauses)
    }

    /// Whether the policy clause `clause` of `table` may raise on a row it
    /// tests, for the role the read runs as. A clause read up the stack, or
    /// past the depth bodies and clauses are followed to, may.
    fn policy_raises(
        &mut self,
        table: &'a DB::Table,
        clause: &'a Expr,
    ) -> Result<bool, LookupError> {
        let address = ptr::from_ref(clause).addr();
        let role = self.nested.role_key();
        if let Some(&(.., raises)) =
            self.nested.raising.iter().find(|(held, by, _)| *held == address && *by == role)
        {
            return Ok(raises);
        }
        if self.nested.running.contains(&address) || self.nested.running.len() >= BODY_DEPTH {
            return Ok(true);
        }
        let frame = self.save();
        let index = self.policy_plan(table, clause);
        self.restore(frame);
        let index = index?;
        let Some(site) = self.nested.policies[index].site.take() else { return Ok(true) };
        let guarantees = self.nested.policies[index].base.clone();
        self.nested.running.push(address);
        let frame = self.enter(site, Some(guarantees.clone()));
        let evaluation = self.evaluated(clause, &guarantees);
        self.nested.policies[index].site = Some(self.leave(frame));
        self.nested.running.pop();
        let raises = evaluation?.may_raise();
        self.nested.raising.push((address, role, raises));
        Ok(raises)
    }

    /// The plan of the policy clause `clause` over the rows of `table`,
    /// binding its names on the first read.
    fn policy_plan(
        &mut self,
        table: &'a DB::Table,
        clause: &'a Expr,
    ) -> Result<usize, LookupError> {
        let address = ptr::from_ref(clause).addr();
        if let Some(index) = self.nested.policies.iter().position(|plan| plan.address == address) {
            return Ok(index);
        }
        let database = self.database;
        let scope = ExpressionScope::Table(table);
        self.roots += 1;
        let root = mem::replace(&mut self.root, self.roots);
        let (named, reads) = (self.named.len(), self.tables.len());
        let mut site = Site::empty();
        let bound =
            ExpressionContext::for_expression(clause, &scope, database, IdentifierCase::AsWritten)
                .and_then(|context| {
                    self.bind_nodes(&context, clause, 0, &mut site)?;
                    self.plan(&context, clause);
                    Ok(())
                })
                .is_ok();
        self.root = root;
        let base = if bound {
            let base = self.domain_here(named..self.named.len())?;
            self.seeded_here(base, reads..self.tables.len())?
        } else {
            Guarantees::new()
        };
        self.nested.policies.push(PolicyPlan { address, site: bound.then_some(site), base });
        Ok(self.nested.policies.len() - 1)
    }

    /// The rows the call of `function` the `FROM` item `factor` writes
    /// returns, as [`Self::factor_rows`] reads them.
    fn call_rows(
        &mut self,
        factor: &TableFactor,
        function: &'a DB::Function,
        relation: Relation,
        reading: &mut Reading<'_>,
    ) -> Result<Option<Rows>, LookupError> {
        let Some(arguments) = factor_arguments(factor) else { return Ok(None) };
        let mut evaluations = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let evaluation = self.evaluated(argument, reading.guarantees)?;
            *reading.error |= evaluation.may_raise();
            evaluations.push(evaluation);
        }
        let Some(answered) = self.body_answer(function, &evaluations)? else { return Ok(None) };
        *reading.error |= answered.error;
        let null_input = self.engine.callee(function, self.database, &self.enums)?.null_input;
        let skipped = |test: fn(&Evaluation) -> bool| {
            evaluations
                .iter()
                .enumerate()
                .any(|(position, argument)| strict_at(&null_input, position) && test(argument))
        };
        let (always, maybe) =
            (skipped(Evaluation::is_always_null), skipped(Evaluation::may_be_null));
        // A function returning one value returns one row, `NULL` where the
        // body returns none.
        let set = function.returns_set();
        let rows = match (set, always, maybe) {
            (false, ..) => Rows::ONE,
            (true, true, _) => Rows::NONE,
            (true, false, true) => Rows::new(true, answered.rows.one, answered.rows.many),
            (true, false, false) => answered.rows,
        };
        let Some(columns) = answered.columns else {
            *reading.outputs = None;
            return Ok(Some(rows));
        };
        // PostgreSQL converts each column the body returns to the type the
        // function declares, which may raise.
        let declared = self.declared_outputs(function);
        if declared.len() != columns.len() {
            *reading.error = true;
        }
        let nulled =
            relation.slot.relation.null_extended || (!set && (maybe || answered.rows.none));
        for (ordinal, column) in columns.iter().enumerate() {
            let ty = declared.get(ordinal).unwrap_or_else(|| column.ty());
            *reading.error |= !passes_safely(column, ty);
            let unbound =
                Evaluation::new(ty.clone(), true, false, Values::Unlisted(Unlisted::any()));
            let named = self.named(Key::Output { relation, ordinal }, unbound, None, None);
            let value = carried(column, self.named[named].evaluation.ty());
            let value = if nulled { value.with_null(true) } else { value };
            reading.guarantees.restrict(Subject::Reference(named), value);
            if let Some(outputs) = reading.outputs.as_mut() {
                outputs.push(named);
            }
        }
        Ok(Some(rows))
    }

    /// The types `function` declares the columns of its result of, one for a
    /// function returning a value or a set of values.
    fn declared_outputs(&self, function: &'a DB::Function) -> Vec<ScalarType> {
        let database = self.database;
        let declared = |type_name: &str| {
            self.engine
                .operand(&declared_type_named(database, type_name), database, &self.enums)
                .ty()
                .clone()
        };
        let columns: Vec<ScalarType> = function
            .returned_columns(database)
            .map(|(_, data_type)| declared(&data_type.name(database)))
            .collect();
        if !columns.is_empty() {
            return columns;
        }
        function
            .return_type_name(database)
            .map(|name| vec![declared(name.strip_prefix("SETOF ").unwrap_or(&name))])
            .unwrap_or_default()
    }

    /// What `values` returns, its expressions read under `guarantees`.
    fn read_values(
        &mut self,
        values: &ValueRows,
        guarantees: Guarantees<usize>,
    ) -> Result<Option<Read>, LookupError> {
        // A `VALUES` list writes at least one row.
        let many = values.rows.len() > 1;
        let rows = Rows::new(false, !many, many);
        let mut read = Read { rows, columns: None, error: false, guarantees };
        for row in &values.rows {
            let mut evaluated = Vec::with_capacity(row.content.len());
            for expression in &row.content {
                let evaluation = self.evaluated(expression, &read.guarantees)?;
                read.error |= evaluation.may_raise();
                evaluated.push(evaluation.with_error(false));
            }
            read.columns = Some(match read.columns.take() {
                None => evaluated,
                Some(held) if held.len() == evaluated.len() => {
                    held.iter().zip(&evaluated).map(|(held, row)| held.join(row)).collect()
                }
                Some(_) => return Ok(None),
            });
        }
        Ok(Some(read))
    }

    /// Reads the keys of `order_by` into `read`, answering whether the read
    /// follows the clause.
    fn read_order_by(&mut self, order_by: &OrderBy, read: &mut Read) -> Result<bool, LookupError> {
        if order_by.interpolate.is_some() {
            return Ok(false);
        }
        if let OrderByKind::Expressions(keys) = &order_by.kind {
            for key in keys {
                if key.with_fill.is_some() {
                    return Ok(false);
                }
                read.error |= self.evaluated(&key.expr, &read.guarantees)?.may_raise();
            }
        }
        Ok(true)
    }

    /// What the body of `function` returns with its parameters bound to
    /// `arguments`. `None` for a body the read does not follow, one not
    /// written in SQL, one whose function is being read up the stack, or one
    /// past the depth bodies are followed to.
    fn body_answer(
        &mut self,
        function: &'a DB::Function,
        arguments: &[Evaluation],
    ) -> Result<Option<QueryEvaluation>, LookupError> {
        let address = ptr::from_ref(function).addr();
        if self.engine != Engine::PostgreSql
            || function.stored_language().is_some_and(|language| language != "sql")
            || self.nested.running.contains(&address)
            || self.nested.running.len() >= BODY_DEPTH
        {
            return Ok(None);
        }
        let role = self.nested.role_key();
        if let Some((.., answered)) = self.nested.results.iter().find(|(held, by, given, _)| {
            *held == address && *by == role && given.as_slice() == arguments
        }) {
            return Ok(answered.clone());
        }
        self.nested.running.push(address);
        let frame = self.save();
        let caller = self.nested.reader;
        // A `SECURITY DEFINER` body runs as the function's owner.
        if function.security_mode() == FunctionSecurity::Definer {
            let database = self.database;
            self.nested.reader = function
                .owner(database)
                .ok()
                .flatten()
                .and_then(|owner| database.role(owner))
                .map_or(Reader::Unknown, Reader::Role);
        }
        let answered = self.read_body(function, arguments);
        self.nested.reader = caller;
        self.restore(frame);
        self.nested.running.pop();
        let answered = answered?;
        self.nested.results.push((address, role, arguments.to_vec(), answered.clone()));
        Ok(answered)
    }

    /// [`Self::body_answer`] once the body is known to be followed.
    fn read_body(
        &mut self,
        function: &'a DB::Function,
        arguments: &[Evaluation],
    ) -> Result<Option<QueryEvaluation>, LookupError> {
        let index = self.body_plan(function)?;
        let Some(body) = self.nested.bodies[index].body.as_ref().map(Rc::clone) else {
            return Ok(None);
        };
        let mut guarantees = self.nested.bodies[index].base.clone();
        let raises = self.bind_parameters(function, arguments, &mut guarantees)
            || self.returns_checked_domain(function);
        let mut answered = match body.as_ref() {
            Body::Query(statement) => {
                let Statement::Query(query) = statement else { return Ok(None) };
                self.answer(query, &guarantees)?
            }
            Body::Return(expression) => {
                let site = mem::replace(&mut self.nested.bodies[index].site, Site::empty());
                let frame = self.enter(site, Some(guarantees.clone()));
                let evaluation = self.evaluated(expression, &guarantees);
                self.nested.bodies[index].site = self.leave(frame);
                let evaluation = evaluation?;
                let error = evaluation.may_raise();
                QueryEvaluation::new(Rows::ONE, Some(vec![evaluation.with_error(false)]), error)
            }
        };
        answered.error |= raises;
        Ok(Some(answered))
    }

    /// Restricts in `guarantees` each input parameter of `function` to the
    /// argument at its position, answering whether passing them may raise,
    /// through the checks of a domain, the input function reading an untyped
    /// literal, or the default of a parameter the call leaves out.
    fn bind_parameters(
        &mut self,
        function: &'a DB::Function,
        arguments: &[Evaluation],
        guarantees: &mut Guarantees<usize>,
    ) -> bool {
        let database = self.database;
        let address = ptr::from_ref(function).addr();
        let inputs = function
            .argument_modes()
            .zip(function.argument_type_names(database))
            .filter(|(mode, _)| is_input(mode))
            .map(|(_, type_name)| type_name);
        let mut raises = false;
        let mut bound = 0;
        for (ordinal, (argument, type_name)) in arguments.iter().zip(inputs).enumerate() {
            raises |= self.checked_type(&type_name);
            let declared = self.engine.operand(
                &declared_type_named(database, &type_name),
                database,
                &self.enums,
            );
            let key = Key::Parameter { function: address, ordinal };
            let named = self.named(key, declared, Some(type_name.into_owned()), None);
            let ty = self.named[named].evaluation.ty();
            raises |= !passes_safely(argument, ty);
            let value = carried(argument, ty);
            guarantees.restrict(Subject::Reference(named), value);
            bound += 1;
        }
        raises || bound < function.argument_modes().filter(is_input).count()
    }

    /// Whether `function` returns a domain whose checks its result passes.
    fn returns_checked_domain(&self, function: &DB::Function) -> bool {
        function
            .return_type_name(self.database)
            .is_some_and(|name| self.checked_type(name.strip_prefix("SETOF ").unwrap_or(&name)))
    }

    /// Whether the type `type_name` names a domain, or an array of one, with
    /// checks.
    fn checked_type(&self, type_name: &str) -> bool {
        PgType::parse(type_name)
            .and_then(|written| self.type_domain(&written))
            .is_some_and(|(domain, _)| super::domain_checks(self.database, domain))
    }

    /// The plan of the body of `function`, binding its names on the first
    /// read.
    fn body_plan(&mut self, function: &'a DB::Function) -> Result<usize, LookupError> {
        let address = ptr::from_ref(function).addr();
        if let Some(index) = self.nested.bodies.iter().position(|plan| plan.function == address) {
            return Ok(index);
        }
        let database = self.database;
        let body = match sql_query_body(database, function) {
            Ok(Some(query)) => Some(Body::Query(Statement::Query(Box::new(query.into_owned())))),
            Ok(None) => {
                body_expression(function, database)
                    .ok()
                    .flatten()
                    .map(|expression| Body::Return(expression.into_owned()))
            }
            Err(_) => None,
        }
        .map(Rc::new);
        self.roots += 1;
        let root = mem::replace(&mut self.root, self.roots);
        let start = self.named.len();
        let mut site = Site::empty();
        let scope = ExpressionScope::FunctionBody(function);
        let case = IdentifierCase::AsWritten;
        let planned = match body.as_deref() {
            Some(Body::Query(statement)) => {
                ExpressionContext::for_statement(statement, &scope, database, case)
                    .map(|context| self.plan(&context, statement))
                    .is_ok()
            }
            Some(Body::Return(expression)) => {
                ExpressionContext::for_expression(expression, &scope, database, case)
                    .and_then(|context| {
                        self.bind_nodes(&context, expression, 0, &mut site)?;
                        self.plan(&context, expression);
                        Ok(())
                    })
                    .is_ok()
            }
            None => false,
        };
        self.root = root;
        let base =
            if planned { self.domain_here(start..self.named.len())? } else { Guarantees::new() };
        let body = body.filter(|_| planned);
        self.nested.bodies.push(BodyPlan { function: address, body, site, base });
        Ok(self.nested.bodies.len() - 1)
    }

    /// What the body of the function `callee` stands for returns as the
    /// value of a call passing `arguments`, `None` for a body the read does
    /// not follow.
    pub(super) fn body_value(
        &mut self,
        callee: &sqlparser_expr_eval::Callee,
        arguments: &[Evaluation],
    ) -> Result<Option<Evaluation>, LookupError> {
        let Some(function) = self
            .nested
            .functions
            .iter()
            .copied()
            .find(|function| ptr::from_ref(*function).addr() == callee.token)
        else {
            return Ok(None);
        };
        let Some(answered) = self.body_answer(function, arguments)? else { return Ok(None) };
        let returns = &callee.returns;
        let declared = self.declared_outputs(function);
        let set = function.returns_set();
        let mut error = answered.error;
        let value = match (answered.columns.as_deref(), declared.as_slice()) {
            // A function returning a set returns none of its values, and
            // one returning a value returns `NULL`.
            _ if !(answered.rows.one || answered.rows.many) => {
                if set {
                    Evaluation::impossible(returns.clone())
                } else {
                    Evaluation::null(returns.clone())
                }
            }
            (Some([column]), [ty]) => {
                error |= !passes_safely(column, ty);
                let value = carried(column, returns);
                let null = value.may_be_null() || (!set && answered.rows.none);
                value.with_null(null)
            }
            (columns, _) => {
                // Columns the read cannot line up with the declared result
                // convert in ways it does not know.
                error |= columns.is_none_or(|columns| columns.len() != declared.len());
                Evaluation::new(returns.clone(), true, false, Values::Unlisted(Unlisted::any()))
            }
        };
        Ok(Some(value.with_error(error)))
    }

    /// What `ARRAY(query)` evaluates to, `None` for another expression or a
    /// query whose single column the read does not know.
    pub(super) fn array_of_query(
        &mut self,
        expression: &Expr,
    ) -> Result<Option<Evaluation>, LookupError> {
        let Expr::Function(Function {
            name,
            args: FunctionArguments::Subquery(query),
            over: None,
            filter: None,
            within_group,
            ..
        }) = expression
        else {
            return Ok(None);
        };
        if !within_group.is_empty() || !is_array_name(name) {
            return Ok(None);
        }
        let current = self.current.take().unwrap_or_default();
        let answered = self.answer(query, &current);
        self.current = Some(current);
        let answered = answered?;
        let Some([column]) = answered.columns.as_deref() else { return Ok(None) };
        // Rows holding arrays build a multidimensional array, which
        // PostgreSQL refuses when their dimensions differ.
        let error = answered.error || column.ty().is_array();
        let ty = ScalarType::Array(ArrayElement::of(column.ty()));
        Ok(Some(Evaluation::new(ty, false, error, Values::Unlisted(Unlisted::any()))))
    }

    /// [`Self::seeded`] under the engine's dialect, with no assumption.
    fn seeded_here(
        &mut self,
        guarantees: Guarantees<usize>,
        reads: Range<usize>,
    ) -> Result<Guarantees<usize>, LookupError> {
        if reads.is_empty() {
            return Ok(guarantees);
        }
        match self.engine {
            Engine::PostgreSql => {
                self.seeded(
                    &PostgreSqlDialect {},
                    &PostgreSqlOptions::default(),
                    guarantees,
                    reads,
                    &[],
                )
            }
            Engine::Sqlite | Engine::Other => {
                self.seeded(&GenericDialect {}, &(), guarantees, reads, &[])
            }
        }
    }

    /// [`Self::domain_guarantees_over`] under the engine's dialect.
    fn domain_here(&mut self, named: Range<usize>) -> Result<Guarantees<usize>, LookupError> {
        match self.engine {
            Engine::PostgreSql => {
                self.domain_guarantees_over(
                    &PostgreSqlDialect {},
                    &PostgreSqlOptions::default(),
                    named,
                )
            }
            Engine::Sqlite | Engine::Other => {
                self.domain_guarantees_over(&GenericDialect {}, &(), named)
            }
        }
    }

    /// What `expression` evaluates to under `guarantees` and the engine's
    /// dialect.
    fn evaluated(
        &mut self,
        expression: &Expr,
        guarantees: &Guarantees<usize>,
    ) -> Result<Evaluation, LookupError> {
        match self.engine {
            Engine::PostgreSql => {
                evaluate(
                    expression,
                    &PostgreSqlDialect {},
                    &PostgreSqlOptions::default(),
                    self,
                    guarantees,
                )
            }
            Engine::Sqlite | Engine::Other => {
                evaluate(expression, &GenericDialect {}, &(), self, guarantees)
            }
        }
    }

    /// `guarantees` narrowed to the rows `condition` is true on, under the
    /// engine's dialect.
    fn narrowed(
        &mut self,
        condition: &Expr,
        guarantees: &Guarantees<usize>,
    ) -> Result<Guarantees<usize>, LookupError> {
        match self.engine {
            Engine::PostgreSql => {
                implied(
                    condition,
                    Outcomes::TRUE,
                    &PostgreSqlDialect {},
                    &PostgreSqlOptions::default(),
                    self,
                    guarantees,
                )
            }
            Engine::Sqlite | Engine::Other => {
                implied(condition, Outcomes::TRUE, &GenericDialect {}, &(), self, guarantees)
            }
        }
    }
}

/// The rows a `FROM` list joins, with each join's constraint and whether its
/// rows stay joined to the end.
type Joined<'s> = (Rows, Vec<(&'s JoinConstraint, bool)>);

/// What a `FROM` item narrows while it is read, the guarantees, whether
/// reading it may raise, and the named values `*` expands to.
struct Reading<'r> {
    guarantees: &'r mut Guarantees<usize>,
    error: &'r mut bool,
    outputs: &'r mut Option<Vec<usize>>,
}

/// Whether `select` is written with no clause beside its select list,
/// `FROM`, `WHERE` and `DISTINCT` the read follows.
fn plain_select(select: &Select) -> bool {
    select.optimizer_hints.is_empty()
        && select.select_modifiers.is_none()
        && select.top.is_none()
        && select.exclude.is_none()
        && select.into.is_none()
        && select.lateral_views.is_empty()
        && select.prewhere.is_none()
        && select.connect_by.is_empty()
        && matches!(&select.group_by, GroupByExpr::Expressions(keys, modifiers)
            if keys.is_empty() && modifiers.is_empty())
        && select.cluster_by.is_empty()
        && select.distribute_by.is_empty()
        && select.sort_by.is_empty()
        && select.having.is_none()
        && select.named_window.is_empty()
        && select.qualify.is_none()
        && select.value_table_mode.is_none()
        && matches!(select.flavor, SelectFlavor::Standard)
}

/// Whether `options` leave a `*` expanding to every column.
fn plain_wildcard(options: &WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_none()
        && options.opt_exclude.is_none()
        && options.opt_except.is_none()
        && options.opt_replace.is_none()
        && options.opt_rename.is_none()
        && options.opt_alias.is_none()
}

/// The arguments the call `factor` writes, each passed by position, `None`
/// for an item that is no such call or that adds an ordinality column.
fn factor_arguments(factor: &TableFactor) -> Option<Vec<&Expr>> {
    let listed = match factor {
        TableFactor::Table { args: Some(args), with_ordinality: false, .. }
            if args.settings.is_none() =>
        {
            &args.args
        }
        TableFactor::Function { args, .. } => args,
        _ => return None,
    };
    listed
        .iter()
        .map(|argument| {
            match argument {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expression)) => Some(expression),
                _ => None,
            }
        })
        .collect()
}

/// Whether `call` passes an argument by name.
pub(super) fn writes_named_argument(call: &Function) -> bool {
    let FunctionArguments::List(list) = &call.args else { return false };
    list.args.iter().any(|argument| !matches!(argument, FunctionArg::Unnamed(_)))
}

/// Whether `name` is the unquoted `ARRAY`.
fn is_array_name(name: &ObjectName) -> bool {
    matches!(name.0.as_slice(), [ObjectNamePart::Identifier(ident)]
        if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("array"))
}

/// Whether a `NULL` argument at `position` skips a call of a function
/// reading arguments as `null_input` says.
fn strict_at(null_input: &NullInput, position: usize) -> bool {
    match null_input {
        NullInput::Strict => true,
        NullInput::CalledOnNull(positions) => positions.contains(&position),
    }
}

/// `value` as a value of `ty`, its values carried where `ty` holds every
/// one of them alike, and only whether it may be `NULL` otherwise.
fn carried(value: &Evaluation, ty: &ScalarType) -> Evaluation {
    let alike = match (value.ty(), ty) {
        (ScalarType::Integer(from), ScalarType::Integer(to)) => from <= to,
        (ScalarType::Text(_), ScalarType::Text(_)) => true,
        (from, to) => from == to,
    };
    if alike {
        return value.clone().with_type(ty.clone()).with_error(false);
    }
    let values =
        if value.values().is_empty() { Values::none() } else { Values::Unlisted(Unlisted::any()) };
    Evaluation::new(ty.clone(), value.may_be_null(), false, values)
}

/// Hands `each` every call a node makes, with the address of the innermost
/// query nested in the node that holds the call, `None` for a call outside
/// them, until `each` fails.
struct Calls<F> {
    queries: Vec<usize>,
    each: F,
}

impl<F: FnMut(Option<usize>, &Function) -> Result<(), LookupError>> Visitor for Calls<F> {
    type Break = LookupError;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.queries.push(ptr::from_ref(query).addr());
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.queries.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        let Expr::Function(call) = expression else { return ControlFlow::Continue(()) };
        match (self.each)(self.queries.last().copied(), call) {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
}

/// Whether the key `key` names a value the query whose `FROM` items bind
/// in `scope` reads as its own, a column of one of those items or a value
/// the body around it binds.
fn local_key(key: &Key, scope: Option<(usize, usize)>) -> bool {
    match key {
        Key::Column { relation: Some(relation), .. } | Key::Output { relation, .. } => {
            scope == Some((relation.root, relation.slot.scope))
        }
        Key::Parameter { .. } | Key::Placeholder(_) | Key::Variable { .. } => true,
        Key::Built(inner) => local_key(inner, scope),
        _ => false,
    }
}

/// Whether converting `value` to `ty` never raises, which an untyped
/// literal passes only to text or to an enum naming each label it holds.
fn passes_safely(value: &Evaluation, ty: &ScalarType) -> bool {
    if value.is_always_null() {
        return true;
    }
    match (value.ty(), ty, value.values()) {
        (ScalarType::UnknownLiteral, ScalarType::Enum(labels), Values::Listed(listed)) => {
            listed.iter().all(
                |label| matches!(label, PgValue::Text(label) if labels.position(label).is_some()),
            )
        }
        (from, ty, _) => coerces_safely(from, ty),
    }
}

/// Whether converting a value of `from` to `ty`, as PostgreSQL converts the
/// result of a SQL function to its declared type, never raises.
fn coerces_safely(from: &ScalarType, ty: &ScalarType) -> bool {
    match (from, ty) {
        (ScalarType::Integer(from), ScalarType::Integer(to)) => from <= to,
        (ScalarType::Text(_) | ScalarType::UnknownLiteral, ScalarType::Text(_))
        | (ScalarType::Integer(_), ScalarType::Numeric | ScalarType::Float)
        | (_, ScalarType::Unknown) => true,
        (from, to) => from == to,
    }
}

/// The rows of every pair of a row of `left` and one of `right`.
fn product(left: Rows, right: Rows) -> Rows {
    let (left_some, right_some) = (left.one || left.many, right.one || right.many);
    Rows::new(
        left.none || right.none,
        left.one && right.one,
        (left.many && right_some) || (right.many && left_some),
    )
}

/// `rows` kept where `condition` is true.
fn filtered(rows: Rows, condition: &Evaluation) -> Rows {
    if !condition.may_be_true() {
        Rows::NONE
    } else if condition.may_be_false() || condition.may_be_null() || condition.may_raise() {
        Rows::new(true, rows.one || rows.many, rows.many)
    } else {
        rows
    }
}

/// `rows` with duplicates removed, which may leave one of many.
fn merged(rows: Rows) -> Rows {
    Rows::new(rows.none, rows.one || rows.many, rows.many)
}

/// `rows` past some of them, which may leave none or one of many.
fn skipped(rows: Rows) -> Rows {
    Rows::new(true, rows.one || rows.many, rows.many)
}

/// Reads `limit` into `read`, answering whether the read follows it. A
/// count other than an unsigned literal may be negative, which raises.
fn limited(limit: &LimitClause, read: &mut Read) -> bool {
    let LimitClause::LimitOffset { limit, offset, limit_by } = limit else { return false };
    if !limit_by.is_empty() {
        return false;
    }
    if let Some(offset) = offset {
        match count(&offset.value) {
            Some(0) => {}
            Some(_) => read.rows = skipped(read.rows),
            None => {
                read.error = true;
                read.rows = skipped(read.rows);
            }
        }
    }
    match limit.as_ref().map(|limit| (limit, count(limit))) {
        None | Some((Expr::Value(ValueWithSpan { value: Value::Null, .. }), _)) => {}
        Some((_, Some(0))) => read.rows = Rows::NONE,
        Some((_, Some(1))) => {
            read.rows = Rows::new(read.rows.none, read.rows.one || read.rows.many, false);
        }
        Some((_, Some(_))) => {}
        Some((_, None)) => {
            read.error = true;
            read.rows = skipped(read.rows);
        }
    }
    true
}

/// The count an unsigned integer literal writes.
fn count(expression: &Expr) -> Option<u64> {
    match expression {
        Expr::Value(ValueWithSpan { value: Value::Number(text, _), .. }) => text.parse().ok(),
        _ => None,
    }
}
