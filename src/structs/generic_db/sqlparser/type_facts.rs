//! The conversion facts `ParserDB` answers about PostgreSQL types.

use alloc::format;

use sqlparser::ast::{
    ArrayElemTypeDef, CharacterLength, CreateDomain, DataType, ExactNumberInfo, Ident,
    IntervalFields, UserDefinedTypeRepresentation, UserDefinedTypeSqlDefinitionOption,
};

use super::{ParserDB, collation_resolution_for_schema};
use crate::{
    impls::{SqlparserDialect, domain_in_schema, domain_named, user_type_holds},
    structs::{DomainFacts, TargetName, TypeCategory, TypeFacts},
    traits::{DatabaseLike, Relation},
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        object_name::{PG_CATALOG, target_name_from_object_name, type_search_path},
        postgres_types::{PgType, built_in_type},
    },
};

/// `VARHDRSZ`, the header length a variable-length typmod counts.
const VARHDRSZ: i32 = 4;

/// The facts of the type `name` names, unqualified names read through the
/// type search path.
pub(super) fn type_facts<'db>(
    database: &'db ParserDB,
    name: &TargetName<'_>,
) -> Option<TypeFacts<'db>> {
    locate(database, name).map(|(_, facts)| facts)
}

/// The type `name` names, qualified with the schema holding it, and its
/// facts.
fn locate<'db>(
    database: &'db ParserDB,
    name: &TargetName<'_>,
) -> Option<(TargetName<'db>, TypeFacts<'db>)> {
    if !matches!(database.dialect(), SqlparserDialect::PostgreSql) {
        return None;
    }
    let (written, quoted) = (name.name(), name.name_is_quoted());
    let qualified = |(schema, schema_quoted): (&str, bool)| {
        let facts = facts_in_schema(database, (schema, schema_quoted), (written, quoted))?;
        let name = TargetName::new(written, quoted).with_schema(schema, schema_quoted);
        Some((name.into_owned(), facts))
    };
    match name.schema() {
        Some(schema) => qualified((schema, name.schema_is_quoted())),
        None => type_search_path(|| database.search_path()).find_map(qualified),
    }
}

/// The facts of the type `name` names in `schema`, with the array type over
/// it.
fn facts_in_schema<'db>(
    database: &'db ParserDB,
    schema: (&str, bool),
    name: (&str, bool),
) -> Option<TypeFacts<'db>> {
    let facts = declared_facts_in_schema(database, schema, name)?;
    Some(match database.ingestion.type_arrays.array_in(schema.0, schema.1, name.0, name.1) {
        Some(array) => facts.with_array_type(array),
        None => facts,
    })
}

/// The facts the declaration of the type `name` names in `schema` gives it.
fn declared_facts_in_schema<'db>(
    database: &'db ParserDB,
    schema: (&str, bool),
    name: (&str, bool),
) -> Option<TypeFacts<'db>> {
    if identifiers_match(schema.0, schema.1, PG_CATALOG, false) {
        return built_in_facts(database, &stored_identifier(name.0, name.1));
    }
    let case = database.identifier_case();
    if let Some(domain) = database
        .domains()
        .find(|domain| domain_named(domain, name, case) && domain_in_schema(domain, schema, case))
    {
        return domain_facts(database, domain);
    }
    if let Some((type_, is_multirange)) = database
        .user_defined_types()
        .find_map(|type_| user_type_holds(type_, name, schema, case).map(|multi| (type_, multi)))
    {
        return Some(user_type_facts(type_.representation(), is_multirange));
    }
    if let Some(element) =
        database.ingestion.type_arrays.element_in(schema.0, schema.1, name.0, name.1)
    {
        // An array's values take the collation of its elements.
        let collation = locate(database, &element).and_then(|(_, facts)| facts.into_collation());
        let facts = TypeFacts::new(TypeCategory::ARRAY, false).with_array_element(element);
        return Some(collated(facts, collation));
    }
    let relation = TargetName::new(name.0, name.1).with_schema(schema.0, schema.1);
    match database.resolve_target_relation(relation, case).ok()?? {
        Relation::Table(_)
        | Relation::ForeignTable(_)
        | Relation::View(_)
        | Relation::MaterializedView(_) => Some(TypeFacts::new(TypeCategory::COMPOSITE, false)),
        Relation::Sequence(_) => None,
    }
}

/// The facts of a `pg_catalog` type stored as `name`, an array named after
/// its element with a leading underscore.
fn built_in_facts<'db>(database: &ParserDB, name: &str) -> Option<TypeFacts<'db>> {
    let facts = if let Some((_, category, preferred)) = built_in_type(name) {
        TypeFacts::new(TypeCategory::new(category), preferred)
    } else {
        let (element, _, _) = name.strip_prefix('_').and_then(built_in_type)?;
        TypeFacts::new(TypeCategory::ARRAY, false)
            .with_array_element(TargetName::new(element, false).with_schema(PG_CATALOG, false))
    };
    Some(collated(facts, built_in_collation(database, name)))
}

/// The collation of the `pg_catalog` type stored as `name`, or `None` when
/// the active catalog holds it not collatable.
fn built_in_collation(database: &ParserDB, name: &str) -> Option<TargetName<'static>> {
    let collatable = database.ingestion.active_postgres_catalog.collatable_types().any(|type_| {
        type_.schema().is_some_and(|schema| {
            identifiers_match(schema, type_.schema_is_quoted(), PG_CATALOG, false)
        }) && identifiers_match(type_.name(), type_.name_is_quoted(), name, true)
    });
    // `name` and its array compare bytewise, every other type by the
    // database's collation.
    let collation = if matches!(name, "name" | "_name") { "C" } else { "default" };
    collatable.then(|| TargetName::new(collation, true).with_schema(PG_CATALOG, false))
}

/// `facts` with the collation `collation` names, or as they are for none.
fn collated<'db>(facts: TypeFacts<'db>, collation: Option<TargetName<'db>>) -> TypeFacts<'db> {
    match collation {
        Some(collation) => facts.with_collation(collation),
        None => facts,
    }
}

/// The facts of a stored user-defined type, or of its multirange.
fn user_type_facts<'db>(
    representation: Option<&UserDefinedTypeRepresentation>,
    is_multirange: bool,
) -> TypeFacts<'db> {
    let (category, preferred, collatable) = match representation {
        _ if is_multirange => (TypeCategory::RANGE, false, false),
        None => (TypeCategory::PSEUDO, false, false),
        Some(UserDefinedTypeRepresentation::Enum { .. }) => (TypeCategory::ENUM, false, false),
        Some(UserDefinedTypeRepresentation::Composite { .. }) => {
            (TypeCategory::COMPOSITE, false, false)
        }
        Some(UserDefinedTypeRepresentation::Range { .. }) => (TypeCategory::RANGE, false, false),
        Some(UserDefinedTypeRepresentation::SqlDefinition { options }) => {
            options.iter().fold(
                (TypeCategory::USER_DEFINED, false, false),
                |(category, preferred, collatable), option| {
                    match option {
                        UserDefinedTypeSqlDefinitionOption::Category(code) => {
                            let category = u8::try_from(*code).map_or(category, TypeCategory::new);
                            (category, preferred, collatable)
                        }
                        UserDefinedTypeSqlDefinitionOption::Preferred(declared) => {
                            (category, *declared, collatable)
                        }
                        UserDefinedTypeSqlDefinitionOption::Collatable(declared) => {
                            (category, preferred, *declared)
                        }
                        _ => (category, preferred, collatable),
                    }
                },
            )
        }
    };
    // A collatable base type's values take the database's collation.
    let collation =
        collatable.then(|| TargetName::new("default", true).with_schema(PG_CATALOG, false));
    collated(TypeFacts::new(category, preferred), collation)
}

/// The facts of `domain`, which takes the category of its base type, is
/// never preferred and takes the collation it declares or its base type's.
fn domain_facts<'db>(database: &'db ParserDB, domain: &'db CreateDomain) -> Option<TypeFacts<'db>> {
    let (facts, base_collation) = domain_over(database, &domain.data_type)?;
    let collation = match &domain.collation {
        Some(declared) => Some(declared_collation(database, declared)?),
        None => base_collation,
    };
    Some(collated(facts, collation))
}

/// The collation the unqualified `collation` names on the search path, or
/// `None` when it names none.
fn declared_collation<'db>(database: &'db ParserDB, collation: &Ident) -> Option<TargetName<'db>> {
    let lookup = (collation.value.as_str(), collation.quote_style.is_some());
    type_search_path(|| database.search_path())
        .find_map(|(schema, schema_quoted)| {
            collation_resolution_for_schema(
                lookup,
                &database.ingestion.collation_metadata,
                &database.ingestion.active_postgres_catalog,
                schema,
                schema_quoted,
            )
        })?
        .into_target_name()
}

/// The facts of a domain over `data_type` before its own collation, with the
/// collation of that base type.
fn domain_over<'db>(
    database: &'db ParserDB,
    data_type: &'db DataType,
) -> Option<(TypeFacts<'db>, Option<TargetName<'db>>)> {
    if let DataType::Custom(name, modifiers) = data_type {
        // A domain over a domain coerces through the innermost base type,
        // and only that innermost domain may apply a modifier.
        let written = target_name_from_object_name(name).filter(|_| modifiers.is_empty())?;
        let (base_name, base) = locate(database, &written)?;
        let domain = base.domain().cloned().unwrap_or_else(|| DomainFacts::new(base_name, None));
        let facts = TypeFacts::new(base.category(), false).with_domain(domain);
        return Some((facts, base.into_collation()));
    }
    let typmod = typmod(data_type).ok()?;
    let (base, category) = match data_type {
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => (array_of(database, element)?, TypeCategory::ARRAY),
        built_in => {
            let PgType::Catalog(name) = PgType::of(built_in) else { return None };
            let (_, category, _) = built_in_type(name)?;
            (
                TargetName::new(name, false).with_schema(PG_CATALOG, false),
                TypeCategory::new(category),
            )
        }
    };
    let collation = locate(database, &base).and_then(|(_, facts)| facts.into_collation());
    Some((TypeFacts::new(category, false).with_domain(DomainFacts::new(base, typmod)), collation))
}

/// The array type over the element `element` declares, a stored type's
/// generated array or a built-in type's `_` array.
fn array_of<'db>(database: &'db ParserDB, element: &'db DataType) -> Option<TargetName<'db>> {
    if let DataType::Custom(owner, _) = element {
        return target_name_from_object_name(database.ingestion.type_arrays.array(owner)?);
    }
    let PgType::Catalog(name) = PgType::of(element) else { return None };
    let array = format!("_{name}");
    Some(TargetName::new(&array, false).with_schema(PG_CATALOG, false).into_owned())
}

/// The modifier `data_type` declares as `pg_type.typtypmod` stores it,
/// `Ok(None)` when it declares none, and `Err` for a modifier this model
/// does not encode.
fn typmod(data_type: &DataType) -> Result<Option<i32>, ()> {
    // A length without a modifier, `VARCHAR(MAX)` included, declares none.
    let length = |length: &Option<CharacterLength>, default: Option<u64>| {
        match length {
            Some(CharacterLength::IntegerLength { length, .. }) => encode(*length + 4),
            Some(CharacterLength::Max) | None => {
                default.map_or(Ok(None), |length| encode(length + 4))
            }
        }
    };
    match data_type {
        DataType::Varchar(declared)
        | DataType::CharacterVarying(declared)
        | DataType::CharVarying(declared)
        | DataType::NationalCharacterVarying(declared)
        | DataType::NationalCharVarying(declared)
        | DataType::NcharVarying(declared) => length(declared, None),
        // A `char` without a length holds one character.
        DataType::Char(declared)
        | DataType::Character(declared)
        | DataType::NationalCharacter(declared)
        | DataType::NationalChar(declared)
        | DataType::Nchar(declared) => length(declared, Some(1)),
        DataType::Numeric(info) | DataType::Decimal(info) | DataType::Dec(info) => {
            match info {
                ExactNumberInfo::None => Ok(None),
                ExactNumberInfo::Precision(precision) => numeric(*precision, 0),
                ExactNumberInfo::PrecisionAndScale(precision, scale) => numeric(*precision, *scale),
            }
        }
        // A `bit` without a length holds one bit.
        DataType::Bit(declared) => encode(declared.unwrap_or(1)),
        DataType::BitVarying(declared) | DataType::VarBit(declared) => {
            declared.map_or(Ok(None), encode)
        }
        DataType::Time(precision, _) | DataType::Timestamp(precision, _) => {
            precision.map_or(Ok(None), encode)
        }
        DataType::Interval { fields, precision } => interval(*fields, *precision),
        // An array declaration applies its element's modifier.
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => typmod(element),
        _ => Ok(None),
    }
}

/// Encodes a plain length or precision typmod.
fn encode(value: u64) -> Result<Option<i32>, ()> {
    i32::try_from(value).map(Some).map_err(|_| ())
}

/// Encodes `numeric(precision, scale)` as `make_numeric_typmod` does.
fn numeric(precision: u64, scale: i64) -> Result<Option<i32>, ()> {
    let precision = i32::try_from(precision).map_err(|_| ())?;
    let scale = i32::try_from(scale).map_err(|_| ())?;
    Ok(Some(((precision << 16) | (scale & 0x7ff)) + VARHDRSZ))
}

/// Encodes an interval's fields and precision as `INTERVAL_TYPMOD` does.
fn interval(fields: Option<IntervalFields>, precision: Option<u64>) -> Result<Option<i32>, ()> {
    const MONTH: i32 = 1 << 1;
    const YEAR: i32 = 1 << 2;
    const DAY: i32 = 1 << 3;
    const HOUR: i32 = 1 << 10;
    const MINUTE: i32 = 1 << 11;
    const SECOND: i32 = 1 << 12;
    const FULL_RANGE: i32 = 0x7fff;
    const FULL_PRECISION: i32 = 0xffff;
    let range = match fields {
        None if precision.is_none() => return Ok(None),
        None => FULL_RANGE,
        Some(IntervalFields::Year) => YEAR,
        Some(IntervalFields::Month) => MONTH,
        Some(IntervalFields::Day) => DAY,
        Some(IntervalFields::Hour) => HOUR,
        Some(IntervalFields::Minute) => MINUTE,
        Some(IntervalFields::Second) => SECOND,
        Some(IntervalFields::YearToMonth) => YEAR | MONTH,
        Some(IntervalFields::DayToHour) => DAY | HOUR,
        Some(IntervalFields::DayToMinute) => DAY | HOUR | MINUTE,
        Some(IntervalFields::DayToSecond) => DAY | HOUR | MINUTE | SECOND,
        Some(IntervalFields::HourToMinute) => HOUR | MINUTE,
        Some(IntervalFields::HourToSecond) => HOUR | MINUTE | SECOND,
        Some(IntervalFields::MinuteToSecond) => MINUTE | SECOND,
    };
    let precision = match precision {
        Some(precision) => i32::try_from(precision).map_err(|_| ())?,
        None => FULL_PRECISION,
    };
    Ok(Some((range << 16) | precision))
}
