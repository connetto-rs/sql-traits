//! The facts each expression node of one query takes.

use alloc::{string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, ObjectNamePart, Query, UnaryOperator, Value,
};

use crate::{
    errors::LookupError,
    impls::dql::definition_graph::reference_sites::SiteRule,
    structs::{
        ColumnBinding, ColumnDefinition, ColumnScope, IdentifierCase, OperandType,
        OperatorResolution, SelectedOperator, TargetName,
    },
    traits::{ColumnLike, DataTypeLike, DatabaseLike},
    utils::{
        coercion::{canonical_owned, type_name},
        object_name::PG_CATALOG,
        operator_typing::resolve_operator,
        postgres_types::PgType,
        type_modifier,
        walk::walk_expressions,
    },
};

/// The binding and type of every expression node of one borrowed query, as
/// PostgreSQL derives them in the scope the node sits in.
///
/// A node is answered by identity, so a node borrowed from a copy of the
/// query is refused with [`LookupError::ExpressionNotInQuery`].
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
/// use sqlparser::{
///     ast::{SelectItem, SetExpr, Statement},
///     dialect::PostgreSqlDialect,
///     parser::Parser,
/// };
///
/// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE docs (title VARCHAR(5));")?;
/// let parsed = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT title, 1 FROM docs")?;
/// let Statement::Query(query) = &parsed[0] else { unreachable!() };
/// let context = ExpressionContext::new(query, &db, IdentifierCase::Folded)?;
/// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
/// let types = select
///     .projection
///     .iter()
///     .map(|item| {
///         let SelectItem::UnnamedExpr(expression) = item else { unreachable!() };
///         context.type_of(expression)
///     })
///     .collect::<Vec<_>>();
/// let varchar = TargetName::new("varchar", false).with_schema("pg_catalog", false);
/// let int4 = TargetName::new("int4", false).with_schema("pg_catalog", false);
/// assert_eq!(
///     types,
///     [
///         Ok(OperandType::Known { type_name: varchar, typmod: Some(9) }),
///         Ok(OperandType::Known { type_name: int4, typmod: None }),
///     ]
/// );
/// # Ok::<(), sql_traits::errors::Error>(())
/// ```
pub struct ExpressionContext<'query, 'db, DB: DatabaseLike> {
    database: &'db DB,
    scope: ColumnScope<'query, 'db, DB>,
    /// The address of every expression node of the query, sorted.
    nodes: Vec<usize>,
    /// The address of every column reference of the query, with the rule its
    /// clause resolves it by, sorted by address.
    sites: Vec<(usize, Option<SiteRule>)>,
    case: IdentifierCase,
}

/// One operator PostgreSQL applies to evaluate an expression node, with the
/// nodes its operands come from and the overload it selects.
#[derive(Debug, Clone)]
pub struct OperatorApplication<'node, 'db, DB: DatabaseLike> {
    name: TargetName<'node>,
    left: Option<&'node Expr>,
    right: &'node Expr,
    resolution: OperatorResolution<'db, DB>,
}

impl<'node, 'db, DB: DatabaseLike> OperatorApplication<'node, 'db, DB> {
    /// Returns the operator name PostgreSQL looks up, schema-qualified when
    /// the query writes `OPERATOR(schema.op)`.
    #[must_use]
    pub const fn name(&self) -> &TargetName<'node> {
        &self.name
    }

    /// Returns the node the left operand comes from, or `None` for a prefix
    /// operator.
    #[must_use]
    pub const fn left(&self) -> Option<&'node Expr> {
        self.left
    }

    /// Returns the node the right operand comes from.
    #[must_use]
    pub const fn right(&self) -> &'node Expr {
        self.right
    }

    /// Returns what PostgreSQL does with the invocation, its operands typed
    /// as [`ExpressionContext::type_of`] types their nodes.
    #[must_use]
    pub const fn resolution(&self) -> &OperatorResolution<'db, DB> {
        &self.resolution
    }
}

/// How PostgreSQL evaluates a node, by the operator it applies.
enum Form<'node> {
    /// The node applies no operator of its own.
    Plain,
    /// An `OpExpr`, returning the operator's result.
    Operator(Applied<'node>),
    /// `IS [NOT] DISTINCT FROM`, a `DistinctExpr` applying `=` and returning
    /// `bool`.
    Distinct(Applied<'node>),
    /// `NULLIF`, a `NullIfExpr` applying `=` and returning its first operand
    /// as `=` converts it.
    NullIf(Applied<'node>),
    /// `IS [NOT] DISTINCT FROM` an undecorated `NULL`, a `NullTest` on the
    /// other operand, applying no operator and returning `bool`.
    NullTest,
}

/// An operator name applied to operand nodes.
struct Applied<'node> {
    name: TargetName<'node>,
    left: Option<&'node Expr>,
    right: &'node Expr,
}

impl<'node> Applied<'node> {
    /// `name`, from `pg_catalog` or the search path, applied to `left` and
    /// `right`.
    fn named(name: &'static str, left: Option<&'node Expr>, right: &'node Expr) -> Self {
        Self { name: TargetName::new(name, false), left, right }
    }
}

impl<'query, 'db, DB: DatabaseLike> ExpressionContext<'query, 'db, DB> {
    /// Builds the context of `query` over `database`, comparing identifiers
    /// under `case`.
    ///
    /// # Errors
    ///
    /// Returns the lookup errors [`ColumnScope::from_query`] returns.
    pub fn new(
        query: &'query Query,
        database: &'db DB,
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        let scope = ColumnScope::from_query(query, database, case)?;
        let mut nodes = Vec::new();
        let _ = walk_expressions(query, |expression| {
            nodes.push(address(expression));
            ControlFlow::<()>::Continue(())
        });
        nodes.sort_unstable();
        let mut sites = scope.site_rules(query);
        sites.sort_unstable_by_key(|(address, _)| *address);
        Ok(Self { database, scope, nodes, sites, case })
    }

    /// Returns the definition and binding of the column `expression`
    /// references, resolved as the clause holding it resolves it, or `None`
    /// for a node that is no column reference or binds nothing.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// query, and the lookup errors resolving the reference returns.
    pub fn reference(
        &self,
        expression: &Expr,
    ) -> Result<Option<(ColumnDefinition<'_, 'query, 'db, DB>, ColumnBinding<'_>)>, LookupError>
    {
        self.check(expression)?;
        let address = address(expression);
        let rule = self
            .sites
            .binary_search_by_key(&address, |(held, _)| *held)
            .ok()
            .and_then(|index| self.sites[index].1);
        match (expression, rule) {
            (Expr::Identifier(_) | Expr::CompoundIdentifier(_), Some(rule)) => {
                self.scope.resolve_site(rule, expression)
            }
            _ => Ok(None),
        }
    }

    /// Returns the type `expression` takes, with the modifier it carries,
    /// [`OperandType::UntypedLiteral`] for an untyped literal, and
    /// [`OperandType::Unknown`] where a fact the type needs is missing.
    ///
    /// An operator node takes the result of the operator PostgreSQL selects,
    /// `IS [NOT] DISTINCT FROM` is `bool`, and `NULLIF` takes its first
    /// operand's type as `=` converts it. A `-` over a numeric literal is
    /// the negative constant PostgreSQL's grammar folds it into.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// query, and the lookup errors resolving a column reference returns.
    pub fn type_of(&self, expression: &Expr) -> Result<OperandType<'_>, LookupError> {
        self.check(expression)?;
        Ok(match expression {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                match self.reference(expression)? {
                    Some((ColumnDefinition::Base { column, database, .. }, _)) => {
                        column_type(database, column.data_type(database))
                    }
                    // An output a `SELECT` of this query defines takes the
                    // type of its defining expression.
                    Some((ColumnDefinition::Expression { expression, .. }, _))
                        if self.holds(expression) =>
                    {
                        self.type_of(expression)?
                    }
                    _ => OperandType::Unknown,
                }
            }
            Expr::Value(value) => literal_type(&value.value),
            Expr::TypedString(typed) => declared_type(self.database, &typed.data_type),
            Expr::Cast { kind: CastKind::Cast | CastKind::DoubleColon, data_type, .. } => {
                declared_type(self.database, data_type)
            }
            Expr::Nested(inner) | Expr::Collate { expr: inner, .. } => self.type_of(inner)?,
            _ => {
                match (folded(expression), form(expression)) {
                    (Some((digits, negative)), _) => catalog(number_type(digits, negative)),
                    (None, Some(Form::Operator(applied))) => {
                        match self.resolution(&applied)? {
                            OperatorResolution::Selected(selected) => {
                                selected
                                    .into_result_type()
                                    .map_or(OperandType::Unknown, |type_name| {
                                        OperandType::Known { type_name, typmod: None }
                                    })
                            }
                            _ => OperandType::Unknown,
                        }
                    }
                    (None, Some(Form::Distinct(applied))) => {
                        match self.resolution(&applied)? {
                            OperatorResolution::Selected(selected) if returns_bool(&selected) => {
                                catalog("bool")
                            }
                            _ => OperandType::Unknown,
                        }
                    }
                    (None, Some(Form::NullIf(applied))) => self.nullif_type(&applied)?,
                    (None, Some(Form::NullTest)) => catalog("bool"),
                    (None, Some(Form::Plain) | None) => OperandType::Unknown,
                }
            }
        })
    }

    /// Returns the operators PostgreSQL applies to evaluate `expression`
    /// itself, in the order it builds them, or `None` for a node whose
    /// operators the context does not derive.
    ///
    /// A column reference, a literal, a typed string, a cast, a `COLLATE`
    /// and a `-` folded into a numeric literal apply none. A binary or
    /// prefix operator, `[NOT] LIKE` and `[NOT] ILIKE` apply one operator
    /// (`~~`, `!~~`, `~~*`, `!~~*`), and `IS [NOT] DISTINCT FROM` and
    /// `NULLIF` apply `=`, except that `IS [NOT] DISTINCT FROM` an
    /// undecorated `NULL` tests the other operand for null and applies none.
    /// A parenthesized node answers for its content.
    /// Row comparisons, `LIKE ... ESCAPE`, `IN`, `BETWEEN`, `SIMILAR TO`,
    /// `ANY`, `ALL`, the boolean connectives and calls answer `None`.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// query, and the lookup errors typing an operand returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (i INT4);")?;
    /// let parsed = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT i > 1.5 FROM t")?;
    /// let Statement::Query(query) = &parsed[0] else { unreachable!() };
    /// let context = ExpressionContext::new(query, &db, IdentifierCase::Folded)?;
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let SelectItem::UnnamedExpr(comparison) = &select.projection[0] else { unreachable!() };
    ///
    /// let applications = context.operators(comparison)?.unwrap_or_default();
    /// let [greater] = applications.as_slice() else { unreachable!() };
    /// assert_eq!(greater.name().name(), ">");
    /// // `i` converts to `numeric`, so `numeric > numeric` runs.
    /// let OperatorResolution::Selected(selected) = greater.resolution() else { unreachable!() };
    /// assert_eq!(selected.left().map(|left| left.target().name()), Some("numeric"));
    /// assert_eq!(selected.result_type().map(TargetName::name), Some("bool"));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn operators<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<Vec<OperatorApplication<'node, 'db, DB>>>, LookupError> {
        self.check(expression)?;
        if let Expr::Nested(inner) = expression {
            return self.operators(inner);
        }
        if folded(expression).is_some() {
            return Ok(Some(Vec::new()));
        }
        let applied = match form(expression) {
            None => return Ok(None),
            Some(Form::Plain | Form::NullTest) => return Ok(Some(Vec::new())),
            Some(Form::Operator(applied) | Form::Distinct(applied) | Form::NullIf(applied)) => {
                applied
            }
        };
        let resolution = self.resolution(&applied)?;
        let Applied { name, left, right } = applied;
        Ok(Some(Vec::from([OperatorApplication { name, left, right, resolution }])))
    }

    /// What PostgreSQL does with `applied`, its operands typed by their nodes.
    fn resolution(
        &self,
        applied: &Applied<'_>,
    ) -> Result<OperatorResolution<'db, DB>, LookupError> {
        let left = applied.left.map(|left| self.type_of(left)).transpose()?;
        let right = self.type_of(applied.right)?;
        Ok(resolve_operator(self.database, &applied.name, left.as_ref(), &right, self.case))
    }

    /// The type of a `NULLIF` applying `applied`, its first operand's type as
    /// `=` converts it, keeping the operand's modifier only when nothing
    /// converts it.
    fn nullif_type(&self, applied: &Applied<'_>) -> Result<OperandType<'_>, LookupError> {
        let left_type = applied.left.map(|node| self.type_of(node)).transpose()?;
        Ok(match self.resolution(applied)? {
            OperatorResolution::Selected(selected) if returns_bool(&selected) => {
                selected.into_left().map_or(OperandType::Unknown, |left| {
                    let typmod = match left_type {
                        Some(OperandType::Known { typmod, .. }) if left.conversion().is_empty() => {
                            typmod
                        }
                        _ => None,
                    };
                    OperandType::Known { type_name: left.into_target(), typmod }
                })
            }
            _ => OperandType::Unknown,
        })
    }

    /// Whether `expression` is a node of the query.
    fn holds(&self, expression: &Expr) -> bool {
        self.nodes.binary_search(&address(expression)).is_ok()
    }

    /// Refuses `expression` unless it is a node of the query.
    fn check(&self, expression: &Expr) -> Result<(), LookupError> {
        if self.holds(expression) {
            Ok(())
        } else {
            Err(LookupError::ExpressionNotInQuery { expression: expression.to_string() })
        }
    }
}

/// Where `expression` sits in memory, which identifies it among the nodes of
/// the borrowed query.
fn address(expression: &Expr) -> usize {
    core::ptr::from_ref(expression).addr()
}

/// The type of a `pg_catalog` type with no modifier.
fn catalog(name: &'static str) -> OperandType<'static> {
    OperandType::Known {
        type_name: TargetName::new(name, false).with_schema(PG_CATALOG, false),
        typmod: None,
    }
}

/// The type of a value of the column type `data_type`.
fn column_type<'a, D: DatabaseLike>(
    database: &'a D,
    data_type: &'a D::DataType,
) -> OperandType<'a> {
    let (Some(pg_type), Some(typmod)) =
        (PgType::parse(&data_type.name(database)), data_type.typmod(database))
    else {
        return OperandType::Unknown;
    };
    known(database, pg_type, typmod.value())
}

/// The type a cast or a typed string names, with the modifier it declares.
fn declared_type<'a, D: DatabaseLike>(database: &'a D, data_type: &DataType) -> OperandType<'a> {
    let Some(typmod) = type_modifier::declared(data_type) else {
        return OperandType::Unknown;
    };
    known(database, PgType::of(data_type), typmod.value())
}

/// The type `pg_type` names in `database`, carrying `typmod`.
fn known<D: DatabaseLike>(database: &D, pg_type: PgType, typmod: Option<i32>) -> OperandType<'_> {
    match type_name(database, &canonical_owned(database, pg_type)) {
        Some(type_name) => OperandType::Known { type_name, typmod },
        None => OperandType::Unknown,
    }
}

/// The type PostgreSQL gives the literal `value`.
fn literal_type(value: &Value) -> OperandType<'static> {
    match value {
        Value::SingleQuotedString(_)
        | Value::DollarQuotedString(_)
        | Value::EscapedStringLiteral(_)
        | Value::UnicodeStringLiteral(_)
        | Value::Null => OperandType::UntypedLiteral,
        Value::Number(text, _) => catalog(number_type(text, false)),
        Value::Boolean(_) => catalog("bool"),
        // `N'…'` reads as a `bpchar` and `X'…'` and `B'…'` as bit strings.
        Value::NationalStringLiteral(_) => catalog("bpchar"),
        Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_) => catalog("bit"),
        _ => OperandType::Unknown,
    }
}

/// The type of the numeric literal `text`, negated when `negative`, the
/// first of `int4` and `int8` holding the integer and `numeric` for
/// everything else.
fn number_type(text: &str, negative: bool) -> &'static str {
    let (radix, digits) = match text.get(..2) {
        Some("0x" | "0X") => (16, &text[2..]),
        Some("0o" | "0O") => (8, &text[2..]),
        Some("0b" | "0B") => (2, &text[2..]),
        _ => (10, text),
    };
    let mut value: u64 = 0;
    for digit in digits.chars().filter(|digit| *digit != '_') {
        let Some(digit) = digit.to_digit(radix) else { return "numeric" };
        match value.checked_mul(u64::from(radix)).and_then(|value| value.checked_add(digit.into()))
        {
            Some(next) => value = next,
            None => return "numeric",
        }
    }
    let (int4, int8) = if negative {
        (u64::from(i32::MIN.unsigned_abs()), i64::MIN.unsigned_abs())
    } else {
        (u64::from(i32::MAX.unsigned_abs()), i64::MAX.unsigned_abs())
    };
    if value <= int4 {
        "int4"
    } else if value <= int8 {
        "int8"
    } else {
        "numeric"
    }
}

/// `expression` without the parentheses around it, which PostgreSQL's
/// grammar keeps no node for.
fn unparenthesized(mut expression: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expression {
        expression = inner;
    }
    expression
}

/// The digits of the numeric constant `doNegate` folds `expression` into and
/// whether the constant is negative, for a `-` over a numeric literal or over
/// such a folded constant.
fn folded(expression: &Expr) -> Option<(&str, bool)> {
    let Expr::UnaryOp { op: UnaryOperator::Minus, expr } = expression else { return None };
    match unparenthesized(expr) {
        Expr::Value(value) => {
            match &value.value {
                Value::Number(digits, _) => Some((digits, true)),
                _ => None,
            }
        }
        inner => folded(inner).map(|(digits, negative)| (digits, !negative)),
    }
}

/// How PostgreSQL evaluates `expression`, or `None` for a form the context
/// does not derive.
fn form(expression: &Expr) -> Option<Form<'_>> {
    Some(match expression {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Value(_)
        | Expr::TypedString(_)
        | Expr::Cast { kind: CastKind::Cast | CastKind::DoubleColon, .. }
        | Expr::Collate { .. } => Form::Plain,
        Expr::BinaryOp { left, op, right } if !is_row(left) && !is_row(right) => {
            Form::Operator(Applied { name: binary_operator_name(op)?, left: Some(left), right })
        }
        Expr::UnaryOp { op, expr } => {
            Form::Operator(Applied::named(prefix_operator_name(*op)?, None, expr))
        }
        Expr::Like { negated, any: false, expr, pattern, escape_char: None } => {
            Form::Operator(Applied::named(if *negated { "!~~" } else { "~~" }, Some(expr), pattern))
        }
        Expr::ILike { negated, any: false, expr, pattern, escape_char: None } => {
            Form::Operator(Applied::named(
                if *negated { "!~~*" } else { "~~*" },
                Some(expr),
                pattern,
            ))
        }
        // An undecorated `NULL` on either side turns the comparison into a
        // null test on the other operand, rows included.
        Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right)
            if is_null(left) || is_null(right) =>
        {
            Form::NullTest
        }
        Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right)
            if !is_row(left) && !is_row(right) =>
        {
            Form::Distinct(Applied::named("=", Some(left), right))
        }
        Expr::Function(function) => {
            let (left, right) = nullif_operands(function)?;
            Form::NullIf(Applied::named("=", Some(left), right))
        }
        _ => return None,
    })
}

/// Whether `expression` is the undecorated `NULL` literal, parentheses
/// aside, as `exprIsNullConstant` reads it.
fn is_null(expression: &Expr) -> bool {
    matches!(unparenthesized(expression), Expr::Value(value) if value.value == Value::Null)
}

/// Whether `expression` is a row constructor, which turns a comparison into a
/// row comparison applying one operator per column.
fn is_row(expression: &Expr) -> bool {
    match unparenthesized(expression) {
        Expr::Tuple(_) => true,
        Expr::Function(function) => {
            unquoted_name(function).is_some_and(|name| name.eq_ignore_ascii_case("row"))
        }
        _ => false,
    }
}

/// The name `function` is called by when it is one unquoted identifier.
fn unquoted_name(function: &Function) -> Option<&str> {
    match function.name.0.as_slice() {
        [ObjectNamePart::Identifier(Ident { value, quote_style: None, .. })] => Some(value),
        _ => None,
    }
}

/// The operands of `function` when it is the `NULLIF` expression, which
/// PostgreSQL's grammar reads only unqualified, unquoted and with two plain
/// arguments.
fn nullif_operands(function: &Function) -> Option<(&Expr, &Expr)> {
    let FunctionArguments::List(list) = &function.args else { return None };
    let plain = unquoted_name(function).is_some_and(|name| name.eq_ignore_ascii_case("nullif"))
        && matches!(function.parameters, FunctionArguments::None)
        && function.filter.is_none()
        && function.null_treatment.is_none()
        && function.over.is_none()
        && function.within_group.is_empty()
        && list.duplicate_treatment.is_none()
        && list.clauses.is_empty();
    match list.args.as_slice() {
        [
            FunctionArg::Unnamed(FunctionArgExpr::Expr(left)),
            FunctionArg::Unnamed(FunctionArgExpr::Expr(right)),
        ] if plain => Some((left, right)),
        _ => None,
    }
}

/// Whether the selected operator returns `bool`, as PostgreSQL requires of
/// the `=` that `IS DISTINCT FROM` and `NULLIF` apply.
fn returns_bool<DB: DatabaseLike>(selected: &SelectedOperator<'_, DB>) -> bool {
    selected
        .result_type()
        .is_some_and(|result| result.schema() == Some(PG_CATALOG) && result.name() == "bool")
}

/// The name PostgreSQL looks up for the prefix operator `op`, or `None` for
/// `NOT`, a boolean connective, for a postfix operator, which PostgreSQL 18
/// no longer has, and for a token PostgreSQL's dialect does not write.
const fn prefix_operator_name(op: UnaryOperator) -> Option<&'static str> {
    Some(match op {
        UnaryOperator::Minus => "-",
        UnaryOperator::Plus => "+",
        UnaryOperator::BitwiseNot => "~",
        UnaryOperator::PGAbs => "@",
        UnaryOperator::PGSquareRoot => "|/",
        UnaryOperator::PGCubeRoot => "||/",
        UnaryOperator::PGPrefixFactorial => "!!",
        UnaryOperator::AtDashAt => "@-@",
        UnaryOperator::DoubleAt => "@@",
        UnaryOperator::Hash => "#",
        UnaryOperator::QuestionDash => "?-",
        UnaryOperator::QuestionPipe => "?|",
        _ => return None,
    })
}

/// The name PostgreSQL looks up for the binary operator `op`, or `None` for
/// the boolean connectives, the keyword forms that call no operator of that
/// name, and the tokens PostgreSQL's dialect does not write.
fn binary_operator_name(op: &BinaryOperator) -> Option<TargetName<'_>> {
    let symbol = match op {
        BinaryOperator::PGCustomBinaryOperator(parts) => {
            return match parts.as_slice() {
                [name] => Some(TargetName::new(name, false)),
                [schema, name] => Some(TargetName::new(name, false).with_schema(schema, false)),
                _ => None,
            };
        }
        BinaryOperator::Custom(symbol) => symbol,
        BinaryOperator::Plus => "+",
        BinaryOperator::Minus => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Modulo => "%",
        BinaryOperator::StringConcat => "||",
        BinaryOperator::Gt => ">",
        BinaryOperator::Lt => "<",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Spaceship => "<=>",
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::BitwiseOr => "|",
        BinaryOperator::BitwiseAnd => "&",
        BinaryOperator::PGExp => "^",
        BinaryOperator::PGBitwiseXor => "#",
        BinaryOperator::PGBitwiseShiftLeft => "<<",
        BinaryOperator::PGBitwiseShiftRight => ">>",
        BinaryOperator::PGOverlap => "&&",
        BinaryOperator::PGRegexMatch => "~",
        BinaryOperator::PGRegexIMatch => "~*",
        BinaryOperator::PGRegexNotMatch => "!~",
        BinaryOperator::PGRegexNotIMatch => "!~*",
        BinaryOperator::PGLikeMatch => "~~",
        BinaryOperator::PGILikeMatch => "~~*",
        BinaryOperator::PGNotLikeMatch => "!~~",
        BinaryOperator::PGNotILikeMatch => "!~~*",
        BinaryOperator::PGStartsWith => "^@",
        BinaryOperator::Arrow => "->",
        BinaryOperator::LongArrow => "->>",
        BinaryOperator::HashArrow => "#>",
        BinaryOperator::HashLongArrow => "#>>",
        BinaryOperator::AtAt => "@@",
        BinaryOperator::AtArrow => "@>",
        BinaryOperator::ArrowAt => "<@",
        BinaryOperator::HashMinus => "#-",
        BinaryOperator::AtQuestion => "@?",
        BinaryOperator::Question => "?",
        BinaryOperator::QuestionAnd => "?&",
        BinaryOperator::QuestionPipe => "?|",
        BinaryOperator::DoubleHash => "##",
        BinaryOperator::LtDashGt => "<->",
        BinaryOperator::AndLt => "&<",
        BinaryOperator::AndGt => "&>",
        BinaryOperator::LtLtPipe => "<<|",
        BinaryOperator::PipeGtGt => "|>>",
        BinaryOperator::AndLtPipe => "&<|",
        BinaryOperator::PipeAndGt => "|&>",
        BinaryOperator::LtCaret => "<^",
        BinaryOperator::GtCaret => ">^",
        BinaryOperator::QuestionHash => "?#",
        BinaryOperator::QuestionDash => "?-",
        BinaryOperator::QuestionDashPipe => "?-|",
        BinaryOperator::QuestionDoublePipe => "?||",
        BinaryOperator::At => "@",
        BinaryOperator::TildeEq => "~=",
        _ => return None,
    };
    Some(TargetName::new(symbol, false))
}
