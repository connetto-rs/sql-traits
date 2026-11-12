//! Submodule implementing the [`DataTypeLike`] trait for `sqlparser`'s
//! [`DataType`] enum.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::{num::NonZeroU32, slice};

use sqlparser::ast::{
    ArrayElemTypeDef, ColumnDef, ColumnOption, DataType, EnumMember, ExactNumberInfo, Expr, Ident,
    MapBracketKind, ObjectName, ObjectNamePart, SqlOption, StructField, TimezoneInfo, TypeModifier,
    UnaryOperator, UnionField, UserDefinedTypeCompositeAttributeDef, UserDefinedTypeRepresentation,
    UserDefinedTypeSqlDefinitionOption, Value, ValueWithSpan,
};

use crate::{
    impls::sqlparser::{SqlparserDialect, bound_domain, bound_user_type},
    structs::{ParserDB, UserDefinedType},
    traits::{
        CharacterLengthUnit, ColumnCollation, DataTypeLike, DatabaseLike, EnumWidth, Enumeration,
        EnumerationKind, EnumerationMember, ExtensionLike, IntervalFields, MapEntry,
        NamedColumnCollation, StructureKind, StructuredField, VectorKind, VectorType,
    },
    utils::{
        identifier_resolution::{identifiers_match, stored_identifier},
        normalize_sqlparser_type,
        object_name::{
            PG_CATALOG, Qualifier, object_name_last_part, qualifier_of, relation_search_path,
            target_name_of_object_name,
        },
        postgres_types::PgType,
        scalar_family::ScalarFamily,
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
        super::dialect::fixed_binary_length(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn max_binary_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::max_binary_length(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn max_bit_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::max_bit_length(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn max_character_length(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::max_character_length(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn character_length_unit(&self, database: &Self::DB) -> Option<CharacterLengthUnit> {
        super::dialect::character_length_unit(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn fractional_seconds_precision(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::fractional_seconds_precision(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn time_zone<'db>(&'db self, _database: &'db Self::DB) -> Option<&'db str> {
        match stored_as(self) {
            Self::Datetime64(_, zone) => zone.as_deref(),
            _ => None,
        }
    }

    #[inline]
    fn numeric_precision(&self, database: &Self::DB) -> Option<u64> {
        super::dialect::numeric_precision(*database.dialect(), stored_as(self))
    }

    #[inline]
    fn numeric_scale(&self, database: &Self::DB) -> Option<i64> {
        super::dialect::numeric_scale(*database.dialect(), stored_as(self))
    }

    fn is_unsigned(&self, database: &Self::DB) -> Option<bool> {
        matches!(
            self.scalar_family(database),
            Some(ScalarFamily::Int | ScalarFamily::Float | ScalarFamily::Decimal)
        )
        .then(|| super::dialect::is_unsigned(*database.dialect(), self))
    }

    #[inline]
    fn interval_fields(&self, _database: &Self::DB) -> Option<IntervalFields> {
        match stored_as(self) {
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
        let Self::Array(definition) = stored_as(self) else {
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
        let mut current = Some(stored_as(self));
        while let Some(Self::Array(definition)) = current {
            dimensions = dimensions.saturating_add(1);
            current = written_element(definition);
        }
        NonZeroU32::new(dimensions)
    }

    fn admits_null(&self, database: &Self::DB) -> bool {
        let mut current = self;
        while let Self::LowCardinality(inner) = current {
            current = inner;
        }
        // ClickHouse's `Variant` and `Dynamic` hold `NULL` of their own and
        // refuse a `Nullable` around them.
        match current {
            Self::Nullable(_) => true,
            Self::Custom(name, _)
                if super::dialect::custom_type_is(name, "Variant")
                    || super::dialect::custom_type_is(name, "Dynamic") =>
            {
                true
            }
            _ => !matches!(*database.dialect(), SqlparserDialect::ClickHouse),
        }
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

    fn structure_kind(&self, database: &Self::DB) -> Option<StructureKind> {
        match stored_as(self) {
            Self::Struct(..) => Some(StructureKind::Struct),
            Self::Union(_) => Some(StructureKind::Union),
            Self::Object(_) => Some(StructureKind::Object),
            Self::Tuple(_) => Some(StructureKind::Tuple),
            Self::Nested(_) => Some(StructureKind::Nested),
            stored => {
                let (user_type, _) = user_type_of(stored, database)?;
                user_type.composite_attributes().map(|_| StructureKind::Struct)
            }
        }
    }

    fn structured_fields<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<impl Iterator<Item = StructuredField<'db, Self>>> {
        let written = match stored_as(self) {
            Self::Struct(fields, _) | Self::Tuple(fields) => WrittenFields::Struct(fields.iter()),
            Self::Union(fields) => WrittenFields::Union(fields.iter()),
            Self::Object(columns) | Self::Nested(columns) => WrittenFields::Column(columns.iter()),
            stored => {
                let (user_type, _) = user_type_of(stored, database)?;
                WrittenFields::Composite(user_type, user_type.composite_attributes()?.iter())
            }
        };
        Some(FieldsOf { written, database })
    }

    fn map_entry<'db>(&'db self, database: &'db Self::DB) -> Option<MapEntry<'db, Self>> {
        let Self::Map(key, value, bracket) = stored_as(self) else {
            return None;
        };
        let value_nullable =
            !matches!(bracket, MapBracketKind::ParenthesesNotNull) && value.admits_null(database);
        Some(MapEntry::new(key, value, value_nullable))
    }

    fn enumeration<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<Enumeration<impl Iterator<Item = EnumerationMember<'db>>>> {
        enumeration_of(self, database)
    }
}

/// The type a value is stored as, through ClickHouse's `LowCardinality`, which
/// changes only how a column stores its values, and `Nullable`, which only
/// lets a value be `NULL`.
fn stored_as(data_type: &DataType) -> &DataType {
    let mut current = data_type;
    while let DataType::LowCardinality(inner) | DataType::Nullable(inner) = current {
        current = inner;
    }
    current
}

/// The fields a structured type writes, by the node each is written as.
enum WrittenFields<'db> {
    /// The fields of a `STRUCT` or a `Tuple`.
    Struct(slice::Iter<'db, StructField>),
    /// The fields of a `UNION`.
    Union(slice::Iter<'db, UnionField>),
    /// The fields of an `OBJECT` or a `Nested`, written as column definitions.
    Column(slice::Iter<'db, ColumnDef>),
    /// The attributes of a composite type, with the type that records the
    /// facts of their collations.
    Composite(&'db UserDefinedType, slice::Iter<'db, UserDefinedTypeCompositeAttributeDef>),
}

/// The fields of a structured type, as the database holding it answers them.
struct FieldsOf<'db> {
    written: WrittenFields<'db>,
    database: &'db ParserDB,
}

impl<'db> Iterator for FieldsOf<'db> {
    type Item = StructuredField<'db, DataType>;

    fn next(&mut self) -> Option<Self::Item> {
        let database = self.database;
        Some(match &mut self.written {
            WrittenFields::Struct(fields) => {
                let field = fields.next()?;
                let (data_type, collation) = collated(&field.field_type);
                let built = StructuredField::new(data_type, data_type.admits_null(database))
                    .with_collation(collation);
                let built = match &field.field_name {
                    Some(name) => built.with_name(&name.value, name.quote_style.is_some()),
                    None => built,
                };
                match &field.options {
                    Some(options) => built.with_options(options.iter().map(option_entry).collect()),
                    None => built,
                }
            }
            WrittenFields::Union(fields) => {
                let field = fields.next()?;
                let (data_type, collation) = collated(&field.field_type);
                StructuredField::new(data_type, data_type.admits_null(database))
                    .with_name(&field.field_name.value, field.field_name.quote_style.is_some())
                    .with_collation(collation)
            }
            WrittenFields::Column(columns) => column_field(columns.next()?, database),
            WrittenFields::Composite(user_type, attributes) => {
                let attribute = attributes.next()?;
                let built = StructuredField::new(
                    &attribute.data_type,
                    attribute.data_type.admits_null(database),
                )
                .with_name(&attribute.name.value, attribute.name.quote_style.is_some());
                let Some(name) = &attribute.collation else {
                    return Some(built);
                };
                let mut named = NamedColumnCollation::new(target_name_of_object_name(name));
                if let Some(resolved) = user_type.attribute_collation(&attribute.name) {
                    named = named
                        .with_postgres_deterministic(resolved.postgres_deterministic)
                        .with_mysql_padding(resolved.mysql_padding);
                }
                built.with_collation(ColumnCollation::Named(named))
            }
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.written {
            WrittenFields::Struct(fields) => fields.size_hint(),
            WrittenFields::Union(fields) => fields.size_hint(),
            WrittenFields::Column(columns) => columns.size_hint(),
            WrittenFields::Composite(_, attributes) => attributes.size_hint(),
        }
    }
}

/// The members of an enumerated type, as each representation stores them.
#[derive(Debug, Clone)]
enum Members<'db> {
    /// The members of an `ENUM` whose engine stores positions.
    Positional(slice::Iter<'db, EnumMember>),
    /// The members of a `SET`.
    Set(slice::Iter<'db, String>),
    /// The labels of a PostgreSQL enum type.
    Labels(slice::Iter<'db, Ident>),
    /// The members of a ClickHouse enum, in the order of their values.
    Valued(vec::IntoIter<EnumerationMember<'db>>),
}

impl<'db> Iterator for Members<'db> {
    type Item = EnumerationMember<'db>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Positional(members) => {
                members.next().map(|member| {
                    let (EnumMember::Name(label) | EnumMember::NamedValue(label, _)) = member;
                    EnumerationMember::new(label, None)
                })
            }
            Self::Set(labels) => labels.next().map(|label| EnumerationMember::new(label, None)),
            Self::Labels(labels) => {
                labels.next().map(|label| EnumerationMember::new(&label.value, None))
            }
            Self::Valued(members) => members.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Positional(members) => members.size_hint(),
            Self::Set(labels) => labels.size_hint(),
            Self::Labels(labels) => labels.size_hint(),
            Self::Valued(members) => members.size_hint(),
        }
    }
}

/// The type a field declares, without the `COLLATE` written after it, and
/// the collation that names.
fn collated(written: &DataType) -> (&DataType, ColumnCollation<'_>) {
    match written {
        DataType::Collate(data_type, name) => (data_type, named_collation(name)),
        data_type => (data_type, ColumnCollation::DatabaseDefault),
    }
}

/// The collation `name` names, without facts, since an inline field's
/// collation is resolved against no catalog.
fn named_collation(name: &ObjectName) -> ColumnCollation<'_> {
    ColumnCollation::Named(NamedColumnCollation::new(target_name_of_object_name(name)))
}

/// A field written as a column definition, with the `NOT NULL` it declares.
fn column_field<'db>(
    column: &'db ColumnDef,
    database: &'db ParserDB,
) -> StructuredField<'db, DataType> {
    let (data_type, collation) = collated(&column.data_type);
    let not_null =
        column.options.iter().any(|option| matches!(option.option, ColumnOption::NotNull));
    StructuredField::new(data_type, !not_null && data_type.admits_null(database))
        .with_name(&column.name.value, column.name.quote_style.is_some())
        .with_collation(collation)
}

/// An option as a key as written and its value as text, a string literal by
/// its contents.
fn option_entry(option: &SqlOption) -> (Cow<'_, str>, Cow<'_, str>) {
    let SqlOption::KeyValue { key, value } = option else {
        return (Cow::Owned(option.to_string()), Cow::Borrowed(""));
    };
    let key = match key.0.as_slice() {
        [ObjectNamePart::Identifier(part)] => Cow::Borrowed(part.value.as_str()),
        _ => Cow::Owned(key.to_string()),
    };
    let value = match value {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(text) | Value::DoubleQuotedString(text),
            ..
        }) => Cow::Borrowed(text.as_str()),
        _ => Cow::Owned(value.to_string()),
    };
    (key, value)
}

/// The enumeration `data_type` declares or names, read through the
/// wrappers [`stored_as`] reads through.
fn enumeration_of<'db>(
    data_type: &'db DataType,
    database: &'db ParserDB,
) -> Option<Enumeration<Members<'db>>> {
    match stored_as(data_type) {
        DataType::Enum(members, width) => {
            let valued = width.is_some()
                || *database.dialect() == SqlparserDialect::ClickHouse
                || members.iter().any(|member| matches!(member, EnumMember::NamedValue(..)));
            if !valued {
                return Some(Enumeration::new(
                    EnumerationKind::Enum,
                    None,
                    Members::Positional(members.iter()),
                ));
            }
            let (width, members) = clickhouse_members(members, *width)?;
            Some(Enumeration::new(
                EnumerationKind::Enum,
                Some(width),
                Members::Valued(members.into_iter()),
            ))
        }
        DataType::Set(labels) => {
            Some(Enumeration::new(EnumerationKind::Set, None, Members::Set(labels.iter())))
        }
        stored @ DataType::Custom(..) => {
            let (user_type, _) = user_type_of(stored, database)?;
            let Some(UserDefinedTypeRepresentation::Enum { labels }) = user_type.representation()
            else {
                return None;
            };
            Some(Enumeration::new(EnumerationKind::Enum, None, Members::Labels(labels.iter())))
        }
        _ => None,
    }
}

/// The width and the members, sorted by value, ClickHouse stores for an enum
/// declared with `members` and the width written, or `None` for a declaration
/// ClickHouse refuses for its values.
fn clickhouse_members(
    members: &[EnumMember],
    written_width: Option<u8>,
) -> Option<(EnumWidth, Vec<EnumerationMember<'_>>)> {
    let (first, rest) = members.split_first()?;
    let base = match first {
        EnumMember::Name(_) => 1,
        EnumMember::NamedValue(_, value) => integer(value)?,
    };
    let written_after_first =
        rest.iter().filter(|member| matches!(member, EnumMember::NamedValue(..))).count();
    if written_after_first != 0 && written_after_first != rest.len() {
        return None;
    }
    let mut stored = Vec::with_capacity(members.len());
    for (position, member) in (0_i64..).zip(members) {
        let (label, value) = match member {
            EnumMember::Name(label) => (label, base.checked_add(position)?),
            EnumMember::NamedValue(label, value) => (label, integer(value)?),
        };
        stored.push(EnumerationMember::new(label, Some(i16::try_from(value).ok()?)));
    }
    let fits_enum8 =
        stored.iter().all(|member| member.value().is_some_and(|value| i8::try_from(value).is_ok()));
    let width = match (written_width, fits_enum8) {
        (Some(8) | None, true) => EnumWidth::Enum8,
        (Some(16), _) | (None, false) => EnumWidth::Enum16,
        _ => return None,
    };
    stored.sort_by_key(|member| member.value());
    Some((width, stored))
}

/// The integer a ClickHouse enum value literal writes, or `None` for one that
/// is not an integer.
fn integer(value: &Expr) -> Option<i64> {
    match value {
        Expr::Value(ValueWithSpan { value: Value::Number(digits, false), .. }) => {
            digits.parse().ok()
        }
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => integer(expr),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => integer(expr)?.checked_neg(),
        _ => None,
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
