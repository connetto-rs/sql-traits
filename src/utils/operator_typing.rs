//! Resolves an operator invocation to the `pg_operator` overload PostgreSQL
//! selects, as `oper`, `binary_oper_exact` and `oper_select_candidate` do.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use crate::{
    structs::{
        IdentifierCase, OperandEvidence, OperandType, OperatorResolution, SelectedOperator,
        TargetName,
    },
    traits::{DatabaseLike, OperatorKind, OperatorLike},
    utils::{
        call_typing::{Actual, Binding, BoundType, Declared, Selection, bind, select},
        coercion::{array_type_name, base, canonical_owned, conversion, type_name},
        object_name::function_search_path,
        postgres_types::PgType,
    },
};

/// An operator of the invoked name, with the names and types of the operands
/// it declares.
struct Candidate<'db, O> {
    operator: &'db O,
    left: Option<TargetName<'db>>,
    right: TargetName<'db>,
    /// The declared operand types, left first.
    declared: Declared,
}

impl<'db, O: OperatorLike> Candidate<'db, O> {
    /// The candidate `operator` makes, or [`None`] when an operand type it
    /// declares is unknown.
    fn of(database: &'db O::DB, operator: &'db O) -> Option<Self> {
        let of = |name: &TargetName<'_>| Some(canonical_owned(database, PgType::from_target(name)));
        let right = operator.right_type(database)?;
        let (left, declared) = match operator.kind() {
            OperatorKind::Prefix => (None, Vec::from([of(&right)])),
            OperatorKind::Binary => {
                let left = operator.left_type(database)?;
                let declared = Vec::from([of(&left), of(&right)]);
                (Some(left), declared)
            }
        };
        Some(Self { operator, left, right, declared })
    }
}

/// The type an operand of the invocation has, [`None`] when the caller does
/// not know it or names no type.
fn input(database: &impl DatabaseLike, operand: &OperandType<'_>) -> Option<(PgType, Option<i32>)> {
    match operand {
        OperandType::Known { type_name, typmod } => {
            let parsed = PgType::parse(&type_name.to_string())?;
            Some((canonical_owned(database, parsed), *typmod))
        }
        OperandType::UntypedLiteral => Some((PgType::UNKNOWN, None)),
        OperandType::Unknown => None,
    }
}

/// The operators of the invoked name and arity on the operator's search
/// path, the earliest of identical signatures kept, as
/// `OpernameGetCandidates` gathers them, or [`None`] when an operator's
/// operand types are unknown.
fn candidates<'db, D: DatabaseLike>(
    database: &'db D,
    operators: impl Iterator<Item = &'db D::Operator>,
    name: &TargetName<'_>,
    kind: OperatorKind,
    case: IdentifierCase,
) -> Option<Vec<Candidate<'db, D::Operator>>> {
    let path: Vec<(&str, bool)> = match name.schema() {
        Some(schema) => Vec::from([(schema, name.schema_is_quoted())]),
        None => function_search_path(|| database.search_path()).collect(),
    };
    let mut reached: Vec<(usize, &'db D::Operator)> = operators
        .filter(|operator| operator.kind() == kind)
        .filter_map(|operator| {
            let declared_as = operator.target_name();
            if declared_as.name() != name.name() {
                return None;
            }
            let (schema, quoted) = (declared_as.schema()?, declared_as.schema_is_quoted());
            let rank = path.iter().position(|&(entry, entry_quoted)| {
                case.identifiers_match(schema, quoted, entry, entry_quoted)
            })?;
            Some((rank, operator))
        })
        .collect();
    reached.sort_by_key(|(rank, _)| *rank);
    let mut kept: Vec<Candidate<'db, D::Operator>> = Vec::with_capacity(reached.len());
    for (_, operator) in reached {
        let candidate = Candidate::of(database, operator)?;
        if kept.iter().all(|held| held.declared != candidate.declared) {
            kept.push(candidate);
        }
    }
    Some(kept)
}

/// The operator PostgreSQL runs for `operator` applied to `right`, and to
/// `left` for a binary operator.
pub(crate) fn resolve_operator<'db, D: DatabaseLike>(
    database: &'db D,
    operator: &TargetName<'_>,
    left: Option<&OperandType<'_>>,
    right: &OperandType<'_>,
    case: IdentifierCase,
) -> OperatorResolution<'db, D> {
    let Some(operators) = database.operators() else { return OperatorResolution::Undetermined };
    let kind = if left.is_some() { OperatorKind::Binary } else { OperatorKind::Prefix };
    let Some((inputs, typmods)) = left
        .into_iter()
        .chain(core::iter::once(right))
        .map(|operand| input(database, operand))
        .collect::<Option<(Vec<PgType>, Vec<Option<i32>>)>>()
    else {
        return OperatorResolution::Undetermined;
    };
    let Some(candidates) = candidates(database, operators, operator, kind, case) else {
        return OperatorResolution::Undetermined;
    };
    match exact(database, &candidates, &inputs) {
        Err(Undecided) => OperatorResolution::Undetermined,
        Ok(Some(candidate)) => {
            let sources: Vec<&PgType> = inputs.iter().collect();
            selected(database, candidate, None, &sources, &typmods)
        }
        Ok(None) => select_candidate(database, &candidates, inputs, &typmods),
    }
}

/// What `oper_select_candidate` makes of the candidates, none of which
/// declares the operand types exactly.
fn select_candidate<'db, D: DatabaseLike>(
    database: &'db D,
    candidates: &[Candidate<'db, D::Operator>],
    inputs: Vec<PgType>,
    typmods: &[Option<i32>],
) -> OperatorResolution<'db, D> {
    let actuals: Vec<Option<Actual<'_>>> =
        inputs.into_iter().map(|pg_type| Some(Actual { pg_type, spelling: "" })).collect();
    let bound: Vec<(usize, Binding<'_>)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(position, candidate)| {
            Some((position, bind(database, candidate.declared.as_slice(), &actuals)?))
        })
        .collect();
    if bound.is_empty() {
        return OperatorResolution::NotFound;
    }
    match select(database, &bound, &actuals) {
        Selection::Chosen(position) => {
            let binding =
                bound.iter().find(|(held, _)| *held == position).map(|(_, binding)| binding);
            let sources: Vec<&PgType> =
                actuals.iter().flatten().map(|actual| &actual.pg_type).collect();
            selected(database, &candidates[position], binding, &sources, typmods)
        }
        Selection::Undetermined(_) => OperatorResolution::Undetermined,
        Selection::Refused => {
            OperatorResolution::Ambiguous(
                bound.iter().map(|(position, _)| candidates[*position].operator).collect(),
            )
        }
    }
}

/// A step whose facts the database does not hold.
struct Undecided;

/// The candidate `binary_oper_exact` finds declaring the operand types
/// exactly, an untyped operand of a binary operator read as the other
/// operand's type and then as that type's domain base type.
fn exact<'c, 'db, D: DatabaseLike>(
    database: &D,
    candidates: &'c [Candidate<'db, D::Operator>],
    inputs: &[PgType],
) -> Result<Option<&'c Candidate<'db, D::Operator>>, Undecided> {
    let declaring = |types: &[&PgType]| {
        candidates.iter().find(|candidate| {
            candidate.declared.iter().map(Option::as_ref).eq(types.iter().copied().map(Some))
        })
    };
    let assumed = match inputs {
        [left, right] if left.is_unknown() => right,
        [left, right] if right.is_unknown() => left,
        _ => return Ok(declaring(&inputs.iter().collect::<Vec<_>>())),
    };
    if let Some(found) = declaring(&[assumed, assumed]) {
        return Ok(Some(found));
    }
    Ok(match base(database, assumed).map_err(|_| Undecided)? {
        Cow::Borrowed(_) => None,
        Cow::Owned(base) => declaring(&[&base, &base]),
    })
}

/// Why an operand of the selected operator takes no type.
enum Refusal {
    /// Only untyped literals reach a polymorphic operand.
    Unbound,
    /// The facts the operand's type needs are missing.
    Undetermined,
}

/// The selection of `candidate`, each operand of type `sources` with the
/// modifiers `typmods` taking its declared type or what `binding` binds a
/// polymorphic declaration to, with the conversion that carries it there.
fn selected<'db, D: DatabaseLike>(
    database: &'db D,
    candidate: &Candidate<'db, D::Operator>,
    binding: Option<&Binding<'_>>,
    sources: &[&PgType],
    typmods: &[Option<i32>],
) -> OperatorResolution<'db, D> {
    let operand = |position: usize, name: &TargetName<'db>| {
        let convert = |target: &PgType| {
            conversion(database, sources[position], typmods[position], target)
                .ok_or(Refusal::Undetermined)
        };
        let declared = candidate.declared[position].as_ref().ok_or(Refusal::Undetermined)?;
        // `"any"` keeps the operand's own type, as `coerce_type` returns it.
        if declared.is_any() {
            let source = type_name(database, sources[position]).ok_or(Refusal::Undetermined)?;
            return Ok(OperandEvidence::new(source, false, convert(declared)?));
        }
        let Some(polymorphic) = declared.polymorphic() else {
            return Ok(OperandEvidence::new(name.clone(), false, convert(declared)?));
        };
        let bound_to = |target: &PgType, name: Option<TargetName<'db>>| {
            let name = name.ok_or(Refusal::Undetermined)?;
            Ok(OperandEvidence::new(name, true, convert(target)?))
        };
        // An exact match of a pseudo-type operand binds nothing.
        match binding.map_or(BoundType::Undetermined, |binding| binding.bound(polymorphic)) {
            BoundType::Type(bound) => bound_to(&bound.pg_type, type_name(database, &bound.pg_type)),
            BoundType::ArrayOf(bound) => {
                let array = bound.pg_type.clone().array();
                bound_to(&array, array_type_name(database, &bound.pg_type))
            }
            BoundType::Unbound => Err(Refusal::Unbound),
            BoundType::Undetermined => Err(Refusal::Undetermined),
        }
    };
    let operands = candidate
        .left
        .as_ref()
        .map(|name| operand(0, name))
        .transpose()
        .and_then(|left| Ok((operand(usize::from(left.is_some()), &candidate.right)?, left)));
    match operands {
        Ok((right, left)) => {
            OperatorResolution::Selected(SelectedOperator::new(
                candidate.operator,
                left,
                right,
                result_type(database, candidate.operator, binding),
            ))
        }
        Err(Refusal::Unbound) => OperatorResolution::UnboundPolymorphic(candidate.operator),
        Err(Refusal::Undetermined) => OperatorResolution::Undetermined,
    }
}

/// The type `operator` returns, its declared result named canonically or the
/// type `binding` binds a polymorphic result to, [`None`] when the database
/// does not answer it or the binding leaves it open.
fn result_type<'db, D: DatabaseLike>(
    database: &'db D,
    operator: &'db D::Operator,
    binding: Option<&Binding<'_>>,
) -> Option<TargetName<'db>> {
    let declared = canonical_owned(database, PgType::from_target(&operator.result_type(database)?));
    let Some(polymorphic) = declared.polymorphic() else {
        return type_name(database, &declared);
    };
    match binding?.bound(polymorphic) {
        BoundType::Type(bound) => type_name(database, &bound.pg_type),
        BoundType::ArrayOf(bound) => array_type_name(database, &bound.pg_type),
        BoundType::Unbound | BoundType::Undetermined => None,
    }
}
