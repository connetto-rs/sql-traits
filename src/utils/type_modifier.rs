//! The type modifier a declared type applies, as `pg_attribute.atttypmod`
//! stores it.

use sqlparser::ast::{
    ArrayElemTypeDef, CharacterLength, DataType, ExactNumberInfo, IntervalFields,
};

use crate::{
    traits::{DataTypeLike, TypeModifier},
    utils::postgres_types::PgType,
};

/// `VARHDRSZ`, the header length a variable-length typmod counts.
const VARHDRSZ: u64 = 4;

/// The modifier `data_type` declares, or `None` for one this model does not
/// encode.
pub(crate) fn declared(data_type: &DataType) -> Option<TypeModifier> {
    // A length without a modifier, `VARCHAR(MAX)` included, declares none.
    let length = |length: &Option<CharacterLength>, default: Option<u64>| {
        match length {
            Some(CharacterLength::IntegerLength { length, .. }) => modified(*length + VARHDRSZ),
            Some(CharacterLength::Max) | None => {
                default.map_or(Some(TypeModifier::Unmodified), |length| modified(length + VARHDRSZ))
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
                ExactNumberInfo::None => Some(TypeModifier::Unmodified),
                ExactNumberInfo::Precision(precision) => numeric(*precision, 0),
                ExactNumberInfo::PrecisionAndScale(precision, scale) => numeric(*precision, *scale),
            }
        }
        // A `bit` without a length holds one bit.
        DataType::Bit(declared) => modified(declared.unwrap_or(1)),
        DataType::BitVarying(declared) | DataType::VarBit(declared) => {
            declared.map_or(Some(TypeModifier::Unmodified), modified)
        }
        DataType::Time(precision, _) | DataType::Timestamp(precision, _) => {
            precision.map_or(Some(TypeModifier::Unmodified), modified)
        }
        DataType::Interval { fields, precision } => interval(*fields, *precision),
        // An array declaration applies its element's modifier.
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => declared(element),
        // A type the model does not know may read its modifiers its own way.
        DataType::Custom(_, modifiers) if !modifiers.is_empty() => None,
        _ => Some(TypeModifier::Unmodified),
    }
}

/// The modifier `data_type` applies, read from the parameters it reports
/// where they tell it apart, or `None`.
pub(crate) fn derived<T: DataTypeLike + ?Sized>(
    data_type: &T,
    database: &T::DB,
) -> Option<TypeModifier> {
    if let Some(element) = data_type.array_element(database) {
        return derived(element, database);
    }
    let PgType::Catalog(name) = PgType::parse(&data_type.normalized_name(database))? else {
        return None;
    };
    let length = |length: Option<u64>, header: u64| {
        length.map_or(Some(TypeModifier::Unmodified), |length| modified(length + header))
    };
    match name {
        "bpchar" | "varchar" => length(data_type.max_character_length(database), VARHDRSZ),
        "bit" | "varbit" => length(data_type.max_bit_length(database), 0),
        "numeric" => {
            match data_type.numeric_precision(database) {
                Some(precision) => {
                    numeric(precision, data_type.numeric_scale(database).unwrap_or(0))
                }
                None => Some(TypeModifier::Unmodified),
            }
        }
        // The precision reported is the one values keep, which a declaration
        // may leave implicit.
        "time" | "timetz" | "timestamp" | "timestamptz" | "interval" => None,
        _ => Some(TypeModifier::Unmodified),
    }
}

/// A plain length or precision modifier.
fn modified(value: u64) -> Option<TypeModifier> {
    i32::try_from(value).ok().map(TypeModifier::Modified)
}

/// `numeric(precision, scale)` as `make_numeric_typmod` encodes it.
fn numeric(precision: u64, scale: i64) -> Option<TypeModifier> {
    let precision = i32::try_from(precision).ok()?;
    let scale = i32::try_from(scale).ok()?;
    let header = i32::try_from(VARHDRSZ).ok()?;
    Some(TypeModifier::Modified(((precision << 16) | (scale & 0x7ff)) + header))
}

/// An interval's fields and precision as `INTERVAL_TYPMOD` encodes them.
fn interval(fields: Option<IntervalFields>, precision: Option<u64>) -> Option<TypeModifier> {
    const MONTH: i32 = 1 << 1;
    const YEAR: i32 = 1 << 2;
    const DAY: i32 = 1 << 3;
    const HOUR: i32 = 1 << 10;
    const MINUTE: i32 = 1 << 11;
    const SECOND: i32 = 1 << 12;
    const FULL_RANGE: i32 = 0x7fff;
    const FULL_PRECISION: i32 = 0xffff;
    let range = match fields {
        None if precision.is_none() => return Some(TypeModifier::Unmodified),
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
        Some(precision) => i32::try_from(precision).ok()?,
        None => FULL_PRECISION,
    };
    Some(TypeModifier::Modified((range << 16) | precision))
}
