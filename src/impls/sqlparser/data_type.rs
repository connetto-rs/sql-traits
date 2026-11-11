//! Submodule implementing the [`DataTypeLike`] trait for `sqlparser`'s
//! [`DataType`] enum.

use alloc::borrow::Cow;
use core::num::NonZeroU32;

use sqlparser::ast::{
    ArrayElemTypeDef, DataType, ExactNumberInfo, TimezoneInfo, TypeModifier,
    UserDefinedTypeRepresentation, UserDefinedTypeSqlDefinitionOption,
};

use crate::{
    impls::sqlparser::{SqlparserDialect, bound_domain, bound_user_type},
    structs::{ParserDB, UserDefinedType},
    traits::{DataTypeLike, DatabaseLike, ExtensionLike, IntervalFields, VectorKind, VectorType},
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        normalize_sqlparser_type,
        object_name::{
            PG_CATALOG, Qualifier, object_name_last_part, qualifier_of, relation_search_path,
        },
        postgres_types::PgType,
    },
};

/// PostgreSQL 18's `pg_range`: each range type, its multirange and its
/// subtype.
static CATALOG_RANGES: [(&str, &str, DataType); 6] = [
    ("daterange", "datemultirange", DataType::Date),
    ("int4range", "int4multirange", DataType::Int4(None)),
    ("int8range", "int8multirange", DataType::Int8(None)),
    ("numrange", "nummultirange", DataType::Numeric(ExactNumberInfo::None)),
    ("tsrange", "tsmultirange", DataType::Timestamp(None, TimezoneInfo::None)),
    ("tstzrange", "tstzmultirange", DataType::Timestamp(None, TimezoneInfo::Tz)),
];

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
            .any(|(entry, quoted)| stored_identifier(entry, quoted) == schema)
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
    fn numeric_precision(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::numeric_precision(*database.dialect(), self)
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
            Qualifier::Named(qualifier, quoted) => stored_identifier(qualifier, quoted) == schema,
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

    fn base_type<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Self> {
        let case = database.identifier_case();
        let mut current = self;
        let mut reached = None;
        // A chain through every domain ends one step after the last of them,
        // so a walk lasting longer has met a domain declared over a type of
        // its own name, which PostgreSQL refuses and which reaches no base.
        for _ in 0..=database.domains().count() {
            let Self::Custom(name, _) = current else {
                return reached;
            };
            let Some(domain) = bound_domain(database.domains(), name, case) else {
                return reached;
            };
            current = &domain.data_type;
            reached = Some(current);
        }
        None
    }

    fn send_function(&self, database: &Self::DB) -> Option<bool> {
        // Only PostgreSQL has binary send functions.
        if !matches!(*database.dialect(), SqlparserDialect::PostgreSql | SqlparserDialect::Generic)
        {
            return None;
        }
        if let Some(base) = self.base_type(database) {
            return base.send_function(database);
        }
        if let Some((user_type, _)) = user_type_of(self, database) {
            // A shell types no value, so only a base type may lack one.
            return Some(match user_type.representation() {
                Some(UserDefinedTypeRepresentation::SqlDefinition { options }) => {
                    options
                        .iter()
                        .any(|option| matches!(option, UserDefinedTypeSqlDefinitionOption::Send(_)))
                }
                defined => defined.is_some(),
            });
        }
        PgType::of(self).send_function()
    }

    fn range_subtype<'db>(&'db self, database: &'db Self::DB) -> Option<&'db Self> {
        if !matches!(self, Self::Custom(..)) {
            return None;
        }
        if let Some((user_type, _)) = user_type_of(self, database) {
            return user_type.range_subtype();
        }
        let PgType::Catalog(name) = PgType::of(self) else {
            return None;
        };
        CATALOG_RANGES
            .iter()
            .find(|(range, multirange, _)| name == *range || name == *multirange)
            .map(|(_, _, subtype)| subtype)
    }

    fn composite_fields<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<impl Iterator<Item = &'db Self>> {
        let (user_type, _) = user_type_of(self, database)?;
        let attributes = user_type.composite_attributes()?;
        Some(attributes.iter().map(|attribute| &attribute.data_type))
    }
}

/// The user-defined type a type name reaches, with whether the name is its
/// multirange's.
fn user_type_of<'db>(
    data_type: &DataType,
    database: &'db ParserDB,
) -> Option<(&'db UserDefinedType, bool)> {
    let DataType::Custom(name, _) = data_type else {
        return None;
    };
    bound_user_type(database.user_defined_types(), name, database.identifier_case())
}
