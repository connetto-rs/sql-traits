//! Submodule implementing the [`DataTypeLike`] trait for `sqlparser`'s
//! [`DataType`] enum.

use alloc::borrow::Cow;
use core::num::NonZeroU32;

use sqlparser::ast::{ArrayElemTypeDef, DataType, TypeModifier};

use crate::{
    structs::ParserDB,
    traits::{DataTypeLike, DatabaseLike, ExtensionLike, IntervalFields, VectorKind, VectorType},
    utils::{
        identifier_resolution::{identifiers_match, normalize_identifier},
        normalize_sqlparser_type,
        object_name::{
            PG_CATALOG, Qualifier, object_name_last_part, qualifier_of, relation_search_path,
        },
    },
};

/// The extension that creates pgvector's types.
const PGVECTOR_EXTENSION: &str = "vector";

/// The schema an extension created without naming one lands in, which the
/// model leaves unwritten.
const DEFAULT_SCHEMA: &str = "public";

/// pgvector's type names, as the catalog spells them, with the kind each
/// names.
const PGVECTOR_TYPES: [(&str, VectorKind); 3] = [
    ("vector", VectorKind::Vector),
    ("halfvec", VectorKind::HalfVec),
    ("sparsevec", VectorKind::SparseVec),
];

/// The field restriction of an interval, as this crate names it.
pub(super) const fn interval_fields(fields: sqlparser::ast::IntervalFields) -> IntervalFields {
    use sqlparser::ast::IntervalFields as Parsed;
    match fields {
        Parsed::Year => IntervalFields::Year,
        Parsed::Month => IntervalFields::Month,
        Parsed::Day => IntervalFields::Day,
        Parsed::Hour => IntervalFields::Hour,
        Parsed::Minute => IntervalFields::Minute,
        Parsed::Second => IntervalFields::Second,
        Parsed::YearToMonth => IntervalFields::YearToMonth,
        Parsed::DayToHour => IntervalFields::DayToHour,
        Parsed::DayToMinute => IntervalFields::DayToMinute,
        Parsed::DayToSecond => IntervalFields::DayToSecond,
        Parsed::HourToMinute => IntervalFields::HourToMinute,
        Parsed::HourToSecond => IntervalFields::HourToSecond,
        Parsed::MinuteToSecond => IntervalFields::MinuteToSecond,
    }
}

/// The element type an array declaration writes, or `None` for an array
/// declared without one.
fn written_element(definition: &ArrayElemTypeDef) -> Option<&DataType> {
    match definition {
        ArrayElemTypeDef::None => None,
        ArrayElemTypeDef::AngleBracket(element)
        | ArrayElemTypeDef::SquareBracket(element, _)
        | ArrayElemTypeDef::Parenthesis(element)
        | ArrayElemTypeDef::ParenthesisNotNull(element)
        | ArrayElemTypeDef::Qualified(element, _) => Some(element),
    }
}

/// Whether an unqualified type name reaches `schema`. PostgreSQL searches the
/// temporary schema and `pg_catalog` for a type as well as the path, so an
/// extension in either is reached whatever the path says.
fn on_type_search_path(database: &ParserDB, schema: &str) -> bool {
    schema == PG_CATALOG
        || relation_search_path(|| database.search_path())
            .any(|(entry, quoted)| normalize_identifier(entry, quoted) == schema)
}

impl DataTypeLike for DataType {
    type DB = ParserDB;

    #[inline]
    fn name<'db>(&'db self, _database: &'db Self::DB) -> Cow<'db, str> {
        normalize_sqlparser_type(self)
    }

    #[inline]
    fn fixed_binary_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::fixed_binary_length(*database.dialect(), self)
    }

    #[inline]
    fn max_character_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::max_character_length(*database.dialect(), self)
    }

    #[inline]
    fn fractional_seconds_precision(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::fractional_seconds_precision(*database.dialect(), self)
    }

    #[inline]
    fn numeric_scale(&self, database: &Self::DB) -> Option<i64> {
        super::dialect::numeric_scale(*database.dialect(), self)
    }

    #[inline]
    fn interval_fields(&self, _database: &Self::DB) -> Option<IntervalFields> {
        match self {
            Self::Interval { fields, .. } => fields.map(interval_fields),
            _ => None,
        }
    }

    fn vector(&self, database: &Self::DB) -> Option<VectorType> {
        let Self::Custom(name, modifiers) = self else {
            return None;
        };
        if name.0.len() > 2 {
            return None;
        }
        let (type_name, type_quoted) = object_name_last_part(name)?;
        let kind = PGVECTOR_TYPES.into_iter().find_map(|(spelling, kind)| {
            identifiers_match(type_name, type_quoted, spelling, false).then_some(kind)
        })?;
        let extension = database
            .extensions()
            .find(|extension| extension.stored_name() == PGVECTOR_EXTENSION)?;
        let schema = extension.stored_schema().unwrap_or(Cow::Borrowed(DEFAULT_SCHEMA));
        let resolves = match qualifier_of(name) {
            Qualifier::Absent => on_type_search_path(database, &schema),
            Qualifier::Named(qualifier, quoted) => {
                normalize_identifier(qualifier, quoted) == schema
            }
            Qualifier::RunTime => false,
        };
        if !resolves {
            return None;
        }
        let dimensions = match modifiers.as_slice() {
            [] => None,
            [TypeModifier::Number(digits, false)] => Some(digits.parse().ok()?),
            _ => return None,
        };
        Some(VectorType::new(kind, dimensions))
    }

    fn array_element<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db Self> {
        let Self::Array(definition) = self else {
            return None;
        };
        let mut element = written_element(definition)?;
        while let Self::Array(definition) = element {
            element = written_element(definition)?;
        }
        Some(element)
    }

    fn array_dimensions(&self, _database: &Self::DB) -> Option<NonZeroU32> {
        let mut dimensions = 0_u32;
        let mut current = Some(self);
        while let Some(Self::Array(definition)) = current {
            dimensions = dimensions.saturating_add(1);
            current = written_element(definition);
        }
        NonZeroU32::new(dimensions)
    }
}
