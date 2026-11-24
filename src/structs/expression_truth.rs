//! What an expression evaluates to on the rows of a table, at a query site
//! or over a domain's `VALUE`, read through `sqlparser-expr-eval` over the
//! values its names bind and the database's functions.

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    cell::RefCell,
    ops::{Bound, ControlFlow},
    ptr,
};

use sqlparser::{
    ast::{
        ArgMode, DataType, Expr, Function, FunctionBehavior, Ident, Query, Value, ValueWithSpan,
        Visitor,
    },
    dialect::{GenericDialect, PostgreSqlDialect, SQLiteDialect},
};
use sqlparser_expr_eval::{
    ArrayElement, Builtin, Callee, CalleeKind, Collation, Context, EnumType, EvalDialect,
    Evaluation, Guarantees, Interval, NullInput, NullResult, Outcomes, PostgreSqlOptions, Resolved,
    ScalarType, Subject, TextRules, Unlisted, Values, Volatility, evaluate, implied,
    sql_values::postgres::{IntegerType, PgNumeric},
};

use crate::{
    errors::LookupError,
    impls::{
        SqlparserDialect,
        dql::{ColumnOrigin, JoinSite, RelationSlot, definition_graph::reference_sites::SiteRule},
    },
    structs::{
        CallResolution, ColumnDefinition, ColumnScope, ExpressionContext, FunctionKind,
        IdentifierCase, OperandType, PlPgSqlVariable, TargetName, VariableKind,
        expression_context::plain_arguments, plpgsql_scope::stored_parts,
    },
    traits::{
        CheckConstraintLike, CollationMatch, ColumnCollation, ColumnLike, DataTypeLike,
        DatabaseLike, DialectLike, DomainLike, EnumerationKind, EnumerationMember, FunctionLike,
        MySqlCollationPadding, NamedColumnCollation, TableLike, TypeModifier, UserDefinedTypeLike,
    },
    utils::{
        call_sites::{ExpressionForm, expression_form},
        call_typing::runnable_among,
        coercion::canonical,
        postgres_types::PgType,
        scalar_family::ScalarFamily,
        walk::{walk, walk_expressions},
    },
};

/// What an expression can evaluate to on the rows of a table.
///
/// A `CHECK` admits a row when its expression is true or `NULL`, so it
/// admits every row when its expression is never false and never raises. A
/// policy's `USING` admits a row only when it is true, so it admits every row
/// when it is never false, never `NULL` and never raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpressionTruth {
    /// Which of true, false and `NULL` the expression may evaluate to, its
    /// values read as booleans.
    pub outcomes: Outcomes,
    /// On which rows evaluating the expression raises an error.
    pub raising: Raising,
}

/// On which rows evaluating an expression raises an error, whatever the
/// type of its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raising {
    /// On none.
    Never,
    /// On some rows, or on rows the evaluation cannot tell apart.
    Sometimes,
    /// On every row, so it neither takes a value nor is `NULL`.
    Always,
}

impl From<&Evaluation> for ExpressionTruth {
    fn from(evaluation: &Evaluation) -> Self {
        let raising = if evaluation.always_raises() {
            Raising::Always
        } else if evaluation.may_raise() {
            Raising::Sometimes
        } else {
            Raising::Never
        };
        Self { outcomes: evaluation.outcomes(), raising }
    }
}

/// What an expression is read under beside the guarantees of the types,
/// `NOT NULL`, domain checks and generated columns of the values it reads.
pub enum Assumption<'a, DB: DatabaseLike> {
    /// A check of a table, true or `NULL` on every row as PostgreSQL
    /// enforces it. A check calling a SQL value function such as
    /// `CURRENT_TIMESTAMP`, a function that is not immutable, or one that the
    /// database's search path or the read check's path resolves to other
    /// functions than the check's own path, assumes nothing.
    Check(&'a DB::CheckConstraint),
    /// An expression taking one of `outcomes` on every row. A table or domain
    /// read resolves its calls under the database's search path, and an
    /// expression whose calls the read check's path resolves to other
    /// functions assumes nothing. A read through an [`ExpressionContext`]
    /// takes the calls the context selects.
    ///
    /// [`ExpressionContext`]: crate::structs::ExpressionContext
    Expression {
        /// The expression assumed.
        expression: &'a Expr,
        /// The outcomes it is assumed to take.
        outcomes: Outcomes,
    },
}

#[expect(clippy::expl_impl_clone_on_copy, reason = "derive would require DB: Clone")]
impl<DB: DatabaseLike> Clone for Assumption<'_, DB> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<DB: DatabaseLike> Copy for Assumption<'_, DB> {}

/// Every validated and enforced check of `table` its inheritors share, but
/// `except`.
///
/// # Errors
///
/// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
/// hold `table`.
pub(crate) fn validated_checks<'a, DB: DatabaseLike>(
    table: &'a DB::Table,
    database: &'a DB,
    except: Option<&DB::CheckConstraint>,
) -> Result<Vec<&'a DB::CheckConstraint>, LookupError> {
    let mut checks = Vec::new();
    for check in table.check_constraints(database)? {
        if except.is_none_or(|except| !ptr::eq(check, except))
            && check.is_enforced()
            && !check.no_inherit()
            && check.is_validated(database)?
        {
            checks.push(check);
        }
    }
    Ok(checks)
}

/// The engine whose semantics an expression is read with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    /// PostgreSQL, whose types, casts, overflow and built-ins fold.
    PostgreSql,
    /// SQLite, whose column types constrain no stored value.
    Sqlite,
    /// Any other dialect, read with the semantics every dialect shares.
    Other,
}

impl Engine {
    /// The engine whose semantics `database`'s dialect reads with.
    fn of<DB: DatabaseLike>(database: &DB) -> Self {
        let dialect = database.dialect().sqlparser_dialect();
        if dialect.is::<PostgreSqlDialect>() {
            Self::PostgreSql
        } else if dialect.is::<SQLiteDialect>() {
            Self::Sqlite
        } else {
            Self::Other
        }
    }
}

/// What `expression` evaluates to on the rows of `table`, under the
/// guarantees the column types, `NOT NULL`, the domain checks, the generated
/// columns and the `assumptions` hold. `own` is the check `expression`
/// belongs to, which resolves its calls under the path it was created with.
///
/// # Errors
///
/// Returns [`LookupError::CheckNotOfTable`] when an assumed check is not one
/// of `table`'s, and the lookup errors resolving a column, a call or a check
/// raises.
pub(crate) fn table_truth<'a, DB: DatabaseLike>(
    table: &'a DB::Table,
    database: &'a DB,
    expression: &Expr,
    case: IdentifierCase,
    own: Option<&'a DB::CheckConstraint>,
    assumptions: &[Assumption<'a, DB>],
) -> Result<ExpressionTruth, LookupError> {
    for assumption in assumptions {
        if let Assumption::Check(check) = *assumption
            && !table.check_constraints(database)?.any(|held| ptr::eq(held, check))
        {
            return Err(LookupError::CheckNotOfTable {
                check: check.expression(database).to_string(),
                table: table.table_name().into(),
            });
        }
    }
    let mut context = TruthContext::new(database, case, own);
    let read = context.read_table(None, table, false)?;
    context.caller = Some(read);
    context.truth(expression, assumptions)
}

/// What `expression`, a node of `context`'s root, evaluates to wherever it
/// is evaluated, under the guarantees what its references bind holds and the
/// `assumptions`, nodes of the same root.
///
/// # Errors
///
/// Returns [`LookupError::ExpressionNotInQuery`] for a node of another root,
/// and the lookup errors resolving a reference, a call or a check raises.
pub(crate) fn site_truth<'a, DB: DatabaseLike>(
    context: &'a ExpressionContext<'_, '_, DB>,
    expression: &Expr,
    assumptions: &[Assumption<'a, DB>],
) -> Result<ExpressionTruth, LookupError> {
    let mut truth = TruthContext::new(context.database(), context.case(), None);
    let mut site = Site { references: Vec::new(), calls: Vec::new() };
    truth.bind_site(context, expression, &mut site)?;
    for assumption in assumptions {
        if let Assumption::Expression { expression, .. } = assumption {
            truth.bind_site(context, expression, &mut site)?;
        }
    }
    truth.site = Some(site);
    truth.truth(expression, assumptions)
}

/// What `expression` evaluates to over the `VALUE` of `domain`, under the
/// guarantees its base type and the checks of the domains it is defined
/// over, not its own, hold, and the `assumptions`.
///
/// # Errors
///
/// Returns the lookup errors resolving a call or a check raises.
pub(crate) fn domain_truth<'a, DB: DatabaseLike>(
    domain: &'a DB::Domain,
    database: &'a DB,
    expression: &Expr,
    case: IdentifierCase,
    assumptions: &[Assumption<'a, DB>],
) -> Result<ExpressionTruth, LookupError> {
    let mut context = TruthContext::new(database, case, None);
    let data_type = domain.data_type(database);
    let rules = text_rules(ColumnCollation::Unknown, &mut context.collations);
    let evaluation = context.engine.values(data_type, database, rules, true, &context.enums);
    let type_name = Some(data_type.name(database).into_owned());
    let value = context.named(Key::Value, evaluation, type_name, data_type.domain(database));
    context.read_value = Some(value);
    context.truth(expression, assumptions)
}

impl<'a, DB: DatabaseLike> TruthContext<'a, DB> {
    /// A read of nothing yet over `database`, comparing names under `case`,
    /// of the expression of `own` when it is set.
    fn new(database: &'a DB, case: IdentifierCase, own: Option<&'a DB::CheckConstraint>) -> Self {
        Self {
            database,
            case,
            engine: Engine::of(database),
            named: Vec::new(),
            tables: Vec::new(),
            site: None,
            caller: None,
            reading: None,
            read_value: None,
            value: None,
            source: Source::Expression,
            own,
            collations: Vec::new(),
            enums: Enums::default(),
            reached: Vec::new(),
        }
    }

    /// What the read expression evaluates to under `assumptions`, read under
    /// the engine's dialect.
    fn truth(
        self,
        expression: &Expr,
        assumptions: &[Assumption<'a, DB>],
    ) -> Result<ExpressionTruth, LookupError> {
        match self.engine {
            Engine::PostgreSql => {
                self.truth_under(
                    &PostgreSqlDialect {},
                    &PostgreSqlOptions::default(),
                    expression,
                    assumptions,
                )
            }
            Engine::Sqlite | Engine::Other => {
                self.truth_under(&GenericDialect {}, &(), expression, assumptions)
            }
        }
    }

    /// [`Self::truth`] under `dialect` and its `options`.
    fn truth_under<D: EvalDialect>(
        mut self,
        dialect: &D,
        options: &D::Options,
        expression: &Expr,
        assumptions: &[Assumption<'a, DB>],
    ) -> Result<ExpressionTruth, LookupError> {
        let database = self.database;
        let mut guarantees = self.domain_guarantees(dialect, options)?;
        self.value = self.read_value;
        let mut generated = Vec::new();
        for read in 0..self.tables.len() {
            self.reading = Some(read);
            for column in self.tables[read].table.columns(database)? {
                if let Some(expression) = column.generation_expression() {
                    self.source = Source::Generated(column);
                    if self.holds_across_statements(expression, Self::runs_immutable)? {
                        generated.push((read, column, expression));
                    }
                }
            }
        }
        let mut seeding = Vec::new();
        for read in 0..self.tables.len() {
            self.reading = Some(read);
            let table = self.tables[read].table;
            let held = assumptions.iter().filter_map(|assumption| {
                match *assumption {
                    Assumption::Check(check) => Some(check),
                    Assumption::Expression { .. } => None,
                }
            });
            for check in self.tables[read].checks.clone().into_iter().chain(held) {
                if !table.check_constraints(database)?.any(|own| ptr::eq(own, check)) {
                    continue;
                }
                self.source = Source::Check(check);
                if self.holds_across_statements(
                    check.expression(database),
                    Self::runs_immutable_alike,
                )? {
                    seeding.push((Some(read), Assumption::Check(check)));
                }
            }
        }
        self.reading = self.caller;
        self.source = self.caller_source();
        for assumption in assumptions {
            if let Assumption::Expression { expression, .. } = *assumption
                && (self.site.is_some() || self.every_call(expression, Self::resolves_alike)?)
            {
                seeding.push((self.caller, *assumption));
            }
        }
        // A check or a generated column may narrow a column only once another
        // has, so they are read again until a round narrows nothing, at most
        // once more than there are of them.
        for _ in 0..=generated.len() + seeding.len() {
            let before = guarantees.clone();
            for &(read, column, expression) in &generated {
                self.reading = Some(read);
                self.source = Source::Generated(column);
                let evaluation = evaluate(expression, dialect, options, &mut self, &guarantees)?;
                let held = self.generated(column, evaluation)?;
                if let Some(named) = self.column_named(read, column) {
                    guarantees.restrict(Subject::Reference(named), held);
                }
            }
            for &(reading, assumption) in &seeding {
                self.reading = reading;
                let (expression, outcomes) = match assumption {
                    Assumption::Check(check) => {
                        self.source = Source::Check(check);
                        (check.expression(database), Outcomes::NOT_FALSE)
                    }
                    Assumption::Expression { expression, outcomes } => {
                        self.source = self.caller_source();
                        (expression, outcomes)
                    }
                };
                guarantees =
                    implied(expression, outcomes, dialect, options, &mut self, &guarantees)?;
            }
            if guarantees == before || guarantees.is_impossible() {
                break;
            }
        }
        // No row satisfies every guarantee, so the expression meets no row.
        if guarantees.is_impossible() {
            return Ok(ExpressionTruth { outcomes: Outcomes::NONE, raising: Raising::Never });
        }
        self.reading = self.caller;
        self.source = self.own.map_or_else(|| self.caller_source(), Source::Check);
        let evaluation = evaluate(expression, dialect, options, &mut self, &guarantees)?;
        Ok(ExpressionTruth::from(&evaluation))
    }

    /// What the checks of the domains of every named value guarantee, each
    /// over its value. PostgreSQL leaves a domain value `NULL` without
    /// checking it on an outer join's null-extended side, on grouping-set
    /// rows and from a scalar subquery finding no row, so the checks hold
    /// only where the value is not `NULL`.
    fn domain_guarantees<D: EvalDialect>(
        &mut self,
        dialect: &D,
        options: &D::Options,
    ) -> Result<Guarantees<usize>, LookupError> {
        let database = self.database;
        let mut guarantees = Guarantees::new();
        for named in 0..self.named.len() {
            let mut domain = self.named[named].domain;
            if domain.is_none() {
                continue;
            }
            let mut checked = Guarantees::new();
            while let Some(current) = domain {
                self.value = Some(named);
                self.source = Source::Domain(current);
                for check in current.check_expressions() {
                    if self.holds_across_statements(check, Self::runs_immutable_alike)? {
                        checked =
                            implied(check, Outcomes::NOT_FALSE, dialect, options, self, &checked)?;
                    }
                }
                domain = current.data_type(database).domain(database);
            }
            let mut null = Guarantees::new();
            let ty = self.named[named].evaluation.ty().clone();
            null.restrict(Subject::Reference(named), Evaluation::null(ty));
            guarantees = guarantees.meet(&checked.join(&null));
        }
        Ok(guarantees)
    }

    /// Where the caller's expression and assumptions resolve their calls.
    fn caller_source(&self) -> Source<'a, DB> {
        if self.site.is_some() { Source::Site } else { Source::Expression }
    }

    /// The value `key` names, held as `evaluation` before any guarantee
    /// narrows it, of the type named `type_name` and of `domain`.
    fn named(
        &mut self,
        key: Key,
        evaluation: Evaluation,
        type_name: Option<String>,
        domain: Option<&'a DB::Domain>,
    ) -> usize {
        if let Some(held) = self.named.iter().position(|named| named.key == key) {
            return held;
        }
        self.named.push(NamedValue { key, evaluation, type_name, domain });
        self.named.len() - 1
    }

    /// The read of the stored rows of `table` that `relation` names, with a
    /// value for each of its columns, holding its validated checks when
    /// `checked`.
    fn read_table(
        &mut self,
        relation: Option<RelationSlot>,
        table: &'a DB::Table,
        checked: bool,
    ) -> Result<usize, LookupError> {
        if let Some(read) = self
            .tables
            .iter()
            .position(|read| read.relation == relation && ptr::eq(read.table, table))
        {
            return Ok(read);
        }
        let database = self.database;
        for column in table.columns(database)? {
            let evaluation =
                self.engine.column(column, database, &mut self.collations, &self.enums)?;
            let data_type = column.data_type(database);
            let key = Key::Column { relation, column: ptr::from_ref(column).addr() };
            let type_name = Some(data_type.name(database).into_owned());
            self.named(key, evaluation, type_name, data_type.domain(database));
        }
        let checks = if checked { validated_checks(table, database, None)? } else { Vec::new() };
        self.tables.push(TableRead { relation, table, scope: None, checks });
        Ok(self.tables.len() - 1)
    }

    /// The value of `column` in the read `read`.
    fn column_named(&self, read: usize, column: &DB::Column) -> Option<usize> {
        let key = Key::Column {
            relation: self.tables[read].relation,
            column: ptr::from_ref(column).addr(),
        };
        self.named.iter().position(|named| named.key == key)
    }

    /// Binds into `site` the references and calls `expression`, a node of
    /// `context`'s root, writes outside the queries it holds, which the
    /// evaluator reads.
    fn bind_site(
        &mut self,
        context: &'a ExpressionContext<'_, '_, DB>,
        expression: &Expr,
        site: &mut Site<'a, DB>,
    ) -> Result<(), LookupError> {
        if !context.is_node(expression) {
            return Err(LookupError::ExpressionNotInQuery { expression: expression.to_string() });
        }
        let mut visitor = TopLevel {
            depth: 0,
            each: |node: &Expr| -> Result<(), LookupError> {
                let named = match node {
                    Expr::Function(call) => {
                        let functions = Self::site_call(context, node)?;
                        site.calls.push((ptr::from_ref(call).addr(), call.clone(), functions));
                        return Ok(());
                    }
                    Expr::Value(ValueWithSpan {
                        value: Value::Placeholder(placeholder), ..
                    }) => Some(self.site_placeholder(context, node, placeholder)?),
                    Expr::Identifier(ident) => {
                        self.site_named(context, node, core::slice::from_ref(ident))?
                    }
                    Expr::CompoundIdentifier(parts) => self.site_named(context, node, parts)?,
                    _ => return Ok(()),
                };
                site.references.push((ptr::from_ref(node).addr(), node.clone(), named));
                Ok(())
            },
        };
        match walk(expression, &mut visitor) {
            ControlFlow::Break(error) => Err(error),
            ControlFlow::Continue(()) => Ok(()),
        }
    }

    /// The functions the call `node` runs, as `context` selects them.
    fn site_call(
        context: &'a ExpressionContext<'_, '_, DB>,
        node: &Expr,
    ) -> Result<Vec<&'a DB::Function>, LookupError> {
        let Some(application) = context.call(node)? else { return Ok(Vec::new()) };
        Ok(match application.resolution() {
            CallResolution::Selected(selected) => alloc::vec![selected.function()],
            CallResolution::Ambiguous(functions) => functions.clone(),
            // No known function leaves the call unknown to the evaluator,
            // which a call whose path is unknown, a cast and a call
            // PostgreSQL refuses all are.
            CallResolution::Undetermined
            | CallResolution::Cast(_)
            | CallResolution::NotFound
            | CallResolution::UnboundPolymorphic(_) => Vec::new(),
        })
    }

    /// The value the placeholder `node`, spelled `placeholder`, reads where
    /// `context` resolves it.
    fn site_placeholder(
        &mut self,
        context: &'a ExpressionContext<'_, '_, DB>,
        node: &Expr,
        placeholder: &str,
    ) -> Result<usize, LookupError> {
        if let Some(variable) = context.positional_variable(placeholder) {
            return self.variable(context, node, &variable, Vec::new());
        }
        let key = match context.positional_parameter(placeholder) {
            Some((function, ordinal)) => {
                Key::Parameter { function: ptr::from_ref(function).addr(), ordinal }
            }
            None => Key::Placeholder(placeholder.into()),
        };
        let evaluation = self.engine.operand(&context.type_of(node)?, self.database, &self.enums);
        Ok(self.named(key, evaluation, None, None))
    }

    /// The value the reference `node`, spelled `parts`, reads where `context`
    /// resolves it, `None` for a node the evaluator reads no value of.
    fn site_named(
        &mut self,
        context: &'a ExpressionContext<'_, '_, DB>,
        node: &Expr,
        parts: &[Ident],
    ) -> Result<Option<usize>, LookupError> {
        let database = self.database;
        let rule = context.rule(node);
        let unbound = || Key::Name { rule, parts: stored_parts(parts) };
        let Some((definition, binding)) = context.reference(node)? else {
            if let Some(value) = context.domain_value(node) {
                let evaluation = self.engine.operand(&value, database, &self.enums);
                return Ok(Some(self.named(Key::Value, evaluation, None, None)));
            }
            let evaluation = Evaluation::new(
                ScalarType::Unknown,
                true,
                false,
                Values::Unlisted(Unlisted::any()),
            );
            return Ok(Some(self.named(unbound(), evaluation, None, None)));
        };
        let relation = context.relation_at(binding.reached(), binding.origin(), node);
        let built = rule.is_some_and(|rule| {
            context.reads_built_rows(rule, relation.map(|slot| slot.scope), node)
        });
        let output = || {
            let key = match (relation, binding.origin()) {
                (
                    Some(relation),
                    ColumnOrigin::Entry { ordinal, .. } | ColumnOrigin::Merged { ordinal, .. },
                ) => Key::Output { relation, ordinal },
                (None, ColumnOrigin::Merged { join, ordinal }) => Key::Merged { join, ordinal },
                _ => unbound(),
            };
            if built { Key::Built(Box::new(key)) } else { key }
        };
        let key = match definition {
            ColumnDefinition::Base { table, column, database: catalog } => {
                let own_catalog = ptr::eq(catalog, database);
                if own_catalog
                    && !built
                    && let Some(relation) = relation.filter(|slot| {
                        slot.relation.table
                            && slot.relation.constrained
                            && !slot.relation.null_extended
                    })
                {
                    let read = self.read_table(Some(relation), table, true)?;
                    return Ok(self.column_named(read, column));
                }
                let key = match relation {
                    Some(relation) if relation.relation.table => {
                        let key = Key::Column {
                            relation: Some(relation),
                            column: ptr::from_ref(column).addr(),
                        };
                        if built { Key::Built(Box::new(key)) } else { key }
                    }
                    _ => output(),
                };
                // A row an outer join, a grouping set or the statement
                // builds, or one read through another relation, may be
                // `NULL` in the column and break the table's constraints.
                let evaluation = self
                    .engine
                    .column(column, catalog, &mut self.collations, &self.enums)?
                    .with_null(true);
                let data_type = column.data_type(catalog);
                let domain = own_catalog.then(|| data_type.domain(catalog)).flatten();
                let type_name = Some(data_type.name(catalog).into_owned());
                return Ok(Some(self.named(key, evaluation, type_name, domain)));
            }
            ColumnDefinition::Parameter { function, ordinal } => {
                Key::Parameter { function: ptr::from_ref(function).addr(), ordinal }
            }
            ColumnDefinition::Variable(variable) => {
                return self.variable(context, node, &variable, Vec::new()).map(Some);
            }
            ColumnDefinition::VariableField(variable) => {
                return self.variable(context, node, &variable, stored_parts(parts)).map(Some);
            }
            _ => output(),
        };
        let evaluation = self.engine.operand(&context.type_of(node)?, database, &self.enums);
        Ok(Some(self.named(key, evaluation, None, None)))
    }

    /// The value of `variable`, or of the field `fields` name when there are
    /// any, which `node` reads.
    fn variable(
        &mut self,
        context: &'a ExpressionContext<'_, '_, DB>,
        node: &Expr,
        variable: &PlPgSqlVariable<'_>,
        fields: Vec<String>,
    ) -> Result<usize, LookupError> {
        let never_null = fields.is_empty()
            && matches!(
                variable.kind(),
                VariableKind::Declared { not_null: true, .. }
                    | VariableKind::LoopIndex
                    | VariableKind::Found
            );
        let evaluation = self
            .engine
            .operand(&context.type_of(node)?, self.database, &self.enums)
            .with_null(!never_null);
        Ok(self.named(Key::Variable { id: variable.id(), fields }, evaluation, None, None))
    }

    /// The value `expression` reads, `None` for a node that is no reference
    /// or one naming nothing in reach.
    fn named_by(&mut self, expression: &Expr) -> Result<Option<usize>, LookupError> {
        if let (Some(named), Expr::Identifier(name)) = (self.value, expression)
            && name.quote_style.is_none()
            && name.value.eq_ignore_ascii_case("value")
        {
            return Ok(Some(named));
        }
        if let Some(read) = self.reading {
            if !matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                return Ok(None);
            }
            let (database, case) = (self.database, self.case);
            let table = self.tables[read].table;
            let scope = self.tables[read]
                .scope
                .get_or_insert_with(|| ColumnScope::for_table(table, database, case));
            let column = match scope.resolve_column_definition(expression)? {
                Some(ColumnDefinition::Base { column, .. }) => ptr::from_ref(column).addr(),
                _ => return Ok(None),
            };
            let key = Key::Column { relation: self.tables[read].relation, column };
            return Ok(self.named.iter().position(|named| named.key == key));
        }
        Ok(self.site.as_ref().and_then(|site| site.named(expression)))
    }
}

/// Hands `each` every expression a node holds outside the queries in it,
/// the nodes the evaluator reads, until `each` fails.
struct TopLevel<F> {
    depth: usize,
    each: F,
}

impl<F: FnMut(&Expr) -> Result<(), LookupError>> Visitor for TopLevel<F> {
    type Break = LookupError;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if self.depth > 0 {
            return ControlFlow::Continue(());
        }
        match (self.each)(expression) {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
}

/// What a value the read names stands for, which tells two references of one
/// value from references of two.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// A column of the rows of a stored table the relation `relation` reads,
    /// `None` for the table a table read reads.
    Column {
        /// The relation reading the table.
        relation: Option<RelationSlot>,
        /// The address of the column.
        column: usize,
    },
    /// Output column `ordinal` of a relation other than a stored table.
    Output {
        /// The relation.
        relation: RelationSlot,
        /// The position of the column among the relation's.
        ordinal: usize,
    },
    /// Column `ordinal` of the columns the join `join` merged.
    Merged {
        /// The join.
        join: JoinSite,
        /// The position of the column among those the join merged.
        ordinal: usize,
    },
    /// Input parameter `ordinal` of the function at address `function`.
    Parameter {
        /// The address of the function.
        function: usize,
        /// The position of the parameter among the inputs.
        ordinal: usize,
    },
    /// The PL/pgSQL variable `id`, or the field of it `fields` names.
    Variable {
        /// The variable's id.
        id: usize,
        /// The parts naming the field, empty for the variable itself.
        fields: Vec<String>,
    },
    /// A positional parameter of a function body.
    Placeholder(String),
    /// The value a domain constraint checks.
    Value,
    /// A name nothing in reach binds, as stored, where `rule` resolves it.
    Name {
        /// The rule resolving references where the name sits.
        rule: Option<SiteRule>,
        /// The parts of the name, as stored.
        parts: Vec<String>,
    },
    /// What the key names, on rows the query builds, which grouping sets
    /// and a `RETURNING` list read.
    Built(Box<Key>),
}

/// A value the read names, and what holds of it before any guarantee.
struct NamedValue<'a, DB: DatabaseLike> {
    key: Key,
    /// What the value holds before any guarantee narrows it.
    evaluation: Evaluation,
    /// The name of its declared type, which selects among the overloads of a
    /// call a check writes over it.
    type_name: Option<String>,
    /// The domain the value belongs to, whose checks and those of the
    /// domains it is defined over hold on it.
    domain: Option<&'a DB::Domain>,
}

/// The stored rows of a table one relation reads.
struct TableRead<'a, DB: DatabaseLike> {
    /// The relation reading the table, `None` for a table read.
    relation: Option<RelationSlot>,
    table: &'a DB::Table,
    /// The table's column scope, built on the first column reference.
    scope: Option<ColumnScope<'a, 'a, DB>>,
    /// The checks every row of the table holds.
    checks: Vec<&'a DB::CheckConstraint>,
}

/// The references and calls of a query site, bound before the read, each
/// by its address and with a copy matching the copies the evaluator reads
/// of some forms.
struct Site<'a, DB: DatabaseLike> {
    references: Vec<(usize, Expr, Option<usize>)>,
    calls: Vec<(usize, Function, Vec<&'a DB::Function>)>,
}

impl<'a, DB: DatabaseLike> Site<'a, DB> {
    /// The value the reference `expression` reads, or that every reference
    /// equal to the copy `expression` is reads.
    fn named(&self, expression: &Expr) -> Option<usize> {
        let address = ptr::from_ref(expression).addr();
        if let Some((.., named)) = self.references.iter().find(|(held, ..)| *held == address) {
            return *named;
        }
        let mut equal = self
            .references
            .iter()
            .filter(|(_, written, _)| written == expression)
            .map(|(.., named)| *named);
        let first = equal.next()??;
        equal.all(|named| named == Some(first)).then_some(first)
    }

    /// The functions the call `call` runs, or that every call equal to the
    /// copy `call` runs, `None` for a call the site does not write.
    fn call(&self, call: &Function) -> Option<Vec<&'a DB::Function>> {
        let address = ptr::from_ref(call).addr();
        if let Some((.., functions)) = self.calls.iter().find(|(held, ..)| *held == address) {
            return Some(functions.clone());
        }
        let mut equal =
            self.calls.iter().filter(|(_, written, _)| written == call).map(|(.., run)| run);
        let first = equal.next()?;
        let alike = equal.all(|run| {
            run.len() == first.len()
                && run.iter().all(|function| first.iter().any(|held| ptr::eq(*function, *held)))
        });
        Some(if alike { first.clone() } else { Vec::new() })
    }
}

/// Where the expression the evaluator reads was written, which decides the
/// search path its calls resolve under.
enum Source<'a, DB: DatabaseLike> {
    /// The caller's expression, resolved under the database's path.
    Expression,
    /// The caller's expression at a query site, resolved as the site's
    /// context selects its calls.
    Site,
    /// A check of the table, resolved under the path it was created with.
    Check(&'a DB::CheckConstraint),
    /// A check of a domain, resolved under the path the domain was created
    /// with.
    Domain(&'a DB::Domain),
    /// The generation expression of a column, resolved under the path the
    /// column was created with.
    Generated(&'a DB::Column),
}

impl<DB: DatabaseLike> Source<'_, DB> {
    /// The address of the check, domain or column, zero for the caller's
    /// expression.
    fn address(&self) -> usize {
        match self {
            Source::Expression | Source::Site => 0,
            Source::Check(check) => ptr::from_ref(*check).addr(),
            Source::Domain(domain) => ptr::from_ref(*domain).addr(),
            Source::Generated(column) => ptr::from_ref(*column).addr(),
        }
    }
}

/// The functions a call resolved to, by the address of where it was written
/// and of the value `VALUE` stood for.
struct Reached<'a, F> {
    source: usize,
    value: Option<usize>,
    call: Function,
    functions: Vec<&'a F>,
}

/// The values a read names and the database's functions, as the evaluator
/// asks for them.
struct TruthContext<'a, DB: DatabaseLike> {
    database: &'a DB,
    case: IdentifierCase,
    engine: Engine,
    /// The values met so far.
    named: Vec<NamedValue<'a, DB>>,
    /// The stored rows the read reads.
    tables: Vec<TableRead<'a, DB>>,
    /// The references and calls of the query site the read is at, if any.
    site: Option<Site<'a, DB>>,
    /// The table read whose columns the caller's names read, if any.
    caller: Option<usize>,
    /// The table read whose columns the expression being read reads, or
    /// `None` for the caller's names.
    reading: Option<usize>,
    /// The value `VALUE` stands for in the caller's expression, if any.
    read_value: Option<usize>,
    /// The value `VALUE` stands for in the expression being read.
    value: Option<usize>,
    /// Where the expression being read was written.
    source: Source<'a, DB>,
    /// The check whose expression is read last, if the expression is one.
    own: Option<&'a DB::CheckConstraint>,
    /// The collations met so far, each named by its position.
    collations: Vec<ColumnCollation<'static>>,
    /// The enum types met so far.
    enums: Enums,
    /// The calls resolved so far, as overload resolution is costly and the
    /// evaluator asks for the same call many times.
    reached: Vec<Reached<'a, DB::Function>>,
}

/// The enum types met so far, by the address of the type declaring each, so
/// that every column and cast of one type reads as the same type.
#[derive(Default)]
struct Enums(RefCell<Vec<(usize, Arc<EnumType>)>>);

impl Enums {
    /// The enum type `pg_type` names in `database`, found through the type
    /// search path.
    fn named<DB: DatabaseLike>(&self, database: &DB, pg_type: &PgType) -> Option<ScalarType> {
        let PgType::Named(target) = &*canonical(database, pg_type) else { return None };
        let schema = target.schema()?;
        let case = database.identifier_case();
        let declared = database.user_defined_types().find(|declared| {
            case.identifiers_match(
                declared.name(),
                declared.name_is_quoted(),
                target.name(),
                target.name_is_quoted(),
            ) && case.identifiers_match(
                declared.schema().unwrap_or(DEFAULT_SCHEMA),
                declared.schema_is_quoted(),
                schema,
                target.schema_is_quoted(),
            )
        })?;
        let address = ptr::from_ref(declared).addr();
        let mut known = self.0.borrow_mut();
        if let Some((_, enum_type)) = known.iter().find(|(known, _)| *known == address) {
            return Some(ScalarType::Enum(Arc::clone(enum_type)));
        }
        let enumeration = declared.enumeration()?;
        if enumeration.kind() != EnumerationKind::Enum {
            return None;
        }
        let name = format!(
            "{}.{}",
            declared.stored_schema().as_deref().unwrap_or(DEFAULT_SCHEMA),
            declared.stored_name()
        );
        let labels = enumeration.into_members().map(EnumerationMember::label);
        let enum_type = Arc::new(EnumType::new(name, labels));
        known.push((address, Arc::clone(&enum_type)));
        Some(ScalarType::Enum(enum_type))
    }
}

const DEFAULT_SCHEMA: &str = "public";

impl Engine {
    /// What `column` holds before any guarantee narrows it, its collation
    /// identified among the `collations` already met.
    fn column<DB: DatabaseLike>(
        self,
        column: &DB::Column,
        database: &DB,
        collations: &mut Vec<ColumnCollation<'static>>,
        enums: &Enums,
    ) -> Result<Evaluation, LookupError> {
        let rules = text_rules(column.collation(database)?, collations);
        let nullable = column.is_nullable(database)?;
        Ok(self.values(column.data_type(database), database, rules, nullable, enums))
    }

    /// What a value of `data_type` holds before any guarantee narrows it, its
    /// text compared under `rules`, `NULL` when `nullable`.
    fn values<DB: DatabaseLike>(
        self,
        data_type: &DB::DataType,
        database: &DB,
        rules: TextRules,
        nullable: bool,
        enums: &Enums,
    ) -> Evaluation {
        let base = data_type.base_type(database).unwrap_or(data_type);
        let scalar_type = self.scalar_type(base, database, rules, enums);
        let mut values = Unlisted::any();
        if scalar_type.is_text()
            && let Some(longest) = base.max_character_length(database)
        {
            values.length = Interval { low: Bound::Included(0), high: Bound::Included(longest) };
        }
        // `NaN` fits every `numeric(p, s)` and sorts above every number, so
        // only the lower bound holds.
        if scalar_type == ScalarType::Numeric
            && let Some(bound) = numeric_bound(base, database)
        {
            values.numbers =
                Interval { low: Bound::Excluded(PgNumeric::from(-bound)), high: Bound::Unbounded };
        }
        Evaluation::new(scalar_type, nullable, false, Values::Unlisted(values))
    }

    /// What a value of the type `operand` holds before any guarantee narrows
    /// it, possibly `NULL`, its text compared under rules not known.
    fn operand<DB: DatabaseLike>(
        self,
        operand: &OperandType<'_>,
        database: &DB,
        enums: &Enums,
    ) -> Evaluation {
        let scalar_type = match (self, operand) {
            (Engine::PostgreSql, OperandType::Known { type_name, .. }) => {
                postgres_scalar_type(
                    &PgType::from_target(type_name),
                    TextRules::UNKNOWN,
                    database,
                    enums,
                )
            }
            _ => ScalarType::Unknown,
        };
        Evaluation::new(scalar_type, true, false, Values::Unlisted(Unlisted::any()))
    }

    /// The scalar type values of `data_type` read as under the engine.
    fn scalar_type<DB: DatabaseLike>(
        self,
        data_type: &DB::DataType,
        database: &DB,
        rules: TextRules,
        enums: &Enums,
    ) -> ScalarType {
        match self {
            Engine::PostgreSql => {
                PgType::parse(&data_type.name(database)).map_or(ScalarType::Other, |pg_type| {
                    postgres_scalar_type(&pg_type, rules, database, enums)
                })
            }
            Engine::Sqlite => ScalarType::Other,
            Engine::Other => {
                match data_type.scalar_family(database) {
                    Some(ScalarFamily::Int | ScalarFamily::Decimal) => ScalarType::Numeric,
                    Some(ScalarFamily::Float) => ScalarType::Float,
                    Some(ScalarFamily::Bool) => ScalarType::Boolean,
                    Some(ScalarFamily::String) => ScalarType::Text(TextRules::UNKNOWN),
                    _ => ScalarType::Other,
                }
            }
        }
    }

    /// What `function` is to the evaluator.
    fn callee<DB: DatabaseLike>(
        self,
        function: &DB::Function,
        database: &DB,
        enums: &Enums,
    ) -> Result<Callee, LookupError> {
        let inputs = function.argument_modes().filter(|mode| !matches!(mode, ArgMode::Out)).count();
        let nulling: Vec<usize> = (0..inputs)
            .filter(|&position| function.returns_null_on_null_argument(database, position))
            .collect();
        let null_input = if nulling.len() == inputs {
            NullInput::Strict
        } else {
            NullInput::CalledOnNull(nulling)
        };
        let null_result = if function.returns_null_only_on_null_input(database) {
            NullResult::OnlyOnNullInput
        } else {
            NullResult::AnyInput
        };
        let volatility = match function.volatility() {
            FunctionBehavior::Immutable => Volatility::Immutable,
            FunctionBehavior::Stable => Volatility::Stable,
            FunctionBehavior::Volatile => Volatility::Volatile,
        };
        let kind = match function.kind(database)? {
            FunctionKind::Normal | FunctionKind::Procedure => CalleeKind::Normal,
            FunctionKind::Aggregate => CalleeKind::Aggregate,
            FunctionKind::Window => CalleeKind::Window,
        };
        let returns = match self {
            Engine::PostgreSql => {
                function
                    .return_type_name(database)
                    .and_then(|name| PgType::parse(name.strip_prefix("SETOF ").unwrap_or(&name)))
                    .map_or(ScalarType::Other, |pg_type| {
                        postgres_scalar_type(&pg_type, TextRules::DEFAULT, database, enums)
                    })
            }
            Engine::Sqlite | Engine::Other => ScalarType::Other,
        };
        let builtin = (self == Engine::PostgreSql).then(|| builtin(function)).flatten();
        Ok(Callee::new(
            builtin,
            null_input,
            null_result,
            volatility,
            kind,
            returns,
            function.returns_set(),
            ptr::from_ref(function).addr(),
        ))
    }
}

impl<DB: DatabaseLike> Context for TruthContext<'_, DB> {
    type Reference = usize;
    type Error = LookupError;

    fn reference(&mut self, expression: &Expr) -> Result<Option<Resolved<usize>>, LookupError> {
        Ok(self.named_by(expression)?.map(|named| {
            Resolved { reference: named, evaluation: self.named[named].evaluation.clone() }
        }))
    }

    fn callees(&mut self, call: &Function) -> Result<Vec<Callee>, LookupError> {
        let (database, engine) = (self.database, self.engine);
        let mut callees = self
            .runnable(call)?
            .into_iter()
            .map(|function| engine.callee(function, database, &self.enums))
            .collect::<Result<Vec<_>, _>>()?;
        // The evaluator reads the call's type from its first callee, so
        // overloads returning different types leave the type unknown.
        if callees.windows(2).any(|pair| pair[0].returns != pair[1].returns) {
            for callee in &mut callees {
                callee.returns = ScalarType::Unknown;
            }
        }
        Ok(callees)
    }

    fn data_type(&self, data_type: &DataType) -> ScalarType {
        if self.engine == Engine::PostgreSql
            && let Some(enum_type) = self.enums.named(self.database, &PgType::of(data_type))
        {
            return enum_type;
        }
        ScalarType::of(data_type)
    }
}

impl<'a, DB: DatabaseLike> TruthContext<'a, DB> {
    /// The functions `call` may run, resolved under the search path of
    /// where it was written, or none when it names no known function or
    /// PostgreSQL reads it as a cast.
    fn runnable(&mut self, call: &Function) -> Result<Vec<&'a DB::Function>, LookupError> {
        if let Source::Site = self.source {
            return Ok(self.site.as_ref().and_then(|site| site.call(call)).unwrap_or_default());
        }
        let (source, value) = (self.source.address(), self.value);
        if let Some(reached) = self.reached.iter().find(|reached| {
            reached.source == source && reached.value == value && reached.call == *call
        }) {
            return Ok(reached.functions.clone());
        }
        let functions = self.resolve(call)?;
        self.reached.push(Reached {
            source,
            value,
            call: call.clone(),
            functions: functions.clone(),
        });
        Ok(functions)
    }

    /// [`Self::runnable`] without looking up the calls already resolved.
    fn resolve(&mut self, call: &Function) -> Result<Vec<&'a DB::Function>, LookupError> {
        let (database, case) = (self.database, self.case);
        let reached = match &self.source {
            Source::Expression | Source::Site => {
                database.functions_reached_by(call, case).map(Iterator::collect)
            }
            Source::Check(check) => {
                let check = *check;
                reached_in(core::iter::once(check.expression(database)), call, |written| {
                    check.functions_reached_by(written, database).map(Iterator::collect)
                })
            }
            Source::Domain(domain) => {
                let domain = *domain;
                reached_in(domain.check_expressions(), call, |written| {
                    domain.functions_reached_by(written, database).map(Iterator::collect)
                })
            }
            Source::Generated(column) => {
                let column = *column;
                reached_in(column.generation_expression(), call, |written| {
                    column.functions_reached_by(written, database).map(Iterator::collect)
                })
            }
        };
        let functions: Vec<&DB::Function> = match reached {
            Ok(functions) => functions,
            // A name no static reader resolves reaches no known function.
            Err(LookupError::InvalidObjectName { .. }) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if self.engine != Engine::PostgreSql || functions.len() < 2 {
            return Ok(functions);
        }
        let type_names = self.argument_type_names(call)?;
        let type_names: Vec<Option<&str>> = type_names.iter().map(Option::as_deref).collect();
        // A cast runs no function.
        Ok(runnable_among(database, call, functions, &type_names)?.unwrap_or_default())
    }

    /// The type name of each argument `call` writes, `None` for one that is
    /// not a reference to a value of a declared type, or nothing for a call
    /// written with clauses beside its arguments.
    fn argument_type_names(&mut self, call: &Function) -> Result<Vec<Option<String>>, LookupError> {
        let Some(arguments) = plain_arguments(call) else { return Ok(Vec::new()) };
        let mut type_names = Vec::with_capacity(arguments.len());
        for argument in arguments {
            type_names.push(
                self.named_by(argument)?.and_then(|named| self.named[named].type_name.clone()),
            );
        }
        Ok(type_names)
    }

    /// Whether `expression` answers the same on a row in every statement,
    /// which a check or a stored generated column must for what was proved
    /// or computed when the row was written to still hold. A call `runs`
    /// refuses and a SQL value function such as `CURRENT_TIMESTAMP` may
    /// answer differently later.
    fn holds_across_statements(
        &mut self,
        expression: &Expr,
        runs: fn(&mut Self, &Function) -> Result<bool, LookupError>,
    ) -> Result<bool, LookupError> {
        let value_function = walk_expressions(expression, |node| {
            match node {
                Expr::Function(call)
                    if matches!(expression_form(&call.name), Some(ExpressionForm::Value(_))) =>
                {
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            }
        })
        .is_break();
        Ok(!value_function && self.every_call(expression, runs)?)
    }

    /// Whether every call `expression` writes that may run a function
    /// passes `passes`.
    fn every_call(
        &mut self,
        expression: &Expr,
        passes: fn(&mut Self, &Function) -> Result<bool, LookupError>,
    ) -> Result<bool, LookupError> {
        let mut answer = Ok(true);
        let _ = walk_expressions(expression, |node| {
            let Expr::Function(call) = node else { return ControlFlow::Continue(()) };
            if expression_form(&call.name).is_some() {
                return ControlFlow::Continue(());
            }
            match passes(self, call) {
                Ok(true) => ControlFlow::Continue(()),
                Ok(false) => {
                    answer = Ok(false);
                    ControlFlow::Break(())
                }
                Err(error) => {
                    answer = Err(error);
                    ControlFlow::Break(())
                }
            }
        });
        answer
    }

    /// Whether `call` runs only immutable functions, under the path it was
    /// written with.
    fn runs_immutable(&mut self, call: &Function) -> Result<bool, LookupError> {
        Ok(all_immutable(&self.runnable(call)?))
    }

    /// Whether `call` runs only immutable functions, as
    /// [`Self::resolves_alike`] resolves it.
    fn runs_immutable_alike(&mut self, call: &Function) -> Result<bool, LookupError> {
        Ok(all_immutable(&self.runnable(call)?) && self.resolves_alike(call)?)
    }

    /// Whether `call` runs the same functions under the path it was written
    /// with as where the caller's expression resolves it and, when the check
    /// being read writes the same call, under that check's path. A table or
    /// domain read resolves the caller's calls under the database's search
    /// path, and a site read as its context selects the site's calls, which
    /// matters only for a call the site writes.
    ///
    /// The guarantees an assumption seeds name its calls as written, so a
    /// call another path resolves to other functions would carry them to
    /// calls of other functions.
    fn resolves_alike(&mut self, call: &Function) -> Result<bool, LookupError> {
        let written = self.runnable(call)?;
        let caller = match &self.site {
            Some(site) => site.call(call).is_some().then_some(Source::Site),
            None => Some(Source::Expression),
        };
        let own = self
            .own
            .filter(|own| writes_call(own.expression(self.database), call))
            .map(Source::Check);
        for path in caller.into_iter().chain(own) {
            let source = core::mem::replace(&mut self.source, path);
            let other = self.runnable(call);
            self.source = source;
            let other = other?;
            if other.len() != written.len()
                || !written
                    .iter()
                    .all(|function| other.iter().any(|other| ptr::eq(*function, *other)))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// What `column` holds as its generation expression evaluates to
    /// `evaluation`. A value the column's type may rewrite on assignment,
    /// and any value outside PostgreSQL, whose assignment rules are the only
    /// ones read here, leaves only whether the column may be `NULL`.
    fn generated(
        &mut self,
        column: &DB::Column,
        evaluation: Evaluation,
    ) -> Result<Evaluation, LookupError> {
        let database = self.database;
        let own = self.engine.column(column, database, &mut self.collations, &self.enums)?;
        let data_type = column.data_type(database);
        let base = data_type.base_type(database).unwrap_or(data_type);
        let modified = base.typmod(database) != Some(TypeModifier::Unmodified);
        let carried = self.engine == Engine::PostgreSql
            && match (own.ty(), evaluation.ty()) {
                (ScalarType::Text(_), ScalarType::Text(_) | ScalarType::UnknownLiteral)
                | (ScalarType::Integer(_), ScalarType::Integer(_))
                | (ScalarType::Enum(_), ScalarType::UnknownLiteral) => true,
                (ScalarType::Boolean | ScalarType::Date | ScalarType::Enum(_), ty) => {
                    ty == own.ty()
                }
                (ScalarType::Numeric | ScalarType::Timestamp, ty) => ty == own.ty() && !modified,
                _ => false,
            };
        // A listed value the type refuses or rewrites, such as text longer
        // than a `varchar(n)` whose excess PostgreSQL trims as spaces,
        // leaves the values unstated.
        let stated = carried
            && match evaluation.values() {
                Values::Listed(listed) => {
                    !listed.is_empty() && listed.iter().all(|value| own.values().admits(value))
                }
                Values::Unlisted(_) => !modified,
                _ => false,
            };
        let held = evaluation.with_type(own.ty().clone()).with_error(false);
        Ok(if stated { held } else { held.with_values(Values::Unlisted(Unlisted::any())) })
    }
}

/// Whether `functions` are immutable, none of them standing for a call that
/// resolves to no function and may run anything.
fn all_immutable<F: FunctionLike>(functions: &[&F]) -> bool {
    !functions.is_empty()
        && functions
            .iter()
            .all(|function| matches!(function.volatility(), FunctionBehavior::Immutable))
}

/// Whether `expression` writes a call equal to `call`.
fn writes_call(expression: &Expr, call: &Function) -> bool {
    walk_expressions(expression, |node| {
        match node {
            Expr::Function(written) if written == call => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    })
    .is_break()
}

/// The functions `reach` answers for the call among `expressions` that
/// `call` is, matched by address and otherwise by value, since the evaluator
/// reads some forms such as `BETWEEN` through copies of their operands.
/// Equal calls of one source resolve alike, as they reach the same
/// functions with the same arguments.
fn reached_in<'a, 'e, F>(
    expressions: impl IntoIterator<Item = &'e Expr>,
    call: &Function,
    mut reach: impl FnMut(&Function) -> Result<Vec<&'a F>, LookupError>,
) -> Result<Vec<&'a F>, LookupError> {
    let mut equal = None;
    for expression in expressions {
        let found = walk_expressions(expression, |node| {
            match node {
                Expr::Function(written) if ptr::eq(written, call) => {
                    ControlFlow::Break(reach(written))
                }
                Expr::Function(written) if equal.is_none() && written == call => {
                    equal = Some(reach(written));
                    ControlFlow::Continue(())
                }
                _ => ControlFlow::Continue(()),
            }
        });
        if let ControlFlow::Break(reached) = found {
            return reached;
        }
    }
    equal.unwrap_or_else(|| reach(call))
}

/// The scalar type a PostgreSQL type reads as, text under `rules` and an
/// enum among the `enums` of `database`.
fn postgres_scalar_type<DB: DatabaseLike>(
    pg_type: &PgType,
    rules: TextRules,
    database: &DB,
    enums: &Enums,
) -> ScalarType {
    match pg_type {
        PgType::Array(element) => {
            ScalarType::Array(ArrayElement::of(&postgres_scalar_type(
                element, rules, database, enums,
            )))
        }
        PgType::Catalog(name) => {
            match *name {
                "bool" => ScalarType::Boolean,
                "int2" => ScalarType::Integer(IntegerType::Smallint),
                "int4" => ScalarType::Integer(IntegerType::Integer),
                "int8" => ScalarType::Integer(IntegerType::Bigint),
                "numeric" => ScalarType::Numeric,
                "float4" | "float8" => ScalarType::Float,
                "text" | "varchar" => ScalarType::Text(rules),
                "date" => ScalarType::Date,
                "timestamp" => ScalarType::Timestamp,
                _ => ScalarType::Other,
            }
        }
        PgType::Named(_) => enums.named(database, pg_type).unwrap_or(ScalarType::Other),
        PgType::Other(_) => ScalarType::Other,
    }
}

/// The magnitude every value of a `numeric(p, s)` stays below, `10^(p - s)`,
/// when it fits an `i64`.
fn numeric_bound<DB: DatabaseLike>(data_type: &DB::DataType, database: &DB) -> Option<i64> {
    let precision = i64::try_from(data_type.numeric_precision(database)?).ok()?;
    let digits = precision.checked_sub(data_type.numeric_scale(database).unwrap_or(0))?;
    10_i64.checked_pow(u32::try_from(digits).ok()?)
}

/// What a column's collation, an implicit one, lets the evaluator decide about
/// its text, with `known` holding the collations already given an identity,
/// one per distinct PostgreSQL rule.
fn text_rules(
    collation: ColumnCollation<'_>,
    known: &mut Vec<ColumnCollation<'static>>,
) -> TextRules {
    const PG: SqlparserDialect = SqlparserDialect::PostgreSql;
    let ColumnCollation::Named(named) = &collation else {
        return match collation {
            ColumnCollation::DatabaseDefault => TextRules::DEFAULT,
            _ => TextRules::UNKNOWN,
        };
    };
    if collation.is_same_rule(PG, &ColumnCollation::DatabaseDefault) == CollationMatch::Same {
        return TextRules::DEFAULT;
    }
    // A name that settles on no stored identity is not even the same rule as
    // itself.
    if collation.is_same_rule(PG, &collation) != CollationMatch::Same {
        return TextRules::UNKNOWN;
    }
    let in_catalog = |name| {
        let catalog = TargetName::new(name, true).with_schema("pg_catalog", false);
        let catalog = ColumnCollation::Named(NamedColumnCollation::new(catalog));
        collation.is_same_rule(PG, &catalog) == CollationMatch::Same
    };
    if in_catalog("C") {
        return TextRules::C;
    }
    let byte_order = in_catalog("POSIX");
    let identity_equality = byte_order
        || (named.postgres_deterministic() == Some(true)
            && named.mysql_padding() != Some(MySqlCollationPadding::PadSpace));
    let found =
        known.iter().position(|other| collation.is_same_rule(PG, other) == CollationMatch::Same);
    let position = found.unwrap_or_else(|| {
        known.push(collation.into_owned());
        known.len() - 1
    });
    let Ok(identity) = u32::try_from(position) else { return TextRules::UNKNOWN };
    TextRules {
        identity_equality,
        byte_order,
        collation: Collation::Named(identity),
        explicit: false,
    }
}

/// The built-in a `pg_catalog` function folds as.
fn builtin<F: FunctionLike>(function: &F) -> Option<Builtin> {
    let target = function.target_name();
    if target.schema() != Some("pg_catalog") {
        return None;
    }
    Some(match function.name() {
        "length" | "char_length" | "character_length" => Builtin::Length,
        "octet_length" => Builtin::OctetLength,
        "lower" => Builtin::Lower,
        "upper" => Builtin::Upper,
        "btrim" => Builtin::Btrim,
        "abs" => Builtin::Abs,
        "concat" => Builtin::Concat,
        "concat_ws" => Builtin::ConcatWs,
        "count" => Builtin::Count,
        _ => return None,
    })
}
