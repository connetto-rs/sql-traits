//! The values PostgreSQL brings to one type where they meet, as
//! `select_common_type` picks it, and how it converts each.

use alloc::{vec, vec::Vec};
use core::ops::ControlFlow;

use sqlparser::ast::{
    CastKind, DataType, Expr, MergeAction, MergeInsertExpr, MergeInsertKind, Statement, Values,
    Visitor,
};

use super::{
    ExpressionContext, address, assignments::direct_values, declared_type, known,
    positional_arguments, unparenthesized,
};
use crate::{
    errors::LookupError,
    structs::{OperandEvidence, OperandType, TargetName, TypeFacts},
    traits::DatabaseLike,
    utils::{
        call_sites::{ExpressionForm, expression_form},
        call_typing::typed_input,
        coercion::{CommonType, canonical_owned, conversion, facts, select_common_type, type_name},
        postgres_types::PgType,
    },
};

/// Where a value meeting others at a common type comes from.
#[derive(Debug, Clone, Copy)]
pub enum CommonTypeInput<'node> {
    /// A `THEN` or `ELSE` result of a `CASE`, an argument of `COALESCE`,
    /// `GREATEST` or `LEAST`, an element of an `ARRAY[...]` or a cell of a
    /// `VALUES` column.
    Expression(&'node Expr),
    /// The `NULL` a `CASE` without `ELSE` results in when no `WHEN` holds.
    ImplicitElse,
}

/// The type values meet at and how PostgreSQL converts each to it.
#[derive(Debug, Clone)]
pub enum CommonTypeResolution<'db, DB: DatabaseLike> {
    /// PostgreSQL converts every input to `common` under
    /// `COERCION_IMPLICIT`.
    Selected {
        /// The type the inputs meet at.
        common: TargetName<'db>,
        /// What each input takes, in the order of the inputs.
        inputs: Vec<OperandEvidence<'db, DB>>,
    },
    /// PostgreSQL refuses the values, since no type they all convert to
    /// implicitly exists, or no array type holds an `ARRAY[...]`'s elements.
    Refused,
    /// The type of an input, or a cast or a type fact the selection needs,
    /// is missing.
    Undetermined,
}

/// Values PostgreSQL brings to one type, as [`ExpressionContext::common_type`]
/// and [`ExpressionContext::values_common_types`] answer them.
#[derive(Debug, Clone)]
pub struct CommonTypeApplication<'node, 'db, DB: DatabaseLike> {
    inputs: Vec<CommonTypeInput<'node>>,
    resolution: CommonTypeResolution<'db, DB>,
}

impl<'node, 'db, DB: DatabaseLike> CommonTypeApplication<'node, 'db, DB> {
    /// Returns where the values come from, in the order the query writes
    /// them, the implicit `ELSE` of a `CASE` last.
    #[must_use]
    pub fn inputs(&self) -> &[CommonTypeInput<'node>] {
        &self.inputs
    }

    /// Returns the type the values meet at and how each converts to it, the
    /// values typed as [`ExpressionContext::type_of`] types their nodes.
    #[must_use]
    pub const fn resolution(&self) -> &CommonTypeResolution<'db, DB> {
        &self.resolution
    }
}

/// A type with the modifier its values carry.
type Modified = (PgType, Option<i32>);

/// How values meet, and the type of the result with the modifier it keeps,
/// the latter only for a resolution that selects a type.
struct Met<'db, DB: DatabaseLike> {
    resolution: CommonTypeResolution<'db, DB>,
    result: Option<Modified>,
}

impl<DB: DatabaseLike> Met<'_, DB> {
    const fn refused() -> Self {
        Self { resolution: CommonTypeResolution::Refused, result: None }
    }

    const fn undetermined() -> Self {
        Self { resolution: CommonTypeResolution::Undetermined, result: None }
    }
}

/// The inputs of one site and how they meet.
type Site<'node, 'db, DB> = (Vec<CommonTypeInput<'node>>, Met<'db, DB>);

/// What a cast written over an `ARRAY[...]` hands its elements.
enum ArrayCast {
    /// This array type, with its modifier.
    Typed(Modified),
    /// A type the facts do not settle.
    Unknown,
}

impl<'db, DB: DatabaseLike> ExpressionContext<'_, 'db, DB> {
    /// Returns the values PostgreSQL brings to one type to evaluate
    /// `expression`, the type `select_common_type` picks for them and how
    /// `coerce_to_common_type` converts each, or `None` for a node that is
    /// no such site.
    ///
    /// The sites are a `CASE`, whose results meet, its `ELSE` result weighed
    /// first and a missing one the `NULL` PostgreSQL supplies, `COALESCE`,
    /// `GREATEST` and `LEAST` written bare with positional arguments, and an
    /// `ARRAY[...]`, whose elements meet. An `ARRAY[...]` a cast to an array
    /// type is written over, alone or nested in such an array, answers
    /// `None`, since PostgreSQL converts its elements to the cast's element
    /// type as the cast asks. A parenthesized node answers for its content.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a node of another
    /// query, and the lookup errors typing an input returns.
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
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (a INT4, n NUMERIC);")?;
    /// let sql = "SELECT CASE WHEN true THEN a ELSE n END FROM t";
    /// let parsed = Parser::parse_sql(&PostgreSqlDialect {}, sql)?;
    /// let Statement::Query(query) = &parsed[0] else { unreachable!() };
    /// let context = ExpressionContext::new(query, &db, IdentifierCase::Folded)?;
    /// let SetExpr::Select(select) = query.body.as_ref() else { unreachable!() };
    /// let SelectItem::UnnamedExpr(case) = &select.projection[0] else { unreachable!() };
    ///
    /// let application = context.common_type(case)?.expect("a CASE");
    /// let CommonTypeResolution::Selected { common, inputs } = application.resolution() else {
    ///     unreachable!()
    /// };
    /// assert_eq!(common.name(), "numeric");
    /// // `a` converts by the cast `numeric(int4)`, `n` stays as it is.
    /// assert!(matches!(inputs[0].conversion(), [CoercionStep::Function(_)]));
    /// assert!(inputs[1].conversion().is_empty());
    /// # Ok::<(), sql_traits::errors::Error>(())
    /// ```
    pub fn common_type<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<CommonTypeApplication<'node, 'db, DB>>, LookupError> {
        self.check(expression)?;
        Ok(self
            .site(expression)?
            .map(|(inputs, met)| CommonTypeApplication { inputs, resolution: met.resolution }))
    }

    /// Returns, for each column of the `VALUES` list `values`, the cells of
    /// its rows and the type PostgreSQL brings them to, as
    /// `transformValuesClause` resolves the column.
    ///
    /// The answer is `None` for the `VALUES` an `INSERT` stores row by row,
    /// whose cells [`Self::assignments`] answers, for the row of the
    /// `INSERT` of a `MERGE`, and for rows of different lengths, which
    /// PostgreSQL refuses.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ExpressionNotInQuery`] for a list of another
    /// query, and the lookup errors typing a cell returns.
    pub fn values_common_types<'node>(
        &self,
        values: &'node Values,
    ) -> Result<Option<Vec<CommonTypeApplication<'node, 'db, DB>>>, LookupError> {
        let Some(first) = values.rows.first() else { return Ok(None) };
        let Some(cell) = first.content.first() else { return Ok(None) };
        self.check(cell)?;
        let width = first.content.len();
        if self.direct_values.binary_search(&values_address(values)).is_ok()
            || values.rows.iter().any(|row| row.content.len() != width)
        {
            return Ok(None);
        }
        let mut columns = Vec::with_capacity(width);
        for column in 0..width {
            let inputs: Vec<CommonTypeInput<'node>> = values
                .rows
                .iter()
                .map(|row| CommonTypeInput::Expression(&row.content[column]))
                .collect();
            let types = self.input_types(&inputs)?;
            let resolution = self.meet(types.as_deref(), None).resolution;
            columns.push(CommonTypeApplication { inputs, resolution });
        }
        Ok(Some(columns))
    }

    /// The type [`Self::type_of`] answers for the common-type site
    /// `expression`, an `ARRAY[...]` a cast types taking the cast's array
    /// type.
    pub(super) fn common_result(&self, expression: &Expr) -> Result<OperandType<'db>, LookupError> {
        if let Expr::Array(_) = expression
            && let Some(cast) = self.array_cast(expression)
        {
            return Ok(match cast {
                ArrayCast::Typed((array, typmod)) => known(self.database, array, typmod),
                ArrayCast::Unknown => OperandType::Unknown,
            });
        }
        Ok(match self.site(expression)? {
            Some((_, Met { result: Some((result, typmod)), .. })) => {
                known(self.database, result, typmod)
            }
            _ => OperandType::Unknown,
        })
    }

    /// The type of the values of the column whose cells, one per row, take
    /// the types `cells`.
    pub(super) fn values_column_type(&self, cells: &[OperandType<'_>]) -> OperandType<'db> {
        let types: Option<Vec<_>> =
            cells.iter().map(|cell| typed_input(self.database, cell)).collect();
        match self.meet(types.as_deref(), None).result {
            Some((result, typmod)) => known(self.database, result, typmod),
            None => OperandType::Unknown,
        }
    }

    /// The inputs of the common-type site `expression` and how they meet,
    /// `None` for a node that is no such site.
    fn site<'node>(
        &self,
        expression: &'node Expr,
    ) -> Result<Option<Site<'node, 'db, DB>>, LookupError> {
        Ok(Some(match expression {
            Expr::Nested(inner) => return self.site(inner),
            Expr::Case { conditions, else_result, .. } => {
                let mut inputs: Vec<CommonTypeInput<'node>> = conditions
                    .iter()
                    .map(|when| CommonTypeInput::Expression(&when.result))
                    .collect();
                inputs.push(
                    else_result
                        .as_deref()
                        .map_or(CommonTypeInput::ImplicitElse, CommonTypeInput::Expression),
                );
                let types = self.input_types(&inputs)?;
                // PostgreSQL weighs the `ELSE` result before the others.
                let met = self.meet(types.as_deref(), Some(inputs.len() - 1));
                (inputs, met)
            }
            Expr::Function(function)
                if expression_form(&function.name) == Some(ExpressionForm::CommonType) =>
            {
                let Some(arguments) =
                    positional_arguments(function).filter(|found| !found.is_empty())
                else {
                    return Ok(None);
                };
                let inputs: Vec<CommonTypeInput<'node>> =
                    arguments.into_iter().map(CommonTypeInput::Expression).collect();
                let types = self.input_types(&inputs)?;
                let met = self.meet(types.as_deref(), None);
                (inputs, met)
            }
            Expr::Array(array) => {
                let inputs: Vec<CommonTypeInput<'node>> =
                    array.elem.iter().map(CommonTypeInput::Expression).collect();
                let met = match self.array_cast(expression) {
                    Some(ArrayCast::Typed(_)) => return Ok(None),
                    Some(ArrayCast::Unknown) => Met::undetermined(),
                    // An empty array has no type to take.
                    None if inputs.is_empty() => Met::refused(),
                    None => {
                        let types = self.input_types(&inputs)?;
                        self.array(self.meet(types.as_deref(), None))
                    }
                };
                (inputs, met)
            }
            _ => return Ok(None),
        }))
    }

    /// The types of `inputs`, `None` when one is unknown.
    fn input_types(
        &self,
        inputs: &[CommonTypeInput<'_>],
    ) -> Result<Option<Vec<Modified>>, LookupError> {
        let mut collected = Vec::with_capacity(inputs.len());
        for input in inputs {
            let input_type = match input {
                CommonTypeInput::Expression(expression) => {
                    typed_input(self.database, &self.type_of(expression)?)
                }
                CommonTypeInput::ImplicitElse => Some((PgType::UNKNOWN, None)),
            };
            let Some(input_type) = input_type else { return Ok(None) };
            collected.push(input_type);
        }
        Ok(Some(collected))
    }

    /// How values of `types` meet, the input at `first` weighed before the
    /// others, the result keeping the modifier every input carries
    /// unconverted, as `select_common_typmod` keeps it.
    fn meet(&self, types: Option<&[Modified]>, first: Option<usize>) -> Met<'db, DB> {
        let Some(types) = types else { return Met::undetermined() };
        let weighed: Vec<&PgType> = first
            .into_iter()
            .chain((0..types.len()).filter(|position| Some(*position) != first))
            .map(|position| &types[position].0)
            .collect();
        let common = match select_common_type(self.database, &weighed, |pg_type| *pg_type) {
            Ok(Some(CommonType::At(position))) => weighed[position].clone(),
            Ok(Some(CommonType::Base(base))) => base,
            Ok(Some(CommonType::AllUnknown)) => PgType::TEXT,
            Ok(None) => return Met::refused(),
            Err(_) => return Met::undetermined(),
        };
        self.selected(types, canonical_owned(self.database, common))
            .unwrap_or_else(Met::undetermined)
    }

    /// How values of `types` convert to `common`, `None` when the facts
    /// name no conversion.
    fn selected(&self, types: &[Modified], common: PgType) -> Option<Met<'db, DB>> {
        let database = self.database;
        let name = type_name(database, &common)?;
        let mut inputs = Vec::with_capacity(types.len());
        let mut kept: Option<Option<i32>> = None;
        let mut agree = true;
        for (source, typmod) in types {
            let steps = conversion(database, source, *typmod, &common)?;
            // A converted value carries no modifier.
            let carried = if steps.is_empty() { *typmod } else { None };
            agree &= *kept.get_or_insert(carried) == carried;
            inputs.push(OperandEvidence::new(name.clone(), false, steps));
        }
        let typmod = if agree { kept.flatten() } else { None };
        Some(Met {
            resolution: CommonTypeResolution::Selected { common: name, inputs },
            result: Some((common, typmod)),
        })
    }

    /// The meeting of the elements of an `ARRAY[...]`, its result the array
    /// type over their common type, or that type itself when the elements
    /// are arrays that build a multidimensional one.
    fn array(&self, met: Met<'db, DB>) -> Met<'db, DB> {
        let Some((common, typmod)) = met.result else { return met };
        if common.element().is_some() {
            return Met { result: Some((common, typmod)), ..met };
        }
        match facts(self.database, &common) {
            Some(facts) if facts.array_type().is_some() => {
                Met { result: Some((common.array(), typmod)), ..met }
            }
            Some(_) => Met::refused(),
            None => Met::undetermined(),
        }
    }

    /// What the cast written over the `ARRAY[...]` `array` hands its
    /// elements, `None` when no cast to an array type is written over it.
    fn array_cast(&self, array: &Expr) -> Option<ArrayCast> {
        let position =
            self.array_casts.binary_search_by_key(&address(array), |(held, _)| *held).ok()?;
        let declared = declared_type(self.database, &self.array_casts[position].1);
        let Some((pg_type, typmod)) = typed_input(self.database, &declared) else {
            return Some(ArrayCast::Unknown);
        };
        // A cast to a domain over an array hands the elements its base type.
        let (target, typmod) = match facts(self.database, &pg_type).and_then(TypeFacts::into_domain)
        {
            Some(domain) => {
                let typmod = domain.base_typmod();
                (canonical_owned(self.database, PgType::from_target(&domain.into_base())), typmod)
            }
            None => (pg_type, typmod),
        };
        target.element().is_some().then_some(ArrayCast::Typed((target, typmod)))
    }
}

/// What the walk over a context's root collects: the address of every
/// expression node, the `ARRAY[...]` nodes a cast is written over, with the
/// cast's type, and the `VALUES` lists PostgreSQL stores row by row.
#[derive(Default)]
pub(super) struct Inventory {
    pub(super) nodes: Vec<usize>,
    pub(super) array_casts: Vec<(usize, DataType)>,
    pub(super) direct_values: Vec<usize>,
}

impl Visitor for Inventory {
    type Break = ();

    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<()> {
        self.nodes.push(address(expression));
        if let Expr::Cast {
            kind: CastKind::Cast | CastKind::DoubleColon, expr, data_type, ..
        } = expression
        {
            // PostgreSQL hands the cast's type down to the sub-arrays too.
            let mut arrays = vec![&**expr];
            while let Some(node) = arrays.pop() {
                let node = unparenthesized(node);
                if let Expr::Array(array) = node {
                    self.array_casts.push((address(node), data_type.clone()));
                    arrays.extend(&array.elem);
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<()> {
        match statement {
            Statement::Insert(insert) => {
                if let Some(source) = insert.source.as_deref()
                    && direct_values(source).is_some()
                    && let sqlparser::ast::SetExpr::Values(values) = source.body.as_ref()
                {
                    self.direct_values.push(values_address(values));
                }
            }
            Statement::Merge(merge) => {
                for clause in &merge.clauses {
                    if let MergeAction::Insert(MergeInsertExpr {
                        kind: MergeInsertKind::Values(values),
                        ..
                    }) = &clause.action
                    {
                        self.direct_values.push(values_address(values));
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl Inventory {
    /// Sorts what the walk collected for lookups by address.
    pub(super) fn sorted(mut self) -> Self {
        self.nodes.sort_unstable();
        self.array_casts.sort_unstable_by_key(|(held, _)| *held);
        self.direct_values.sort_unstable();
        self
    }
}

/// The address of the `VALUES` list `values`, which identifies it.
fn values_address(values: &Values) -> usize {
    core::ptr::from_ref(values).addr()
}
