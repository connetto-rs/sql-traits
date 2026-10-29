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
    structs::{IdentifierCase, TargetName},
    traits::{DatabaseLike, FunctionLike},
    utils::{
        identifier_resolution::identifiers_match,
        object_name::{
            function_search_path, overqualified_object_name, run_time_object_name,
            target_name_from_object_name,
        },
        postgres_types::{CommonType, PgType, Polymorphic, parse_data_type, select_common_type},
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
        Some(Polymorphic::Any) => Some(declared.clone()),
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
        .map(|((declared, name), _)| Input { name, declared: PgType::parse(&declared) })
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
pub(crate) fn actuals<'a>(argument_types: &[Option<&'a str>]) -> Vec<Option<Actual<'a>>> {
    argument_types
        .iter()
        .map(|name| {
            name.and_then(|spelling| {
                PgType::parse(spelling).map(|pg_type| Actual { pg_type, spelling })
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
struct Bound<'a> {
    pg_type: PgType,
    spelling: Spelling<'a>,
}

/// How to write a bound type.
#[derive(Clone)]
enum Spelling<'a> {
    /// The caller's own name for it.
    Given(&'a str),
    /// The element type of the array the caller named.
    ElementOf(&'a str),
    /// `text`, what unknown types alone resolve to.
    Text,
}

impl<'a> Bound<'a> {
    fn given(actual: &Actual<'a>) -> Self {
        Self { pg_type: actual.pg_type.clone(), spelling: Spelling::Given(actual.spelling) }
    }

    /// The element of an array the caller named.
    fn element_of(actual: &Actual<'a>) -> Option<Self> {
        Some(Self {
            pg_type: actual.pg_type.element()?.clone(),
            spelling: Spelling::ElementOf(actual.spelling),
        })
    }

    /// The element of the array type this binds.
    fn element(&self) -> Option<Self> {
        let Spelling::Given(spelling) = self.spelling else { return None };
        Some(Self {
            pg_type: self.pg_type.element()?.clone(),
            spelling: Spelling::ElementOf(spelling),
        })
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
                let PgType::Catalog(name) = self.pg_type else { return None };
                Some(Cow::Borrowed(name))
            }
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
    pub(crate) declared: Vec<Option<PgType>>,
    /// What `anyelement` binds to, when the call decides it.
    element: Family<Bound<'a>>,
    /// What `anyarray` binds to, when an argument decides it directly.
    array: Option<Bound<'a>>,
    /// What `anycompatible` binds to.
    compatible: Family<Bound<'a>>,
}

/// What one family of polymorphic parameters binds to.
enum Family<T> {
    /// The arguments decide this type.
    Decided(T),
    /// No argument of the family has a type, or every one is an untyped
    /// literal.
    Unbound,
    /// An argument of the family has a type the caller does not know.
    Undetermined,
}

/// Binds the polymorphic parameters as `enforce_generic_type_consistency` does,
/// or [`None`] when the function cannot take the argument types.
pub(crate) fn bind<'a>(
    declared: Vec<Option<PgType>>,
    actuals: &[Option<Actual<'a>>],
) -> Option<Binding<'a>> {
    let (element, array, compatible) = bind_families(&declared, actuals)?;
    Some(Binding { declared, element, array, compatible })
}

/// What `anyelement`, `anyarray` and `anycompatible` bind to, `anyarray` only
/// when an argument decides it directly.
type Families<'a> = (Family<Bound<'a>>, Option<Bound<'a>>, Family<Bound<'a>>);

/// The work of [`bind`], over borrowed declared types.
fn bind_families<'a>(
    declared: &[Option<PgType>],
    actuals: &[Option<Actual<'a>>],
) -> Option<Families<'a>> {
    let mut element: Option<Bound<'a>> = None;
    let mut array: Option<Bound<'a>> = None;
    let mut element_undetermined = false;
    let mut nonarray = false;
    let mut compatible: Vec<Bound<'a>> = Vec::new();
    let mut compatible_undetermined = false;
    let mut compatible_nonarray = false;

    for (declared, actual) in declared.iter().zip(actuals) {
        // A declaration this model cannot read takes anything.
        let Some(declared) = declared else { continue };
        let known = actual.as_ref().filter(|actual| !actual.pg_type.is_unknown());
        match declared.polymorphic() {
            None => {
                if actual
                    .as_ref()
                    .is_some_and(|actual| !actual.pg_type.coerces_implicitly_to(declared))
                {
                    return None;
                }
            }
            Some(Polymorphic::Any | Polymorphic::Range) => {}
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
                        if !actual.pg_type.is_array()
                            || array.as_ref().is_some_and(|bound| bound.pg_type != actual.pg_type)
                        {
                            return None;
                        }
                        array = Some(Bound::given(actual));
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
                    (Some(_), Some(actual)) => compatible.push(Bound::element_of(actual)?),
                }
            }
        }
    }

    let element = element_family(element, array.as_ref(), element_undetermined, nonarray)?;
    let compatible = compatible_family(compatible, compatible_undetermined, compatible_nonarray)?;
    Some((element, array, compatible))
}

/// What `anyelement` binds to, the element of the `anyarray` argument when no
/// `anyelement` argument decides it, or [`None`] when the two disagree.
fn element_family<'a>(
    element: Option<Bound<'a>>,
    array: Option<&Bound<'a>>,
    undetermined: bool,
    nonarray: bool,
) -> Option<Family<Bound<'a>>> {
    let from_array = match array {
        Some(array) => Some(array.element()?),
        None => None,
    };
    let element = match (element, from_array) {
        (Some(element), Some(from_array)) if element.pg_type != from_array.pg_type => return None,
        (Some(element), _) => Some(element),
        (None, from_array) => from_array,
    };
    if nonarray && element.as_ref().is_some_and(|bound| bound.pg_type.is_array()) {
        return None;
    }
    Some(match element {
        _ if undetermined => Family::Undetermined,
        Some(bound) => Family::Decided(bound),
        None => Family::Unbound,
    })
}

/// What `anycompatible` binds to, the common type of its arguments, or
/// [`None`] when they have none.
fn compatible_family(
    mut bounds: Vec<Bound<'_>>,
    undetermined: bool,
    nonarray: bool,
) -> Option<Family<Bound<'_>>> {
    if undetermined {
        return Some(Family::Undetermined);
    }
    let position = match select_common_type(&bounds, |bound| &bound.pg_type)? {
        CommonType::At(position) => position,
        CommonType::AllUnknown => return Some(Family::Unbound),
    };
    let common = bounds.swap_remove(position);
    (!(nonarray && common.pg_type.is_array())).then_some(Family::Decided(common))
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
        let text = Bound { pg_type: PgType::TEXT, spelling: Spelling::Text };
        match polymorphic {
            Polymorphic::Element { .. } => {
                match &self.element {
                    Family::Decided(bound) => bound.name(),
                    Family::Unbound | Family::Undetermined => None,
                }
            }
            Polymorphic::Array => {
                if let (Some(array), Family::Decided(_)) = (&self.array, &self.element) {
                    return array.name();
                }
                match &self.element {
                    Family::Decided(bound) => bound.array_name(),
                    Family::Unbound | Family::Undetermined => None,
                }
            }
            Polymorphic::Compatible { .. } => {
                match &self.compatible {
                    Family::Decided(bound) => bound.name(),
                    Family::Unbound => text.name(),
                    Family::Undetermined => None,
                }
            }
            Polymorphic::CompatibleArray => {
                match &self.compatible {
                    Family::Decided(bound) => bound.array_name(),
                    Family::Unbound => text.array_name(),
                    Family::Undetermined => None,
                }
            }
            Polymorphic::Any | Polymorphic::Range => None,
        }
    }
}

/// Which of the functions a call reaches it runs.
pub(crate) enum Selection<T> {
    /// PostgreSQL runs this one.
    Chosen(T),
    /// PostgreSQL refuses the call, since no function takes the argument
    /// types or several do and nothing tells them apart.
    Refused,
    /// The caller does not know every argument's type, and these functions
    /// take the call whatever the unknown types are.
    Undetermined(Vec<T>),
}

/// The declared type at a position, compared the way PostgreSQL compares a
/// candidate's argument type.
fn declared_type<'b>(binding: &'b Binding<'_>, position: usize) -> Option<&'b PgType> {
    binding.declared.get(position).and_then(Option::as_ref)
}

/// Keeps the candidates scoring best, which keeps them all when none scores.
fn keep_best<'c, 'b, T>(
    kept: Vec<&'c (T, Binding<'b>)>,
    score: impl Fn(&Binding<'b>) -> usize,
) -> Vec<&'c (T, Binding<'b>)> {
    let best = kept.iter().map(|(_, binding)| score(binding)).max().unwrap_or(0);
    kept.into_iter().filter(|(_, binding)| score(binding) == best).collect()
}

/// Picks the function a call runs as `func_get_detail` and
/// `func_select_candidate` do.
pub(crate) fn select<T: Copy>(
    candidates: &[(T, Binding<'_>)],
    actuals: &[Option<Actual<'_>>],
) -> Selection<T> {
    let only = |kept: &[&(T, Binding<'_>)]| {
        match kept {
            [(chosen, _)] => Some(*chosen),
            _ => None,
        }
    };
    let all: Vec<&(T, Binding<'_>)> = candidates.iter().collect();
    if all.is_empty() {
        return Selection::Refused;
    }
    let Some(inputs) = actuals
        .iter()
        .map(|actual| actual.as_ref().map(|actual| &actual.pg_type))
        .collect::<Option<Vec<&PgType>>>()
    else {
        return only(&all).map_or_else(
            || Selection::Undetermined(all.iter().map(|(candidate, _)| *candidate).collect()),
            Selection::Chosen,
        );
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
    if let Some(chosen) = only(&exact).or_else(|| only(&all)) {
        return Selection::Chosen(chosen);
    }

    let known = |position: usize| inputs.get(position).filter(|input| !input.is_unknown());
    let kept = keep_best(all, |binding| {
        (0..inputs.len())
            .filter(|&position| {
                known(position)
                    .is_some_and(|input| declared_type(binding, position) == Some(*input))
            })
            .count()
    });
    if let Some(chosen) = only(&kept) {
        return Selection::Chosen(chosen);
    }
    let kept = keep_best(kept, |binding| {
        (0..inputs.len())
            .filter(|&position| {
                known(position).is_some_and(|input| {
                    declared_type(binding, position).is_some_and(|declared| {
                        declared == *input
                            || (declared.is_preferred() && declared.category() == input.category())
                    })
                })
            })
            .count()
    });
    if let Some(chosen) = only(&kept) {
        return Selection::Chosen(chosen);
    }
    let unknowns: Vec<usize> =
        (0..inputs.len()).filter(|&position| inputs[position].is_unknown()).collect();
    if unknowns.is_empty() {
        return Selection::Refused;
    }

    let kept = leaning_categories(kept, &unknowns);
    if let Some(chosen) = only(&kept) {
        return Selection::Chosen(chosen);
    }

    // Last gasp, untyped literals take the one type all other arguments share.
    let mut others = inputs.iter().filter(|input| !input.is_unknown());
    if let Some(shared) = others.next().filter(|first| others.all(|other| other == *first)) {
        let assumed: Vec<Option<Actual<'_>>> = inputs
            .iter()
            .map(|_| Some(Actual { pg_type: (*shared).clone(), spelling: "" }))
            .collect();
        let matching: Vec<_> = kept
            .iter()
            .filter(|(_, binding)| bind_families(&binding.declared, &assumed).is_some())
            .collect();
        if let [(chosen, _)] = matching.as_slice() {
            return Selection::Chosen(*chosen);
        }
    }
    Selection::Refused
}

/// The untyped-literal category step of `func_select_candidate`, keeping all
/// candidates when it settles nothing.
fn leaning_categories<'k, T>(
    kept: Vec<&'k (T, Binding<'k>)>,
    unknowns: &[usize],
) -> Vec<&'k (T, Binding<'k>)> {
    let mut slots = Vec::with_capacity(unknowns.len());
    for &position in unknowns {
        let mut category: Option<u8> = None;
        let mut preferred = false;
        let mut conflict = false;
        for (_, binding) in &kept {
            let declared = declared_type(binding, position);
            let current = declared.map_or(b'U', PgType::category);
            let current_preferred = declared.is_some_and(PgType::is_preferred);
            match category {
                None => {
                    category = Some(current);
                    preferred = current_preferred;
                }
                Some(settled) if settled == current => preferred |= current_preferred,
                Some(_) if declared.is_some_and(PgType::is_string_category) => {
                    category = Some(current);
                    preferred = current_preferred;
                }
                Some(_) => conflict = true,
            }
        }
        let Some(category) = category else { return kept };
        if conflict && category != PgType::TEXT.category() {
            return kept;
        }
        slots.push((position, category, preferred));
    }
    let survivors: Vec<_> = kept
        .iter()
        .copied()
        .filter(|(_, binding)| {
            slots.iter().all(|&(position, category, preferred)| {
                let declared = declared_type(binding, position);
                declared.map_or(b'U', PgType::category) == category
                    && (!preferred || declared.is_some_and(PgType::is_preferred))
            })
        })
        .collect();
    if survivors.is_empty() { kept } else { survivors }
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
        .map(|(mode, declared)| (mode, PgType::parse(&declared)))
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
    let Some(written) = written_argument_names(call) else { return Ok(Vec::new()) };
    let path: Vec<(&str, bool)> = match target.schema() {
        Some(schema) => vec![(schema, target.schema_is_quoted())],
        None => function_search_path(|| database.search_path()).collect(),
    };

    let mut reached: Vec<(usize, &'db D::Function, Declared)> = database
        .functions()
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
    let actuals = actuals(argument_types);
    let mut answers: Vec<Resolved<'a, D::Function>> = Vec::new();
    let mut bound: Vec<(usize, Binding<'a>)> = Vec::new();
    for (function, declared) in reached_functions(database, call, case)? {
        let Some(binding) = bind(declared, &actuals) else { continue };
        let result = function.result_type_name(database).and_then(|result| binding.result(result));
        bound.push((answers.len(), binding));
        answers.push((function, result));
    }
    Ok(match select(&bound, &actuals) {
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
