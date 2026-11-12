//! Whether PostgreSQL converts a value of one type to another without being
//! asked, as `can_coerce_type` decides under `COERCION_IMPLICIT`, and the
//! common type `select_common_type` settles on, both read from the casts and
//! the type facts a database answers.
//!
//! A database answering no casts proves no conversion, and one answering no
//! facts about a type proves nothing about it, so an answer needing either is
//! [`Proof::Unknown`] or [`Undetermined`].

use alloc::{borrow::Cow, vec::Vec};

use crate::{
    structs::{CoercionStep, IdentifierCase, TargetName, TypeCategory, TypeFacts},
    traits::{CastContext, CastLike, CastMethod, DatabaseLike, Relation, TableLike},
    utils::{
        object_name::{PG_CATALOG, type_search_path},
        postgres_types::PgType,
    },
};

/// What a database's facts prove about a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Proof {
    /// The facts prove the claim.
    Yes,
    /// The facts disprove the claim.
    No,
    /// The facts the claim needs are missing.
    Unknown,
}

impl Proof {
    /// The proof of a claim the facts settle.
    const fn of(holds: bool) -> Self {
        if holds { Self::Yes } else { Self::No }
    }

    /// Whether this claim or `other` holds, `other` read only when this one
    /// is not proven.
    fn or_else(self, other: impl FnOnce() -> Self) -> Self {
        if self == Self::Yes {
            return Self::Yes;
        }
        match (self, other()) {
            (_, Self::Yes) => Self::Yes,
            (Self::No, Self::No) => Self::No,
            _ => Self::Unknown,
        }
    }
}

/// An answer the database's facts do not settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Undetermined;

/// A borrowed view of `name`.
fn view<'n>(name: &'n TargetName<'_>) -> TargetName<'n> {
    let view = TargetName::new(name.name(), name.name_is_quoted());
    match name.schema() {
        Some(schema) => view.with_schema(schema, name.schema_is_quoted()),
        None => view,
    }
}

/// The type `pg_type` denotes in `database`, a stored name qualified with the
/// schema the type search path finds it in and a generated array name read
/// as the array over its element. A name the database holds no facts about
/// stays as written.
pub(crate) fn canonical<'p, D: DatabaseLike>(database: &D, pg_type: &'p PgType) -> Cow<'p, PgType> {
    match pg_type {
        PgType::Array(element) => {
            match canonical(database, element) {
                Cow::Borrowed(_) => Cow::Borrowed(pg_type),
                Cow::Owned(element) => Cow::Owned(element.array()),
            }
        }
        PgType::Named(name) => located(database, name).map_or(Cow::Borrowed(pg_type), Cow::Owned),
        PgType::Catalog(_) | PgType::Other(_) => Cow::Borrowed(pg_type),
    }
}

/// The type the stored `name` denotes when the database's facts spell it
/// otherwise, qualified or as an array, and [`None`] when they do not.
fn located<D: DatabaseLike>(database: &D, name: &TargetName<'_>) -> Option<PgType> {
    let (qualified, facts) = match name.schema() {
        Some(_) => (None, database.type_facts(view(name))?),
        None => {
            type_search_path(|| database.search_path()).find_map(|(schema, quoted)| {
                let qualified = view(name).with_schema(schema, quoted);
                database.type_facts(qualified.clone()).map(|facts| (Some(qualified), facts))
            })?
        }
    };
    match facts.array_element() {
        Some(element) => Some(canonical_owned(database, PgType::from_target(element)).array()),
        None => qualified.map(|qualified| PgType::from_target(&qualified)),
    }
}

/// [`canonical`] over an owned type.
pub(crate) fn canonical_owned<D: DatabaseLike>(database: &D, pg_type: PgType) -> PgType {
    match canonical(database, &pg_type) {
        Cow::Borrowed(_) => pg_type,
        Cow::Owned(canonical) => canonical,
    }
}

/// The facts of a canonical type.
pub(crate) fn facts<'db, D: DatabaseLike>(
    database: &'db D,
    pg_type: &PgType,
) -> Option<TypeFacts<'db>> {
    match pg_type {
        PgType::Catalog(name) => {
            database.type_facts(TargetName::new(name, true).with_schema(PG_CATALOG, false))
        }
        // Every array type is in the array category and none is preferred.
        PgType::Array(element) => {
            facts(database, element).map(|_| TypeFacts::new(TypeCategory::ARRAY, false))
        }
        PgType::Named(name) => database.type_facts(view(name)),
        PgType::Other(_) => None,
    }
}

/// The name the database gives a canonical type, an array type by the
/// `pg_type.typarray` of its element.
pub(crate) fn type_name<'db, D: DatabaseLike>(
    database: &'db D,
    pg_type: &PgType,
) -> Option<TargetName<'db>> {
    match pg_type {
        PgType::Catalog(name) => Some(TargetName::new(name, false).with_schema(PG_CATALOG, false)),
        PgType::Named(name) => Some(name.clone()),
        PgType::Array(element) => array_type_name(database, element),
        PgType::Other(_) => None,
    }
}

/// The name the database gives the array type over the canonical type
/// `element`, its `pg_type.typarray`.
pub(crate) fn array_type_name<'db, D: DatabaseLike>(
    database: &'db D,
    element: &PgType,
) -> Option<TargetName<'db>> {
    facts(database, element)?.array_type().cloned()
}

/// The base type a canonical type coerces through, borrowed unless the type
/// is a domain.
pub(crate) fn base<'p, D: DatabaseLike>(
    database: &D,
    pg_type: &'p PgType,
) -> Result<Cow<'p, PgType>, Undetermined> {
    match pg_type {
        PgType::Named(_) => {
            let facts = facts(database, pg_type).ok_or(Undetermined)?;
            Ok(match facts.domain() {
                Some(domain) => {
                    Cow::Owned(canonical_owned(database, PgType::from_target(domain.base())))
                }
                None => Cow::Borrowed(pg_type),
            })
        }
        PgType::Other(_) => Err(Undetermined),
        // No type of `pg_catalog` and no array type is a domain.
        PgType::Catalog(_) | PgType::Array(_) => Ok(Cow::Borrowed(pg_type)),
    }
}

/// Where the coercion search records the steps it finds.
///
/// A proof needs no steps, so `()` records none and spares the lookups that
/// only name them, while a plan collects them in execution order.
pub(crate) trait Steps<'db, D: DatabaseLike + 'db> {
    /// Whether the sink keeps steps, so the search names them.
    const PLANS: bool;

    /// The position the next step takes.
    fn mark(&self) -> usize;

    /// Forgets the steps recorded since `mark`.
    fn reset(&mut self, mark: usize);

    /// Records the step `step` builds.
    fn push(&mut self, step: impl FnOnce() -> CoercionStep<'db, D>);

    /// Records the step `step` names, [`Proof::Unknown`] when it cannot be
    /// named. A sink keeping no steps never names one.
    fn note(&mut self, step: impl FnOnce() -> Option<CoercionStep<'db, D>>) -> Proof;

    /// Runs `search` for an array's elements, recording its steps as one
    /// element-wise step, which the caller discards with [`Self::reset`]
    /// when the search fails.
    fn elements(&mut self, search: impl FnOnce(&mut Self) -> Proof) -> Proof;
}

impl<'db, D: DatabaseLike + 'db> Steps<'db, D> for () {
    const PLANS: bool = false;

    fn mark(&self) -> usize {
        0
    }

    fn reset(&mut self, _mark: usize) {}

    fn push(&mut self, _step: impl FnOnce() -> CoercionStep<'db, D>) {}

    fn note(&mut self, _step: impl FnOnce() -> Option<CoercionStep<'db, D>>) -> Proof {
        Proof::Yes
    }

    fn elements(&mut self, search: impl FnOnce(&mut Self) -> Proof) -> Proof {
        search(self)
    }
}

impl<'db, D: DatabaseLike + 'db> Steps<'db, D> for Vec<CoercionStep<'db, D>> {
    const PLANS: bool = true;

    fn mark(&self) -> usize {
        self.len()
    }

    fn reset(&mut self, mark: usize) {
        self.truncate(mark);
    }

    fn push(&mut self, step: impl FnOnce() -> CoercionStep<'db, D>) {
        Vec::push(self, step());
    }

    fn note(&mut self, step: impl FnOnce() -> Option<CoercionStep<'db, D>>) -> Proof {
        step().map_or(Proof::Unknown, |step| {
            Vec::push(self, step);
            Proof::Yes
        })
    }

    fn elements(&mut self, search: impl FnOnce(&mut Self) -> Proof) -> Proof {
        let mut element = Vec::new();
        let proof = search(&mut element);
        Vec::push(self, CoercionStep::Array { element });
        proof
    }
}

/// Whether PostgreSQL converts a value of the canonical type `source` to the
/// canonical type `target` without being asked, as `can_coerce_type` decides
/// under `COERCION_IMPLICIT` for a declared target that is not polymorphic.
pub(crate) fn can_coerce<D: DatabaseLike>(database: &D, source: &PgType, target: &PgType) -> Proof {
    coerce(database, source, None, target, &mut ())
}

/// The steps PostgreSQL applies to convert a value of the canonical type
/// `source`, carrying the modifier `typmod`, to the canonical type `target`,
/// as `coerce_type` builds them, or [`None`] when the facts do not prove a
/// conversion.
pub(crate) fn conversion<'db, D: DatabaseLike>(
    database: &'db D,
    source: &PgType,
    typmod: Option<i32>,
    target: &PgType,
) -> Option<Vec<CoercionStep<'db, D>>> {
    let mut steps = Vec::new();
    (coerce(database, source, typmod, target, &mut steps) == Proof::Yes).then_some(steps)
}

/// The search behind [`can_coerce`] and [`conversion`], recording the steps
/// it finds in `steps`.
fn coerce<'db, D: DatabaseLike, S: Steps<'db, D>>(
    database: &'db D,
    source: &PgType,
    typmod: Option<i32>,
    target: &PgType,
    steps: &mut S,
) -> Proof {
    if source == target || target.is_any() {
        return Proof::Yes;
    }
    if source.is_unknown() {
        return literal(database, target, steps);
    }
    if database.casts().is_none() {
        return Proof::Unknown;
    }
    let attempt = |steps: &mut S, search: &dyn Fn(&mut S) -> Proof| {
        let mark = steps.mark();
        let proof = search(steps);
        if proof != Proof::Yes {
            steps.reset(mark);
        }
        proof
    };
    attempt(steps, &|steps| pathway(database, source, typmod, target, steps))
        .or_else(|| attempt(steps, &|steps| record(database, source, target, steps)))
        .or_else(|| attempt(steps, &|steps| inherits(database, source, target, steps)))
}

/// The reading of an untyped literal as `target`, by the input function of
/// its base type without a modifier and then the domain's steps.
fn literal<'db, D: DatabaseLike, S: Steps<'db, D>>(
    database: &'db D,
    target: &PgType,
    steps: &mut S,
) -> Proof {
    if !S::PLANS {
        return Proof::Yes;
    }
    let Ok(base) = base(database, target) else { return Proof::Unknown };
    let input =
        steps.note(|| Some(CoercionStep::LiteralInput { type_name: type_name(database, &base)? }));
    match (input, base) {
        (Proof::Yes, Cow::Owned(base)) => domain(database, target, &base, None, steps),
        (input, _) => input,
    }
}

/// Whether `find_coercion_pathway` finds an implicit conversion between two
/// canonical types, recording the steps `coerce_type` takes along it.
fn pathway<'db, D: DatabaseLike, S: Steps<'db, D>>(
    database: &'db D,
    source: &PgType,
    typmod: Option<i32>,
    target: &PgType,
    steps: &mut S,
) -> Proof {
    let (Ok(source_base), Ok(target_base)) = (base(database, source), base(database, target))
    else {
        return Proof::Unknown;
    };
    let mut typmod = typmod;
    let source_domain = match &source_base {
        Cow::Owned(_) => facts(database, source).and_then(TypeFacts::into_domain),
        Cow::Borrowed(_) => None,
    };
    if let Some(domain) = source_domain {
        typmod = domain.base_typmod();
        steps.push(|| CoercionStep::DomainToBase { base: domain.into_base() });
    }
    let converted = if source_base == target_base {
        Proof::Yes
    } else {
        // A converted value carries no modifier, and an array's modifier is
        // its elements'.
        let element_typmod = typmod.take();
        match cast(database, &source_base, &target_base) {
            Some(cast) if cast.context() == CastContext::Implicit => {
                steps.note(|| {
                    Some(match cast.method() {
                        CastMethod::Binary => CoercionStep::Relabel(cast),
                        CastMethod::Function => CoercionStep::Function(cast),
                        CastMethod::InOut => CoercionStep::InOut(cast),
                    })
                })
            }
            Some(_) => Proof::No,
            // Arrays convert element by element without a cast of their own.
            None => {
                match (&*source_base, &*target_base) {
                    (PgType::Array(source), PgType::Array(target)) => {
                        steps.elements(|steps| {
                            pathway(database, source, element_typmod, target, steps)
                        })
                    }
                    _ => Proof::No,
                }
            }
        }
    };
    match (converted, target_base) {
        (Proof::Yes, Cow::Owned(base)) => domain(database, target, &base, typmod, steps),
        (converted, _) => converted,
    }
}

/// The steps `coerce_to_domain` adds for the domain `target` over `base`, to
/// a value carrying the modifier `typmod`, the length coercion the domain's
/// modifier needs and the domain's check.
fn domain<'db, D: DatabaseLike, S: Steps<'db, D>>(
    database: &'db D,
    target: &PgType,
    base: &PgType,
    typmod: Option<i32>,
    steps: &mut S,
) -> Proof {
    if !S::PLANS {
        return Proof::Yes;
    }
    let applied = facts(database, target).and_then(|facts| facts.domain()?.base_typmod());
    if let Some(applied) = applied.filter(|applied| typmod != Some(*applied))
        && let Some(cast) = cast(database, base, base)
    {
        steps.push(|| CoercionStep::LengthCoercion { cast, typmod: applied });
    }
    steps.note(|| Some(CoercionStep::DomainCheck { domain: type_name(database, target)? }))
}

/// The cast the database holds from `source` to `target`, of a database
/// known to answer casts.
fn cast<'db, D: DatabaseLike>(
    database: &'db D,
    source: &PgType,
    target: &PgType,
) -> Option<&'db D::Cast> {
    let of = |name: TargetName<'_>| canonical_owned(database, PgType::from_target(&name));
    database.casts().into_iter().flatten().find(|cast| {
        of(cast.source_type(database)) == *source && of(cast.target_type(database)) == *target
    })
}

/// Whether the facts prove `pg_type` a composite type, `ISCOMPLEX`.
fn is_complex<D: DatabaseLike>(database: &D, pg_type: &PgType) -> Proof {
    match facts(database, pg_type) {
        Some(facts) => {
            Proof::of(facts.category() == TypeCategory::COMPOSITE && facts.domain().is_none())
        }
        None => Proof::Unknown,
    }
}

/// Whether `record` and a composite type meet, which `can_coerce_type`
/// accepts in either direction, and a composite type meets `record[]`.
fn record<'db, D: DatabaseLike, S: Steps<'db, D>>(
    database: &'db D,
    source: &PgType,
    target: &PgType,
    steps: &mut S,
) -> Proof {
    let (proof, step) = if *source == PgType::RECORD {
        (is_complex(database, target), CoercionStep::RecordToComposite)
    } else if *target == PgType::RECORD {
        (is_complex(database, source), CoercionStep::CompositeToRecord)
    } else {
        match source {
            PgType::Array(element) if *target == PgType::RECORD_ARRAY => {
                (is_complex(database, element), CoercionStep::CompositeToRecord)
            }
            _ => return Proof::No,
        }
    };
    if proof == Proof::Yes { steps.note(|| Some(step)) } else { proof }
}

/// Whether the row type of a table converts to the row type of a table it
/// inherits from, or to the composite type it was created `OF`.
fn inherits<'db, D: DatabaseLike, S: Steps<'db, D>>(
    database: &'db D,
    source: &PgType,
    target: &PgType,
    steps: &mut S,
) -> Proof {
    let (PgType::Named(source_name), PgType::Named(target_name)) = (source, target) else {
        return Proof::No;
    };
    let relation = |name: &TargetName<'_>| {
        database
            .resolve_target_relation(view(name), IdentifierCase::AsWritten)
            .map_err(|_| Undetermined)
    };
    let relations = relation(source_name).and_then(|source| Ok((source, relation(target_name)?)));
    let proof = relations.and_then(|relations| {
        match relations {
            (Some(Relation::Table(source)), Some(Relation::Table(target))) => {
                descends(database, source, target).map(Proof::of)
            }
            // No interface tells which composite type a typed table was
            // created `OF`.
            (Some(Relation::Table(_)), None) if is_complex(database, target) != Proof::No => {
                Ok(Proof::Unknown)
            }
            _ => Ok(Proof::No),
        }
    });
    match proof.unwrap_or(Proof::Unknown) {
        Proof::Yes => steps.note(|| Some(CoercionStep::Inherited)),
        proof => proof,
    }
}

/// Whether `table` is `ancestor` or descends from it by inheritance or
/// partitioning, both of which `pg_inherits` records.
fn descends<D: DatabaseLike>(
    database: &D,
    table: &D::Table,
    ancestor: &D::Table,
) -> Result<bool, Undetermined> {
    let mut pending: Vec<&D::Table> = Vec::from([table]);
    while let Some(current) = pending.pop() {
        if core::ptr::eq(current, ancestor) {
            return Ok(true);
        }
        pending.extend(current.inherits_from(database).map_err(|_| Undetermined)?);
    }
    let mut pending: Vec<&D::Table> = Vec::from([ancestor]);
    while let Some(current) = pending.pop() {
        for partition in current.partitions(database).map_err(|_| Undetermined)? {
            if core::ptr::eq(partition, table) {
                return Ok(true);
            }
            pending.push(partition);
        }
    }
    Ok(false)
}

/// The type `select_common_type` settles on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommonType {
    /// The type at this position of the input.
    At(usize),
    /// The base type of a domain among the inputs.
    Base(PgType),
    /// Every input was an untyped literal, so the type is `text`.
    AllUnknown,
}

/// The type PostgreSQL's `select_common_type` and `verify_common_type` settle
/// on for `items`, [`None`] when they have none and PostgreSQL refuses the
/// values.
pub(crate) fn select_common_type<D: DatabaseLike, T>(
    database: &D,
    items: &[T],
    type_of: impl Fn(&T) -> &PgType,
) -> Result<Option<CommonType>, Undetermined> {
    let types: Vec<Cow<'_, PgType>> =
        items.iter().map(|item| canonical(database, type_of(item))).collect();
    // Only values all of one type keep a domain type.
    if let Some((first, rest)) = types.split_first()
        && !first.is_unknown()
        && rest.iter().all(|other| other == first)
    {
        return Ok(Some(CommonType::At(0)));
    }
    let mut chosen: Option<(usize, Cow<'_, PgType>)> = None;
    for (position, candidate) in types.iter().enumerate() {
        if candidate.is_unknown() {
            continue;
        }
        let candidate = base(database, candidate)?;
        let Some((_, current)) = &chosen else {
            chosen = Some((position, candidate));
            continue;
        };
        if *current == candidate {
            continue;
        }
        let (Some(current_facts), Some(candidate_facts)) =
            (facts(database, current), facts(database, &candidate))
        else {
            return Err(Undetermined);
        };
        if candidate_facts.category() != current_facts.category() {
            return Ok(None);
        }
        if current_facts.is_preferred() {
            continue;
        }
        // The candidate wins when the current type converts to it and it
        // does not convert back.
        let widens = match (
            can_coerce(database, current, &candidate),
            can_coerce(database, &candidate, current),
        ) {
            (Proof::No, _) | (_, Proof::Yes) => false,
            (Proof::Yes, Proof::No) => true,
            _ => return Err(Undetermined),
        };
        if widens {
            chosen = Some((position, candidate));
        }
    }
    let Some((position, common)) = chosen else {
        return Ok(Some(CommonType::AllUnknown));
    };
    for candidate in &types {
        if !proven(can_coerce(database, candidate, &common))? {
            return Ok(None);
        }
    }
    Ok(Some(match common {
        Cow::Borrowed(_) => CommonType::At(position),
        Cow::Owned(base) => CommonType::Base(base),
    }))
}

/// The type PostgreSQL gives values that must share one, by
/// [`select_common_type`], spelled as the caller spelled it where it can.
pub(crate) fn common_type_name<'a, D: DatabaseLike>(
    database: &D,
    types: &[Option<&'a str>],
) -> Option<Cow<'a, str>> {
    let names: Vec<&'a str> = types.iter().copied().collect::<Option<_>>()?;
    let parsed: Vec<PgType> =
        names.iter().map(|name| PgType::parse(name)).collect::<Option<_>>()?;
    match select_common_type(database, &parsed, |pg_type| pg_type).ok()?? {
        CommonType::At(position) => names.get(position).map(|name| Cow::Borrowed(*name)),
        CommonType::Base(base) => Some(base.spelling()),
        CommonType::AllUnknown => Some(Cow::Borrowed("TEXT")),
    }
}

/// Whether a proof settles its claim, and which way.
pub(crate) const fn proven(proof: Proof) -> Result<bool, Undetermined> {
    match proof {
        Proof::Yes => Ok(true),
        Proof::No => Ok(false),
        Proof::Unknown => Err(Undetermined),
    }
}
