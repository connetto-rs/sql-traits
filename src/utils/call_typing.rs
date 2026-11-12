//! Matches a written call against a function's parameters and resolves the
//! types its polymorphic parameters and result take, as PostgreSQL's
//! `func_get_detail` and `enforce_generic_type_consistency` do.

use alloc::{borrow::Cow, boxed::Box, string::ToString, vec::Vec};

use sqlparser::ast::{
    ArgMode, ArrayElemTypeDef, DataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments,
};

use crate::{
    errors::LookupError,
    structs::{IdentifierCase, TargetName, TypeCategory},
    traits::{DatabaseLike, FunctionLike},
    utils::{
        call_sites::{ExpressionForm, expression_form},
        coercion::{
            CommonType, Proof, Undetermined, base, can_coerce, canonical_owned, common_type_name,
            facts, select_common_type,
        },
        identifier_resolution::identifiers_match,
        object_name::{
            function_search_path, overqualified_object_name, run_time_object_name,
            target_name_from_object_name,
        },
        postgres_types::{PgType, Polymorphic, parse_data_type},
    },
};

/// The name an argument is passed under in named notation.
type WrittenName<'c> = Option<(&'c str, bool)>;

/// The arguments a call writes, by the name each is passed under.
///
/// [`None`] when an argument is named by something other than an identifier,
/// which no parameter answers. `count(*)` passes nothing.
pub(crate) fn written_argument_names(call: &Function) -> Option<Vec<WrittenName<'_>>> {
    match &call.args {
        FunctionArguments::None => Some(Vec::new()),
        FunctionArguments::Subquery(_) => Some(vec![None]),
        FunctionArguments::List(list)
            if matches!(
                list.args.as_slice(),
                [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]
            ) =>
        {
            Some(Vec::new())
        }
        FunctionArguments::List(list) => {
            list.args
                .iter()
                .map(|argument| {
                    match argument {
                        FunctionArg::Unnamed(_) => Some(None),
                        FunctionArg::Named { name, .. }
                        | FunctionArg::ExprNamed { name: Expr::Identifier(name), .. } => {
                            Some(Some((name.value.as_str(), name.quote_style.is_some())))
                        }
                        FunctionArg::ExprNamed { .. } => None,
                    }
                })
                .collect()
        }
    }
}

/// Whether input parameters of `modes`, the last `defaults` defaulted, take
/// `arguments` positional arguments.
pub(crate) fn accepts_positional(modes: &[ArgMode], defaults: usize, arguments: usize) -> bool {
    let variadic = modes.last() == Some(&ArgMode::Variadic);
    let required = modes.len().saturating_sub(defaults);
    arguments >= required && (variadic || arguments <= modes.len())
}

/// The modes of a function's input parameters, the ones a call passes.
pub(crate) fn input_modes<F: FunctionLike>(function: &F) -> Vec<ArgMode> {
    function.argument_modes().filter(|mode| *mode != ArgMode::Out).collect()
}

/// One input parameter, with the name it is declared under and its type.
struct Input<'f> {
    name: Option<TargetName<'f>>,
    declared: Option<PgType>,
}

/// The type a `VARIADIC` parameter declares each argument it collects to be.
fn variadic_element(declared: &PgType) -> Option<PgType> {
    match declared.polymorphic() {
        _ if declared.is_any() => Some(declared.clone()),
        Some(Polymorphic::Array) => Some(PgType::Catalog("anyelement")),
        Some(Polymorphic::CompatibleArray) => Some(PgType::Catalog("anycompatible")),
        _ => declared.element().cloned(),
    }
}

/// The type each written argument is declared to take, [`None`] where the
/// declaration does not parse.
pub(crate) type Declared = Vec<Option<PgType>>;

/// The type each written argument is declared to take, by PostgreSQL's
/// `MatchNamedCall` rules, or [`None`] when the function does not take the
/// call.
pub(crate) fn declared_at_call<F: FunctionLike>(
    function: &F,
    database: &F::DB,
    written: &[WrittenName<'_>],
) -> Option<Declared> {
    let modes: Vec<ArgMode> = function.argument_modes().collect();
    let inputs: Vec<Input<'_>> = function
        .argument_type_names(database)
        .zip(function.argument_names(database))
        .zip(&modes)
        .filter(|(_, mode)| **mode != ArgMode::Out)
        .map(|((declared, name), _)| {
            let declared = PgType::parse(&declared).map(|parsed| canonical_owned(database, parsed));
            Input { name, declared }
        })
        .collect();
    let input_modes: Vec<ArgMode> =
        modes.into_iter().filter(|mode| *mode != ArgMode::Out).collect();
    let defaults = function.argument_default_count();
    let positional = written.iter().take_while(|name| name.is_none()).count();
    if written[positional..].iter().any(Option::is_none) {
        return None;
    }

    if positional == written.len() {
        if !accepts_positional(&input_modes, defaults, written.len()) {
            return None;
        }
        let variadic = input_modes.last() == Some(&ArgMode::Variadic);
        let fixed = if variadic { inputs.len() - 1 } else { inputs.len() };
        return Some(
            (0..written.len())
                .map(|position| {
                    if position < fixed {
                        inputs[position].declared.clone()
                    } else {
                        inputs
                            .last()
                            .and_then(|input| input.declared.as_ref())
                            .and_then(variadic_element)
                    }
                })
                .collect(),
        );
    }

    // Named notation passes a `VARIADIC` parameter its array whole.
    let mut filled = vec![false; inputs.len()];
    let mut declared = Vec::with_capacity(written.len());
    for (position, name) in written.iter().enumerate() {
        let index = match name {
            None => position,
            Some((name, quoted)) => {
                inputs.iter().position(|input| {
                    input.name.as_ref().is_some_and(|declared| {
                        identifiers_match(declared.name(), declared.name_is_quoted(), name, *quoted)
                    })
                })?
            }
        };
        let slot = filled.get_mut(index)?;
        if *slot {
            return None;
        }
        *slot = true;
        declared.push(inputs[index].declared.clone());
    }
    let required = inputs.len().saturating_sub(defaults);
    filled[..required].iter().all(|filled| *filled).then_some(declared)
}

/// The type of one argument, as the caller gave it.
#[derive(Clone)]
pub(crate) struct Actual<'a> {
    /// The type the name denotes.
    pub(crate) pg_type: PgType,
    /// The name as the caller wrote it.
    pub(crate) spelling: &'a str,
}

/// Reads the argument types a caller gives, [`None`] where the caller does
/// not know one or names no type.
pub(crate) fn actuals<'a, D: DatabaseLike>(
    database: &D,
    argument_types: &[Option<&'a str>],
) -> Vec<Option<Actual<'a>>> {
    argument_types
        .iter()
        .map(|name| {
            name.and_then(|spelling| {
                let pg_type = canonical_owned(database, PgType::parse(spelling)?);
                Some(Actual { pg_type, spelling })
            })
        })
        .collect()
}

/// Checks that the call passes one type per argument.
pub(crate) fn check_argument_count(
    call: &Function,
    written: usize,
    given: usize,
) -> Result<(), LookupError> {
    if written == given {
        Ok(())
    } else {
        Err(LookupError::ArgumentTypeCount {
            call: call.to_string(),
            arguments: written,
            types: given,
        })
    }
}

/// A type a polymorphic parameter binds to, with how to spell it.
#[derive(Clone)]
pub(crate) struct Bound<'a> {
    /// The bound type.
    pub(crate) pg_type: PgType,
    spelling: Spelling<'a>,
}

/// How to write a bound type.
#[derive(Clone)]
enum Spelling<'a> {
    /// The caller's own name for it.
    Given(&'a str),
    /// The element type of the array the caller named.
    ElementOf(&'a str),
    /// The type's own spelling, for the base type of a domain the caller
    /// named.
    Canonical,
    /// `text`, what unknown types alone resolve to.
    Text,
}

impl<'a> Bound<'a> {
    fn given(actual: &Actual<'a>) -> Self {
        Self { pg_type: actual.pg_type.clone(), spelling: Spelling::Given(actual.spelling) }
    }

    /// The type the caller named with domains reduced to their base types, as
    /// the array families bind it.
    fn flattened<D: DatabaseLike>(database: &D, actual: &Actual<'a>) -> Result<Self, Undetermined> {
        Ok(match base(database, &actual.pg_type)? {
            Cow::Borrowed(_) => Self::given(actual),
            Cow::Owned(pg_type) => Self { pg_type, spelling: Spelling::Canonical },
        })
    }

    /// The element of the array type this binds.
    fn element(&self) -> Option<Self> {
        let spelling = match self.spelling {
            Spelling::Given(spelling) => Spelling::ElementOf(spelling),
            Spelling::ElementOf(_) | Spelling::Canonical | Spelling::Text => Spelling::Canonical,
        };
        Some(Self { pg_type: self.pg_type.element()?.clone(), spelling })
    }

    /// The name of the bound type.
    fn name(&self) -> Option<Cow<'a, str>> {
        match self.spelling {
            Spelling::Given(spelling) => Some(Cow::Borrowed(spelling)),
            Spelling::ElementOf(spelling) => {
                if let DataType::Array(
                    ArrayElemTypeDef::AngleBracket(element)
                    | ArrayElemTypeDef::SquareBracket(element, _)
                    | ArrayElemTypeDef::Parenthesis(element)
                    | ArrayElemTypeDef::ParenthesisNotNull(element)
                    | ArrayElemTypeDef::Qualified(element, _),
                ) = parse_data_type(spelling)?
                {
                    return Some(Cow::Owned(innermost_element(*element).to_string()));
                }
                // `_int4` names the array by the catalog's own spelling.
                Some(self.pg_type.spelling())
            }
            Spelling::Canonical => Some(self.pg_type.spelling()),
            Spelling::Text => Some(Cow::Borrowed("TEXT")),
        }
    }

    /// The name of the array type over the bound type.
    fn array_name(&self) -> Option<Cow<'a, str>> {
        if self.pg_type.is_array() {
            return None;
        }
        let element = parse_data_type(&self.name()?)?;
        Some(Cow::Owned(
            DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(element), None)).to_string(),
        ))
    }
}

/// The element of a multidimensional array declaration, which is the element
/// of every dimension.
fn innermost_element(data_type: DataType) -> DataType {
    match data_type {
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => innermost_element(*element),
        element => element,
    }
}

/// What the polymorphic parameters of a function bind to for one call.
pub(crate) struct Binding<'a> {
    /// The type the call's arguments are declared to take, by position.
    pub(crate) declared: Cow<'a, [Option<PgType>]>,
    /// What `anyelement` binds to, when the call decides it.
    element: Family<Bound<'a>>,
    /// What `anyarray` binds to, when an argument decides it directly.
    array: Option<Bound<'a>>,
    /// What `anycompatible` binds to.
    compatible: Family<Bound<'a>>,
    /// Whether an argument of a known or unknown type, other than an untyped
    /// literal, reaches a range pseudo-type.
    range_bound: bool,
    /// Whether the database's facts leave open that the function takes the
    /// argument types.
    uncertain: bool,
}

/// What one family of polymorphic parameters binds to.
enum Family<T> {
    /// The arguments decide this type.
    Decided(T),
    /// No argument of the family has a type, or every one is an untyped
    /// literal.
    Unbound,
    /// An argument of the family has a type the caller does not know, or the
    /// database's facts do not settle the family's type.
    Undetermined,
}

/// Binds the polymorphic parameters as `enforce_generic_type_consistency` does,
/// or [`None`] when the database proves the function cannot take the argument
/// types.
pub(crate) fn bind<'a, D: DatabaseLike>(
    database: &D,
    declared: impl Into<Cow<'a, [Option<PgType>]>>,
    actuals: &[Option<Actual<'a>>],
) -> Option<Binding<'a>> {
    let declared = declared.into();
    let ((element, array, compatible, range_bound), uncertain) =
        bind_families(database, &declared, actuals)?;
    Some(Binding { declared, element, array, compatible, range_bound, uncertain })
}

/// What `anyelement`, `anyarray` and `anycompatible` bind to, `anyarray` only
/// when an argument decides it directly, and whether an argument other than
/// an untyped literal reaches a range pseudo-type.
type Families<'a> = (Family<Bound<'a>>, Option<Bound<'a>>, Family<Bound<'a>>, bool);

/// The work of [`bind`], over borrowed declared types, with whether the
/// database's facts leave the binding open.
fn bind_families<'a, D: DatabaseLike>(
    database: &D,
    declared: &[Option<PgType>],
    actuals: &[Option<Actual<'a>>],
) -> Option<(Families<'a>, bool)> {
    let mut element: Option<Bound<'a>> = None;
    let mut array: Option<Bound<'a>> = None;
    let mut element_undetermined = false;
    let mut nonarray = false;
    let mut compatible: Vec<Bound<'a>> = Vec::new();
    let mut compatible_undetermined = false;
    let mut compatible_nonarray = false;
    let mut range_bound = false;
    let mut uncertain = false;

    for (declared, actual) in declared.iter().zip(actuals) {
        // A declaration this model cannot read takes anything.
        let Some(declared) = declared else { continue };
        let known = actual.as_ref().filter(|actual| !actual.pg_type.is_unknown());
        match declared.polymorphic() {
            None => {
                if let Some(actual) = actual {
                    match can_coerce(database, &actual.pg_type, declared) {
                        Proof::Yes => {}
                        Proof::No => return None,
                        Proof::Unknown => uncertain = true,
                    }
                }
            }
            // A value outside the range category is no range. Which range or
            // multirange binds, and over which subtype, needs `pg_range`
            // facts this model does not hold.
            Some(Polymorphic::Range) => {
                range_bound |= actual.as_ref().is_none_or(|actual| !actual.pg_type.is_unknown());
                if let Some(actual) = known {
                    let category = base(database, &actual.pg_type)
                        .ok()
                        .and_then(|base| facts(database, &base))
                        .map(|facts| facts.category());
                    match category {
                        Some(category) if category != TypeCategory::RANGE => return None,
                        _ => uncertain = true,
                    }
                }
            }
            Some(Polymorphic::Element { nonarray: refuses_arrays }) => {
                nonarray |= refuses_arrays;
                match (actual, known) {
                    (None, _) => element_undetermined = true,
                    (Some(_), None) => {}
                    (Some(_), Some(actual)) => {
                        if element.as_ref().is_some_and(|bound| bound.pg_type != actual.pg_type) {
                            return None;
                        }
                        element = Some(Bound::given(actual));
                    }
                }
            }
            Some(Polymorphic::Array) => {
                match (actual, known) {
                    (None, _) => element_undetermined = true,
                    (Some(_), None) => {}
                    (Some(_), Some(actual)) => {
                        let Ok(bound) = Bound::flattened(database, actual) else {
                            element_undetermined = true;
                            uncertain = true;
                            continue;
                        };
                        if !bound.pg_type.is_array()
                            || array.as_ref().is_some_and(|array| array.pg_type != bound.pg_type)
                        {
                            return None;
                        }
                        array = Some(bound);
                    }
                }
            }
            Some(Polymorphic::Compatible { nonarray: refuses_arrays }) => {
                compatible_nonarray |= refuses_arrays;
                match (actual, known) {
                    (None, _) => compatible_undetermined = true,
                    (Some(_), None) => {}
                    (Some(_), Some(actual)) => compatible.push(Bound::given(actual)),
                }
            }
            Some(Polymorphic::CompatibleArray) => {
                match (actual, known) {
                    (None, _) => compatible_undetermined = true,
                    (Some(_), None) => {}
                    (Some(_), Some(actual)) => {
                        let Ok(bound) = Bound::flattened(database, actual) else {
                            compatible_undetermined = true;
                            uncertain = true;
                            continue;
                        };
                        compatible.push(bound.element()?);
                    }
                }
            }
        }
    }

    let (element, element_uncertain) =
        element_family(database, element, array.as_ref(), element_undetermined, nonarray)?;
    let (compatible, compatible_uncertain) =
        compatible_family(database, compatible, compatible_undetermined, compatible_nonarray)?;
    Some((
        (element, array, compatible, range_bound),
        uncertain || element_uncertain || compatible_uncertain,
    ))
}

/// How a `nonarray` form takes the bound type, [`None`] when the type is an
/// array or a domain over one, which the form refuses, and otherwise whether
/// the database's facts leave that open.
fn takes_as_nonarray<D: DatabaseLike>(database: &D, bound: &Bound<'_>) -> Option<bool> {
    match base(database, &bound.pg_type) {
        Ok(base) => (!base.is_array()).then_some(false),
        Err(Undetermined) => Some(true),
    }
}

/// What `anyelement` binds to, the element of the `anyarray` argument when no
/// `anyelement` argument decides it, with whether the database's facts leave
/// it open, or [`None`] when the two disagree.
fn element_family<'a, D: DatabaseLike>(
    database: &D,
    element: Option<Bound<'a>>,
    array: Option<&Bound<'a>>,
    undetermined: bool,
    nonarray: bool,
) -> Option<(Family<Bound<'a>>, bool)> {
    let from_array = match array {
        Some(array) => Some(array.element()?),
        None => None,
    };
    let element = match (element, from_array) {
        (Some(element), Some(from_array)) if element.pg_type != from_array.pg_type => return None,
        (Some(element), _) => Some(element),
        (None, from_array) => from_array,
    };
    let uncertain = match &element {
        Some(bound) if nonarray => takes_as_nonarray(database, bound)?,
        _ => false,
    };
    Some((
        match element {
            _ if undetermined => Family::Undetermined,
            Some(bound) => Family::Decided(bound),
            None => Family::Unbound,
        },
        uncertain,
    ))
}

/// What `anycompatible` binds to, the common type of its arguments, with
/// whether the database's facts leave that type open, or [`None`] when they
/// prove the arguments have none.
fn compatible_family<'a, D: DatabaseLike>(
    database: &D,
    mut bounds: Vec<Bound<'a>>,
    undetermined: bool,
    nonarray: bool,
) -> Option<(Family<Bound<'a>>, bool)> {
    if undetermined {
        return Some((Family::Undetermined, false));
    }
    let common = match select_common_type(database, &bounds, |bound| &bound.pg_type) {
        Err(Undetermined) => return Some((Family::Undetermined, true)),
        Ok(None) => return None,
        Ok(Some(CommonType::AllUnknown)) => return Some((Family::Unbound, false)),
        Ok(Some(CommonType::At(position))) => bounds.swap_remove(position),
        Ok(Some(CommonType::Base(pg_type))) => Bound { pg_type, spelling: Spelling::Canonical },
    };
    let uncertain = if nonarray { takes_as_nonarray(database, &common)? } else { false };
    Some((Family::Decided(common), uncertain))
}

impl<'a> Binding<'a> {
    /// The type a call returns when the function's declared result is
    /// `result`, or [`None`] when the arguments do not decide it.
    ///
    /// `anyelement` takes the type its arguments agree on, and an untyped
    /// literal alone leaves it undecided, where PostgreSQL refuses the call.
    /// `anycompatible` takes the common type, `text` when only untyped
    /// literals bind it. The array forms take the array over that type.
    pub(crate) fn result(&self, result: Cow<'a, str>) -> Option<Cow<'a, str>> {
        let Some(polymorphic) = PgType::parse(&result).and_then(|parsed| parsed.polymorphic())
        else {
            return Some(result);
        };
        match self.bound(polymorphic) {
            BoundType::Type(bound) => bound.name(),
            BoundType::ArrayOf(bound) => bound.array_name(),
            BoundType::Unbound | BoundType::Undetermined => None,
        }
    }

    /// What a parameter declared `polymorphic` binds to for the call.
    pub(crate) fn bound(&self, polymorphic: Polymorphic) -> BoundType<'_, 'a> {
        let (family, array) = match polymorphic {
            Polymorphic::Element { .. } => (&self.element, false),
            Polymorphic::Array => {
                if let (Some(array), Family::Decided(_)) = (&self.array, &self.element) {
                    return BoundType::Type(Cow::Borrowed(array));
                }
                (&self.element, true)
            }
            Polymorphic::Compatible { .. } => (&self.compatible, false),
            Polymorphic::CompatibleArray => (&self.compatible, true),
            // Untyped literals alone bind no range.
            Polymorphic::Range => {
                return if self.range_bound { BoundType::Undetermined } else { BoundType::Unbound };
            }
        };
        let bound = match family {
            Family::Decided(bound) => Cow::Borrowed(bound),
            // `anycompatible` resolves untyped literals alone as `text`.
            Family::Unbound
                if matches!(polymorphic, Polymorphic::Compatible { .. })
                    || polymorphic == Polymorphic::CompatibleArray =>
            {
                Cow::Owned(Bound { pg_type: PgType::TEXT, spelling: Spelling::Text })
            }
            Family::Unbound => return BoundType::Unbound,
            Family::Undetermined => return BoundType::Undetermined,
        };
        if array { BoundType::ArrayOf(bound) } else { BoundType::Type(bound) }
    }
}

/// What a polymorphic parameter binds to for one call.
pub(crate) enum BoundType<'b, 'a> {
    /// This type.
    Type(Cow<'b, Bound<'a>>),
    /// The array type over this type.
    ArrayOf(Cow<'b, Bound<'a>>),
    /// Only untyped literals reach the parameter, which PostgreSQL refuses.
    Unbound,
    /// What the parameter binds to is not known.
    Undetermined,
}

/// Which of the functions a call reaches it runs.
pub(crate) enum Selection<T> {
    /// PostgreSQL runs this one.
    Chosen(T),
    /// PostgreSQL refuses the call, since no function takes the argument
    /// types or several do and nothing tells them apart.
    Refused,
    /// PostgreSQL runs one of these functions or refuses the call, and the
    /// argument types the caller does not know or the facts the database
    /// does not hold leave open which.
    Undetermined(Vec<T>),
}

/// The declared type at a position, compared the way PostgreSQL compares a
/// candidate's argument type.
fn declared_type<'b>(binding: &'b Binding<'_>, position: usize) -> Option<&'b PgType> {
    binding.declared.get(position).and_then(Option::as_ref)
}

/// The candidates `kept` holds, left undetermined.
fn undetermined<T: Copy>(kept: &[&(T, Binding<'_>)]) -> Selection<T> {
    Selection::Undetermined(kept.iter().map(|(candidate, _)| *candidate).collect())
}

/// The one candidate `kept` holds.
fn only<T: Copy>(kept: &[&(T, Binding<'_>)]) -> Option<T> {
    match kept {
        [(chosen, _)] => Some(*chosen),
        _ => None,
    }
}

/// Keeps the candidates of `kept` with the best of `scores`, which keeps them
/// all when none scores.
fn keep_best<'c, 'b, T>(
    kept: &[&'c (T, Binding<'b>)],
    scores: Vec<usize>,
) -> Vec<&'c (T, Binding<'b>)> {
    let best = scores.iter().copied().max().unwrap_or(0);
    kept.iter().zip(scores).filter(|(_, score)| *score == best).map(|(kept, _)| *kept).collect()
}

/// Picks the function a call runs as `func_get_detail` and
/// `func_select_candidate` do.
pub(crate) fn select<D: DatabaseLike, T: Copy>(
    database: &D,
    candidates: &[(T, Binding<'_>)],
    actuals: &[Option<Actual<'_>>],
) -> Selection<T> {
    let all: Vec<&(T, Binding<'_>)> = candidates.iter().collect();
    if all.is_empty() {
        return Selection::Refused;
    }
    let Some(inputs) = actuals
        .iter()
        .map(|actual| actual.as_ref().map(|actual| &actual.pg_type))
        .collect::<Option<Vec<&PgType>>>()
    else {
        return match all.as_slice() {
            [(chosen, binding)] if !binding.uncertain => Selection::Chosen(*chosen),
            _ => undetermined(&all),
        };
    };

    let exact: Vec<&(T, Binding<'_>)> = all
        .iter()
        .copied()
        .filter(|(_, binding)| {
            inputs
                .iter()
                .enumerate()
                .all(|(position, input)| declared_type(binding, position) == Some(*input))
        })
        .collect();
    if let Some(chosen) = only(&exact) {
        return Selection::Chosen(chosen);
    }
    // Which candidates take the argument types at all is open.
    if all.iter().any(|(_, binding)| binding.uncertain) {
        return undetermined(&all);
    }
    if let Some(chosen) = only(&all) {
        return Selection::Chosen(chosen);
    }
    // The ranking reads domains as their base types.
    let Ok(inputs) = inputs
        .iter()
        .map(|input| base(database, input))
        .collect::<Result<Vec<Cow<'_, PgType>>, _>>()
    else {
        return undetermined(&all);
    };
    rank(database, &all, &inputs)
}

/// The ranking steps of `func_select_candidate` over candidates that all
/// take the argument types, the domains among which are reduced to their
/// base types.
fn rank<D: DatabaseLike, T: Copy>(
    database: &D,
    all: &[&(T, Binding<'_>)],
    inputs: &[Cow<'_, PgType>],
) -> Selection<T> {
    let known = |position: usize| inputs.get(position).filter(|input| !input.is_unknown());
    let exact_matches = all
        .iter()
        .map(|(_, binding)| {
            (0..inputs.len())
                .filter(|&position| {
                    known(position)
                        .is_some_and(|input| declared_type(binding, position) == Some(input))
                })
                .count()
        })
        .collect();
    let kept = keep_best(all, exact_matches);
    if let Some(chosen) = only(&kept) {
        return Selection::Chosen(chosen);
    }
    let preferred_matches = inputs
        .iter()
        .map(|input| {
            if input.is_unknown() {
                return Ok(None);
            }
            facts(database, input).map(|facts| Some(facts.category())).ok_or(Undetermined)
        })
        .collect::<Result<Vec<Option<TypeCategory>>, _>>()
        .and_then(|categories| {
            kept.iter()
                .map(|(_, binding)| preferred_matches(database, binding, inputs, &categories))
                .collect::<Result<Vec<usize>, _>>()
        });
    let Ok(preferred_matches) = preferred_matches else { return undetermined(&kept) };
    let kept = keep_best(&kept, preferred_matches);
    if let Some(chosen) = only(&kept) {
        return Selection::Chosen(chosen);
    }
    let unknowns: Vec<usize> =
        (0..inputs.len()).filter(|&position| inputs[position].is_unknown()).collect();
    if unknowns.is_empty() {
        return Selection::Refused;
    }

    let kept = match leaning_categories(database, &kept, &unknowns) {
        Ok(Some(survivors)) => survivors,
        Ok(None) => kept,
        Err(Undetermined) => return undetermined(&kept),
    };
    if let Some(chosen) = only(&kept) {
        return Selection::Chosen(chosen);
    }

    // Last gasp, untyped literals take the one type all other arguments share.
    let mut others = inputs.iter().filter(|input| !input.is_unknown());
    if let Some(shared) = others.next().filter(|first| others.all(|other| other == *first)) {
        let assumed: Vec<Option<Actual<'_>>> = inputs
            .iter()
            .map(|_| Some(Actual { pg_type: PgType::clone(shared), spelling: "" }))
            .collect();
        let mut taking = Vec::new();
        let mut open = false;
        for candidate in &kept {
            match bind_families(database, &candidate.1.declared, &assumed) {
                None => {}
                Some((_, uncertain)) => {
                    open |= uncertain;
                    taking.push(*candidate);
                }
            }
        }
        match (taking.as_slice(), open) {
            ([(chosen, _)], false) => return Selection::Chosen(*chosen),
            ([_, ..], true) => return undetermined(&taking),
            _ => {}
        }
    }
    Selection::Refused
}

/// How many positions of a known argument type `binding` declares that type
/// for, or a preferred type of the argument's category.
fn preferred_matches<D: DatabaseLike>(
    database: &D,
    binding: &Binding<'_>,
    inputs: &[Cow<'_, PgType>],
    categories: &[Option<TypeCategory>],
) -> Result<usize, Undetermined> {
    let mut matches = 0;
    for (position, category) in categories.iter().enumerate() {
        let (Some(category), Some(declared)) = (category, declared_type(binding, position)) else {
            continue;
        };
        if *declared == *inputs[position] {
            matches += 1;
            continue;
        }
        let facts = facts(database, declared).ok_or(Undetermined)?;
        if facts.is_preferred() && facts.category() == *category {
            matches += 1;
        }
    }
    Ok(matches)
}

/// The untyped-literal category step of `func_select_candidate`, the
/// candidates it keeps or [`None`] when it settles nothing.
fn leaning_categories<'k, 'b, D: DatabaseLike, T>(
    database: &D,
    kept: &[&'k (T, Binding<'b>)],
    unknowns: &[usize],
) -> Result<Option<Vec<&'k (T, Binding<'b>)>>, Undetermined> {
    let declared_facts = |binding: &Binding<'_>, position: usize| {
        declared_type(binding, position)
            .and_then(|declared| facts(database, declared))
            .map(|facts| (facts.category(), facts.is_preferred()))
            .ok_or(Undetermined)
    };
    let mut slots = Vec::with_capacity(unknowns.len());
    for &position in unknowns {
        let mut category: Option<TypeCategory> = None;
        let mut preferred = false;
        let mut conflict = false;
        for (_, binding) in kept {
            let (current, current_preferred) = declared_facts(binding, position)?;
            match category {
                None => {
                    category = Some(current);
                    preferred = current_preferred;
                }
                Some(settled) if settled == current => preferred |= current_preferred,
                Some(_) if current == TypeCategory::STRING => {
                    category = Some(current);
                    preferred = current_preferred;
                }
                Some(_) => conflict = true,
            }
        }
        let Some(category) = category else { return Ok(None) };
        if conflict && category != TypeCategory::STRING {
            return Ok(None);
        }
        slots.push((position, category, preferred));
    }
    let mut survivors = Vec::with_capacity(kept.len());
    for candidate in kept {
        let mut keep = true;
        for &(position, category, preferred) in &slots {
            let (current, current_preferred) = declared_facts(&candidate.1, position)?;
            keep &= current == category && (!preferred || current_preferred);
        }
        if keep {
            survivors.push(*candidate);
        }
    }
    Ok((!survivors.is_empty()).then_some(survivors))
}

/// The input arguments of a function, by mode and type, which identify it
/// among the functions of its name.
fn input_signature<F: FunctionLike>(
    function: &F,
    database: &F::DB,
) -> Vec<(ArgMode, Option<PgType>)> {
    function
        .argument_modes()
        .zip(function.argument_type_names(database))
        .filter(|(mode, _)| *mode != ArgMode::Out)
        .map(|(mode, declared)| {
            (mode, PgType::parse(&declared).map(|parsed| canonical_owned(database, parsed)))
        })
        .collect()
}

/// The functions a call may run, gathered and hidden as `FuncnameGetCandidates`
/// does, each with the type every written argument is declared to take.
///
/// # Errors
///
/// Returns [`LookupError::InvalidObjectName`] when the called name has more
/// than two parts or a part built at run time.
pub(crate) fn reached_functions<'db, D: DatabaseLike>(
    database: &'db D,
    call: &Function,
    case: IdentifierCase,
) -> Result<Vec<(&'db D::Function, Declared)>, LookupError> {
    if call.name.0.len() > 2 {
        return Err(overqualified_object_name(&call.name));
    }
    let target =
        target_name_from_object_name(&call.name).ok_or_else(|| run_time_object_name(&call.name))?;
    if expression_form(&call.name).is_some() {
        return Ok(Vec::new());
    }
    let Some(written) = written_argument_names(call) else { return Ok(Vec::new()) };
    let path: Vec<(&str, bool)> = match target.schema() {
        Some(schema) => vec![(schema, target.schema_is_quoted())],
        None => function_search_path(|| database.search_path()).collect(),
    };

    let mut reached: Vec<(usize, &'db D::Function, Declared)> = database
        .functions_named(target.name())
        .filter(|function| {
            case.identifiers_match(
                function.name(),
                function.name_is_quoted(),
                target.name(),
                target.name_is_quoted(),
            )
        })
        .filter_map(|function| {
            let declared_in = function.target_name();
            let (schema, quoted) = declared_in
                .schema()
                .map_or(("public", false), |schema| (schema, declared_in.schema_is_quoted()));
            let rank = path.iter().position(|&(entry, entry_quoted)| {
                case.identifiers_match(schema, quoted, entry, entry_quoted)
            })?;
            Some((rank, function, declared_at_call(function, database, &written)?))
        })
        .collect();
    reached.sort_by_key(|(rank, _, _)| *rank);

    let mut signatures: Vec<Vec<(ArgMode, Option<PgType>)>> = Vec::with_capacity(reached.len());
    let mut functions = Vec::with_capacity(reached.len());
    for (_, function, declared) in reached {
        let signature = input_signature(function, database);
        if !signatures.contains(&signature) {
            signatures.push(signature);
            functions.push((function, declared));
        }
    }
    Ok(functions)
}

/// What `call`, an expression PostgreSQL's grammar writes as `form`, returns
/// given its argument types.
///
/// # Errors
///
/// Returns [`LookupError::ArgumentTypeCount`] when `argument_types` does not
/// hold one entry per argument `call` writes.
pub(crate) fn expression_result<'a, D: DatabaseLike>(
    database: &D,
    form: ExpressionForm,
    call: &Function,
    argument_types: &[Option<&'a str>],
) -> Result<Option<Cow<'a, str>>, LookupError> {
    let written = written_argument_names(call).map_or(0, |written| written.len());
    check_argument_count(call, written, argument_types.len())?;
    Ok(match form {
        ExpressionForm::CommonType => common_type_name(database, argument_types),
        // An untyped first argument takes the type `=` compares it under.
        ExpressionForm::FirstArgument => {
            match argument_types.first().copied().flatten() {
                Some("unknown") => common_type_name(database, argument_types),
                first => first.map(Cow::Borrowed),
            }
        }
        ExpressionForm::Value(name) => Some(Cow::Borrowed(name)),
    })
}

/// A function a call may run, with the type that call returns.
pub(crate) type Resolved<'a, F> = (&'a F, Option<Cow<'a, str>>);

/// The function a call runs and what it returns, given its argument types.
///
/// # Errors
///
/// Returns [`LookupError::InvalidObjectName`] for a called name no static
/// reader resolves, and [`LookupError::ArgumentTypeCount`] when
/// `argument_types` does not hold one entry per argument `call` writes.
pub(crate) fn resolve_call<'a, D: DatabaseLike>(
    database: &'a D,
    call: &Function,
    argument_types: &[Option<&'a str>],
    case: IdentifierCase,
) -> Result<Selection<Resolved<'a, D::Function>>, LookupError> {
    let Some(written) = written_argument_names(call) else { return Ok(Selection::Refused) };
    check_argument_count(call, written.len(), argument_types.len())?;
    let actuals = actuals(database, argument_types);
    let mut answers: Vec<Resolved<'a, D::Function>> = Vec::new();
    let mut bound: Vec<(usize, Binding<'a>)> = Vec::new();
    for (function, declared) in reached_functions(database, call, case)? {
        let Some(binding) = bind(database, declared, &actuals) else { continue };
        let result = function.result_type_name(database).and_then(|result| binding.result(result));
        bound.push((answers.len(), binding));
        answers.push((function, result));
    }
    Ok(match select(database, &bound, &actuals) {
        Selection::Chosen(position) => {
            answers.into_iter().nth(position).map_or(Selection::Refused, Selection::Chosen)
        }
        Selection::Refused => Selection::Refused,
        Selection::Undetermined(positions) => {
            Selection::Undetermined(
                answers
                    .into_iter()
                    .enumerate()
                    .filter(|(position, _)| positions.contains(position))
                    .map(|(_, answer)| answer)
                    .collect(),
            )
        }
    })
}
