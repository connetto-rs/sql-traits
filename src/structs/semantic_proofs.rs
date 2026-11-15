//! The casts and constants PostgreSQL derives for an expression, as
//! sqlparser-canonicalize's [`SemanticContext`] reads them.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use core::ops::ControlFlow;

use sqlparser::{
    ast::{
        BinaryOperator, CastKind, DataType, Expr, Ident, InTarget, Query, UnaryOperator, Value,
        ValueWithSpan, Visitor,
    },
    dialect::PostgreSqlDialect,
    parser::Parser,
};
use sqlparser_canonicalize::{
    ArrayCoercions, CanonicalizeError, Conversion, MembershipCoercions, MembershipElement,
    OperatorCoercions, PatternCoercions, RangeCoercions, SemanticContext,
};

use crate::{
    errors::LookupError,
    structs::{
        ArraySource, CallResolution, CoercionStep, ColumnDefinition, ExpressionContext,
        ExpressionScope, IdentifierCase, Operand, OperandEvidence, OperandType,
        OperatorApplication, OperatorResolution, TargetName, expression_context::Collapse,
    },
    traits::{CastLike, ColumnLike, DatabaseLike, FunctionLike},
    utils::{
        coercion::{canonical, facts},
        identifier_resolution::identifiers_match,
        object_name::PG_CATALOG,
        postgres_types::{PgType, parse_data_type},
        walk::walk,
    },
};

/// The rewrites PostgreSQL's deparse of one expression proves, built once
/// from the [`ExpressionContext`] of the expression.
///
/// A column reference outside any subquery of an expression a table scopes
/// is spelled by its stored name alone, as `pg_get_constraintdef` writes it.
/// A binary operation other than `AND` and `OR` whose operator PostgreSQL
/// selects spells each converted operand as the cast or the constant the
/// deparse writes. An untyped literal read as `int2`, `int4` or `int8` is the
/// constant it becomes, and a literal read as another type and an operand
/// relabelled, read as its domain's base or converted by a cast function of
/// one argument are cast to the type the operator takes, a type outside
/// `pg_catalog` only when the reading session's `search_path` is known. A
/// literal under `COLLATE` over a type without a collation is the constant
/// of its cast, as PostgreSQL drops the `COLLATE`. A number under minus signs
/// is the one constant the grammar folds it into, when the operator takes it
/// unconverted. A `LIKE`, `ILIKE` or `SIMILAR TO` match is the operator
/// PostgreSQL stores, its pattern under the escape function the grammar
/// inserts, and a `BETWEEN` range the comparisons with its bounds, when every
/// operand they convert is proved. An `IN` list is the `= ANY (ARRAY[...])`
/// and the comparisons PostgreSQL resolves it into, when every element of the
/// array is an untyped literal or of the array's element type. Every other
/// node is left as written.
///
/// [`Self::view`] lends the proofs to a canonicalizer.
///
/// # Example
///
/// ```rust
/// use sql_traits::prelude::*;
/// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
/// use sqlparser_canonicalize::Canonicalizer;
///
/// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (s TEXT, i INT4);")?;
/// let table = db.table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?;
/// let scope = ExpressionScope::Table(table.expect("t exists"));
/// let canonicalizer = Canonicalizer::new(&PostgreSqlDialect {});
/// let canonical = |check: &str| -> Result<String, Box<dyn std::error::Error>> {
///     let check = Parser::new(&PostgreSqlDialect {}).try_with_sql(check)?.parse_expr()?;
///     let proofs = SemanticProofs::for_expression(&check, &scope, &db, IdentifierCase::Folded)?;
///     Ok(canonicalizer.normalize_with_context(&proofs.view())?)
/// };
/// assert_eq!(
///     canonical("s IN ('a', 'b')")?,
///     canonical("(s = ANY (ARRAY['a'::text, 'b'::text]))")?
/// );
/// assert_eq!(canonical("t.i = '7'")?, canonical("(i = 7)")?);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct SemanticProofs<'root> {
    root: &'root Expr,
    /// The address of every expression node of the root, sorted.
    nodes: Vec<usize>,
    /// The spelling of each proved column reference, by address.
    columns: Vec<(usize, Ident)>,
    /// The conversions of each proved binary operation, by address.
    operators: Vec<(usize, Coercions)>,
    /// The conversions of each proved `LIKE`, `ILIKE` or `SIMILAR TO` match,
    /// by address.
    patterns: Vec<(usize, Match)>,
    /// The conversions of the comparisons with the low and the high bound of
    /// each proved `BETWEEN` range, by address.
    ranges: Vec<(usize, (Coercions, Coercions))>,
    /// The resolution of each proved `IN` list, by address.
    memberships: Vec<(usize, Membership)>,
}

/// A converted operand as PostgreSQL's deparse spells it.
#[derive(Debug, Clone)]
enum Spelled {
    /// Cast to this type.
    Cast(DataType),
    /// This constant in place of the literal.
    Constant(Box<Expr>),
}

impl Spelled {
    const fn conversion(&self) -> Conversion<'_> {
        match self {
            Self::Cast(target) => Conversion::Cast(target),
            Self::Constant(constant) => Conversion::Constant(constant),
        }
    }
}

/// The conversions of both operands of a binary operator.
#[derive(Debug, Clone)]
struct Coercions {
    left: Option<Spelled>,
    right: Option<Spelled>,
}

impl Coercions {
    fn borrowed(&self) -> OperatorCoercions<'_> {
        OperatorCoercions {
            left: self.left.as_ref().map(Spelled::conversion),
            right: self.right.as_ref().map(Spelled::conversion),
        }
    }
}

/// The conversions of a `LIKE`, `ILIKE` or `SIMILAR TO` match, the pattern
/// and the escape those of the escape function's arguments when PostgreSQL
/// calls one.
#[derive(Debug, Clone)]
struct Match {
    subject: Option<Spelled>,
    pattern: Option<Spelled>,
    escape: Option<Spelled>,
}

/// The resolution of an `IN` list.
#[derive(Debug, Clone)]
struct Membership {
    /// The conversion of the tested operand and the cast of the built array,
    /// when the list builds one.
    array: Option<(Option<Spelled>, Option<DataType>)>,
    /// The resolution of each element, in list order.
    elements: Vec<Element>,
}

/// How one `IN` list element is compared.
#[derive(Debug, Clone)]
enum Element {
    /// In the built array, with its conversion into the array's element type.
    Array(Option<Spelled>),
    /// Compared on its own.
    Comparison(Coercions),
}

impl<'root> SemanticProofs<'root> {
    /// Builds the proofs of `expression`, written outside a query, over
    /// `database`, its column references reaching what `scope` names,
    /// comparing identifiers under `case`.
    ///
    /// A conversion into a type outside `pg_catalog` stays unproved, since
    /// the deparse spells that type as the reading session's `search_path`
    /// reaches it. [`Self::for_expression_deparsed_on_path`] states the path.
    ///
    /// # Errors
    ///
    /// Returns the errors [`ExpressionContext::for_expression`] returns and
    /// the lookup errors typing an operand of a comparison or an `IN` list
    /// returns.
    pub fn for_expression<'db, DB: DatabaseLike>(
        expression: &'root Expr,
        scope: &ExpressionScope<'_, 'db, DB>,
        database: &'db DB,
        case: IdentifierCase,
    ) -> Result<Self, LookupError> {
        Self::prove(expression, scope, database, case, None)
    }

    /// Builds the proofs of [`Self::for_expression`] for a deparse a session
    /// whose `search_path` names the schemas of `search_path` reads, with
    /// `$user` replaced by the schema it stands for.
    ///
    /// A type is spelled by its name alone when it is the first type of that
    /// name on the path, `pg_catalog` searched first unless the path names
    /// it, and qualified with its schema otherwise, as `format_type` writes
    /// it. A type of `pg_catalog` that an earlier schema hides stays
    /// unproved.
    ///
    /// # Errors
    ///
    /// Returns the errors [`Self::for_expression`] returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    /// use sqlparser_canonicalize::Canonicalizer;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TYPE mood AS ENUM ('happy', 'sad'); CREATE TABLE t (m mood);",
    /// )?;
    /// let table = db.table_by_target(TargetName::new("t", false), IdentifierCase::Folded)?;
    /// let scope = ExpressionScope::Table(table.expect("t exists"));
    /// let canonical = |check: &str, path: &[(&str, bool)]| {
    ///     let check = Parser::new(&PostgreSqlDialect {}).try_with_sql(check)?.parse_expr()?;
    ///     let case = IdentifierCase::Folded;
    ///     let proofs =
    ///         SemanticProofs::for_expression_deparsed_on_path(&check, &scope, &db, case, path)?;
    ///     let canonicalizer = Canonicalizer::new(&PostgreSqlDialect {});
    ///     Ok::<_, Box<dyn std::error::Error>>(canonicalizer.normalize_with_context(&proofs.view())?)
    /// };
    /// let public = [("public", false)];
    /// assert_eq!(canonical("m <> 'sad'", &public)?, canonical("(m <> 'sad'::mood)", &public)?);
    /// assert_eq!(canonical("m <> 'sad'", &[])?, canonical("(m <> 'sad'::public.mood)", &[])?);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn for_expression_deparsed_on_path<'db, DB: DatabaseLike, S: AsRef<str>>(
        expression: &'root Expr,
        scope: &ExpressionScope<'_, 'db, DB>,
        database: &'db DB,
        case: IdentifierCase,
        search_path: &[(S, bool)],
    ) -> Result<Self, LookupError> {
        let path: Vec<(&str, bool)> =
            search_path.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted)).collect();
        Self::prove(expression, scope, database, case, Some(&path))
    }

    /// Builds the proofs for a deparse read on `reader_path`, `None` when
    /// unknown.
    fn prove<'db, DB: DatabaseLike>(
        expression: &'root Expr,
        scope: &ExpressionScope<'_, 'db, DB>,
        database: &'db DB,
        case: IdentifierCase,
        reader_path: Option<&[(&str, bool)]>,
    ) -> Result<Self, LookupError> {
        let context = ExpressionContext::for_expression(expression, scope, database, case)?;
        let mut prover = Prover {
            context: &context,
            database,
            reader_path,
            spells_columns: matches!(scope, ExpressionScope::Table(_)),
            depth: 0,
            proofs: Self {
                root: expression,
                nodes: Vec::new(),
                columns: Vec::new(),
                operators: Vec::new(),
                patterns: Vec::new(),
                ranges: Vec::new(),
                memberships: Vec::new(),
            },
        };
        if let ControlFlow::Break(error) = walk(expression, &mut prover) {
            return Err(error);
        }
        let mut proofs = prover.proofs;
        proofs.nodes.sort_unstable();
        proofs.columns.sort_unstable_by_key(|(address, _)| *address);
        proofs.operators.sort_unstable_by_key(|(address, _)| *address);
        proofs.patterns.sort_unstable_by_key(|(address, _)| *address);
        proofs.ranges.sort_unstable_by_key(|(address, _)| *address);
        proofs.memberships.sort_unstable_by_key(|(address, _)| *address);
        Ok(proofs)
    }

    /// Returns the proofs as the [`SemanticContext`] a canonicalizer reads.
    #[must_use]
    pub fn view(&self) -> SemanticProofView<'_> {
        let memberships = self
            .memberships
            .iter()
            .map(|(address, membership)| {
                let array = membership.array.as_ref().map(|(left, array)| {
                    ArrayCoercions {
                        left: left.as_ref().map(Spelled::conversion),
                        array: array.as_ref(),
                    }
                });
                let elements = membership
                    .elements
                    .iter()
                    .map(|element| {
                        match element {
                            Element::Array(conversion) => {
                                MembershipElement::Array(
                                    conversion.as_ref().map(Spelled::conversion),
                                )
                            }
                            Element::Comparison(coercions) => {
                                MembershipElement::Comparison(coercions.borrowed())
                            }
                        }
                    })
                    .collect();
                (*address, array, elements)
            })
            .collect();
        SemanticProofView { proofs: self, memberships }
    }

    /// Refuses a node that is not one of the root's.
    fn check(&self, occurrence: &Expr) -> Result<usize, CanonicalizeError> {
        let address = address(occurrence);
        if self.nodes.binary_search(&address).is_ok() {
            Ok(address)
        } else {
            Err(CanonicalizeError::Unsupported(
                "The node is not one of the proved expression's".to_string(),
            ))
        }
    }
}

/// The [`SemanticContext`] of [`SemanticProofs`], which
/// [`SemanticProofs::view`] returns.
#[derive(Debug)]
pub struct SemanticProofView<'p> {
    proofs: &'p SemanticProofs<'p>,
    /// The array comparison and the element resolutions of each proved `IN`
    /// list, by address.
    memberships: Vec<(usize, Option<ArrayCoercions<'p>>, Vec<MembershipElement<'p>>)>,
}

impl SemanticContext for SemanticProofView<'_> {
    fn expression(&self) -> &Expr {
        self.proofs.root
    }

    fn column_reference(&self, occurrence: &Expr) -> Result<Option<&[Ident]>, CanonicalizeError> {
        let address = self.proofs.check(occurrence)?;
        Ok(found(&self.proofs.columns, address).map(core::slice::from_ref))
    }

    fn operator_coercions(
        &self,
        occurrence: &Expr,
    ) -> Result<Option<OperatorCoercions<'_>>, CanonicalizeError> {
        let address = self.proofs.check(occurrence)?;
        Ok(found(&self.proofs.operators, address).map(Coercions::borrowed))
    }

    fn pattern_coercions(
        &self,
        occurrence: &Expr,
    ) -> Result<Option<PatternCoercions<'_>>, CanonicalizeError> {
        let address = self.proofs.check(occurrence)?;
        Ok(found(&self.proofs.patterns, address).map(|pattern| {
            PatternCoercions {
                subject: pattern.subject.as_ref().map(Spelled::conversion),
                pattern: pattern.pattern.as_ref().map(Spelled::conversion),
                escape: pattern.escape.as_ref().map(Spelled::conversion),
            }
        }))
    }

    fn range_coercions(
        &self,
        occurrence: &Expr,
    ) -> Result<Option<RangeCoercions<'_>>, CanonicalizeError> {
        let address = self.proofs.check(occurrence)?;
        Ok(found(&self.proofs.ranges, address)
            .map(|(low, high)| RangeCoercions { low: low.borrowed(), high: high.borrowed() }))
    }

    fn membership_coercions(
        &self,
        occurrence: &Expr,
    ) -> Result<Option<MembershipCoercions<'_>>, CanonicalizeError> {
        let address = self.proofs.check(occurrence)?;
        Ok(self.memberships.binary_search_by_key(&address, |(held, ..)| *held).ok().map(|index| {
            let (_, array, elements) = &self.memberships[index];
            MembershipCoercions { array: *array, elements }
        }))
    }
}

/// The entry of `entries`, sorted by address, at `address`.
fn found<T>(entries: &[(usize, T)], address: usize) -> Option<&T> {
    entries.binary_search_by_key(&address, |(held, _)| *held).ok().map(|index| &entries[index].1)
}

/// Where `expression` sits in memory, which identifies it among the nodes of
/// the root.
fn address(expression: &Expr) -> usize {
    core::ptr::from_ref(expression).addr()
}

/// Walks the root, proving each node it meets.
struct Prover<'c, 'query, 'db, 'root, DB: DatabaseLike> {
    context: &'c ExpressionContext<'query, 'db, DB>,
    database: &'db DB,
    /// The `search_path` of the session reading the deparse, `None` when
    /// unknown.
    reader_path: Option<&'c [(&'c str, bool)]>,
    /// Whether the scope is one table, whose columns a bare name reaches
    /// outside any subquery.
    spells_columns: bool,
    /// How many queries nested in the root the walk is inside.
    depth: usize,
    proofs: SemanticProofs<'root>,
}

impl<DB: DatabaseLike> Visitor for Prover<'_, '_, '_, '_, DB> {
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
        match self.prove(expression) {
            Ok(()) => ControlFlow::Continue(()),
            Err(error) => ControlFlow::Break(error),
        }
    }
}

impl<DB: DatabaseLike> Prover<'_, '_, '_, '_, DB> {
    /// Records what `expression` proves.
    fn prove(&mut self, expression: &Expr) -> Result<(), LookupError> {
        let address = address(expression);
        self.proofs.nodes.push(address);
        match expression {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_)
                if self.spells_columns && self.depth == 0 =>
            {
                if let Some((ColumnDefinition::Base { column, .. }, _)) =
                    self.context.reference(expression)?
                {
                    let stored = column.stored_column_name();
                    self.proofs.columns.push((address, Ident::with_quote('"', stored)));
                }
            }
            Expr::BinaryOp { op, .. }
                if !matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
            {
                let proved = self.operator(expression)?;
                self.proofs.operators.extend(proved.map(|coercions| (address, coercions)));
            }
            Expr::Like { .. } | Expr::ILike { .. } | Expr::SimilarTo { .. } => {
                let proved = self.pattern(expression)?;
                self.proofs.patterns.extend(proved.map(|pattern| (address, pattern)));
            }
            Expr::Between { .. } => {
                let proved = self.range(expression)?;
                self.proofs.ranges.extend(proved.map(|bounds| (address, bounds)));
            }
            Expr::In { expr: left, target: InTarget::List(list), global: false, .. } => {
                let proved = self.membership(expression, left, list)?;
                self.proofs.memberships.extend(proved.map(|membership| (address, membership)));
            }
            _ => {}
        }
        Ok(())
    }

    /// The conversions of the binary operation `expression`, `None` when one
    /// is unproved.
    fn operator(&self, expression: &Expr) -> Result<Option<Coercions>, LookupError> {
        let applications = self.context.operators(expression)?.unwrap_or_default();
        let [application] = applications.as_slice() else { return Ok(None) };
        Ok(self.applied(application))
    }

    /// The conversions of `application` to two operand nodes, `None` when
    /// one is unproved.
    fn applied(&self, application: &OperatorApplication<'_, '_, DB>) -> Option<Coercions> {
        let (Some(left), Operand::Node(right), OperatorResolution::Selected(selected)) =
            (application.left(), application.right(), application.resolution())
        else {
            return None;
        };
        self.coercions(selected.left(), left, selected.right(), right)
    }

    /// The conversions of the `LIKE`, `ILIKE` or `SIMILAR TO` match
    /// `expression`, `None` when one is unproved.
    fn pattern(&self, expression: &Expr) -> Result<Option<Match>, LookupError> {
        let applications = self.context.operators(expression)?.unwrap_or_default();
        let [application] = applications.as_slice() else { return Ok(None) };
        let (Some(subject), OperatorResolution::Selected(selected)) =
            (application.left(), application.resolution())
        else {
            return Ok(None);
        };
        let subject = selected.left().map(|evidence| self.spelled(evidence, subject));
        let (pattern, escape) = match application.right() {
            Operand::Node(pattern) => (self.spelled(selected.right(), pattern), Ok(None)),
            // The escape function's result is the operator's operand as it
            // is, its arguments converted to its parameters.
            Operand::Call(call) if selected.right().conversion().is_empty() => {
                let CallResolution::Selected(function) = call.resolution() else { return Ok(None) };
                let mut arguments = function
                    .arguments()
                    .iter()
                    .zip(call.arguments())
                    .map(|(argument, written)| self.spelled(argument.evidence(), written));
                (arguments.next().unwrap_or(Ok(None)), arguments.next().unwrap_or(Ok(None)))
            }
            _ => return Ok(None),
        };
        let (Some(Ok(subject)), Ok(pattern), Ok(escape)) = (subject, pattern, escape) else {
            return Ok(None);
        };
        Ok(Some(Match { subject, pattern, escape }))
    }

    /// The conversions of the comparisons with the low and the high bound of
    /// the `BETWEEN` range `expression`, `None` when one is unproved.
    fn range(&self, expression: &Expr) -> Result<Option<(Coercions, Coercions)>, LookupError> {
        let applications = self.context.operators(expression)?.unwrap_or_default();
        let [low, high] = applications.as_slice() else { return Ok(None) };
        Ok(self.applied(low).zip(self.applied(high)))
    }

    /// The conversions of `left` and `right` as `left_evidence` and
    /// `right_evidence` have them, `None` when one is unproved.
    fn coercions(
        &self,
        left_evidence: Option<&OperandEvidence<'_, DB>>,
        left: &Expr,
        right_evidence: &OperandEvidence<'_, DB>,
        right: &Expr,
    ) -> Option<Coercions> {
        Some(Coercions {
            left: self.spelled(left_evidence?, left).ok()?,
            right: self.spelled(right_evidence, right).ok()?,
        })
    }

    /// The resolution of the `IN` list `expression`, testing `left` against
    /// `list`, `None` when part of it is unproved.
    fn membership(
        &self,
        expression: &Expr,
        left: &Expr,
        list: &[Expr],
    ) -> Result<Option<Membership>, LookupError> {
        let Some(applications) = self.context.operators(expression)? else { return Ok(None) };
        let mut array = None;
        let mut built: &[&Expr] = &[];
        let mut separate = Vec::new();
        for application in &applications {
            let OperatorResolution::Selected(selected) = application.resolution() else {
                return Ok(None);
            };
            let right = application.right();
            if let Operand::Any(ArraySource::Built(elements))
            | Operand::All(ArraySource::Built(elements)) = right
            {
                // An unproved array leaves its elements unresolved.
                array = self.array_coercions(selected.left(), left, selected.right());
                built = if array.is_some() { elements } else { &[] };
                continue;
            }
            let Operand::Node(element) = right else { return Ok(None) };
            let coercions = self.coercions(selected.left(), left, selected.right(), element);
            separate.push((address(element), coercions));
        }
        let common = match built {
            [] => None,
            built => self.common_type(left, built)?,
        };
        let mut elements = Vec::with_capacity(list.len());
        for element in list {
            let resolution = if built.iter().any(|held| core::ptr::eq(*held, element)) {
                self.array_element(common.as_ref(), element)?.ok().map(Element::Array)
            } else {
                let address = address(element);
                separate
                    .iter()
                    .position(|(held, _)| *held == address)
                    .and_then(|index| separate.swap_remove(index).1)
                    .map(Element::Comparison)
            };
            let Some(resolution) = resolution else { return Ok(None) };
            elements.push(resolution);
        }
        Ok(Some(Membership { array, elements }))
    }

    /// The conversion of `left` and the cast of the array an `IN` list
    /// builds, as the evidence of the array comparison has them, `None` when
    /// one is unproved.
    fn array_coercions(
        &self,
        left_evidence: Option<&OperandEvidence<'_, DB>>,
        left: &Expr,
        elements: &OperandEvidence<'_, DB>,
    ) -> Option<(Option<Spelled>, Option<DataType>)> {
        let converted = self.spelled(left_evidence?, left).ok()?;
        let cast = match elements.conversion() {
            [] => None,
            steps => {
                let target = PgType::from_target(elements.target()).array();
                Some(self.converts(steps).then(|| self.deparsed(&target)).flatten()?)
            }
        };
        Some((converted, cast))
    }

    /// The element type of the array PostgreSQL builds of `built`, the
    /// elements of an `IN` list testing `left`.
    fn common_type(&self, left: &Expr, built: &[&Expr]) -> Result<Option<PgType>, LookupError> {
        let common = self.context.collapse(left, built)?;
        let Collapse::Into(OperandType::Known { type_name, .. }) = common else { return Ok(None) };
        Ok(Some(PgType::from_target(&type_name)))
    }

    /// The conversion of `element` into `common`, the element type of the
    /// array it is built into.
    fn array_element(
        &self,
        common: Option<&PgType>,
        element: &Expr,
    ) -> Result<Result<Option<Spelled>, Unproved>, LookupError> {
        let converted = match self.context.type_of(element)? {
            OperandType::UntypedLiteral => {
                common.and_then(|common| self.literal(common, element)).map(Some)
            }
            OperandType::Known { type_name, .. }
                if common == Some(&PgType::from_target(&type_name)) =>
            {
                match folded(element) {
                    Ok(Some((pg_type, constant))) => {
                        (common == Some(&pg_type))
                            .then(|| Some(Spelled::Constant(Box::new(constant))))
                    }
                    Ok(None) => Some(None),
                    Err(Unproved) => None,
                }
            }
            _ => None,
        };
        Ok(converted.ok_or(Unproved))
    }

    /// The spelling of `operand` converted as `evidence` has it, `None` when
    /// there is no conversion.
    fn spelled(
        &self,
        evidence: &OperandEvidence<'_, DB>,
        operand: &Expr,
    ) -> Result<Option<Spelled>, Unproved> {
        if let Some((pg_type, constant)) = folded(operand)? {
            // A converted constant is spelled as a cast of its own cast.
            let typed = evidence.conversion().is_empty()
                && PgType::from_target(evidence.target()) == pg_type;
            return typed.then(|| Some(Spelled::Constant(Box::new(constant)))).ok_or(Unproved);
        }
        let spelled = match evidence.conversion() {
            [] => return Ok(None),
            [CoercionStep::LiteralInput { type_name }] => {
                self.literal(&PgType::from_target(type_name), operand)
            }
            steps => {
                let target = PgType::from_target(evidence.target());
                self.converts(steps).then(|| self.deparsed(&target)).flatten().map(Spelled::Cast)
            }
        };
        spelled.map(Some).ok_or(Unproved)
    }

    /// Whether `steps` read a value as its domain's base, relabel it or
    /// convert it by a cast function of one argument, which the deparse
    /// writes as one cast to the target.
    fn converts(&self, steps: &[CoercionStep<'_, DB>]) -> bool {
        let single = |step: &CoercionStep<'_, DB>| {
            matches!(step, CoercionStep::Relabel(_))
                || matches!(step, CoercionStep::Function(cast) if cast
                    .function(self.database)
                    .is_some_and(|function| function.argument_type_names(self.database).count() == 1))
        };
        let rest =
            if let [CoercionStep::DomainToBase { .. }, rest @ ..] = steps { rest } else { steps };
        !steps.is_empty() && rest.len() <= 1 && rest.iter().all(single)
    }

    /// The untyped literal `operand` read as `target`: the constant it
    /// becomes for an integer type, as `get_const_expr` writes it, and a cast
    /// to `target` otherwise, or `None` for a target the deparse spells
    /// differently.
    ///
    /// A `COLLATE` over the literal takes the cast under it, and a target
    /// without a collation drops it, leaving the constant of the cast.
    fn literal(&self, target: &PgType, operand: &Expr) -> Option<Spelled> {
        let (written, collated) = written_literal(operand);
        if let (
            PgType::Catalog(name @ ("int2" | "int4" | "int8")),
            Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(text), .. }),
        ) = (target, written)
            && let Some(constant) = integer_constant(name, text)
        {
            return Some(Spelled::Constant(Box::new(constant)));
        }
        let cast = self.deparsed(target)?;
        if collated && facts(self.database, target)?.collation().is_none() {
            let constant = Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(written.clone()),
                data_type: cast,
                format: None,
            };
            return Some(Spelled::Constant(Box::new(constant)));
        }
        Some(Spelled::Cast(cast))
    }

    /// The type `pg_type` is, spelled as `format_type_with_typemod` spells it
    /// with no modifier in the reading session, or `None` when that spelling
    /// is unknown.
    fn deparsed(&self, pg_type: &PgType) -> Option<DataType> {
        parse_data_type(&self.spelling(&canonical(self.database, pg_type))?)
    }

    /// The text of [`Self::deparsed`] for a canonical type.
    fn spelling(&self, pg_type: &PgType) -> Option<String> {
        match pg_type {
            PgType::Array(element) => Some(format!("{}[]", self.spelling(element)?)),
            PgType::Catalog(name) => {
                let hidden = self.reader_path.is_some_and(|path| {
                    self.first_holder(path, name).is_some_and(|(schema, quoted)| {
                        !identifiers_match(schema, quoted, PG_CATALOG, false)
                    })
                });
                if hidden { None } else { catalog_spelling(name) }
            }
            named => {
                let (PgType::Named(name), Some(path)) = (named, self.reader_path) else {
                    return None;
                };
                let schema = name.schema()?;
                let typname = quote_identifier(name.name());
                let visible = self
                    .first_holder(path, name.name())
                    .is_some_and(|(first, quoted)| identifiers_match(first, quoted, schema, true));
                Some(if visible {
                    typname
                } else {
                    format!("{}.{typname}", quote_identifier(schema))
                })
            }
        }
    }

    /// The first schema on the type search path of `path` holding a type
    /// stored as `name`, `pg_catalog` searched first unless `path` names it.
    fn first_holder<'p>(&self, path: &'p [(&'p str, bool)], name: &str) -> Option<(&'p str, bool)> {
        let catalog = path
            .iter()
            .any(|&(schema, quoted)| identifiers_match(schema, quoted, PG_CATALOG, false));
        (!catalog).then_some((PG_CATALOG, false)).into_iter().chain(path.iter().copied()).find(
            |&(schema, quoted)| {
                let qualified = TargetName::new(name, true).with_schema(schema, quoted);
                self.database.type_facts(qualified).is_some()
            },
        )
    }
}

/// A conversion no rewrite is proved for.
struct Unproved;

/// The literal under the parentheses and `COLLATE` clauses of `operand`, and
/// whether a `COLLATE` holds it.
fn written_literal(operand: &Expr) -> (&Expr, bool) {
    let mut collated = false;
    let mut node = operand;
    loop {
        match node {
            Expr::Nested(inner) => node = inner,
            Expr::Collate { expr, .. } => {
                collated = true;
                node = expr;
            }
            written => return (written, collated),
        }
    }
}

/// The constant the `int2`, `int4` or `int8` named `name` reads `text` as,
/// or `None` for text other than decimal digits under an optional sign
/// between white space, or out of range.
fn integer_constant(name: &'static str, text: &str) -> Option<Expr> {
    // `int4in` skips the white space `isspace` accepts around the digits.
    let value = text.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b').parse::<i64>();
    let value = value.ok()?;
    let fits = match name {
        "int2" => i16::try_from(value).is_ok(),
        "int4" => i32::try_from(value).is_ok(),
        _ => true,
    };
    constant(name, &value.to_string()).filter(|_| fits)
}

/// The constant PostgreSQL's grammar folds `operand` into, a number under one
/// or more minus signs and any parentheses, with its type, or `None` for any
/// other operand.
///
/// A number written other than as decimal digits around an optional point is
/// unproved.
fn folded(operand: &Expr) -> Result<Option<(PgType, Expr)>, Unproved> {
    let mut negative = None;
    let mut node = operand;
    let text = loop {
        match node {
            Expr::Nested(inner) => node = inner,
            Expr::UnaryOp { op: UnaryOperator::Minus, expr } => {
                negative = Some(!negative.unwrap_or(false));
                node = expr;
            }
            Expr::Value(ValueWithSpan { value: Value::Number(text, _), .. }) => break text,
            _ => return Ok(None),
        }
    };
    let Some(negative) = negative else { return Ok(None) };
    let (whole, fraction) = text
        .split_once('.')
        .map_or((text.as_str(), None), |(whole, fraction)| (whole, Some(fraction)));
    let decimal = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if !decimal(whole) || !fraction.is_none_or(decimal) {
        return Err(Unproved);
    }
    // `numeric_in` drops the leading zeros and the sign of a zero.
    let whole = whole.trim_start_matches('0');
    let zero = whole.is_empty() && fraction.is_none_or(|digits| digits.bytes().all(|b| b == b'0'));
    let sign = if negative && !zero { "-" } else { "" };
    let whole = if whole.is_empty() { "0" } else { whole };
    let (name, value) = match fraction {
        // `make_const` reads an integer as the narrowest of `int4` and
        // `int8` that holds it.
        None => {
            let value = format!("{sign}{whole}");
            let name = match value.parse::<i64>() {
                Ok(integer) if i32::try_from(integer).is_ok() => "int4",
                Ok(_) => "int8",
                Err(_) => "numeric",
            };
            (name, value)
        }
        Some("") => ("numeric", format!("{sign}{whole}")),
        Some(fraction) => ("numeric", format!("{sign}{whole}.{fraction}")),
    };
    constant(name, &value).map(|constant| Some((PgType::Catalog(name), constant))).ok_or(Unproved)
}

/// The constant `value` of the `pg_catalog` type `name` as `get_const_expr`
/// writes it: bare for a non-negative `int4` and a non-negative `numeric`
/// with a point, and quoted under a cast otherwise.
fn constant(name: &'static str, value: &str) -> Option<Expr> {
    let bare =
        !value.starts_with('-') && (name == "int4" || (name == "numeric" && value.contains('.')));
    let spelling =
        if bare { value.to_string() } else { format!("'{value}'::{}", catalog_spelling(name)?) };
    Parser::new(&PostgreSqlDialect {}).try_with_sql(&spelling).ok()?.parse_expr().ok()
}

/// The spelling `format_type_with_typemod` gives a `pg_catalog` type with no
/// modifier where it differs from the type's stored name.
const DEPARSED_NAMES: [(&str, &str); 14] = [
    ("bit", "\"bit\""),
    ("bool", "boolean"),
    ("char", "\"char\""),
    ("float4", "real"),
    ("float8", "double precision"),
    ("int2", "smallint"),
    ("int4", "integer"),
    ("int8", "bigint"),
    ("time", "time without time zone"),
    ("timestamp", "timestamp without time zone"),
    ("timestamptz", "timestamp with time zone"),
    ("timetz", "time with time zone"),
    ("varbit", "bit varying"),
    ("varchar", "character varying"),
];

/// The keywords PostgreSQL 18's `quote_identifier` quotes, every keyword but
/// the unreserved ones, as `pg_get_keywords` lists them in byte order.
const QUOTED_KEYWORDS: [&str; 164] = [
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// The spelling `format_type_with_typemod` gives the `pg_catalog` type stored
/// as `name` with no modifier, when `pg_catalog` comes first on the type
/// search path.
fn catalog_spelling(name: &str) -> Option<String> {
    let renamed = DEPARSED_NAMES.binary_search_by_key(&name, |(stored, _)| stored);
    // `quote_identifier` leaves a lowercase name bare.
    let bare = name.starts_with(|first: char| first.is_ascii_lowercase())
        && name.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'));
    let spelling = renamed.map(|index| DEPARSED_NAMES[index].1).ok();
    spelling.or_else(|| bare.then_some(name)).map(ToString::to_string)
}

/// `name` as `quote_identifier` writes it: bare when it is lowercase letters,
/// digits and underscores not led by a digit and no keyword
/// [`QUOTED_KEYWORDS`] holds, and quoted otherwise.
fn quote_identifier(name: &str) -> String {
    let bare = name.starts_with(|first: char| first.is_ascii_lowercase() || first == '_')
        && name.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && QUOTED_KEYWORDS.binary_search(&name).is_err();
    if bare { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}
