//! The facts each expression node of one query takes.

use alloc::{borrow::Cow, string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    BinaryOperator, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, InTarget, ObjectNamePart, Query, UnaryOperator, Value, Visitor,
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
        coercion::{CommonType, base, canonical_owned, facts, select_common_type, type_name},
        object_name::PG_CATALOG,
        operator_typing::resolve_operator,
        postgres_types::PgType,
        type_modifier,
        walk::{walk, walk_expressions},
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
    right: Operand<'node>,
    resolution: OperatorResolution<'db, DB>,
}

/// Where the right operand of an operator application comes from.
#[derive(Debug, Clone)]
pub enum Operand<'node> {
    /// The value of this node.
    Node(&'node Expr),
    /// Each element of an array, the application holding when it holds for
    /// any of them, as `op ANY (array)` and a collapsed `IN` list apply it.
    Any(ArraySource<'node>),
    /// Each element of an array, the application holding when it holds for
    /// all of them, as `op ALL (array)` and a collapsed `NOT IN` list apply
    /// it.
    All(ArraySource<'node>),
}

/// The array an [`Operand::Any`] or [`Operand::All`] compares against.
#[derive(Debug, Clone)]
pub enum ArraySource<'node> {
    /// The array this node evaluates to.
    Node(&'node Expr),
    /// The array PostgreSQL builds from these nodes, each converted to their
    /// common type, as it collapses the constant elements of an `IN` list.
    Built(Vec<&'node Expr>),
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

    /// Returns where the right operand comes from.
    #[must_use]
    pub const fn right(&self) -> &Operand<'node> {
        &self.right
    }

    /// Returns what PostgreSQL does with the invocation, its operands typed
    /// as [`ExpressionContext::type_of`] types their nodes.
    #[must_use]
    pub const fn resolution(&self) -> &OperatorResolution<'db, DB> {
        &self.resolution
    }
}

/// How PostgreSQL evaluates a node, by the operators it applies.
enum Form<'node> {
    /// These applications, in the order PostgreSQL builds them, and what the
    /// node returns of them.
    Applies(Vec<Applied<'node>>, Returns),
    /// `[NOT] IN` over a list, whose applications depend on the types of its
    /// elements and the columns they reference.
    InList {
        /// The node tested for membership.
        left: &'node Expr,
        /// The listed nodes.
        elements: &'node [Expr],
        /// Whether the list is `NOT IN`.
        negated: bool,
    },
}

/// What a node returns of the operators it applies.
#[derive(Clone, Copy)]
enum Returns {
    /// The result of its one operator, as an `OpExpr`, and nothing the
    /// context derives for a node applying none.
    Result,
    /// `bool`, when every operator returns `bool`, as a `DistinctExpr`, a
    /// `NullTest`, a `ScalarArrayOpExpr` and the `AND`/`OR` joining several
    /// do.
    Bool,
    /// Its one operator's left operand as that operator converts it, as a
    /// `NullIfExpr`.
    FirstOperand,
}

/// The applications a node holds and what it returns of them.
type Applications<'node, 'db, DB> = (Vec<OperatorApplication<'node, 'db, DB>>, Returns);

/// What PostgreSQL does with the elements of an `IN` list it may build into
/// one array.
enum Collapse<'a> {
    /// It builds them into one array of elements of this type.
    Into(OperandType<'a>),
    /// It compares with each of them apart.
    Apart,
    /// A type that decides it is unknown.
    Unknown,
}

/// An operator name applied to operand nodes.
struct Applied<'node> {
    name: TargetName<'node>,
    left: Option<&'node Expr>,
    right: &'node Expr,
    /// How the operator applies to the elements of the array `right`
    /// evaluates to, `None` when it applies to that value itself.
    quantifier: Option<Quantifier>,
}

impl<'node> Applied<'node> {
    /// `name`, from `pg_catalog` or the search path, applied to `left` and
    /// `right`.
    fn named(name: &'static str, left: Option<&'node Expr>, right: &'node Expr) -> Self {
        Self { name: TargetName::new(name, false), left, right, quantifier: None }
    }
}

/// Whether an operator applied to an array's elements must hold for one of
/// them or for all.
#[derive(Clone, Copy)]
enum Quantifier {
    Any,
    All,
}

impl Quantifier {
    /// The operand comparing with each element of `source`.
    const fn over(self, source: ArraySource<'_>) -> Operand<'_> {
        match self {
            Self::Any => Operand::Any(source),
            Self::All => Operand::All(source),
        }
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
        match (expression, self.rule(expression)) {
            (Expr::Identifier(_) | Expr::CompoundIdentifier(_), Some(rule)) => {
                self.scope.resolve_site(rule, expression)
            }
            _ => Ok(None),
        }
    }

    /// The rule the clause holding the column reference `expression` resolves
    /// it by, `None` for a node that is no column reference or sits where no
    /// scope was derived.
    fn rule(&self, expression: &Expr) -> Option<SiteRule> {
        let address = address(expression);
        self.sites
            .binary_search_by_key(&address, |(held, _)| *held)
            .ok()
            .and_then(|index| self.sites[index].1)
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
                match folded(expression) {
                    Some((digits, negative)) => catalog(number_type(digits, negative)),
                    None => {
                        match self.applications(expression)? {
                            Some((applications, returns)) => {
                                self.returned(applications, returns)?
                            }
                            None => OperandType::Unknown,
                        }
                    }
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
    /// `op ANY|SOME|ALL (array)` and `[NOT] [I]LIKE ANY (array)` apply their
    /// operator to each element of the array. `[NOT] BETWEEN` applies `>=`
    /// and `<=`, or `<` and `>`, one per bound. `IN` applies `=`, and `NOT
    /// IN` `<>`, once to an array built from the elements holding no column
    /// reference of the list's own query level, when there are several and
    /// they have a common type with an array type, and once to each other
    /// element. A parenthesized node answers for its content. Row
    /// comparisons, `LIKE ... ESCAPE`, `SIMILAR TO`, comparisons with a
    /// subquery, the boolean connectives and calls answer `None`.
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
        Ok(self.applications(expression)?.map(|(applications, _)| applications))
    }

    /// The applications PostgreSQL builds for `expression` and what the node
    /// returns of them, `None` for a form the context does not derive.
    fn applications<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<Applications<'node, 'db, DB>>, LookupError> {
        let (applied, returns) = match form(expression) {
            None => return Ok(None),
            Some(Form::InList { left, elements, negated }) => {
                let applications = self.in_list(left, elements, negated)?;
                return Ok(applications.map(|applications| (applications, Returns::Bool)));
            }
            Some(Form::Applies(applied, returns)) => (applied, returns),
        };
        let mut applications = Vec::with_capacity(applied.len());
        for Applied { name, left, right, quantifier } in applied {
            let right_type = self.type_of(right)?;
            let (right, right_type) = match quantifier {
                None => (Operand::Node(right), right_type),
                Some(quantifier) => {
                    (
                        quantifier.over(ArraySource::Node(right)),
                        element_type(self.database, right_type),
                    )
                }
            };
            applications.push(self.resolved(name, left, right, &right_type)?);
        }
        Ok(Some((applications, returns)))
    }

    /// The application of `name` to `left` and `right`, whose value or
    /// elements are of type `right_type`.
    fn resolved<'node>(
        &self,
        name: TargetName<'node>,
        left: Option<&'node Expr>,
        right: Operand<'node>,
        right_type: &OperandType<'_>,
    ) -> Result<OperatorApplication<'node, 'db, DB>, LookupError> {
        let left_type = left.map(|left| self.type_of(left)).transpose()?;
        let resolution =
            resolve_operator(self.database, &name, left_type.as_ref(), right_type, self.case);
        Ok(OperatorApplication { name, left, right, resolution })
    }

    /// What a node applying `applications` returns, as `returns` says.
    fn returned(
        &self,
        mut applications: Vec<OperatorApplication<'_, 'db, DB>>,
        returns: Returns,
    ) -> Result<OperandType<'_>, LookupError> {
        Ok(match returns {
            Returns::Bool => {
                let all_bool = applications.iter().all(|application| {
                    matches!(&application.resolution, OperatorResolution::Selected(selected)
                        if returns_bool(selected))
                });
                if all_bool { catalog("bool") } else { OperandType::Unknown }
            }
            Returns::FirstOperand => {
                match applications.pop() {
                    Some(OperatorApplication {
                        left: Some(left),
                        resolution: OperatorResolution::Selected(selected),
                        ..
                    }) if returns_bool(&selected) => {
                        let left_type = self.type_of(left)?;
                        selected.into_left().map_or(OperandType::Unknown, |evidence| {
                            let typmod = match left_type {
                                OperandType::Known { typmod, .. }
                                    if evidence.conversion().is_empty() =>
                                {
                                    typmod
                                }
                                _ => None,
                            };
                            OperandType::Known { type_name: evidence.into_target(), typmod }
                        })
                    }
                    _ => OperandType::Unknown,
                }
            }
            Returns::Result => {
                match applications.pop().map(|application| application.resolution) {
                    Some(OperatorResolution::Selected(selected)) => {
                        selected.into_result_type().map_or(OperandType::Unknown, |type_name| {
                            OperandType::Known { type_name, typmod: None }
                        })
                    }
                    _ => OperandType::Unknown,
                }
            }
        })
    }

    /// The applications of `left [NOT] IN (elements)`, `None` when a type or
    /// a reference they depend on is unknown or a row is compared.
    fn in_list<'node>(
        &self,
        left: &'node Expr,
        elements: &'node [Expr],
        negated: bool,
    ) -> Result<Option<Vec<OperatorApplication<'node, 'db, DB>>>, LookupError> {
        if is_row(left) || elements.iter().any(is_row) {
            return Ok(None);
        }
        let (name, quantifier) =
            if negated { ("<>", Quantifier::All) } else { ("=", Quantifier::Any) };
        let mut kept = Vec::new();
        let mut collapsible = Vec::new();
        for element in elements {
            match self.holds_own_columns(element)? {
                Some(true) => kept.push(element),
                Some(false) => collapsible.push(element),
                None => return Ok(None),
            }
        }
        let mut applications = Vec::with_capacity(elements.len());
        let collapse = if collapsible.len() > 1 {
            self.collapse(left, &collapsible)?
        } else {
            Collapse::Apart
        };
        let separate = match collapse {
            Collapse::Unknown => return Ok(None),
            Collapse::Into(element_type) => {
                let built = quantifier.over(ArraySource::Built(collapsible));
                let name = TargetName::new(name, false);
                applications.push(self.resolved(name, Some(left), built, &element_type)?);
                kept
            }
            Collapse::Apart => elements.iter().collect(),
        };
        for element in separate {
            let element_type = self.type_of(element)?;
            let name = TargetName::new(name, false);
            applications.push(self.resolved(
                name,
                Some(left),
                Operand::Node(element),
                &element_type,
            )?);
        }
        Ok(Some(applications))
    }

    /// What PostgreSQL does with the `IN` list elements `elements` it may
    /// build into one array, deciding by the common type of `left` and the
    /// elements.
    fn collapse(&self, left: &Expr, elements: &[&Expr]) -> Result<Collapse<'_>, LookupError> {
        let mut types = Vec::with_capacity(elements.len() + 1);
        for node in core::iter::once(left).chain(elements.iter().copied()) {
            types.push(match self.type_of(node)? {
                OperandType::Known { type_name, .. } => PgType::from_target(&type_name),
                OperandType::UntypedLiteral => PgType::UNKNOWN,
                OperandType::Unknown => return Ok(Collapse::Unknown),
            });
        }
        let common = select_common_type(self.database, &types, |pg_type| pg_type).ok();
        Ok(common
            .and_then(|common| {
                let scalar = match common {
                    None => return Some(Collapse::Apart),
                    Some(CommonType::At(position)) => types.swap_remove(position),
                    Some(CommonType::Base(base)) => base,
                    Some(CommonType::AllUnknown) => PgType::TEXT,
                };
                let scalar = canonical_owned(self.database, scalar);
                let arrays = facts(self.database, &scalar)?.array_type().is_some();
                // `record` keeps its elements apart for the row comparisons.
                Some(if arrays && scalar != PgType::RECORD {
                    Collapse::Into(known(self.database, scalar, None))
                } else {
                    Collapse::Apart
                })
            })
            .unwrap_or(Collapse::Unknown))
    }

    /// Whether `element` holds a column reference that binds in the query
    /// level the element sits in, `None` when a reference in it binds nothing
    /// or names a whole row.
    fn holds_own_columns(&self, element: &Expr) -> Result<Option<bool>, LookupError> {
        let mut visitor = OwnColumns { context: self, depth: 0 };
        match walk(element, &mut visitor) {
            ControlFlow::Continue(()) => Ok(Some(false)),
            ControlFlow::Break(found) => found,
        }
    }

    /// Whether the column reference `reference` binds `levels_up` query
    /// levels out from where it sits, `None` when it binds nothing or names a
    /// whole row.
    fn binds_levels_up(
        &self,
        reference: &Expr,
        levels_up: usize,
    ) -> Result<Option<bool>, LookupError> {
        match self.rule(reference) {
            Some(SiteRule::Scope(cursor)) => {
                self.scope.binds_levels_up(cursor, reference, levels_up)
            }
            _ => Ok(None),
        }
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
    let (applied, returns) = match expression {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Value(_)
        | Expr::TypedString(_)
        | Expr::Cast { kind: CastKind::Cast | CastKind::DoubleColon, .. }
        | Expr::Collate { .. } => (Vec::new(), Returns::Result),
        Expr::BinaryOp { left, op, right } if !is_row(left) && !is_row(right) => {
            let name = binary_operator_name(op)?;
            (
                Vec::from([Applied { name, left: Some(left), right, quantifier: None }]),
                Returns::Result,
            )
        }
        Expr::UnaryOp { op, expr } => {
            (Vec::from([Applied::named(prefix_operator_name(*op)?, None, expr)]), Returns::Result)
        }
        Expr::Like { negated, any, expr, pattern, escape_char: None } => {
            like(if *negated { "!~~" } else { "~~" }, *any, expr, pattern)
        }
        Expr::ILike { negated, any, expr, pattern, escape_char: None } => {
            like(if *negated { "!~~*" } else { "~~*" }, *any, expr, pattern)
        }
        // An undecorated `NULL` on either side turns the comparison into a
        // null test on the other operand, rows included.
        Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right)
            if is_null(left) || is_null(right) =>
        {
            (Vec::new(), Returns::Bool)
        }
        Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right)
            if !is_row(left) && !is_row(right) =>
        {
            (Vec::from([Applied::named("=", Some(left), right)]), Returns::Bool)
        }
        Expr::Function(function) => {
            let (left, right) = nullif_operands(function)?;
            (Vec::from([Applied::named("=", Some(left), right)]), Returns::FirstOperand)
        }
        Expr::Between { expr, negated, low, high }
            if !is_row(expr) && !is_row(low) && !is_row(high) =>
        {
            let (lower, upper) = if *negated { ("<", ">") } else { (">=", "<=") };
            let bounds =
                [Applied::named(lower, Some(expr), low), Applied::named(upper, Some(expr), high)];
            (Vec::from(bounds), Returns::Bool)
        }
        Expr::AnyOp { left, compare_op, right, .. } => {
            quantified(compare_op, Quantifier::Any, left, right)?
        }
        Expr::AllOp { left, compare_op, right } => {
            quantified(compare_op, Quantifier::All, left, right)?
        }
        Expr::In { expr, target: InTarget::List(elements), negated, global: false } => {
            return Some(Form::InList { left: expr, elements, negated: *negated });
        }
        _ => return None,
    };
    Some(Form::Applies(applied, returns))
}

/// The applications of `expr [NOT] [I]LIKE pattern` under the operator
/// `name`, or of `expr [NOT] [I]LIKE ANY (pattern)` when `any`.
fn like<'node>(
    name: &'static str,
    any: bool,
    expr: &'node Expr,
    pattern: &'node Expr,
) -> (Vec<Applied<'node>>, Returns) {
    let quantifier = any.then_some(Quantifier::Any);
    let applied = Applied {
        name: TargetName::new(name, false),
        left: Some(expr),
        right: pattern,
        quantifier,
    };
    (Vec::from([applied]), if any { Returns::Bool } else { Returns::Result })
}

/// The application of `left op ANY|ALL (right)` over an array, `None` for a
/// subquery on the right or an operator PostgreSQL does not name.
fn quantified<'node>(
    op: &'node BinaryOperator,
    quantifier: Quantifier,
    left: &'node Expr,
    right: &'node Expr,
) -> Option<(Vec<Applied<'node>>, Returns)> {
    if matches!(unparenthesized(right), Expr::Subquery(_)) {
        return None;
    }
    let name = binary_operator_name(op)?;
    let applied = Applied { name, left: Some(left), right, quantifier: Some(quantifier) };
    Some((Vec::from([applied]), Returns::Bool))
}

/// The type of each element of an array of type `array`, a domain read as
/// its base type as `get_base_element_type` reads it, an untyped literal
/// staying untyped as PostgreSQL resolves it against the operator's array
/// type, and nothing for a value that is no array or whose domain base the
/// database does not answer.
fn element_type<'a, D: DatabaseLike>(database: &'a D, array: OperandType<'a>) -> OperandType<'a> {
    let OperandType::Known { type_name, typmod } = array else { return array };
    element_of(database, &type_name, typmod).unwrap_or(OperandType::Unknown)
}

/// The type of each element of the array type `array`, carrying `typmod`,
/// a domain read as its base type.
fn element_of<'a, D: DatabaseLike>(
    database: &'a D,
    array: &TargetName<'_>,
    typmod: Option<i32>,
) -> Option<OperandType<'a>> {
    let declared = canonical_owned(database, PgType::from_target(array));
    let domain_base = match base(database, &declared).ok()? {
        Cow::Owned(base) => Some(base),
        Cow::Borrowed(_) => None,
    };
    // The modifier a column of a domain carries is the domain's, not its
    // elements'.
    let (array, typmod) = match domain_base {
        Some(base) => (base, None),
        None => (declared, typmod),
    };
    Some(known(database, array.into_element()?, typmod))
}

/// Walks an `IN` list element for a column reference that binds in the
/// list's own query level.
struct OwnColumns<'c, 'query, 'db, DB: DatabaseLike> {
    context: &'c ExpressionContext<'query, 'db, DB>,
    /// How many queries nested in the element the walk is inside.
    depth: usize,
}

impl<DB: DatabaseLike> Visitor for OwnColumns<'_, '_, '_, DB> {
    /// Whether a reference of the list's own level was found, `None` when a
    /// reference binds nothing.
    type Break = Result<Option<bool>, LookupError>;

    fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        if !matches!(
            expression,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::QualifiedWildcard(..)
        ) {
            return ControlFlow::Continue(());
        }
        match self.context.binds_levels_up(expression, self.depth) {
            Ok(Some(false)) => ControlFlow::Continue(()),
            found => ControlFlow::Break(found),
        }
    }
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
