//! The date and time literals of a policy expression, read as PostgreSQL reads
//! them while it parses `CREATE POLICY` and `ALTER POLICY`, under the session's
//! `TimeZone` and `DateStyle` and at a time of creation never known here.
//!
//! An untyped literal takes its type from the other side of a comparison, the
//! subject of `BETWEEN` or `IN`, or the common type of the arguments of
//! `coalesce`, `nullif`, `greatest` and `least`, where that side is a table's
//! column or a cast, and stays as written anywhere else.

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
    BinaryOperator, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, InTarget, ObjectName, ObjectNamePart, Query, Select, TableFactor,
    TableWithJoins, TimezoneInfo, TypedString, Value, ValueWithSpan, Visit, VisitMut, Visitor,
    VisitorMut, visit_expressions,
};

use super::{
    ParserDBBuilder, SessionSettings,
    function_bodies::{wrapping_query, wrapping_select},
    view_rewrites,
};
use crate::{
    structs::{
        ColumnDefinition, ColumnDefinitionScope, ColumnScope, IdentifierCase, ParserDB,
        metadata::{ClauseLiterals, UnreadableLiteral},
    },
    traits::ColumnLike,
};

/// The session state a policy's literals are read under.
#[derive(Clone, Copy)]
pub(super) struct LiteralSession<'a> {
    time_zone: Option<&'a PgTimeZone>,
    date_order: Option<DateOrder>,
}

impl<'a> LiteralSession<'a> {
    /// The state `settings` hold where the policy statement runs.
    pub(super) fn of(settings: &'a SessionSettings) -> Self {
        Self {
            time_zone: settings.time_zone(),
            date_order: settings.date_style().and_then(date_order),
        }
    }

    /// The context PostgreSQL's input functions read, which never knows the
    /// time the policy was created at.
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

/// What an operand gives a literal beside it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operand {
    /// An untyped quoted literal, which takes its type from the other side.
    Untyped,
    /// A value of a date or time type.
    Typed(Temporal),
    /// A value of another type, or of one not known here.
    Opaque,
}

/// The common type of `operands`, ignoring untyped literals, or [`None`] when
/// one is opaque, two do not meet or none is typed.
fn common(operands: impl IntoIterator<Item = Operand>) -> Option<Temporal> {
    let mut found: Option<Temporal> = None;
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
    expr.visit(&mut OwnLevelColumns { queries: 0 }).is_break()
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
    Untyped(Temporal),
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

/// Finds every literal of a policy expression read as a date or time value,
/// with the value PostgreSQL stores for it.
struct Coercions<'scope, 'query, 'db, 'session> {
    scope: Option<(&'scope ColumnScope<'query, 'db, ParserDB>, &'db ParserDB)>,
    /// The scope of each select the walk stands in, innermost last.
    selects: Vec<Option<ColumnDefinitionScope<'scope, 'query, 'db, ParserDB>>>,
    session: LiteralSession<'session>,
    /// Whether a literal stood beside a column, whose type only a scope gives.
    wants_columns: bool,
    found: Vec<Coercion>,
    unreadable: Option<UnreadableLiteral>,
}

impl<'scope, 'query, 'db, 'session> Coercions<'scope, 'query, 'db, 'session> {
    fn new(
        scope: Option<(&'scope ColumnScope<'query, 'db, ParserDB>, &'db ParserDB)>,
        session: LiteralSession<'session>,
    ) -> Self {
        Self {
            scope,
            selects: Vec::new(),
            session,
            wants_columns: false,
            found: Vec::new(),
            unreadable: None,
        }
    }

    fn finish(self) -> Result<Vec<Coercion>, UnreadableLiteral> {
        match self.unreadable {
            Some(unreadable) => Err(unreadable),
            None => Ok(self.found),
        }
    }

    /// Reads `text`, held by `node`, as `temporal`.
    fn coerce(&mut self, node: &Expr, text: &str, temporal: Temporal, form: Form) {
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
    fn coerce_untyped(&mut self, expr: &Expr, temporal: Temporal) {
        if let Some((node, text)) = untyped_literal(expr) {
            self.coerce(node, text, temporal, Form::Untyped(temporal));
        }
    }

    /// Types an untyped literal on one side of a comparison by the other.
    fn compared(&mut self, left: &Expr, right: &Expr) {
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
            |data_type: &DataType| Temporal::of(data_type).map_or(Operand::Opaque, Operand::Typed);
        match expr {
            Expr::Nested(inner) => self.operand(inner),
            Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(_), .. }) => {
                Operand::Untyped
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => self.column(expr),
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
                    None => Operand::Opaque,
                }
            }
            _ => Operand::Opaque,
        }
    }

    /// The type of the column `reference` names in the innermost select.
    fn column(&mut self, reference: &Expr) -> Operand {
        self.wants_columns = true;
        let Some((_, database)) = self.scope else { return Operand::Opaque };
        let Some(Some(select)) = self.selects.last() else { return Operand::Opaque };
        match select.resolve_column_definition(reference) {
            Ok(Some(ColumnDefinition::Base { column, .. })) => {
                Temporal::of(column.data_type(database)).map_or(Operand::Opaque, Operand::Typed)
            }
            _ => Operand::Opaque,
        }
    }
}

impl Visitor for Coercions<'_, '_, '_, '_> {
    type Break = ();

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
        let scope = self.scope.and_then(|(scope, _)| scope.scope_for_select(select));
        self.selects.push(scope);
        ControlFlow::Continue(())
    }

    fn post_visit_select(&mut self, _select: &Select) -> ControlFlow<Self::Break> {
        self.selects.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        match expr {
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
                if let Some(temporal) = Temporal::of(data_type)
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
                if let Some(temporal) = Temporal::of(data_type) {
                    self.coerce(expr, text, temporal, Form::Typed);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// Writes the value of every literal `coercions` reads back into its node.
struct Respell {
    coercions: Vec<Coercion>,
}

impl VisitorMut for Respell {
    type Break = ();

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        let at = address(expr);
        let Some(index) = self.coercions.iter().position(|coercion| coercion.address == at) else {
            return ControlFlow::Continue(());
        };
        let Coercion { form, stored, .. } = self.coercions.swap_remove(index);
        match (form, expr) {
            (Form::Untyped(temporal), expr) => {
                *expr = Expr::Cast {
                    kind: CastKind::DoubleColon,
                    expr: Box::new(Expr::Value(
                        Value::SingleQuotedString(stored).with_empty_span(),
                    )),
                    data_type: temporal.data_type(),
                    format: None,
                };
            }
            (Form::Typed, Expr::TypedString(typed)) => {
                typed.value.value = Value::SingleQuotedString(stored);
            }
            // The literal a cast holds.
            (Form::Typed, expr) => {
                *expr = Expr::Value(Value::SingleQuotedString(stored).with_empty_span());
            }
        }
        ControlFlow::Continue(())
    }
}

/// Whether `expr` holds a quoted literal anywhere.
fn holds_quoted_literal(expr: &Expr) -> bool {
    visit_expressions(expr, |node| {
        match node {
            Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(_), .. })
            | Expr::TypedString(_) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    })
    .is_break()
}

/// Every literal of `query` read as a date or time value, with the value
/// stored, or the first one whose value is not known here.
fn coercions(
    builder: &ParserDBBuilder,
    query: &Query,
    session: LiteralSession<'_>,
) -> Result<Vec<Coercion>, UnreadableLiteral> {
    let mut unscoped = Coercions::new(None, session);
    let walk = query.visit(&mut unscoped);
    debug_assert!(walk.is_continue(), "the walk never breaks");
    if !unscoped.wants_columns {
        return unscoped.finish();
    }
    let read = view_rewrites::catalog(builder, core::iter::once(query));
    // A query whose scopes cannot be derived types only its casts.
    let scope = ColumnScope::from_query(query, &read, IdentifierCase::AsWritten).ok();
    let mut scoped = Coercions::new(scope.as_ref().map(|scope| (scope, &read)), session);
    let walk = query.visit(&mut scoped);
    debug_assert!(walk.is_continue(), "the walk never breaks");
    scoped.finish()
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
    let mut query = wrapping_query(wrapping_select(Vec::new(), alloc::vec![from], Some(clause)));
    let literals = match coercions(builder, &query, session) {
        Ok(coercions) if coercions.is_empty() => ClauseLiterals::AsWritten,
        Ok(coercions) => {
            // The clause is the first expression of the wrapper query.
            let written =
                visit_expressions(&query, |clause| ControlFlow::Break(Box::new(clause.clone())))
                    .break_value();
            let walk = VisitMut::visit(&mut query, &mut Respell { coercions });
            debug_assert!(walk.is_continue(), "the respelling never breaks");
            written.map_or(ClauseLiterals::AsWritten, ClauseLiterals::Stored)
        }
        Err(unreadable) => ClauseLiterals::Unreadable(unreadable),
    };
    let walk = VisitMut::visit(&mut query, &mut Restore { target: expression });
    debug_assert!(walk.is_break(), "the clause is the first expression of the wrapper query");
    match literals {
        // A typed literal can store the very text it was written with.
        ClauseLiterals::Stored(written) if *written == *expression => ClauseLiterals::AsWritten,
        literals => literals,
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
