//! What an expression evaluates to on the rows of a table, read through
//! `sqlparser-expr-eval` over the table's columns and the database's
//! functions.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::{
    cell::RefCell,
    ops::{Bound, ControlFlow},
    ptr,
};

use sqlparser::{
    ast::{ArgMode, DataType, Expr, Function, FunctionBehavior},
    dialect::{GenericDialect, PostgreSqlDialect, SQLiteDialect},
};
use sqlparser_expr_eval::{
    ArrayElement, Builtin, Callee, CalleeKind, Collation, Context, EnumType, EvalDialect,
    Evaluation, Guarantees, Interval, NullInput, Outcomes, PostgreSqlOptions, Resolved, ScalarType,
    Subject, TextRules, Unlisted, Values, Volatility, evaluate, implied,
    sql_values::postgres::{IntegerType, PgNumeric},
};

use crate::{
    errors::LookupError,
    impls::SqlparserDialect,
    structs::{
        ColumnDefinition, ColumnScope, FunctionKind, IdentifierCase, TargetName,
        expression_context::plain_arguments,
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
        walk::walk_expressions,
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
    /// Which of true, false and `NULL` the expression may evaluate to.
    pub outcomes: Outcomes,
    /// Whether evaluating the expression may raise an error.
    pub may_raise: bool,
}

impl From<&Evaluation> for ExpressionTruth {
    fn from(evaluation: &Evaluation) -> Self {
        Self { outcomes: evaluation.outcomes(), may_raise: evaluation.may_raise() }
    }
}

/// Whether the table's `CHECK` constraints seed the guarantees an expression
/// is read under.
#[derive(Clone, Copy)]
pub(crate) enum Checks {
    /// None of them do, so a check is judged on its own.
    Ignored,
    /// Every validated and enforced one the table's inheritors share does,
    /// but the check whose expression is read.
    Validated,
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
/// columns and the `checks` hold. `own` is the check `expression` belongs
/// to, which seeds nothing and resolves its calls under the path it was
/// created with.
///
/// # Errors
///
/// Returns the lookup errors resolving a column, a call or a check raises.
pub(crate) fn table_truth<DB: DatabaseLike>(
    table: &DB::Table,
    database: &DB,
    expression: &Expr,
    case: IdentifierCase,
    own: Option<&DB::CheckConstraint>,
    checks: Checks,
) -> Result<ExpressionTruth, LookupError> {
    let engine = Engine::of(database);
    let context = TableContext {
        table,
        database,
        case,
        engine,
        scope: None,
        value: None,
        source: Source::Expression,
        own,
        collations: Vec::new(),
        enums: Enums::default(),
        reached: Vec::new(),
    };
    match engine {
        Engine::PostgreSql => {
            truth_under(
                &PostgreSqlDialect {},
                &PostgreSqlOptions::default(),
                context,
                expression,
                checks,
            )
        }
        Engine::Sqlite | Engine::Other => {
            truth_under(&GenericDialect {}, &(), context, expression, checks)
        }
    }
}

/// [`table_truth`] under `dialect` and its `options`.
fn truth_under<D: EvalDialect, DB: DatabaseLike>(
    dialect: &D,
    options: &D::Options,
    mut context: TableContext<'_, DB>,
    expression: &Expr,
    checks: Checks,
) -> Result<ExpressionTruth, LookupError> {
    let (table, database) = (context.table, context.database);
    let mut guarantees = Guarantees::new();
    for column in table.columns(database)? {
        let mut domain = column.data_type(database).domain(database);
        while let Some(current) = domain {
            context.value = Some(column);
            context.source = Source::Domain(current);
            for check in current.check_expressions() {
                if context.holds_across_statements(check, TableContext::runs_immutable_alike)? {
                    guarantees = implied(
                        check,
                        Outcomes::NOT_FALSE,
                        dialect,
                        options,
                        &mut context,
                        &guarantees,
                    )?;
                }
            }
            domain = current.data_type(database).domain(database);
        }
    }
    context.value = None;
    let mut generated = Vec::new();
    for column in table.columns(database)? {
        if let Some(expression) = column.generation_expression() {
            context.source = Source::Generated(column);
            if context.holds_across_statements(expression, TableContext::runs_immutable)? {
                generated.push((column, expression));
            }
        }
    }
    let mut seeding = Vec::new();
    if let Checks::Validated = checks {
        for check in table.check_constraints(database)? {
            if context.own.is_none_or(|own| !ptr::eq(check, own))
                && check.is_enforced()
                && !check.no_inherit()
                && check.is_validated(database)?
            {
                context.source = Source::Check(check);
                if context.holds_across_statements(
                    check.expression(database),
                    TableContext::runs_immutable_alike,
                )? {
                    seeding.push(check);
                }
            }
        }
    }
    // A check or a generated column may narrow a column only once another
    // has, so they are read again until a round narrows nothing, at most
    // once more than there are of them.
    for _ in 0..=generated.len() + seeding.len() {
        let before = guarantees.clone();
        for &(column, expression) in &generated {
            context.source = Source::Generated(column);
            let evaluation = evaluate(expression, dialect, options, &mut context, &guarantees)?;
            let held = context.generated(column, evaluation)?;
            guarantees.restrict(Subject::Reference(ptr::from_ref(column).addr()), held);
        }
        for check in &seeding {
            context.source = Source::Check(*check);
            guarantees = implied(
                check.expression(database),
                Outcomes::NOT_FALSE,
                dialect,
                options,
                &mut context,
                &guarantees,
            )?;
        }
        if guarantees == before || guarantees.is_impossible() {
            break;
        }
    }
    // No row satisfies every guarantee, so the expression meets no row.
    if guarantees.is_impossible() {
        return Ok(ExpressionTruth { outcomes: Outcomes::NONE, may_raise: false });
    }
    context.source = context.own.map_or(Source::Expression, Source::Check);
    let evaluation = evaluate(expression, dialect, options, &mut context, &guarantees)?;
    Ok(ExpressionTruth::from(&evaluation))
}

/// Where the expression the evaluator reads was written, which decides the
/// search path its calls resolve under.
enum Source<'a, DB: DatabaseLike> {
    /// The caller's expression, resolved under the database's path.
    Expression,
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
            Source::Expression => 0,
            Source::Check(check) => ptr::from_ref(*check).addr(),
            Source::Domain(domain) => ptr::from_ref(*domain).addr(),
            Source::Generated(column) => ptr::from_ref(*column).addr(),
        }
    }
}

/// The functions a call resolved to, by the address of where it was written
/// and of the column `VALUE` stood for.
struct Reached<'a, F> {
    source: usize,
    value: usize,
    call: Function,
    functions: Vec<&'a F>,
}

/// The table's columns and the database's functions, as the evaluator asks
/// for them.
struct TableContext<'a, DB: DatabaseLike> {
    table: &'a DB::Table,
    database: &'a DB,
    case: IdentifierCase,
    engine: Engine,
    /// The table's column scope, built on the first column reference.
    scope: Option<ColumnScope<'a, 'a, DB>>,
    /// The column `VALUE` stands for while a domain check is read.
    value: Option<&'a DB::Column>,
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
    ) -> Result<Resolved<usize>, LookupError> {
        let data_type = column.data_type(database);
        let base = data_type.base_type(database).unwrap_or(data_type);
        let rules = text_rules(column.collation(database)?, collations);
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
        let evaluation = Evaluation::new(
            scalar_type,
            column.is_nullable(database)?,
            false,
            Values::Unlisted(values),
        );
        Ok(Resolved { reference: ptr::from_ref(column).addr(), evaluation })
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
            volatility,
            kind,
            returns,
            function.returns_set(),
            ptr::from_ref(function).addr(),
        ))
    }
}

impl<DB: DatabaseLike> Context for TableContext<'_, DB> {
    type Reference = usize;
    type Error = LookupError;

    fn reference(&mut self, expression: &Expr) -> Result<Option<Resolved<usize>>, LookupError> {
        if let (Some(column), Expr::Identifier(name)) = (self.value, expression)
            && name.quote_style.is_none()
            && name.value.eq_ignore_ascii_case("value")
        {
            return self
                .engine
                .column(column, self.database, &mut self.collations, &self.enums)
                .map(Some);
        }
        if !matches!(expression, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            return Ok(None);
        }
        let (table, database, case, engine) = (self.table, self.database, self.case, self.engine);
        let scope = self.scope.get_or_insert_with(|| ColumnScope::for_table(table, database, case));
        match scope.resolve_column_definition(expression)? {
            Some(ColumnDefinition::Base { column, database, .. }) => {
                engine.column(column, database, &mut self.collations, &self.enums).map(Some)
            }
            _ => Ok(None),
        }
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

impl<'a, DB: DatabaseLike> TableContext<'a, DB> {
    /// The functions `call` may run, resolved under the search path of
    /// where it was written, or none when it names no known function or
    /// PostgreSQL reads it as a cast.
    fn runnable(&mut self, call: &Function) -> Result<Vec<&'a DB::Function>, LookupError> {
        let source = self.source.address();
        let value = self.value.map_or(0, |column| ptr::from_ref(column).addr());
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
            Source::Expression => database.functions_reached_by(call, case).map(Iterator::collect),
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
    /// not a column reference, or nothing for a call written with clauses
    /// beside its arguments.
    fn argument_type_names(&mut self, call: &Function) -> Result<Vec<Option<String>>, LookupError> {
        let Some(arguments) = plain_arguments(call) else { return Ok(Vec::new()) };
        let mut type_names = Vec::with_capacity(arguments.len());
        for argument in arguments {
            type_names.push(self.column_type_name(argument)?);
        }
        Ok(type_names)
    }

    /// The name of the type declared for the column `argument` names.
    fn column_type_name(&mut self, argument: &Expr) -> Result<Option<String>, LookupError> {
        let (table, database, case) = (self.table, self.database, self.case);
        if let (Some(column), Expr::Identifier(name)) = (self.value, argument)
            && name.quote_style.is_none()
            && name.value.eq_ignore_ascii_case("value")
        {
            return Ok(Some(column.data_type(database).name(database).into_owned()));
        }
        if !matches!(argument, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            return Ok(None);
        }
        let scope = self.scope.get_or_insert_with(|| ColumnScope::for_table(table, database, case));
        Ok(match scope.resolve_column_definition(argument)? {
            Some(ColumnDefinition::Base { column, database, .. }) => {
                Some(column.data_type(database).name(database).into_owned())
            }
            _ => None,
        })
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
        let mut answer = Ok(true);
        let _ = walk_expressions(expression, |node| {
            let Expr::Function(call) = node else { return ControlFlow::Continue(()) };
            match expression_form(&call.name) {
                Some(ExpressionForm::Value(_)) => {
                    answer = Ok(false);
                    return ControlFlow::Break(());
                }
                Some(ExpressionForm::CommonType | ExpressionForm::NullIf) => {
                    return ControlFlow::Continue(());
                }
                None => {}
            }
            match runs(self, call) {
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

    /// Whether `call` runs only immutable functions, the same ones under
    /// the path it was written with as under the database's search path and,
    /// when the check being read writes the same call, under that check's
    /// path.
    ///
    /// The guarantees a check seeds name its calls as written, so a call the
    /// database's search path resolves to other functions than the check's
    /// own path would carry them to calls of other functions.
    fn runs_immutable_alike(&mut self, call: &Function) -> Result<bool, LookupError> {
        let written = self.runnable(call)?;
        if !all_immutable(&written) {
            return Ok(false);
        }
        let own = self
            .own
            .filter(|own| writes_call(own.expression(self.database), call))
            .map(Source::Check);
        for path in core::iter::once(Source::Expression).chain(own) {
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
        let own =
            self.engine.column(column, database, &mut self.collations, &self.enums)?.evaluation;
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
