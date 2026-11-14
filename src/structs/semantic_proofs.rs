//! The casts and constants PostgreSQL derives for an expression, as
//! sqlparser-canonicalize's [`SemanticContext`] reads them.

use alloc::{boxed::Box, string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::{
    ast::{BinaryOperator, DataType, Expr, Ident, InTarget, Query, Value, Visitor},
    dialect::PostgreSqlDialect,
    parser::Parser,
};
use sqlparser_canonicalize::{
    ArrayCoercions, CanonicalizeError, ComparisonCoercions, Conversion, MembershipCoercions,
    MembershipElement, SemanticContext,
};

use crate::{
    errors::LookupError,
    structs::{
        ArraySource, CoercionStep, ColumnDefinition, ExpressionContext, ExpressionScope,
        IdentifierCase, Operand, OperandEvidence, OperandType, OperatorResolution,
        expression_context::Collapse,
    },
    traits::{CastLike, ColumnLike, DatabaseLike, FunctionLike},
    utils::{
        postgres_types::{PgType, parse_data_type},
        walk::walk,
    },
};

/// The rewrites PostgreSQL's deparse of one expression proves, built once
/// from the [`ExpressionContext`] of the expression.
///
/// A column reference outside any subquery of an expression a table scopes
/// is spelled by its stored name alone, as `pg_get_constraintdef` writes it.
/// A comparison `=`, `<>`, `<`, `>`, `<=` or `>=` whose operator PostgreSQL
/// selects spells each converted operand as the cast or the constant the
/// deparse writes. An untyped literal read as `int2`, `int4` or `int8` is the
/// constant it becomes, a literal read as another type of `pg_catalog` and an
/// operand relabelled, read as its domain's base or converted by a cast
/// function of one argument are cast to the type the operator takes. An `IN`
/// list is the `= ANY (ARRAY[...])` and the comparisons PostgreSQL resolves
/// it into, when every element of the array is an untyped literal or of the
/// array's element type. Every other node is left as written.
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
    /// The conversions of each proved comparison, by address.
    comparisons: Vec<(usize, Coercions)>,
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

/// The conversions of both operands of a comparison.
#[derive(Debug, Clone)]
struct Coercions {
    left: Option<Spelled>,
    right: Option<Spelled>,
}

impl Coercions {
    fn borrowed(&self) -> ComparisonCoercions<'_> {
        ComparisonCoercions {
            left: self.left.as_ref().map(Spelled::conversion),
            right: self.right.as_ref().map(Spelled::conversion),
        }
    }
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
        let context = ExpressionContext::for_expression(expression, scope, database, case)?;
        let mut prover = Prover {
            context: &context,
            database,
            spells_columns: matches!(scope, ExpressionScope::Table(_)),
            depth: 0,
            proofs: Self {
                root: expression,
                nodes: Vec::new(),
                columns: Vec::new(),
                comparisons: Vec::new(),
                memberships: Vec::new(),
            },
        };
        if let ControlFlow::Break(error) = walk(expression, &mut prover) {
            return Err(error);
        }
        let mut proofs = prover.proofs;
        proofs.nodes.sort_unstable();
        proofs.columns.sort_unstable_by_key(|(address, _)| *address);
        proofs.comparisons.sort_unstable_by_key(|(address, _)| *address);
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

    fn comparison_coercions(
        &self,
        occurrence: &Expr,
    ) -> Result<Option<ComparisonCoercions<'_>>, CanonicalizeError> {
        let address = self.proofs.check(occurrence)?;
        Ok(found(&self.proofs.comparisons, address).map(Coercions::borrowed))
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
            Expr::BinaryOp {
                op:
                    BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::Gt
                    | BinaryOperator::LtEq
                    | BinaryOperator::GtEq,
                ..
            } => {
                let proved = self.comparison(expression)?;
                self.proofs.comparisons.extend(proved.map(|coercions| (address, coercions)));
            }
            Expr::In { expr: left, target: InTarget::List(list), global: false, .. } => {
                let proved = self.membership(expression, left, list)?;
                self.proofs.memberships.extend(proved.map(|membership| (address, membership)));
            }
            _ => {}
        }
        Ok(())
    }

    /// The conversions of the comparison `expression`, `None` when one is
    /// unproved.
    fn comparison(&self, expression: &Expr) -> Result<Option<Coercions>, LookupError> {
        let applications = self.context.operators(expression)?.unwrap_or_default();
        let [application] = applications.as_slice() else { return Ok(None) };
        let (Some(left), Operand::Node(right), OperatorResolution::Selected(selected)) =
            (application.left(), application.right(), application.resolution())
        else {
            return Ok(None);
        };
        Ok(self.coercions(selected.left(), left, selected.right(), right))
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
                Some(self.converts(steps).then(|| deparsed(&target)).flatten()?)
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
                common.and_then(|common| literal(common, element)).map(Some)
            }
            typed => {
                common
                    .filter(|common| {
                        matches!(&typed, OperandType::Known { type_name, .. }
                        if PgType::from_target(type_name) == **common)
                    })
                    .map(|_| None)
            }
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
        let spelled = match evidence.conversion() {
            [] => return Ok(None),
            [CoercionStep::LiteralInput { type_name }] => {
                literal(&PgType::from_target(type_name), operand)
            }
            steps => {
                let target = PgType::from_target(evidence.target());
                self.converts(steps).then(|| deparsed(&target)).flatten().map(Spelled::Cast)
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
}

/// A conversion no rewrite is proved for.
struct Unproved;

/// The untyped literal `operand` read as `target`: the constant it becomes
/// for an integer type, as `get_const_expr` writes it, and a cast to
/// `target` otherwise, or `None` for a target the deparse spells
/// differently.
fn literal(target: &PgType, operand: &Expr) -> Option<Spelled> {
    if let (PgType::Catalog(name @ ("int2" | "int4" | "int8")), Expr::Value(value)) =
        (target, operand)
        && let Value::SingleQuotedString(text) = &value.value
        && let Some(constant) = integer_constant(name, text)
    {
        return Some(Spelled::Constant(Box::new(constant)));
    }
    deparsed(target).map(Spelled::Cast)
}

/// The constant the `int2`, `int4` or `int8` named `name` reads `text` as:
/// a bare non-negative `int4` and a quoted cast otherwise, or `None` for text
/// other than decimal digits under an optional sign between white space, or
/// out of range.
fn integer_constant(name: &str, text: &str) -> Option<Expr> {
    // `int4in` skips the white space `isspace` accepts around the digits.
    let value = text.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b').parse::<i64>();
    let value = value.ok()?;
    let (fits, spelling) = match name {
        "int2" => (i16::try_from(value).is_ok(), "smallint"),
        "int4" => (i32::try_from(value).is_ok(), "integer"),
        _ => (true, "bigint"),
    };
    let constant = if name == "int4" && value >= 0 {
        value.to_string()
    } else {
        format!("'{value}'::{spelling}")
    };
    let parsed = Parser::new(&PostgreSqlDialect {}).try_with_sql(&constant).ok()?.parse_expr();
    parsed.ok().filter(|_| fits)
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

/// The type `pg_type` is, spelled as `format_type_with_typemod` spells it
/// with no modifier, or `None` for a type outside `pg_catalog`, whose
/// qualification depends on the deparsing session's `search_path`.
fn deparsed(pg_type: &PgType) -> Option<DataType> {
    parse_data_type(&deparsed_spelling(pg_type)?)
}

/// The text of [`deparsed`].
fn deparsed_spelling(pg_type: &PgType) -> Option<alloc::string::String> {
    match pg_type {
        PgType::Array(element) => Some(format!("{}[]", deparsed_spelling(element)?)),
        PgType::Catalog(name) => {
            let renamed = DEPARSED_NAMES.binary_search_by_key(name, |(stored, _)| stored);
            // `quote_identifier` leaves a lowercase name bare.
            let bare = name.starts_with(|first: char| first.is_ascii_lowercase())
                && name.bytes().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'));
            let spelling = renamed.map(|index| DEPARSED_NAMES[index].1).ok();
            spelling.or_else(|| bare.then_some(*name)).map(ToString::to_string)
        }
        _ => None,
    }
}
