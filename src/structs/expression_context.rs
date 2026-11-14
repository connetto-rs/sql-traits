//! The facts each expression node of one query takes.

use alloc::{borrow::Cow, boxed::Box, string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    ArgMode, BinaryOperator, CastKind, DataType, DuplicateTreatment, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArgumentClause, FunctionArguments, Ident, InTarget, ObjectNamePart,
    Query, Statement, TableFactor, UnaryOperator, Value, Visit, Visitor,
};

use crate::{
    errors::{LookupError, Misapplied, Unresolved},
    impls::dql::{
        definition_graph::reference_sites::SiteRule,
        scoped_roots::{ScopedRoot, is_input},
    },
    structs::{
        AggregateKind, CallResolution, ColumnBinding, ColumnDefinition, ColumnScope,
        ExpressionScope, FunctionKind, IdentifierCase, OperandType, OperatorResolution,
        SelectedOperator, TargetName,
    },
    traits::{ColumnLike, DataTypeLike, DatabaseLike, FunctionLike},
    utils::{
        call_sites::expression_form,
        call_typing::{
            Matched, called_name, called_object_name, reached_by_name, resolve_function,
            written_argument_names, written_names,
        },
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
    /// The type of each input parameter of the function whose body the
    /// context types, in the order `$n` counts them.
    parameters: Vec<OperandType<'db>>,
    /// The type of `VALUE` in a domain constraint.
    value: Option<OperandType<'db>>,
    /// What the deparsed text of a stored expression pins when PostgreSQL
    /// parses it again.
    pins: Option<&'query (dyn ReparsePins<DB> + 'query)>,
}

/// An application of an operator, by the address of the node applying it
/// and its position among that node's applications.
pub(crate) type Site = (usize, usize);

/// What the text PostgreSQL deparses a stored expression into pins when
/// `ALTER COLUMN ... TYPE` has it parsed again: the previous type of each
/// retyped column while the old selections are read, the operator each
/// application and the function each call selected then, the casts the
/// deparse writes around the operands and arguments those selections
/// converted and in place of a call read as a cast, and the schema it
/// qualifies an operator or a function with that its name alone would not
/// reach.
pub(crate) trait ReparsePins<DB: DatabaseLike> {
    /// The type a reference to the column `column` reads as, in place of
    /// the column's own.
    fn column_type(&self, column: &str, quoted: bool) -> Option<&DataType>;
    /// Whether the application at `site` may select `operator`.
    fn admits(&self, site: Site, operator: &DB::Operator) -> bool;
    /// The type the left operand, when `left`, or the right operand of the
    /// application at `site` is cast to.
    fn operand(&self, site: Site, left: bool) -> Option<&OperandType<'static>>;
    /// The schema the name of the operator the application at `site`
    /// applies is qualified with.
    fn schema(&self, site: Site) -> Option<(&str, bool)>;
    /// Whether the call whose node sits at `call` may select `function`.
    fn admits_call(&self, call: usize, function: &DB::Function) -> bool;
    /// The type argument `index` of the call whose node sits at `call` is
    /// cast to.
    fn argument(&self, call: usize, index: usize) -> Option<&OperandType<'static>>;
    /// The schema the called name of the call whose node sits at `call` is
    /// qualified with.
    fn call_schema(&self, call: usize) -> Option<(&str, bool)>;
    /// The type the call whose node sits at `call` is deparsed as a cast to,
    /// for a call read as a cast.
    fn cast(&self, call: usize) -> Option<&TargetName<'static>>;
}

/// One operator PostgreSQL applies to evaluate an expression node, with the
/// nodes its operands come from and the overload it selects.
#[derive(Debug, Clone)]
pub struct OperatorApplication<'node, 'db, DB: DatabaseLike> {
    name: TargetName<'node>,
    left: Option<&'node Expr>,
    right: Operand<'node, 'db, DB>,
    resolution: OperatorResolution<'db, DB>,
    /// Why PostgreSQL refuses the application, with the type of each operand
    /// the selection weighed, the left one first.
    refused: Option<Refused>,
}

/// Where the right operand of an operator application comes from.
#[derive(Debug, Clone)]
pub enum Operand<'node, 'db, DB: DatabaseLike> {
    /// The value of this node.
    Node(&'node Expr),
    /// Each element of an array, the application holding when it holds for
    /// any of them, as `op ANY (array)` and a collapsed `IN` list apply it.
    Any(ArraySource<'node>),
    /// Each element of an array, the application holding when it holds for
    /// all of them, as `op ALL (array)` and a collapsed `NOT IN` list apply
    /// it.
    All(ArraySource<'node>),
    /// What a call PostgreSQL's grammar inserts returns, as the
    /// `pg_catalog.like_escape` over the pattern of `LIKE ... ESCAPE` and the
    /// `pg_catalog.similar_to_escape` over the pattern of `SIMILAR TO`.
    Call(Box<CallApplication<'node, 'db, DB>>),
}

/// One function PostgreSQL calls to evaluate an expression node, with the
/// nodes its arguments come from and the function it selects.
#[derive(Debug, Clone)]
pub struct CallApplication<'node, 'db, DB: DatabaseLike> {
    name: TargetName<'node>,
    arguments: Vec<&'node Expr>,
    resolution: CallResolution<'db, DB>,
    /// Why PostgreSQL refuses the call.
    refused: Option<CallRefusal>,
}

impl<'node, 'db, DB: DatabaseLike> CallApplication<'node, 'db, DB> {
    /// Returns the function name PostgreSQL looks up, as the query writes it
    /// or, for a call its grammar inserts, qualified with `pg_catalog`.
    #[must_use]
    pub const fn name(&self) -> &TargetName<'node> {
        &self.name
    }

    /// Returns the nodes the arguments come from, in the order the call
    /// writes them.
    #[must_use]
    pub fn arguments(&self) -> &[&'node Expr] {
        &self.arguments
    }

    /// Returns what PostgreSQL does with the call, its arguments typed as
    /// [`ExpressionContext::type_of`] types their nodes.
    #[must_use]
    pub const fn resolution(&self) -> &CallResolution<'db, DB> {
        &self.resolution
    }

    /// The name looked up and why PostgreSQL refuses the call, for a call
    /// PostgreSQL refuses.
    pub(crate) fn into_refusal(self) -> Option<(TargetName<'node>, CallRefusal)> {
        Some((self.name, self.refused?))
    }
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
    pub const fn right(&self) -> &Operand<'node, 'db, DB> {
        &self.right
    }

    /// Returns what PostgreSQL does with the invocation, its operands typed
    /// as [`ExpressionContext::type_of`] types their nodes.
    #[must_use]
    pub const fn resolution(&self) -> &OperatorResolution<'db, DB> {
        &self.resolution
    }

    /// The name looked up, why PostgreSQL refuses the application and the
    /// type of each operand the selection weighed, the left one first, for
    /// an application PostgreSQL refuses.
    pub(crate) fn into_refusal(self) -> Option<(TargetName<'node>, Refused)> {
        Some((self.name, self.refused?))
    }
}

/// Why PostgreSQL refuses an application or a call, and the type of each
/// operand or argument it weighed.
pub(crate) type Refused = (Unresolved, Vec<Box<str>>);

/// Why PostgreSQL refuses a call.
#[derive(Debug, Clone)]
pub(crate) enum CallRefusal {
    /// No function, or no single one, takes the arguments, typed as the
    /// selection weighed them.
    Unresolved(Refused),
    /// The call writes clauses the function it selects refuses, or clauses
    /// no call may combine.
    Misapplied(Misapplied),
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
pub(crate) enum Collapse<'a> {
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
    right: Right<'node>,
}

impl<'node> Applied<'node> {
    /// `name`, from `pg_catalog` or the search path, applied to `left` and
    /// `right`.
    fn named(name: &'static str, left: Option<&'node Expr>, right: &'node Expr) -> Self {
        Self { name: TargetName::new(name, false), left, right: Right::Node(right) }
    }
}

/// Where the right operand of an applied operator comes from.
enum Right<'node> {
    /// The value of this node.
    Node(&'node Expr),
    /// Each element of the array this node evaluates to.
    Elements(&'node Expr, Quantifier),
    /// What the `pg_catalog` function of this name returns of these nodes.
    Call(&'static str, Vec<&'node Expr>),
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
    const fn over<'db, DB: DatabaseLike>(self, source: ArraySource<'_>) -> Operand<'_, 'db, DB> {
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
        let sites = scope.site_rules(query);
        Ok(Self::assemble(query, scope, sites, database, case, Vec::new(), None))
    }

    /// Builds the context of `expression`, written outside a query, over
    /// `database`, its column references reaching what `scope` names and the
    /// relations of the queries it holds, comparing identifiers under
    /// `case`.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors resolving the queries
    /// `expression` holds runs into.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::Expr, dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a INT4, b NUMERIC);")?;
    /// let table = db.table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?;
    /// let check = Parser::new(&PostgreSqlDialect {}).try_with_sql("t.a > b")?.parse_expr()?;
    /// let scope = ExpressionScope::Table(table.expect("t exists"));
    /// let context = ExpressionContext::for_expression(&check, &scope, &db, IdentifierCase::Folded)?;
    /// let Expr::BinaryOp { left, .. } = &check else { unreachable!() };
    /// let int4 = TargetName::new("int4", false).with_schema("pg_catalog", false);
    /// assert_eq!(context.type_of(left)?, OperandType::Known { type_name: int4, typmod: None });
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn for_expression(
        expression: &'query Expr,
        scope: &ExpressionScope<'_, 'db, DB>,
        database: &'db DB,
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        Self::scoped(ScopedRoot::Expression(expression), expression, scope, database, case)
    }

    /// Builds the context of `statement`, a query or an `INSERT`, `UPDATE`,
    /// `DELETE` or `MERGE` written outside a query, over `database`, its
    /// column references reaching its own relations first and then what
    /// `scope` names, comparing identifiers under `case`.
    ///
    /// An `UPDATE` reaches its target and its `FROM` relations, a `DELETE`
    /// its target and its `USING` relations, and an `INSERT`'s `RETURNING`
    /// its target, which its `ON CONFLICT DO UPDATE` reaches with the
    /// `excluded` row. A `MERGE` reaches its source and target, except that
    /// `WHEN NOT MATCHED` reaches the source alone and `WHEN NOT MATCHED BY
    /// SOURCE` the target alone. `RETURNING` also reaches the target's `old`
    /// and `new` rows when no relation in reach takes either name. A
    /// statement holds the same scopes behind a `WITH` and as a
    /// data-modifying CTE, whose output is its `RETURNING` list. Any other
    /// statement binds no column reference.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors resolving the statement runs
    /// into.
    pub fn for_statement(
        statement: &'query Statement,
        scope: &ExpressionScope<'_, 'db, DB>,
        database: &'db DB,
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        Self::scoped(ScopedRoot::Statement(statement), statement, scope, database, case)
    }

    /// Builds the context of `root`, which `node` is, under `scope`.
    fn scoped<N: Visit + ?Sized>(
        root: ScopedRoot<'query>,
        node: &'query N,
        scope: &ExpressionScope<'_, 'db, DB>,
        database: &'db DB,
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        let (column_scope, sites) = ColumnScope::for_root(root, scope, database, case)?;
        let parameters = match scope {
            ExpressionScope::FunctionBody(function) => {
                function
                    .argument_modes()
                    .zip(function.argument_type_names(database))
                    .filter(|(mode, _)| is_input(mode))
                    .map(|(_, name)| {
                        PgType::parse(&name)
                            .map_or(OperandType::Unknown, |pg_type| known(database, pg_type, None))
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        let value = match scope {
            ExpressionScope::DomainValue(data_type) => Some(declared_type(database, data_type)),
            _ => None,
        };
        Ok(Self::assemble(node, column_scope, sites, database, case, parameters, value))
    }

    /// The context of the nodes of `root`, resolved in `scope` by the rules
    /// `sites` holds.
    fn assemble<N: Visit + ?Sized>(
        root: &N,
        scope: ColumnScope<'query, 'db, DB>,
        mut sites: Vec<(usize, Option<SiteRule>)>,
        database: &'db DB,
        case: IdentifierCase,
        parameters: Vec<OperandType<'db>>,
        value: Option<OperandType<'db>>,
    ) -> Self {
        let mut nodes = Vec::new();
        let _ = walk_expressions(root, |expression| {
            nodes.push(address(expression));
            ControlFlow::<()>::Continue(())
        });
        nodes.sort_unstable();
        sites.sort_unstable_by_key(|(address, _)| *address);
        Self { database, scope, nodes, sites, case, parameters, value, pins: None }
    }

    /// The context typing as `pins` has PostgreSQL's deparsed text pin it.
    pub(crate) const fn pinned(mut self, pins: &'query (dyn ReparsePins<DB> + 'query)) -> Self {
        self.pins = Some(pins);
        self
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
    /// operand's type as `=` converts it. A call [`Self::call`] answers takes
    /// the result of the function PostgreSQL selects. A `-` over a numeric
    /// literal is the negative constant PostgreSQL's grammar folds it into.
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
                        let previous = self.pins.and_then(|pins| {
                            pins.column_type(column.column_name(), column.column_name_is_quoted())
                        });
                        match previous {
                            Some(data_type) => declared_type(database, data_type),
                            None => column_type(database, column.data_type(database)),
                        }
                    }
                    // An output a `SELECT` of this query defines takes the
                    // type of its defining expression.
                    Some((ColumnDefinition::Expression { expression, .. }, _))
                        if self.holds(expression) =>
                    {
                        self.type_of(expression)?
                    }
                    Some((ColumnDefinition::Parameter { ordinal, .. }, _)) => {
                        self.parameters.get(ordinal).cloned().unwrap_or(OperandType::Unknown)
                    }
                    None => self.domain_value(expression).unwrap_or(OperandType::Unknown),
                    _ => OperandType::Unknown,
                }
            }
            Expr::Value(value) => {
                match &value.value {
                    Value::Placeholder(placeholder) => self.positional(placeholder),
                    other => literal_type(other),
                }
            }
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
                            None => {
                                match self.call(expression)? {
                                    Some(CallApplication {
                                        resolution: CallResolution::Selected(selected),
                                        ..
                                    }) => without_typmod(selected.into_result_type()),
                                    Some(CallApplication {
                                        resolution: CallResolution::Cast(target),
                                        ..
                                    }) => without_typmod(Some(target)),
                                    _ => OperandType::Unknown,
                                }
                            }
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
    /// With `ESCAPE` the `LIKE` operator compares with [`Operand::Call`], the
    /// `pg_catalog.like_escape` call over the pattern and the escape, and
    /// `[NOT] SIMILAR TO` applies `~` or `!~` to the
    /// `pg_catalog.similar_to_escape` call over the pattern and any escape.
    /// `op ANY|SOME|ALL (array)` and `[NOT] [I]LIKE ANY (array)` apply their
    /// operator to each element of the array. `[NOT] BETWEEN` applies `>=`
    /// and `<=`, or `<` and `>`, one per bound. `IN` applies `=`, and `NOT
    /// IN` `<>`, once to an array built from the elements holding no column
    /// reference of the list's own query level, when there are several and
    /// they have a common type with an array type, and once to each other
    /// element. A parenthesized node answers for its content. Row
    /// comparisons, comparisons with a subquery, the boolean connectives and
    /// calls answer `None`.
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

    /// Returns the function PostgreSQL calls to evaluate `expression`, or
    /// `None` for a node that is no call.
    ///
    /// A call answers with the function it selects among those its name
    /// reaches, each argument converted to the parameter it passes, or the
    /// cast PostgreSQL reads it as. An aggregate or window call is one too:
    /// its arguments are those it writes, `DISTINCT`, `ORDER BY`, `FILTER`
    /// and `OVER` aside, followed by the ordering arguments of `WITHIN
    /// GROUP`, and `count(*)` passes none. `ROW(...)`, the expressions
    /// PostgreSQL's grammar writes like a call, such as `COALESCE`, `NULLIF`
    /// and `CURRENT_DATE`, and a call written with clauses PostgreSQL has no
    /// grammar for answer `None`. A parenthesized node answers for its
    /// content.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// query, [`LookupError::InvalidObjectName`] for a called name with more
    /// than two parts or a part built at run time, and the lookup errors
    /// typing an argument return.
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
    /// let parsed = Parser::parse_sql(&PostgreSqlDialect {}, "SELECT round(i, 2) FROM t")?;
    /// let Statement::Query(query) = &parsed[0] else { unreachable!() };
    /// let context = ExpressionContext::new(query, &db, IdentifierCase::Folded)?;
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let SelectItem::UnnamedExpr(round) = &select.projection[0] else { unreachable!() };
    ///
    /// let call = context.call(round)?.expect("`round` is a plain call");
    /// // `i` converts to `numeric`, so `round(numeric, int4)` runs.
    /// let CallResolution::Selected(selected) = call.resolution() else { unreachable!() };
    /// assert_eq!(selected.arguments()[0].evidence().target().name(), "numeric");
    /// assert_eq!(selected.result_type().map(TargetName::name), Some("numeric"));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn call<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<CallApplication<'node, 'db, DB>>, LookupError> {
        self.check(expression)?;
        let function = match expression {
            Expr::Nested(inner) => return self.call(inner),
            Expr::Function(function) => function,
            _ => return Ok(None),
        };
        let Some((arguments, clauses)) = call_arguments(function) else { return Ok(None) };
        let name = called_name(function)?;
        if expression_form(&function.name).is_some() || is_row(expression) {
            return Ok(None);
        }
        if let Some(rule) = clauses.written_rule() {
            let refused = Some(CallRefusal::Misapplied(rule));
            let resolution = CallResolution::Undetermined;
            return Ok(Some(CallApplication { name, arguments, resolution, refused }));
        }
        let Some(written) = written_argument_names(function) else { return Ok(None) };
        let call = core::ptr::from_ref(function).addr();
        if let Some(target) = self.pins.and_then(|pins| pins.cast(call)) {
            let resolution = CallResolution::Cast(target.clone());
            let refused = clauses.plain_rule().map(CallRefusal::Misapplied);
            return Ok(Some(CallApplication { name, arguments, resolution, refused }));
        }
        let qualified;
        let lookup = match self.pins.and_then(|pins| pins.call_schema(call)) {
            Some((schema, quoted)) if name.schema().is_none() => {
                qualified =
                    TargetName::new(name.name(), name.name_is_quoted()).with_schema(schema, quoted);
                &qualified
            }
            _ => &name,
        };
        let mut reached = reached_by_name(self.database, lookup, &written, self.case);
        if let Some(pins) = self.pins {
            reached.retain(|(function, _)| pins.admits_call(call, function));
        }
        let castable = matches!(written.as_slice(), [None]);
        self.applied_call(name, arguments, &reached, castable, Some(call), Some(clauses)).map(Some)
    }

    /// Returns the function PostgreSQL calls for `factor`, a `FROM` item
    /// written as a call such as `generate_series(1, n)`, `LATERAL`
    /// or not, or `None` for any other item.
    ///
    /// The call is resolved as [`Self::call`] resolves one, its arguments
    /// typed in the scope the item sees.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for an argument of
    /// another query, [`LookupError::InvalidObjectName`] for a called name
    /// with more than two parts or a part built at run time, and the lookup
    /// errors typing an argument return.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("")?;
    /// let parsed =
    ///     Parser::parse_sql(&PostgreSqlDialect {}, "SELECT g FROM generate_series(1, 3) AS g")?;
    /// let Statement::Query(query) = &parsed[0] else { unreachable!() };
    /// let context = ExpressionContext::new(query, &db, IdentifierCase::Folded)?;
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    ///
    /// let call = context.relation_call(&select.from[0].relation)?.expect("a function in `FROM`");
    /// let CallResolution::Selected(selected) = call.resolution() else { unreachable!() };
    /// assert_eq!(selected.result_type().map(TargetName::name), Some("int4"));
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn relation_call<'node>(
        &self,
        factor: &'node TableFactor,
    ) -> Result<Option<CallApplication<'node, 'db, DB>>, LookupError> {
        let (name, listed) = match factor {
            TableFactor::Table { name, args: Some(args), .. } => (name, args.args.as_slice()),
            TableFactor::Function { name, args, .. } => (name, args.as_slice()),
            _ => return Ok(None),
        };
        if expression_form(name).is_some() {
            return Ok(None);
        }
        let (Some(arguments), Some(written)) = (listed_arguments(listed), written_names(listed))
        else {
            return Ok(None);
        };
        let target = called_object_name(name)?;
        let reached = reached_by_name(self.database, &target, &written, self.case);
        let castable = matches!(written.as_slice(), [None]);
        let clauses = CallClauses::listed(listed);
        self.applied_call(target, arguments, &reached, castable, None, Some(clauses)).map(Some)
    }

    /// The call of `name` passing `arguments`, one of the `reached`
    /// functions selected by the arguments' types or, when `castable` says
    /// the call passes one argument under no name, the cast it may read as,
    /// the arguments of the call whose node sits at `call` typed as the pins
    /// have them, refused for the `clauses` it writes as PostgreSQL refuses
    /// them, which a call PostgreSQL's grammar inserts has none of.
    fn applied_call<'node>(
        &self,
        name: TargetName<'node>,
        arguments: Vec<&'node Expr>,
        reached: &[(&'db DB::Function, Matched)],
        castable: bool,
        call: Option<usize>,
        clauses: Option<CallClauses>,
    ) -> Result<CallApplication<'node, 'db, DB>, LookupError> {
        let pinned = |index| {
            let pins = self.pins?;
            pins.argument(call?, index)
        };
        let types = arguments
            .iter()
            .enumerate()
            .map(|(index, argument)| {
                match pinned(index) {
                    Some(pinned) => Ok(pinned.clone()),
                    None => self.type_of(argument),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let resolution =
            resolve_function(self.database, reached, &types, castable.then_some(&name));
        let unresolved =
            |reason| CallRefusal::Unresolved((reason, types.iter().map(weighed_name).collect()));
        let of_kind = |called: &DB::Function| -> Result<Option<CallRefusal>, LookupError> {
            let Some(clauses) = clauses else { return Ok(None) };
            let aggregate = called.aggregate_kind(self.database)?;
            if let Some(rule) =
                clauses.kind_rule(called.kind(self.database)?, aggregate, types.len())
            {
                return Ok(Some(CallRefusal::Misapplied(rule)));
            }
            let divided = divides(called, aggregate, types.len(), clauses.within_group);
            Ok((!divided).then(|| unresolved(Unresolved::DoesNotExist)))
        };
        let refused = match &resolution {
            CallResolution::NotFound => Some(unresolved(Unresolved::DoesNotExist)),
            CallResolution::Ambiguous(_) => Some(unresolved(Unresolved::NotUnique)),
            CallResolution::Selected(selected) => of_kind(selected.function())?,
            CallResolution::UnboundPolymorphic(function) => {
                of_kind(function)?.or_else(|| Some(unresolved(Unresolved::PolymorphicUnknown)))
            }
            CallResolution::Cast(_) => {
                clauses.and_then(CallClauses::plain_rule).map(CallRefusal::Misapplied)
            }
            CallResolution::Undetermined => None,
        };
        Ok(CallApplication { name, arguments, resolution, refused })
    }

    /// The call of the `pg_catalog` function `function` PostgreSQL's grammar
    /// inserts over `arguments`, passed positionally.
    fn inserted_call<'node>(
        &self,
        function: &'static str,
        arguments: Vec<&'node Expr>,
    ) -> Result<CallApplication<'node, 'db, DB>, LookupError> {
        let name = TargetName::new(function, false).with_schema(PG_CATALOG, false);
        let written = Vec::from_iter(core::iter::repeat_n(None, arguments.len()));
        let reached = reached_by_name(self.database, &name, &written, self.case);
        self.applied_call(name, arguments, &reached, false, None, None)
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
                let applications = self.in_list(address(expression), left, elements, negated)?;
                return Ok(applications.map(|applications| (applications, Returns::Bool)));
            }
            Some(Form::Applies(applied, returns)) => (applied, returns),
        };
        let mut applications = Vec::with_capacity(applied.len());
        for (position, Applied { name, left, right }) in applied.into_iter().enumerate() {
            let (right, right_type) = match right {
                Right::Node(node) => (Operand::Node(node), self.type_of(node)?),
                Right::Elements(node, quantifier) => {
                    (
                        quantifier.over(ArraySource::Node(node)),
                        element_type(self.database, self.type_of(node)?),
                    )
                }
                Right::Call(function, arguments) => {
                    let call = self.inserted_call(function, arguments)?;
                    let call_type = match &call.resolution {
                        CallResolution::Selected(selected) => {
                            without_typmod(selected.result_type().cloned())
                        }
                        _ => OperandType::Unknown,
                    };
                    (Operand::Call(Box::new(call)), call_type)
                }
            };
            let site = (address(expression), position);
            applications.push(self.resolved(site, name, left, right, &right_type)?);
        }
        Ok(Some((applications, returns)))
    }

    /// The application at `site` of `name` to `left` and `right`, whose
    /// value or elements are of type `right_type`, as the pins have it.
    fn resolved<'node>(
        &self,
        site: Site,
        name: TargetName<'node>,
        left: Option<&'node Expr>,
        right: Operand<'node, 'db, DB>,
        right_type: &OperandType<'_>,
    ) -> Result<OperatorApplication<'node, 'db, DB>, LookupError> {
        let left_type = left.map(|left| self.type_of(left)).transpose()?;
        let pinned = |left| self.pins.and_then(|pins| pins.operand(site, left));
        let left_type = left_type.as_ref().map(|actual| pinned(true).unwrap_or(actual));
        let right_type = pinned(false).unwrap_or(right_type);
        let qualified;
        let lookup = match self.pins.and_then(|pins| pins.schema(site)) {
            Some((schema, quoted)) if name.schema().is_none() => {
                qualified =
                    TargetName::new(name.name(), name.name_is_quoted()).with_schema(schema, quoted);
                &qualified
            }
            _ => &name,
        };
        let admits =
            |operator: &DB::Operator| self.pins.is_none_or(|pins| pins.admits(site, operator));
        let resolution =
            resolve_operator(self.database, lookup, left_type, right_type, self.case, &admits);
        let reason = match resolution {
            OperatorResolution::NotFound => Some(Unresolved::DoesNotExist),
            OperatorResolution::Ambiguous(_) => Some(Unresolved::NotUnique),
            OperatorResolution::UnboundPolymorphic(_) => Some(Unresolved::PolymorphicUnknown),
            OperatorResolution::Selected(_) | OperatorResolution::Undetermined => None,
        };
        let refused = reason.map(|reason| {
            (reason, left_type.into_iter().chain([right_type]).map(weighed_name).collect())
        });
        Ok(OperatorApplication { name, left, right, resolution, refused })
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
                        without_typmod(selected.into_result_type())
                    }
                    _ => OperandType::Unknown,
                }
            }
        })
    }

    /// The applications of `left [NOT] IN (elements)`, written by the node
    /// at `node`, `None` when a type or a reference they depend on is
    /// unknown or a row is compared.
    fn in_list<'node>(
        &self,
        node: usize,
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
                applications.push(self.resolved(
                    (node, 0),
                    name,
                    Some(left),
                    built,
                    &element_type,
                )?);
                kept
            }
            Collapse::Apart => elements.iter().collect(),
        };
        for element in separate {
            let element_type = self.type_of(element)?;
            let name = TargetName::new(name, false);
            let site = (node, applications.len());
            applications.push(self.resolved(
                site,
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
    pub(crate) fn collapse(
        &self,
        left: &Expr,
        elements: &[&Expr],
    ) -> Result<Collapse<'_>, LookupError> {
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

    /// The type of `VALUE` when `expression` is that keyword in a domain
    /// constraint's context.
    fn domain_value(&self, expression: &Expr) -> Option<OperandType<'db>> {
        match expression {
            Expr::Identifier(Ident { value, quote_style: None, .. })
                if value.eq_ignore_ascii_case("value") =>
            {
                self.value.clone()
            }
            _ => None,
        }
    }

    /// The type of the parameter `$n` names in a function body's context,
    /// and [`OperandType::Unknown`] outside one.
    fn positional(&self, placeholder: &str) -> OperandType<'db> {
        placeholder
            .strip_prefix('$')
            .and_then(|position| position.parse::<usize>().ok())
            .and_then(|position| self.parameters.get(position.checked_sub(1)?))
            .cloned()
            .unwrap_or(OperandType::Unknown)
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

/// Whether `expression` itself applies operators that
/// [`ExpressionContext::operators`] derives, read from how it is written.
pub(crate) fn applies_operators(expression: &Expr) -> bool {
    folded(expression).is_none()
        && match form(expression) {
            Some(Form::Applies(applied, _)) => !applied.is_empty(),
            Some(Form::InList { .. }) => true,
            None => false,
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
            let applied = Applied { name, left: Some(left), right: Right::Node(right) };
            (Vec::from([applied]), Returns::Result)
        }
        Expr::UnaryOp { op, expr } => {
            (Vec::from([Applied::named(prefix_operator_name(*op)?, None, expr)]), Returns::Result)
        }
        Expr::Like { negated, any, expr, pattern, escape_char } => {
            like(if *negated { "!~~" } else { "~~" }, *any, expr, pattern, escape_char.as_deref())?
        }
        Expr::ILike { negated, any, expr, pattern, escape_char } => {
            like(
                if *negated { "!~~*" } else { "~~*" },
                *any,
                expr,
                pattern,
                escape_char.as_deref(),
            )?
        }
        Expr::SimilarTo { negated, expr, pattern, escape_char } => {
            let arguments =
                core::iter::once(pattern.as_ref()).chain(escape_char.as_deref()).collect();
            let applied = Applied {
                name: TargetName::new(if *negated { "!~" } else { "~" }, false),
                left: Some(expr),
                right: Right::Call("similar_to_escape", arguments),
            };
            (Vec::from([applied]), Returns::Result)
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

/// The applications of `expr [NOT] [I]LIKE pattern [ESCAPE escape]` under
/// the operator `name`, or of `expr [NOT] [I]LIKE ANY (pattern)` when `any`.
fn like<'node>(
    name: &'static str,
    any: bool,
    expr: &'node Expr,
    pattern: &'node Expr,
    escape: Option<&'node Expr>,
) -> Option<(Vec<Applied<'node>>, Returns)> {
    let (right, returns) = match (any, escape) {
        (false, None) => (Right::Node(pattern), Returns::Result),
        (false, Some(escape)) => {
            (Right::Call("like_escape", Vec::from([pattern, escape])), Returns::Result)
        }
        (true, None) => (Right::Elements(pattern, Quantifier::Any), Returns::Bool),
        // PostgreSQL's grammar takes no `ESCAPE` after `LIKE ANY`.
        (true, Some(_)) => return None,
    };
    let applied = Applied { name: TargetName::new(name, false), left: Some(expr), right };
    Some((Vec::from([applied]), returns))
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
    let applied = Applied { name, left: Some(left), right: Right::Elements(right, quantifier) };
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

/// The nodes the arguments of `function` come from, in the order it writes
/// them, or `None` for a call written with an aggregate's or a window
/// function's clauses, with a subquery or with a `*` argument.
fn plain_arguments(function: &Function) -> Option<Vec<&Expr>> {
    let plain = matches!(function.parameters, FunctionArguments::None)
        && function.filter.is_none()
        && function.null_treatment.is_none()
        && function.over.is_none()
        && function.within_group.is_empty();
    match &function.args {
        _ if !plain => None,
        FunctionArguments::None => Some(Vec::new()),
        FunctionArguments::Subquery(_) => None,
        FunctionArguments::List(list) => {
            if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
                return None;
            }
            list.args.iter().map(argument_node).collect()
        }
    }
}

/// The clauses a call writes that PostgreSQL checks against the kind of
/// function the call selects.
#[derive(Debug, Clone, Copy)]
struct CallClauses {
    /// The call passes `*`.
    star: bool,
    /// An argument is passed by name.
    named: bool,
    /// `DISTINCT` precedes the arguments.
    distinct: bool,
    /// The arguments end in `ORDER BY`.
    ordered: bool,
    /// How many values `WITHIN GROUP` orders by.
    within_group: usize,
    /// The call writes `FILTER`.
    filter: bool,
    /// The call writes `OVER`.
    over: bool,
}

impl CallClauses {
    /// The clauses of a call passing `arguments` and writing nothing else,
    /// as a function in `FROM` does.
    fn listed(arguments: &[FunctionArg]) -> Self {
        Self {
            star: matches!(arguments, [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]),
            named: arguments.iter().any(|argument| !matches!(argument, FunctionArg::Unnamed(_))),
            distinct: false,
            ordered: false,
            within_group: 0,
            filter: false,
            over: false,
        }
    }

    /// The rule PostgreSQL's grammar refuses the clauses by, whatever
    /// function the call names.
    const fn written_rule(self) -> Option<Misapplied> {
        if self.within_group == 0 {
            None
        } else if self.ordered {
            Some(Misapplied::OrderByWithinGroup)
        } else if self.distinct {
            Some(Misapplied::DistinctWithinGroup)
        } else {
            None
        }
    }

    /// The rule the clauses break on a call of a function that is neither
    /// an aggregate nor a window function, or a call read as a cast.
    const fn plain_rule(self) -> Option<Misapplied> {
        if self.star {
            Some(Misapplied::StarOnPlainFunction)
        } else if self.distinct {
            Some(Misapplied::DistinctOnPlainFunction)
        } else if self.within_group > 0 {
            Some(Misapplied::WithinGroupOnPlainFunction)
        } else if self.ordered {
            Some(Misapplied::OrderByOnPlainFunction)
        } else if self.filter {
            Some(Misapplied::FilterOnPlainFunction)
        } else if self.over {
            Some(Misapplied::OverOnPlainFunction)
        } else {
            None
        }
    }

    /// The rule the clauses break on a call passing `arguments` arguments
    /// to a function of `kind`, an aggregate of `aggregate`, in the order
    /// `ParseFuncOrColumn` checks them.
    const fn kind_rule(
        self,
        kind: FunctionKind,
        aggregate: Option<AggregateKind>,
        arguments: usize,
    ) -> Option<Misapplied> {
        let ordered_set = matches!(
            aggregate,
            Some(AggregateKind::OrderedSet { .. } | AggregateKind::HypotheticalSet { .. })
        );
        let rule = match kind {
            FunctionKind::Normal => return self.plain_rule(),
            FunctionKind::Aggregate if ordered_set => {
                if self.within_group == 0 {
                    Some(Misapplied::WithinGroupRequired)
                } else if self.over {
                    Some(Misapplied::OverOnOrderedSetAggregate)
                } else {
                    None
                }
            }
            FunctionKind::Aggregate if self.within_group > 0 => {
                Some(Misapplied::WithinGroupOnNormalAggregate)
            }
            FunctionKind::Window if !self.over => Some(Misapplied::OverRequired),
            FunctionKind::Window if self.within_group > 0 => {
                Some(Misapplied::WithinGroupOnWindowFunction)
            }
            FunctionKind::Aggregate | FunctionKind::Window => None,
        };
        if rule.is_some() {
            return rule;
        }
        let aggregate = matches!(kind, FunctionKind::Aggregate);
        let starless = aggregate && arguments == 0 && !self.star;
        if !self.over {
            if starless {
                Some(Misapplied::StarRequired)
            } else if self.named {
                Some(Misapplied::NamedArgumentOnAggregate)
            } else {
                None
            }
        } else if self.distinct {
            Some(Misapplied::DistinctOverWindow)
        } else if starless {
            Some(Misapplied::StarRequired)
        } else if self.ordered {
            Some(Misapplied::OrderByOverWindow)
        } else if !aggregate && self.filter {
            Some(Misapplied::FilterOnWindowFunction)
        } else {
            None
        }
    }
}

/// Whether `arguments` arguments, the last `ordered` of them those `WITHIN
/// GROUP` orders by, split into direct and ordered ones as `called`, an
/// aggregate of `aggregate`, declares, which holds for any call of a
/// function no ordered-set aggregate.
///
/// A hypothetical-set aggregate whose direct and ordered arguments are all
/// variadic takes as many direct ones as ordered ones, and an ordered-set
/// one of that shape at least one direct one more than ordered ones.
fn divides<F: FunctionLike>(
    called: &F,
    aggregate: Option<AggregateKind>,
    arguments: usize,
    ordered: usize,
) -> bool {
    let (direct, hypothetical) = match aggregate {
        Some(AggregateKind::OrderedSet { direct_arguments }) => (direct_arguments, false),
        Some(AggregateKind::HypotheticalSet { direct_arguments }) => (direct_arguments, true),
        Some(AggregateKind::Normal) | None => return true,
    };
    let (inputs, variadic) = called
        .argument_modes()
        .filter(|mode| !matches!(mode, ArgMode::Out))
        .fold((0, false), |(inputs, _), mode| (inputs + 1, matches!(mode, ArgMode::Variadic)));
    if !variadic || direct < inputs {
        return arguments - ordered == direct;
    }
    let collected = (arguments + 1).saturating_sub(inputs);
    if hypothetical { collected == 2 * ordered } else { collected > ordered }
}

/// The nodes the arguments of a call come from, those it writes followed by
/// the ordering arguments of `WITHIN GROUP`, with the clauses the call
/// writes, or `None` for a call written with a subquery, a `*` but
/// `count(*)`'s, or a clause PostgreSQL has no grammar for. `DISTINCT`,
/// `ORDER BY`, `FILTER` and `OVER` pass nothing.
fn call_arguments(function: &Function) -> Option<(Vec<&Expr>, CallClauses)> {
    if !matches!(function.parameters, FunctionArguments::None) || function.null_treatment.is_some()
    {
        return None;
    }
    let (mut arguments, mut clauses) = match &function.args {
        FunctionArguments::None => (Vec::new(), CallClauses::listed(&[])),
        FunctionArguments::Subquery(_) => return None,
        FunctionArguments::List(list) => {
            let ordered = |clause: &FunctionArgumentClause| {
                matches!(clause, FunctionArgumentClause::OrderBy(_))
            };
            if !list.clauses.iter().all(ordered) {
                return None;
            }
            let mut clauses = CallClauses::listed(&list.args);
            clauses.distinct = list.duplicate_treatment == Some(DuplicateTreatment::Distinct);
            clauses.ordered = !list.clauses.is_empty();
            (listed_arguments(&list.args)?, clauses)
        }
    };
    arguments.extend(function.within_group.iter().map(|ordering| &ordering.expr));
    clauses.within_group = function.within_group.len();
    clauses.filter = function.filter.is_some();
    clauses.over = function.over.is_some();
    Some((arguments, clauses))
}

/// The nodes `arguments` pass, none for the lone `*` of `count(*)`, or
/// `None` for another `*`.
fn listed_arguments(arguments: &[FunctionArg]) -> Option<Vec<&Expr>> {
    if matches!(arguments, [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]) {
        return Some(Vec::new());
    }
    arguments.iter().map(argument_node).collect()
}

/// The node an argument passes, `None` for a `*`.
const fn argument_node(argument: &FunctionArg) -> Option<&Expr> {
    match argument {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(argument))
        | FunctionArg::Named { arg: FunctionArgExpr::Expr(argument), .. }
        | FunctionArg::ExprNamed { arg: FunctionArgExpr::Expr(argument), .. } => Some(argument),
        _ => None,
    }
}

/// The operands of `function` when it is the `NULLIF` expression, which
/// PostgreSQL's grammar reads only unqualified, unquoted and with two plain
/// positional arguments.
fn nullif_operands(function: &Function) -> Option<(&Expr, &Expr)> {
    if !unquoted_name(function).is_some_and(|name| name.eq_ignore_ascii_case("nullif")) {
        return None;
    }
    let FunctionArguments::List(list) = &function.args else { return None };
    if !list.args.iter().all(|argument| matches!(argument, FunctionArg::Unnamed(_))) {
        return None;
    }
    match *plain_arguments(function)?.as_slice() {
        [left, right] => Some((left, right)),
        _ => None,
    }
}

/// The type of a value of type `type_name`, which carries no modifier.
fn without_typmod(type_name: Option<TargetName<'_>>) -> OperandType<'_> {
    type_name
        .map_or(OperandType::Unknown, |type_name| OperandType::Known { type_name, typmod: None })
}

/// How a refusal names the type an operand or an argument was weighed as,
/// `unknown` for an untyped literal.
fn weighed_name(weighed: &OperandType<'_>) -> Box<str> {
    match weighed {
        OperandType::Known { type_name, .. } => type_name.name().into(),
        OperandType::UntypedLiteral | OperandType::Unknown => "unknown".into(),
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
