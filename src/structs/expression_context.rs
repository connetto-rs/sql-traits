//! The facts each expression node of one query takes.

use alloc::{string::ToString, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{CastKind, DataType, Expr, Query, Value};

use crate::{
    errors::LookupError,
    impls::dql::definition_graph::reference_sites::SiteRule,
    structs::{
        ColumnBinding, ColumnDefinition, ColumnScope, IdentifierCase, OperandType, TargetName,
    },
    traits::{ColumnLike, DataTypeLike, DatabaseLike},
    utils::{
        coercion::{canonical_owned, type_name},
        object_name::PG_CATALOG,
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
        Ok(Self { database, scope, nodes, sites })
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
        Value::Number(text, _) => catalog(number_type(text)),
        Value::Boolean(_) => catalog("bool"),
        // `N'…'` reads as a `bpchar` and `X'…'` and `B'…'` as bit strings.
        Value::NationalStringLiteral(_) => catalog("bpchar"),
        Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_) => catalog("bit"),
        _ => OperandType::Unknown,
    }
}

/// The type of the numeric literal `text`, the first of `int4` and `int8`
/// holding an integer and `numeric` for everything else.
fn number_type(text: &str) -> &'static str {
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
    if i32::try_from(value).is_ok() {
        "int4"
    } else if i64::try_from(value).is_ok() {
        "int8"
    } else {
        "numeric"
    }
}
