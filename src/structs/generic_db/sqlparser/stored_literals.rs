//! The date and time literals of a check or a policy expression, read as
//! PostgreSQL reads them while it parses the statement creating the
//! expression, under the session's `TimeZone` and `DateStyle` and at a time
//! of creation never known here.
//!
//! Literals take their types from comparisons, row fields, `BETWEEN`, `IN`,
//! `CASE` results and common-type calls, including domain, function and
//! operator results. Arrays and ranges read their elements and bounds under
//! those types. An operand whose type cannot be resolved leaves its literals as
//! written.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sql_values::{
    DateOrder, DateTimeError, InputContext, PgDate, PgTime, PgTimeTz, PgTimeZone, PgTimestamp,
    PgTimestampTz, PgType,
};
use sqlparser::ast::{
    ArrayElemTypeDef, BinaryOperator, CastKind, DataType, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, Ident, InTarget, ObjectName, ObjectNamePart, Query,
    TableFactor, TableWithJoins, TimezoneInfo, TypedString, Value, ValueWithSpan, Visitor,
    VisitorMut,
};

use super::{
    ParserDBBuilder, SessionSettings,
    function_bodies::{wrapping_query, wrapping_select},
    view_rewrites,
};
use crate::{
    structs::{
        ExpressionContext, IdentifierCase, OperandType, ParserDB, TableAttribute,
        metadata::{BoundLiteral, CheckLiterals, ClauseLiterals, UnreadableLiteral, respell},
    },
    utils::{
        coercion::{base, canonical_owned},
        postgres_types::PgType as CatalogType,
        walk::{walk, walk_expressions, walk_mut},
    },
};

mod containers;

use containers::{read_array, read_range};

/// The session state an expression's literals are read under.
#[derive(Clone, Copy)]
pub(super) struct LiteralSession<'a> {
    time_zone: Option<&'a PgTimeZone>,
    date_order: Option<DateOrder>,
}

impl<'a> LiteralSession<'a> {
    /// The state `settings` hold where the statement creating the expression
    /// runs.
    pub(super) fn of(settings: &'a SessionSettings) -> Self {
        Self {
            time_zone: settings.time_zone(),
            date_order: settings.date_style().and_then(date_order),
        }
    }

    /// The context PostgreSQL's input functions read, which never knows the
    /// time the expression was created at.
    fn context(self) -> InputContext<'a> {
        let context = InputContext::new();
        let context = match self.time_zone {
            Some(zone) => context.with_time_zone(zone),
            None => context,
        };
        match self.date_order {
            Some(order) => context.with_date_order(order),
            None => context,
        }
    }
}

/// The field order a `DateStyle` setting gives, as `check_datestyle` reads it,
/// or [`None`] where it names none or PostgreSQL refuses it.
///
/// A setting naming only an output style keeps the order already in effect,
/// which is not known here, except `German`, which reads `DMY`.
fn date_order(date_style: &str) -> Option<DateOrder> {
    let mut order = None;
    let mut style = None;
    for token in date_style.split(',').map(str::trim) {
        let is = |names: &[&str]| names.iter().any(|name| token.eq_ignore_ascii_case(name));
        let (named_order, named_style) = if is(&["YMD"]) {
            (Some(DateOrder::Ymd), None)
        } else if is(&["DMY", "EURO", "EUROPEAN"]) {
            (Some(DateOrder::Dmy), None)
        } else if is(&["MDY", "US", "NONEURO", "NONEUROPEAN"]) {
            (Some(DateOrder::Mdy), None)
        } else {
            let named =
                ["ISO", "SQL", "POSTGRES", "GERMAN"].into_iter().find(|name| is(&[name]))?;
            (None, Some(named))
        };
        // A keyword repeating the order or style already named is accepted.
        if let Some(named) = named_order
            && order.replace(named).is_some_and(|previous| previous != named)
        {
            return None;
        }
        if let Some(named) = named_style
            && style.replace(named).is_some_and(|previous| previous != named)
        {
            return None;
        }
    }
    order.or((style == Some("GERMAN")).then_some(DateOrder::Dmy))
}

/// The date and time types whose input reads session state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Temporal {
    Date,
    Timestamp,
    TimestampTz,
    Time,
    TimeTz,
}

impl Temporal {
    fn of(data_type: &DataType) -> Option<Self> {
        let zoned =
            |zone: &TimezoneInfo| matches!(zone, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz);
        match data_type {
            DataType::Date => Some(Self::Date),
            DataType::Timestamp(_, zone) if zoned(zone) => Some(Self::TimestampTz),
            DataType::Timestamp(..) => Some(Self::Timestamp),
            DataType::Time(_, zone) if zoned(zone) => Some(Self::TimeTz),
            DataType::Time(..) => Some(Self::Time),
            _ => None,
        }
    }

    /// The type a constant of this type is cast to, as `pg_policies` names it.
    fn data_type(self) -> DataType {
        match self {
            Self::Date => DataType::Date,
            Self::Timestamp => DataType::Timestamp(None, TimezoneInfo::WithoutTimeZone),
            Self::TimestampTz => DataType::Timestamp(None, TimezoneInfo::WithTimeZone),
            Self::Time => DataType::Time(None, TimezoneInfo::WithoutTimeZone),
            Self::TimeTz => DataType::Time(None, TimezoneInfo::WithTimeZone),
        }
    }

    fn pg_type(self) -> PgType {
        match self {
            Self::Date => PgType::Date,
            Self::Timestamp => PgType::Timestamp,
            Self::TimestampTz => PgType::TimestampTz,
            Self::Time => PgType::Time,
            Self::TimeTz => PgType::TimeTz,
        }
    }

    /// The type both convert to implicitly, as `select_common_type` picks it:
    /// a date widens to a timestamp and on to a `timestamptz`, a time to a
    /// `timetz`, and the two families do not meet.
    fn common(self, other: Self) -> Option<Self> {
        let place = |temporal| {
            match temporal {
                Self::Date => (0, 0),
                Self::Timestamp => (0, 1),
                Self::TimestampTz => (0, 2),
                Self::Time => (1, 0),
                Self::TimeTz => (1, 1),
            }
        };
        let ((family, rank), (other_family, other_rank)) = (place(self), place(other));
        (family == other_family).then_some(if rank >= other_rank { self } else { other })
    }

    /// The text PostgreSQL stores for `text` read as this type in `context`.
    fn read(self, text: &str, context: InputContext<'_>) -> Result<String, DateTimeError> {
        Ok(match self {
            Self::Date => PgDate::parse(text, context)?.to_string(),
            Self::Timestamp => PgTimestamp::parse(text, context)?.to_string(),
            Self::TimestampTz => PgTimestampTz::parse(text, context)?.to_string(),
            Self::Time => PgTime::parse(text, context)?.to_string(),
            Self::TimeTz => PgTimeTz::parse(text, context)?.to_string(),
        })
    }
}

/// A temporal input function, optionally applied to container members.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputType {
    Scalar(Temporal),
    Array(Temporal),
    Range(Temporal),
}

impl InputType {
    fn of(data_type: &DataType) -> Option<Self> {
        Temporal::of(data_type)
            .map(Self::Scalar)
            .or_else(|| Self::catalog(&CatalogType::of(data_type)))
    }

    fn catalog(data_type: &CatalogType) -> Option<Self> {
        match data_type {
            CatalogType::Catalog(name) => {
                Some(match *name {
                    "date" => Self::Scalar(Temporal::Date),
                    "timestamp" => Self::Scalar(Temporal::Timestamp),
                    "timestamptz" => Self::Scalar(Temporal::TimestampTz),
                    "time" => Self::Scalar(Temporal::Time),
                    "timetz" => Self::Scalar(Temporal::TimeTz),
                    "daterange" => Self::Range(Temporal::Date),
                    "tsrange" => Self::Range(Temporal::Timestamp),
                    "tstzrange" => Self::Range(Temporal::TimestampTz),
                    _ => return None,
                })
            }
            CatalogType::Array(element) => {
                match Self::catalog(element)? {
                    Self::Scalar(temporal) => Some(Self::Array(temporal)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn data_type(self) -> DataType {
        match self {
            Self::Scalar(temporal) => temporal.data_type(),
            Self::Array(temporal) => {
                DataType::Array(ArrayElemTypeDef::SquareBracket(
                    Box::new(temporal.data_type()),
                    None,
                ))
            }
            Self::Range(temporal) => {
                let name = match temporal {
                    Temporal::Date => "daterange",
                    Temporal::Timestamp => "tsrange",
                    _ => "tstzrange",
                };
                DataType::Custom(ObjectName::from(alloc::vec![Ident::new(name)]), Vec::new())
            }
        }
    }

    fn pg_type(self) -> PgType {
        match self {
            Self::Scalar(temporal) | Self::Array(temporal) | Self::Range(temporal) => {
                temporal.pg_type()
            }
        }
    }

    fn common(self, other: Self) -> Option<Self> {
        if self == other {
            return Some(self);
        }
        match (self, other) {
            (Self::Scalar(left), Self::Scalar(right)) => left.common(right).map(Self::Scalar),
            (Self::Array(left), Self::Array(right)) => left.common(right).map(Self::Array),
            _ => None,
        }
    }

    fn read(self, text: &str, context: InputContext<'_>) -> Result<String, DateTimeError> {
        match self {
            Self::Scalar(temporal) => temporal.read(text, context),
            Self::Array(temporal) => read_array(text, temporal, context),
            Self::Range(temporal) => read_range(text, temporal, context),
        }
    }
}

/// What an operand gives a literal beside it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operand {
    /// An untyped quoted literal, which takes its type from the other side.
    Untyped,
    /// A value of a date or time type.
    Typed(InputType),
    /// A value of another type, or of one not known here.
    Opaque,
}

/// The common type of `operands`, ignoring untyped literals, or [`None`] when
/// one is opaque, two do not meet or none is typed.
fn common(operands: impl IntoIterator<Item = Operand>) -> Option<InputType> {
    let mut found: Option<InputType> = None;
    for operand in operands {
        match operand {
            Operand::Untyped => {}
            Operand::Opaque => return None,
            Operand::Typed(temporal) => {
                found = Some(match found {
                    None => temporal,
                    Some(found) => found.common(temporal)?,
                });
            }
        }
    }
    found
}

/// The text of `expr` when it is an untyped quoted literal, in parentheses or
/// not, with the node holding it.
fn untyped_literal(expr: &Expr) -> Option<(&Expr, &str)> {
    match expr {
        Expr::Nested(inner) => untyped_literal(inner),
        Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(text), .. }) => {
            Some((expr, text))
        }
        _ => None,
    }
}

/// The functions whose untyped arguments take a type from the others.
#[derive(Clone, Copy)]
enum RowPure {
    /// `coalesce`, `greatest` and `least`, of their arguments' common type.
    Common,
    /// `nullif`, of its first argument's type.
    Nullif,
}

/// Which of [`RowPure`] `function` calls, with its arguments.
///
/// A quoted name calls a function of that name, which a schema may create.
fn row_pure(function: &Function) -> Option<(RowPure, Vec<&Expr>)> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    let is =
        |candidate: &str| name.quote_style.is_none() && name.value.eq_ignore_ascii_case(candidate);
    let kind = if ["coalesce", "greatest", "least"].into_iter().any(&is) {
        RowPure::Common
    } else if is("nullif") {
        RowPure::Nullif
    } else {
        return None;
    };
    let FunctionArguments::List(list) = &function.args else { return None };
    // A whole-row argument such as `t.*` is a record, beside which no literal
    // is typed here.
    let arguments = list
        .args
        .iter()
        .map(|argument| {
            match argument {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
                _ => None,
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some((kind, arguments))
}

/// Whether `expr` reads a column of its own query level, which keeps an `IN`
/// list item out of the common type PostgreSQL gives the list's constants.
fn reads_columns(expr: &Expr) -> bool {
    walk(expr, &mut OwnLevelColumns { queries: 0 }).is_break()
}

/// Breaks at a column reference outside every subquery, as
/// `contain_vars_of_level` finds one at level zero.
struct OwnLevelColumns {
    queries: usize,
}

impl Visitor for OwnLevelColumns {
    type Break = ();

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.queries += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.queries -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if self.queries == 0 && matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

fn address(expr: &Expr) -> usize {
    core::ptr::from_ref(expr).addr()
}

/// How a literal is written back.
#[derive(Clone, Copy)]
enum Form {
    /// An untyped literal, cast to the type it was read as.
    Untyped(InputType),
    /// A literal an explicit cast or a typed literal types, respelled in
    /// place.
    Typed,
}

/// The value a literal holds, for the node at `address`.
struct Coercion {
    address: usize,
    form: Form,
    stored: String,
}

/// Finds every literal of an expression read as a date or time value, with
/// the value PostgreSQL stores for it.
struct Coercions<'scope, 'query, 'db, 'session> {
    scope: Option<(&'scope ExpressionContext<'query, 'db, ParserDB>, &'db ParserDB)>,
    session: LiteralSession<'session>,
    /// Whether a literal stood beside a column, whose type only a scope gives.
    wants_columns: bool,
    found: Vec<Coercion>,
    unreadable: Option<UnreadableLiteral>,
}

impl<'scope, 'query, 'db, 'session> Coercions<'scope, 'query, 'db, 'session> {
    fn new(
        scope: Option<(&'scope ExpressionContext<'query, 'db, ParserDB>, &'db ParserDB)>,
        session: LiteralSession<'session>,
    ) -> Self {
        Self { scope, session, wants_columns: false, found: Vec::new(), unreadable: None }
    }

    fn finish(self) -> Result<Vec<Coercion>, UnreadableLiteral> {
        match self.unreadable {
            Some(unreadable) => Err(unreadable),
            None => Ok(self.found),
        }
    }

    /// Reads `text`, held by `node`, as `temporal`.
    fn coerce(&mut self, node: &Expr, text: &str, temporal: InputType, form: Form) {
        if self.unreadable.is_some() {
            return;
        }
        match temporal.read(text, self.session.context()) {
            Ok(stored) => self.found.push(Coercion { address: address(node), form, stored }),
            Err(reason) => {
                self.unreadable = Some(UnreadableLiteral {
                    literal: text.to_string(),
                    data_type: temporal.pg_type(),
                    reason,
                });
            }
        }
    }

    /// Reads `expr` as `temporal` when it is an untyped literal.
    fn coerce_untyped(&mut self, expr: &Expr, temporal: InputType) {
        if let Some((node, text)) = untyped_literal(expr) {
            self.coerce(node, text, temporal, Form::Untyped(temporal));
        }
    }

    /// Types an untyped literal on one side of a comparison by the other.
    fn compared(&mut self, left: &Expr, right: &Expr) {
        if let (Expr::Tuple(left), Expr::Tuple(right)) = (left, right) {
            if left.len() == right.len() {
                for (left, right) in left.iter().zip(right) {
                    self.compared(left, right);
                }
            }
            return;
        }
        for (literal, other) in [(left, right), (right, left)] {
            if untyped_literal(literal).is_some()
                && let Operand::Typed(temporal) = self.operand(other)
            {
                self.coerce_untyped(literal, temporal);
            }
        }
    }

    /// Types every untyped literal among `operands` by their common type.
    fn by_common_type(&mut self, operands: &[&Expr]) {
        if !operands.iter().any(|operand| untyped_literal(operand).is_some()) {
            return;
        }
        let types: Vec<Operand> = operands.iter().map(|operand| self.operand(operand)).collect();
        if let Some(temporal) = common(types) {
            for operand in operands {
                self.coerce_untyped(operand, temporal);
            }
        }
    }

    fn operand(&mut self, expr: &Expr) -> Operand {
        let declared =
            |data_type: &DataType| InputType::of(data_type).map_or(Operand::Opaque, Operand::Typed);
        match expr {
            Expr::Nested(inner) => self.operand(inner),
            Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(_), .. }) => {
                Operand::Untyped
            }
            Expr::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                data_type,
                format: None,
                ..
            }
            | Expr::TypedString(TypedString { data_type, .. }) => declared(data_type),
            Expr::Function(function) => {
                match row_pure(function) {
                    Some((RowPure::Common, arguments)) => {
                        let types: Vec<Operand> =
                            arguments.iter().map(|argument| self.operand(argument)).collect();
                        common(types).map_or(Operand::Opaque, Operand::Typed)
                    }
                    // A literal first argument takes the second's type.
                    Some((RowPure::Nullif, arguments)) => {
                        arguments
                            .iter()
                            .map(|argument| self.operand(argument))
                            .find(|operand| *operand != Operand::Untyped)
                            .unwrap_or(Operand::Opaque)
                    }
                    None => self.resolved(expr),
                }
            }
            Expr::Case { conditions, else_result, .. } => {
                let types: Vec<_> = else_result
                    .iter()
                    .map(AsRef::as_ref)
                    .chain(conditions.iter().map(|condition| &condition.result))
                    .map(|result| self.operand(result))
                    .collect();
                common(types).map_or(Operand::Opaque, Operand::Typed)
            }
            Expr::Value(ValueWithSpan { value: Value::Null, .. }) => Operand::Untyped,
            _ => self.resolved(expr),
        }
    }

    /// Resolves columns, domains and selected call or operator results.
    fn resolved(&mut self, expression: &Expr) -> Operand {
        self.wants_columns = true;
        let Some((context, database)) = self.scope else { return Operand::Opaque };
        let Ok(OperandType::Known { type_name, .. }) = context.type_of(expression) else {
            return Operand::Opaque;
        };
        let declared = canonical_owned(database, CatalogType::from_target(&type_name));
        let Ok(declared) = base(database, &declared) else { return Operand::Opaque };
        InputType::catalog(&declared).map_or(Operand::Opaque, Operand::Typed)
    }
}

impl Visitor for Coercions<'_, '_, '_, '_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::Case { operand, conditions, else_result, .. } => {
                if let Some(operand) = operand {
                    for condition in conditions {
                        self.compared(operand, &condition.condition);
                    }
                }
                let results: Vec<_> = else_result
                    .iter()
                    .map(AsRef::as_ref)
                    .chain(conditions.iter().map(|condition| &condition.result))
                    .collect();
                self.by_common_type(&results);
            }
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
                if untyped_literal(right).is_some()
                    && let Operand::Typed(InputType::Scalar(temporal)) = self.operand(left)
                {
                    self.coerce_untyped(right, InputType::Array(temporal));
                }
            }
            Expr::BinaryOp {
                left,
                op:
                    BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq,
                right,
            }
            | Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right) => self.compared(left, right),
            Expr::Between { expr: subject, low, high, .. } => {
                if untyped_literal(subject).is_some() {
                    // PostgreSQL compares the subject with each bound apart,
                    // so it takes one type only where both bounds give it.
                    if let (Operand::Typed(low), Operand::Typed(high)) =
                        (self.operand(low), self.operand(high))
                        && low == high
                    {
                        self.coerce_untyped(subject, low);
                    }
                } else if (untyped_literal(low).is_some() || untyped_literal(high).is_some())
                    && let Operand::Typed(temporal) = self.operand(subject)
                {
                    self.coerce_untyped(low, temporal);
                    self.coerce_untyped(high, temporal);
                }
            }
            Expr::In { expr: subject, target: InTarget::List(list), .. } => {
                if list.iter().any(|item| untyped_literal(item).is_some())
                    && let Operand::Typed(temporal) = self.operand(subject)
                {
                    // Two constants or more are compared under the common type
                    // they share with the subject, and a lone one with the
                    // subject alone.
                    let constants: Vec<&Expr> =
                        list.iter().filter(|item| !reads_columns(item)).collect();
                    let target = if constants.len() > 1 {
                        let types: Vec<Operand> =
                            constants.iter().map(|constant| self.operand(constant)).collect();
                        common(core::iter::once(Operand::Typed(temporal)).chain(types))
                    } else {
                        Some(temporal)
                    };
                    if let Some(target) = target {
                        for item in list {
                            self.coerce_untyped(item, target);
                        }
                    }
                }
            }
            // `nullif` compares its two arguments, which types a literal as
            // their common type.
            Expr::Function(function) => {
                if let Some((_, arguments)) = row_pure(function) {
                    self.by_common_type(&arguments);
                }
            }
            Expr::Cast {
                kind: CastKind::Cast | CastKind::DoubleColon,
                expr: inner,
                data_type,
                format: None,
            } => {
                if let Some(temporal) = InputType::of(data_type)
                    && let Some((node, text)) = untyped_literal(inner)
                {
                    self.coerce(node, text, temporal, Form::Typed);
                }
            }
            Expr::TypedString(TypedString {
                data_type,
                value: ValueWithSpan { value: Value::SingleQuotedString(text), .. },
                ..
            }) => {
                if let Some(temporal) = InputType::of(data_type) {
                    self.coerce(expr, text, temporal, Form::Typed);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// Whether `expr` holds a quoted literal anywhere.
fn holds_quoted_literal(expr: &Expr) -> bool {
    walk_expressions(expr, |node| {
        match node {
            Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(_), .. })
            | Expr::TypedString(_) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    })
    .is_break()
}

/// Builds the relation, function and type catalog used to read `queries`.
fn catalog<'query>(
    builder: &ParserDBBuilder,
    queries: impl IntoIterator<Item = &'query Query>,
) -> ParserDB {
    let mut database = view_rewrites::catalog(builder, queries);
    database.functions = builder.functions().to_vec();
    database.builtins = builder.builtins().clone();
    database.domains = builder.domains().to_vec();
    database.user_defined_types = builder.user_defined_types().to_vec();
    database.extensions = builder.extensions().to_vec();
    database
}

/// Every literal of `query` read as a date or time value, with the value
/// stored, or the first one whose value is not known here.
fn coercions(
    database: &ParserDB,
    query: &Query,
    session: LiteralSession<'_>,
) -> Result<Vec<Coercion>, UnreadableLiteral> {
    let mut unscoped = Coercions::new(None, session);
    let walked = walk(query, &mut unscoped);
    debug_assert!(walked.is_continue(), "the walk never breaks");
    if !unscoped.wants_columns {
        return unscoped.finish();
    }
    let scope = ExpressionContext::new(query, database, IdentifierCase::AsWritten).ok();
    let mut scoped = Coercions::new(scope.as_ref().map(|scope| (scope, database)), session);
    let walked = walk(query, &mut scoped);
    debug_assert!(walked.is_continue(), "the walk never breaks");
    scoped.finish()
}

/// Reads the stored literals of the clause wrapped in `query`.
fn read(
    database: &ParserDB,
    query: &Query,
    session: LiteralSession<'_>,
) -> Result<Vec<BoundLiteral>, UnreadableLiteral> {
    coercions(database, query, session).map(|coercions| placed(query, coercions))
}

/// Wraps a clause on `table` in the query supplying its column scope.
fn clause_query(table: &ObjectName, clause: Expr) -> Query {
    let from = TableWithJoins {
        relation: TableFactor::Table {
            name: table.clone(),
            alias: None,
            args: None,
            with_hints: Vec::new(),
            version: None,
            with_ordinality: false,
            partitions: Vec::new(),
            json_path: None,
            sample: None,
            index_directives: Vec::new(),
            inheritance: None,
        },
        joins: Vec::new(),
    };
    wrapping_query(wrapping_select(Vec::new(), alloc::vec![from], Some(clause)))
}

/// The literals `coercions` reads in the clause of the wrapper `query`, each
/// by its place among the clause's nodes.
fn placed(query: &Query, mut coercions: Vec<Coercion>) -> Vec<BoundLiteral> {
    let mut bound = Vec::with_capacity(coercions.len());
    // The clause is the first expression of the wrapper query, so its nodes
    // are counted from the first one met.
    let mut place = 0;
    let _ = walk_expressions(query, |node| {
        let at = address(node);
        if let Some(index) = coercions.iter().position(|coercion| coercion.address == at) {
            let Coercion { form, stored, .. } = coercions.swap_remove(index);
            let cast = match form {
                Form::Untyped(temporal) => Some(temporal.data_type()),
                Form::Typed => None,
            };
            bound.push(BoundLiteral { place, cast, stored });
        }
        place += 1;
        if coercions.is_empty() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    });
    bound
}

/// Rewrites every date and time literal of `expression`, a clause of a policy
/// on `table`, as the value PostgreSQL stores for it under `session`, keeping
/// the clause as written beside it, or answers the first literal whose value
/// is not known here and leaves the clause as written.
pub(super) fn bind(
    builder: &ParserDBBuilder,
    table: &ObjectName,
    expression: &mut Expr,
    session: LiteralSession<'_>,
) -> ClauseLiterals {
    if !holds_quoted_literal(expression) {
        return ClauseLiterals::AsWritten;
    }
    let clause = core::mem::replace(expression, Expr::Value(Value::Null.with_empty_span()));
    let mut query = clause_query(table, clause);
    let database = catalog(builder, core::iter::once(&query));
    let literals = read(&database, &query, session);
    let walked = walk_mut(&mut query, &mut Restore { target: expression });
    debug_assert!(walked.is_break(), "the clause is the first expression of the wrapper query");
    match literals {
        Ok(bound) if bound.is_empty() => ClauseLiterals::AsWritten,
        Ok(bound) => {
            let written = expression.clone();
            respell(expression, &bound);
            // A typed literal can store the very text it was written with.
            if *expression == written {
                ClauseLiterals::AsWritten
            } else {
                ClauseLiterals::Stored(Box::new(written))
            }
        }
        Err(unreadable) => ClauseLiterals::Unreadable(unreadable),
    }
}

/// Reads the literals of every check created since the last call under
/// `session`, the session of the statement creating them.
pub(super) fn bind_pending_checks(builder: &mut ParserDBBuilder, session: LiteralSession<'_>) {
    let pending: Vec<_> = builder
        .check_constraints()
        .iter()
        .enumerate()
        .filter(|(_, (_, metadata))| *metadata.literals() == CheckLiterals::Pending)
        .map(|(index, (check, metadata))| {
            let query = holds_quoted_literal(metadata.expression()).then(|| {
                clause_query(
                    &TableAttribute::table(check.as_ref()).name,
                    metadata.expression().clone(),
                )
            });
            (index, query)
        })
        .collect();
    let database = pending
        .iter()
        .any(|(_, query)| query.is_some())
        .then(|| catalog(builder, pending.iter().filter_map(|(_, query)| query.as_ref())));
    let checks = builder.check_constraints_mut();
    for (index, query) in pending {
        let literals = match (database.as_ref(), query.as_ref()) {
            (Some(database), Some(query)) => {
                match read(database, query, session) {
                    Ok(bound) if bound.is_empty() => CheckLiterals::AsWritten,
                    Ok(bound) => CheckLiterals::Stored(bound),
                    Err(unreadable) => CheckLiterals::Unreadable(unreadable),
                }
            }
            _ => CheckLiterals::AsWritten,
        };
        checks[index].1.set_literals(literals);
    }
}

/// Swaps `target` with the first expression a walk of the wrapper query meets,
/// which is its clause, and stops.
struct Restore<'a> {
    target: &'a mut Expr,
}

impl VisitorMut for Restore<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        core::mem::swap(self.target, expr);
        ControlFlow::Break(())
    }
}
