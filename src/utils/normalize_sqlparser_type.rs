//! Submodule providing a function for normalizing `SQLParser` data types.

use alloc::{borrow::Cow, format, string::String};

use sqlparser::ast::{ArrayElemTypeDef, DataType, GeometricTypeKind, ObjectName, TimezoneInfo};

use crate::utils::object_name::object_name_part_value;

/// Normalizes `SQLParser` data types to a standard representation.
///
/// Every declaration the parser can produce has an answer here. The match is
/// exhaustive on purpose and carries no catch-all arm, so a `sqlparser` upgrade
/// that introduces a type stops this crate from building until the type is
/// classified, rather than reaching a caller as a runtime failure.
///
/// A parameter that only sizes or decorates a type is dropped, so
/// `VARCHAR(255)` and `BIT(8)` normalize to `VARCHAR` and `BIT`, and a member
/// or column list is dropped the same way. The pinned `GEOGRAPHY(Point, 4326)`
/// and `GEOMETRY(Point, 4326)` spellings are the exception and keep theirs. An
/// element type is kept, so an array recurses into what it holds.
/// MySQL's `ZEROFILL` padding reports its normalized inner type.
///
/// # Examples
///
/// ```
/// use sql_traits::utils::normalize_sqlparser_type;
/// use sqlparser::ast::{DataType, ExactNumberInfo, ObjectName, ObjectNamePart};
///
/// assert_eq!(normalize_sqlparser_type(&DataType::Text), "TEXT");
/// assert_eq!(normalize_sqlparser_type(&DataType::Int(None)), "INT");
/// assert_eq!(normalize_sqlparser_type(&DataType::BigInt(None)), "BIGINT");
/// assert_eq!(normalize_sqlparser_type(&DataType::Uuid), "UUID");
/// assert_eq!(normalize_sqlparser_type(&DataType::Date), "DATE");
/// assert_eq!(normalize_sqlparser_type(&DataType::JSON), "JSON");
/// assert_eq!(normalize_sqlparser_type(&DataType::Bytea), "BYTEA");
/// assert_eq!(normalize_sqlparser_type(&DataType::Decimal(ExactNumberInfo::None)), "DECIMAL",);
///
/// // `CHARACTER VARYING` is the same type as `VARCHAR`, spelled the long way.
/// assert_eq!(normalize_sqlparser_type(&DataType::CharacterVarying(None)), "VARCHAR");
///
/// // A declared field list is dropped, exactly as a declared length is.
/// assert_eq!(
///     normalize_sqlparser_type(&DataType::Interval { fields: None, precision: None }),
///     "INTERVAL"
/// );
/// assert_eq!(normalize_sqlparser_type(&DataType::Bit(Some(8))), "BIT");
/// assert_eq!(normalize_sqlparser_type(&DataType::Table(None)), "TABLE");
///
/// // The return type of every trigger function.
/// assert_eq!(normalize_sqlparser_type(&DataType::Trigger), "TRIGGER");
///
/// // MySQL enumerations normalize to their family token, dropping members.
/// use sqlparser::ast::EnumMember;
/// assert_eq!(
///     normalize_sqlparser_type(&DataType::Enum(vec![EnumMember::Name("a".into())], None)),
///     "ENUM",
/// );
/// assert_eq!(normalize_sqlparser_type(&DataType::Set(vec!["a".to_string()])), "SET");
///
/// // A Snowflake structured object drops its fields, answering the same token
/// // as a bare `OBJECT`, which parses as a custom type of that name.
/// assert_eq!(normalize_sqlparser_type(&DataType::Object(vec![])), "OBJECT");
///
/// // Custom types
/// let custom = DataType::Custom(
///     ObjectName(vec![ObjectNamePart::Identifier(sqlparser::ast::Ident::new("GEOGRAPHY"))]),
///     vec!["Point".to_string(), "4326".to_string()],
/// );
/// assert_eq!(normalize_sqlparser_type(&custom), "GEOGRAPHY(Point, 4326)");
///
/// let custom_geom = DataType::Custom(
///     ObjectName(vec![ObjectNamePart::Identifier(sqlparser::ast::Ident::new("GEOMETRY"))]),
///     vec!["Point".to_string(), "4326".to_string()],
/// );
/// assert_eq!(normalize_sqlparser_type(&custom_geom), "GEOMETRY(Point, 4326)");
///
/// let custom_other = DataType::Custom(
///     ObjectName(vec![ObjectNamePart::Identifier(sqlparser::ast::Ident::new("OTHER"))]),
///     vec![],
/// );
/// assert_eq!(normalize_sqlparser_type(&custom_other), "OTHER");
///
/// // A composite type or a domain living in a schema keeps the schema.
/// let qualified = DataType::Custom(
///     ObjectName(vec![
///         ObjectNamePart::Identifier(sqlparser::ast::Ident::new("app")),
///         ObjectNamePart::Identifier(sqlparser::ast::Ident::new("my_type")),
///     ]),
///     vec![],
/// );
/// assert_eq!(normalize_sqlparser_type(&qualified), "app.my_type");
///
/// // Arrays canonicalize to `<element>[]` whatever spelling declared them,
/// // recursing on the element type so nesting survives.
/// use sqlparser::ast::ArrayElemTypeDef;
/// let text_array =
///     DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(DataType::Text), None));
/// assert_eq!(normalize_sqlparser_type(&text_array), "TEXT[]");
///
/// // A declared length is dropped, exactly as `VARCHAR(255)` drops its length.
/// let bounded =
///     DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(DataType::Text), Some(3)));
/// assert_eq!(normalize_sqlparser_type(&bounded), "TEXT[]");
///
/// // `ARRAY<INT>` and `INT[]` are the same type spelled two ways.
/// let angle = DataType::Array(ArrayElemTypeDef::AngleBracket(Box::new(DataType::Int(None))));
/// assert_eq!(normalize_sqlparser_type(&angle), "INT[]");
///
/// // An array with no declared element type has nothing to recurse on.
/// assert_eq!(normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::None)), "ARRAY");
///
/// // `Nullable` and `LowCardinality` decorate storage rather than name a type,
/// // so they report the type they wrap.
/// let low_cardinality = DataType::LowCardinality(Box::new(DataType::Text));
/// assert_eq!(normalize_sqlparser_type(&low_cardinality), "TEXT");
///
/// // A collation decides how values compare, so it reports the type it wraps.
/// let collated = DataType::Collate(
///     Box::new(DataType::String(None)),
///     ObjectName(vec![ObjectNamePart::Identifier(sqlparser::ast::Ident::new("UTF8_LCASE"))]),
/// );
/// assert_eq!(normalize_sqlparser_type(&collated), "STRING");
/// ```
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one arm per sqlparser data type: splitting the table would reintroduce a catch-all and with it the possibility of an unclassified type"
)]
pub fn normalize_sqlparser_type(sqlparser_type: &DataType) -> Cow<'_, str> {
    match sqlparser_type {
        // INT family. Unsigned variants fold to their signed token, dropping
        // `UNSIGNED` as the display width is dropped. MySQL's cast targets
        // (`SIGNED`, `UNSIGNED INTEGER`) name the integer family and nothing
        // narrower.
        DataType::Int(_)
        | DataType::Integer(_)
        | DataType::IntUnsigned(_)
        | DataType::IntegerUnsigned(_)
        | DataType::Signed
        | DataType::SignedInteger
        | DataType::Unsigned
        | DataType::UnsignedInteger => "INT".into(),
        DataType::SmallInt(_) | DataType::SmallIntUnsigned(_) | DataType::USmallInt => {
            "SMALLINT".into()
        }
        DataType::BigInt(_) | DataType::BigIntUnsigned(_) | DataType::UBigInt => "BIGINT".into(),
        DataType::TinyInt(_) | DataType::TinyIntUnsigned(_) | DataType::UTinyInt => {
            "TINYINT".into()
        }
        DataType::MediumInt(_) | DataType::MediumIntUnsigned(_) => "MEDIUMINT".into(),
        DataType::Int2(_) | DataType::Int2Unsigned(_) => "INT2".into(),
        DataType::Int4(_) | DataType::Int4Unsigned(_) => "INT4".into(),
        DataType::Int8(_) | DataType::Int8Unsigned(_) => "INT8".into(),
        DataType::HugeInt | DataType::UHugeInt => "HUGEINT".into(),
        // `ClickHouse` widths name a size the aliases above do not, so they
        // stay distinct rather than folding into the platform integer.
        DataType::Int16 => "INT16".into(),
        DataType::Int32 => "INT32".into(),
        DataType::Int64 => "INT64".into(),
        DataType::Int128 => "INT128".into(),
        DataType::Int256 => "INT256".into(),
        DataType::UInt8 => "UINT8".into(),
        DataType::UInt16 => "UINT16".into(),
        DataType::UInt32 => "UINT32".into(),
        DataType::UInt64 => "UINT64".into(),
        DataType::UInt128 => "UINT128".into(),
        DataType::UInt256 => "UINT256".into(),
        // FLOAT family
        DataType::Real | DataType::RealUnsigned => "REAL".into(),
        DataType::Float(_) | DataType::FloatUnsigned(_) => "FLOAT".into(),
        DataType::Float4 => "FLOAT4".into(),
        DataType::Float8 => "FLOAT8".into(),
        DataType::Float32 => "FLOAT32".into(),
        DataType::Float64 => "FLOAT64".into(),
        DataType::Double(_) | DataType::DoubleUnsigned(_) => "DOUBLE".into(),
        DataType::DoublePrecision | DataType::DoublePrecisionUnsigned => "DOUBLE PRECISION".into(),
        // DECIMAL family
        DataType::Decimal(_)
        | DataType::DecimalUnsigned(_)
        | DataType::Dec(_)
        | DataType::DecUnsigned(_)
        | DataType::BigDecimal(_) => "DECIMAL".into(),
        DataType::Numeric(_) | DataType::BigNumeric(_) => "NUMERIC".into(),
        // BOOL family
        DataType::Bool | DataType::Boolean => "BOOLEAN".into(),
        // STRING family. `CHARACTER VARYING` and `CHARACTER LARGE OBJECT` are
        // the long spellings of `VARCHAR` and `CLOB`.
        DataType::Text => "TEXT".into(),
        DataType::TinyText => "TINYTEXT".into(),
        DataType::MediumText => "MEDIUMTEXT".into(),
        DataType::LongText => "LONGTEXT".into(),
        DataType::Varchar(_) | DataType::CharacterVarying(_) | DataType::CharVarying(_) => {
            "VARCHAR".into()
        }
        DataType::Char(_) | DataType::Character(_) => "CHAR".into(),
        DataType::Clob(_) | DataType::CharacterLargeObject(_) | DataType::CharLargeObject(_) => {
            "CLOB".into()
        }
        DataType::Nvarchar(_) => "NVARCHAR".into(),
        DataType::String(_) => "STRING".into(),
        DataType::FixedString(_) => "FIXEDSTRING".into(),
        // BYTES family
        DataType::Bytea => "BYTEA".into(),
        DataType::Bytes(_) => "BYTES".into(),
        DataType::Blob(_) => "BLOB".into(),
        DataType::TinyBlob => "TINYBLOB".into(),
        DataType::MediumBlob => "MEDIUMBLOB".into(),
        DataType::LongBlob => "LONGBLOB".into(),
        DataType::Binary(_) => "BINARY".into(),
        DataType::Varbinary(_) => "VARBINARY".into(),
        // BIT family, following `VARCHAR` in preferring the short spelling of
        // the varying form.
        DataType::Bit(_) => "BIT".into(),
        DataType::BitVarying(_) | DataType::VarBit(_) => "VARBIT".into(),
        // DATE
        DataType::Date => "DATE".into(),
        DataType::Date32 => "DATE32".into(),
        // TIME family
        DataType::Time(_, TimezoneInfo::None) => "TIME".into(),
        DataType::Time(_, TimezoneInfo::WithoutTimeZone) => "TIME WITHOUT TIME ZONE".into(),
        DataType::Time(_, TimezoneInfo::WithTimeZone) => "TIME WITH TIME ZONE".into(),
        DataType::Time(_, TimezoneInfo::Tz) => "TIMETZ".into(),
        // TIMESTAMP family. Snowflake's `TIMESTAMP_NTZ` is the same type as the
        // standard timestamp without a zone.
        DataType::Timestamp(_, TimezoneInfo::None) => "TIMESTAMP".into(),
        DataType::Timestamp(_, TimezoneInfo::WithoutTimeZone) | DataType::TimestampNtz(_) => {
            "TIMESTAMP WITHOUT TIME ZONE".into()
        }
        DataType::Timestamp(_, TimezoneInfo::WithTimeZone) => "TIMESTAMP WITH TIME ZONE".into(),
        DataType::Timestamp(_, TimezoneInfo::Tz) => "TIMESTAMPTZ".into(),
        // MySQL wall-clock timestamp, precision dropped like a length.
        DataType::Datetime(_) => "DATETIME".into(),
        DataType::Datetime64(..) => "DATETIME64".into(),
        // A field list such as `YEAR TO MONTH` narrows the same type, exactly
        // as a length narrows `VARCHAR`.
        DataType::Interval { .. } => "INTERVAL".into(),
        // UUID
        DataType::Uuid => "UUID".into(),
        // JSON family
        DataType::JSON => "JSON".into(),
        DataType::JSONB => "JSONB".into(),
        // PostgreSQL catalog and text search types
        DataType::Regclass => "REGCLASS".into(),
        DataType::TsVector => "TSVECTOR".into(),
        DataType::TsQuery => "TSQUERY".into(),
        // PostgreSQL geometric types
        DataType::GeometricType(GeometricTypeKind::Point) => "POINT".into(),
        DataType::GeometricType(GeometricTypeKind::Line) => "LINE".into(),
        DataType::GeometricType(GeometricTypeKind::LineSegment) => "LSEG".into(),
        DataType::GeometricType(GeometricTypeKind::GeometricBox) => "BOX".into(),
        DataType::GeometricType(GeometricTypeKind::GeometricPath) => "PATH".into(),
        DataType::GeometricType(GeometricTypeKind::Polygon) => "POLYGON".into(),
        DataType::GeometricType(GeometricTypeKind::Circle) => "CIRCLE".into(),
        // Composite types. The member, field or column list is not part of the
        // type family, just as a length is not.
        DataType::Enum(..) => "ENUM".into(),
        DataType::Set(..) => "SET".into(),
        DataType::Struct(..) => "STRUCT".into(),
        DataType::Union(_) => "UNION".into(),
        DataType::Tuple(_) => "TUPLE".into(),
        DataType::Nested(_) => "NESTED".into(),
        // Snowflake's structured object. A bare `OBJECT` parses as a custom
        // type of that name, so both spellings answer the same token.
        DataType::Object(_) => "OBJECT".into(),
        DataType::Map(..) => "MAP".into(),
        // A set-returning function declares a row shape rather than a value.
        DataType::Table(_) | DataType::NamedTable { .. } => "TABLE".into(),
        // The return type of every trigger function.
        DataType::Trigger => "TRIGGER".into(),
        DataType::AnyType => "ANY TYPE".into(),
        // The declaration named no type at all.
        DataType::Unspecified => "UNSPECIFIED".into(),
        // Storage and display decorations report the type they wrap.
        DataType::Nullable(inner) | DataType::LowCardinality(inner) | DataType::Zerofill(inner) => {
            normalize_sqlparser_type(inner)
        }
        // Collation decides how values compare, not what type they are.
        DataType::Collate(inner, _) => normalize_sqlparser_type(inner),
        // An array's token is assembled from its element's, so unlike every
        // other token it cannot be a borrow into the input. A `NOT NULL`
        // element constraint decorates the element as `Nullable` does a column.
        DataType::Array(ArrayElemTypeDef::None) => "ARRAY".into(),
        DataType::Array(
            ArrayElemTypeDef::AngleBracket(element)
            | ArrayElemTypeDef::Parenthesis(element)
            | ArrayElemTypeDef::ParenthesisNotNull(element)
            | ArrayElemTypeDef::SquareBracket(element, _)
            | ArrayElemTypeDef::Qualified(element, _),
        ) => format!("{}[]", normalize_sqlparser_type(element)).into(),
        // Custom: the written name, with pinned GEOGRAPHY/GEOMETRY recognition.
        // A qualified name keeps its schema, since a composite type or a domain
        // in a schema is an ordinary declaration.
        DataType::Custom(ObjectName(parts), segments) => {
            match parts.as_slice() {
                // No name was written, so no type was named. Answering the
                // empty string here would collapse into a neighbouring
                // declaration.
                //
                // The parser never produces a name with no parts, which is why
                // `last_str` and `target_name_of_object_name` lean on that and
                // panic. This function does not, because it is total over any
                // `DataType` a caller can build and its answer is the whole
                // point of the totality.
                [] => "UNSPECIFIED".into(),
                [only] => {
                    // A part built at run time names no type here, which is
                    // the same position an empty name leaves the reader in.
                    let Some(name) = object_name_part_value(only) else {
                        return "UNSPECIFIED".into();
                    };
                    if segments == &["Point", "4326"] {
                        if name == "GEOGRAPHY" {
                            return "GEOGRAPHY(Point, 4326)".into();
                        }
                        if name == "GEOMETRY" {
                            return "GEOMETRY(Point, 4326)".into();
                        }
                    }
                    name.into()
                }
                qualified => {
                    let mut joined = String::new();
                    for (index, part) in qualified.iter().enumerate() {
                        let Some(value) = object_name_part_value(part) else {
                            return "UNSPECIFIED".into();
                        };
                        if index > 0 {
                            joined.push('.');
                        }
                        joined.push_str(value);
                    }
                    joined.into()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use sqlparser::ast::{DataType, Ident, ObjectName, ObjectNamePart, TimezoneInfo};

    use super::*;

    #[test]
    fn test_normalize_sqlparser_type_simple() {
        assert_eq!(normalize_sqlparser_type(&DataType::Uuid), "UUID");
        assert_eq!(normalize_sqlparser_type(&DataType::Text), "TEXT");
        assert_eq!(normalize_sqlparser_type(&DataType::Varchar(None)), "VARCHAR");
        assert_eq!(normalize_sqlparser_type(&DataType::Int(None)), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::Integer(None)), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::Real), "REAL");
        assert_eq!(normalize_sqlparser_type(&DataType::SmallInt(None)), "SMALLINT");
        assert_eq!(normalize_sqlparser_type(&DataType::Bool), "BOOLEAN");
        assert_eq!(normalize_sqlparser_type(&DataType::Boolean), "BOOLEAN");
    }

    #[test]
    fn test_normalize_sqlparser_type_timestamp() {
        assert_eq!(
            normalize_sqlparser_type(&DataType::Timestamp(None, TimezoneInfo::None)),
            "TIMESTAMP"
        );
        // `WithTimeZone` corresponds to the verbose SQL form and renders as
        // `TIMESTAMP WITH TIME ZONE`, while the compact `TIMESTAMPTZ` form
        // comes from `TimezoneInfo::Tz`. Both fold to the canonical
        // `TIMESTAMPTZ` type token in `fingerprint_type_token`.
        assert_eq!(
            normalize_sqlparser_type(&DataType::Timestamp(None, TimezoneInfo::WithTimeZone)),
            "TIMESTAMP WITH TIME ZONE"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Timestamp(None, TimezoneInfo::Tz)),
            "TIMESTAMPTZ"
        );
    }

    #[test]
    fn test_normalize_sqlparser_type_custom() {
        let geography = DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("GEOGRAPHY"))]),
            vec!["Point".to_string(), "4326".to_string()],
        );
        assert_eq!(normalize_sqlparser_type(&geography), "GEOGRAPHY(Point, 4326)");

        let geometry = DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("GEOMETRY"))]),
            vec!["Point".to_string(), "4326".to_string()],
        );
        assert_eq!(normalize_sqlparser_type(&geometry), "GEOMETRY(Point, 4326)");

        let other = DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("OTHER"))]),
            vec![],
        );
        assert_eq!(normalize_sqlparser_type(&other), "OTHER");

        // The pinned recognition keys on the name as well as the modifiers, so
        // another type carrying the same modifiers keeps its own name and drops
        // them like any other custom type.
        let same_modifiers = DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("shape"))]),
            vec!["Point".to_string(), "4326".to_string()],
        );
        assert_eq!(normalize_sqlparser_type(&same_modifiers), "shape");
    }

    /// A name in a schema keeps every part, and a name of any depth resolves
    /// without the caller having to guard the call.
    #[test]
    fn test_normalize_sqlparser_type_qualified_custom() {
        let qualified = |parts: &[&str]| {
            DataType::Custom(
                ObjectName(
                    parts
                        .iter()
                        .map(|part| ObjectNamePart::Identifier(Ident::new(*part)))
                        .collect(),
                ),
                vec![],
            )
        };

        assert_eq!(normalize_sqlparser_type(&qualified(&["app", "my_type"])), "app.my_type");
        assert_eq!(
            normalize_sqlparser_type(&qualified(&["db", "app", "my_type"])),
            "db.app.my_type"
        );

        // A quoted part contributes its value, matching how an unqualified
        // custom type already reports itself.
        let quoted = DataType::Custom(
            ObjectName(vec![
                ObjectNamePart::Identifier(Ident::with_quote('"', "App")),
                ObjectNamePart::Identifier(Ident::with_quote('"', "My Type")),
            ]),
            vec![],
        );
        assert_eq!(normalize_sqlparser_type(&quoted), "App.My Type");

        // The pinned geometry recognition is a one-part rule, so a qualified
        // name of the same shape stays a qualified name.
        let qualified_geography = DataType::Custom(
            ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new("app")),
                ObjectNamePart::Identifier(Ident::new("GEOGRAPHY")),
            ]),
            vec!["Point".to_string(), "4326".to_string()],
        );
        assert_eq!(normalize_sqlparser_type(&qualified_geography), "app.GEOGRAPHY");
    }

    /// A part that carries no characters still separates the parts around it,
    /// and a name with no parts at all reports that rather than answering the
    /// empty string, which would collapse into a neighbouring declaration.
    ///
    /// `CREATE TABLE t (a "".foo)` parses, so an empty part is reachable. A
    /// name with no parts is not, and is covered to keep the empty string out
    /// of the answers this function can give.
    #[test]
    fn test_normalize_sqlparser_type_degenerate_custom_names() {
        let custom = |parts: &[&str]| {
            DataType::Custom(
                ObjectName(
                    parts
                        .iter()
                        .map(|part| ObjectNamePart::Identifier(Ident::with_quote('"', *part)))
                        .collect(),
                ),
                vec![],
            )
        };

        assert_eq!(normalize_sqlparser_type(&custom(&["", "foo"])), ".foo");
        assert_eq!(normalize_sqlparser_type(&custom(&["a", "", "c"])), "a..c");
        assert_eq!(normalize_sqlparser_type(&custom(&["", ""])), ".");
        assert_eq!(normalize_sqlparser_type(&custom(&[])), "UNSPECIFIED");
    }

    /// sqlparser models a name part that is a call as its own variant, and
    /// what it will name is unknown until the statement runs, which leaves the
    /// reader where an empty name does.
    #[test]
    fn test_normalize_sqlparser_type_function_name_part() {
        use sqlparser::ast::ObjectNamePartFunction;

        let function_part = |name: &str| {
            ObjectNamePart::Function(ObjectNamePartFunction {
                name: Ident::new(name),
                args: alloc::vec::Vec::new(),
            })
        };

        assert_eq!(
            normalize_sqlparser_type(&DataType::Custom(
                ObjectName(vec![function_part("IDENTIFIER")]),
                vec![]
            )),
            "UNSPECIFIED"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Custom(
                ObjectName(vec![
                    ObjectNamePart::Identifier(Ident::new("app")),
                    function_part("IDENTIFIER"),
                ]),
                vec![]
            )),
            "UNSPECIFIED"
        );
    }

    // Every `DataType` variant with a canonical scalar family must normalize to
    // a string accepted by `scalar_family`.

    #[test]
    fn test_normalize_sqlparser_type_int_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::Int(None)), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::Int(Some(10))), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::Integer(None)), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::SmallInt(None)), "SMALLINT");
        assert_eq!(normalize_sqlparser_type(&DataType::SmallInt(Some(5))), "SMALLINT");
        assert_eq!(normalize_sqlparser_type(&DataType::BigInt(None)), "BIGINT");
        assert_eq!(normalize_sqlparser_type(&DataType::BigInt(Some(20))), "BIGINT");
        assert_eq!(normalize_sqlparser_type(&DataType::TinyInt(None)), "TINYINT");
        // TINYINT display widths are collapsed to a single canonical
        // family name here; width-sensitive interpretation like MySQL's
        // `TINYINT(1)` -> boolean lives on the dialect
        // (`SqlparserDialect::is_bool` matches on the raw `DataType`).
        assert_eq!(normalize_sqlparser_type(&DataType::TinyInt(Some(1))), "TINYINT");
        assert_eq!(normalize_sqlparser_type(&DataType::TinyInt(Some(2))), "TINYINT");
        assert_eq!(normalize_sqlparser_type(&DataType::TinyInt(Some(4))), "TINYINT");
        assert_eq!(normalize_sqlparser_type(&DataType::MediumInt(None)), "MEDIUMINT");
        assert_eq!(normalize_sqlparser_type(&DataType::Int2(None)), "INT2");
        assert_eq!(normalize_sqlparser_type(&DataType::Int4(None)), "INT4");
        assert_eq!(normalize_sqlparser_type(&DataType::Int8(None)), "INT8");
    }

    #[test]
    fn test_normalize_sqlparser_type_float_family() {
        use sqlparser::ast::ExactNumberInfo;
        assert_eq!(normalize_sqlparser_type(&DataType::Real), "REAL");
        assert_eq!(normalize_sqlparser_type(&DataType::Float(ExactNumberInfo::None)), "FLOAT");
        assert_eq!(
            normalize_sqlparser_type(&DataType::Float(ExactNumberInfo::Precision(24))),
            "FLOAT"
        );
        assert_eq!(normalize_sqlparser_type(&DataType::Double(ExactNumberInfo::None)), "DOUBLE");
        assert_eq!(normalize_sqlparser_type(&DataType::DoublePrecision), "DOUBLE PRECISION");
    }

    #[test]
    fn test_normalize_sqlparser_type_decimal_family() {
        use sqlparser::ast::ExactNumberInfo;
        assert_eq!(normalize_sqlparser_type(&DataType::Decimal(ExactNumberInfo::None)), "DECIMAL");
        assert_eq!(
            normalize_sqlparser_type(&DataType::Decimal(ExactNumberInfo::PrecisionAndScale(10, 2))),
            "DECIMAL"
        );
        assert_eq!(normalize_sqlparser_type(&DataType::Numeric(ExactNumberInfo::None)), "NUMERIC");
        assert_eq!(normalize_sqlparser_type(&DataType::Dec(ExactNumberInfo::None)), "DECIMAL");
        assert_eq!(
            normalize_sqlparser_type(&DataType::BigNumeric(ExactNumberInfo::None)),
            "NUMERIC"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::BigDecimal(ExactNumberInfo::None)),
            "DECIMAL"
        );
    }

    #[test]
    fn test_normalize_sqlparser_type_string_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::Text), "TEXT");
        assert_eq!(normalize_sqlparser_type(&DataType::Varchar(None)), "VARCHAR");
        assert_eq!(normalize_sqlparser_type(&DataType::Char(None)), "CHAR");
        assert_eq!(normalize_sqlparser_type(&DataType::Character(None)), "CHAR");
        assert_eq!(normalize_sqlparser_type(&DataType::Clob(None)), "CLOB");
        assert_eq!(normalize_sqlparser_type(&DataType::Nvarchar(None)), "NVARCHAR");
    }

    /// The long SQL spellings name the same types as the short ones, so they
    /// land on the same token and fingerprint identically.
    #[test]
    fn test_normalize_sqlparser_type_long_string_spellings() {
        assert_eq!(normalize_sqlparser_type(&DataType::CharacterVarying(None)), "VARCHAR");
        assert_eq!(normalize_sqlparser_type(&DataType::CharVarying(None)), "VARCHAR");
        assert_eq!(normalize_sqlparser_type(&DataType::CharacterLargeObject(None)), "CLOB");
        assert_eq!(normalize_sqlparser_type(&DataType::CharLargeObject(Some(10))), "CLOB");
        assert_eq!(normalize_sqlparser_type(&DataType::TinyText), "TINYTEXT");
        assert_eq!(normalize_sqlparser_type(&DataType::MediumText), "MEDIUMTEXT");
        assert_eq!(normalize_sqlparser_type(&DataType::LongText), "LONGTEXT");
        assert_eq!(normalize_sqlparser_type(&DataType::String(None)), "STRING");
        assert_eq!(normalize_sqlparser_type(&DataType::FixedString(16)), "FIXEDSTRING");
    }

    #[test]
    fn test_normalize_sqlparser_type_bytes_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::Bytea), "BYTEA");
        assert_eq!(normalize_sqlparser_type(&DataType::Bytes(None)), "BYTES");
        assert_eq!(normalize_sqlparser_type(&DataType::Blob(None)), "BLOB");
        assert_eq!(normalize_sqlparser_type(&DataType::Binary(None)), "BINARY");
        assert_eq!(normalize_sqlparser_type(&DataType::Varbinary(None)), "VARBINARY");
        assert_eq!(normalize_sqlparser_type(&DataType::TinyBlob), "TINYBLOB");
        assert_eq!(normalize_sqlparser_type(&DataType::MediumBlob), "MEDIUMBLOB");
        assert_eq!(normalize_sqlparser_type(&DataType::LongBlob), "LONGBLOB");
    }

    /// A bit string drops its width the way a text type drops its length, and
    /// both spellings of the varying form agree.
    #[test]
    fn test_normalize_sqlparser_type_bit_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::Bit(None)), "BIT");
        assert_eq!(normalize_sqlparser_type(&DataType::Bit(Some(8))), "BIT");
        assert_eq!(normalize_sqlparser_type(&DataType::BitVarying(Some(8))), "VARBIT");
        assert_eq!(normalize_sqlparser_type(&DataType::VarBit(Some(8))), "VARBIT");
    }

    #[test]
    fn test_normalize_sqlparser_type_date_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::Date), "DATE");
        assert_eq!(normalize_sqlparser_type(&DataType::Date32), "DATE32");
    }

    #[test]
    fn test_normalize_sqlparser_type_time_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::Time(None, TimezoneInfo::None)), "TIME");
        assert_eq!(
            normalize_sqlparser_type(&DataType::Time(None, TimezoneInfo::WithoutTimeZone)),
            "TIME WITHOUT TIME ZONE"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Time(None, TimezoneInfo::WithTimeZone)),
            "TIME WITH TIME ZONE"
        );
        assert_eq!(normalize_sqlparser_type(&DataType::Time(None, TimezoneInfo::Tz)), "TIMETZ");
        assert_eq!(normalize_sqlparser_type(&DataType::Time(Some(6), TimezoneInfo::None)), "TIME");
    }

    #[test]
    fn test_normalize_sqlparser_type_json_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::JSON), "JSON");
        assert_eq!(normalize_sqlparser_type(&DataType::JSONB), "JSONB");
    }

    #[test]
    fn test_normalize_sqlparser_type_enum_set_family() {
        use sqlparser::ast::EnumMember;
        // The member list is dropped, mirroring how VARCHAR drops its length.
        assert_eq!(
            normalize_sqlparser_type(&DataType::Enum(vec![EnumMember::Name("a".into())], None)),
            "ENUM"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Enum(
                vec![EnumMember::Name("a".into()), EnumMember::Name("b".into())],
                Some(8)
            )),
            "ENUM"
        );
        assert_eq!(normalize_sqlparser_type(&DataType::Set(vec!["a".to_string()])), "SET");
        assert_eq!(
            normalize_sqlparser_type(&DataType::Set(vec!["a".to_string(), "b".to_string()])),
            "SET"
        );
    }

    /// A composite reports its family and drops the shape it carries, the same
    /// rule that turns an enumeration into `ENUM`.
    #[test]
    fn test_normalize_sqlparser_type_composite_family() {
        use sqlparser::ast::{StructBracketKind, StructField, UnionField};

        let field = || {
            StructField {
                field_name: Some(Ident::new("a")),
                field_type: DataType::Int(None),
                options: None,
            }
        };

        assert_eq!(
            normalize_sqlparser_type(&DataType::Struct(
                vec![field()],
                StructBracketKind::AngleBrackets
            )),
            "STRUCT"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Union(vec![UnionField {
                field_name: Ident::new("a"),
                field_type: DataType::Int(None),
            }])),
            "UNION"
        );
        assert_eq!(normalize_sqlparser_type(&DataType::Tuple(vec![field()])), "TUPLE");
        assert_eq!(normalize_sqlparser_type(&DataType::Nested(vec![])), "NESTED");
        assert_eq!(normalize_sqlparser_type(&DataType::Object(vec![])), "OBJECT");
        assert_eq!(
            normalize_sqlparser_type(&DataType::Map(
                Box::new(DataType::Text),
                Box::new(DataType::Int(None)),
                sqlparser::ast::MapBracketKind::AngleBrackets
            )),
            "MAP"
        );
    }

    /// The three shapes a `PostgreSQL` routine declaration reaches: a trigger
    /// return, a row-set return and an interval.
    #[test]
    fn test_normalize_sqlparser_type_routine_and_interval() {
        assert_eq!(normalize_sqlparser_type(&DataType::Trigger), "TRIGGER");
        assert_eq!(normalize_sqlparser_type(&DataType::Table(None)), "TABLE");
        assert_eq!(normalize_sqlparser_type(&DataType::Table(Some(vec![]))), "TABLE");
        assert_eq!(
            normalize_sqlparser_type(&DataType::NamedTable {
                name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("t"))]),
                columns: vec![],
            }),
            "TABLE"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Interval { fields: None, precision: None }),
            "INTERVAL"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Interval {
                fields: Some(sqlparser::ast::IntervalFields::YearToMonth),
                precision: Some(3),
            }),
            "INTERVAL"
        );
    }

    /// `PostgreSQL` types the normalizer used to have no answer for at all.
    #[test]
    fn test_normalize_sqlparser_type_postgres_catalog_and_geometry() {
        assert_eq!(normalize_sqlparser_type(&DataType::Regclass), "REGCLASS");
        assert_eq!(normalize_sqlparser_type(&DataType::TsVector), "TSVECTOR");
        assert_eq!(normalize_sqlparser_type(&DataType::TsQuery), "TSQUERY");

        for (kind, token) in [
            (GeometricTypeKind::Point, "POINT"),
            (GeometricTypeKind::Line, "LINE"),
            (GeometricTypeKind::LineSegment, "LSEG"),
            (GeometricTypeKind::GeometricBox, "BOX"),
            (GeometricTypeKind::GeometricPath, "PATH"),
            (GeometricTypeKind::Polygon, "POLYGON"),
            (GeometricTypeKind::Circle, "CIRCLE"),
        ] {
            assert_eq!(normalize_sqlparser_type(&DataType::GeometricType(kind)), token);
        }
    }

    /// A decoration reports the type it wraps, so a family predicate such as
    /// `ColumnLike::is_textual` still sees the text underneath.
    #[test]
    fn test_normalize_sqlparser_type_transparent_wrappers() {
        assert_eq!(
            normalize_sqlparser_type(&DataType::Nullable(Box::new(DataType::Int(None)))),
            "INT"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::LowCardinality(Box::new(DataType::Text))),
            "TEXT"
        );
        // Nesting a wrapper around an array still reaches the element type.
        assert_eq!(
            normalize_sqlparser_type(&DataType::Nullable(Box::new(DataType::Array(
                ArrayElemTypeDef::SquareBracket(Box::new(DataType::Text), None)
            )))),
            "TEXT[]"
        );
    }

    #[test]
    fn test_normalize_sqlparser_type_timestamp_extended() {
        // Existing arms (TimezoneInfo::None, WithTimeZone) get widened
        // to accept Some(precision); add WithoutTimeZone and Tz.
        assert_eq!(
            normalize_sqlparser_type(&DataType::Timestamp(Some(6), TimezoneInfo::None)),
            "TIMESTAMP"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Timestamp(None, TimezoneInfo::WithoutTimeZone)),
            "TIMESTAMP WITHOUT TIME ZONE"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Timestamp(None, TimezoneInfo::Tz)),
            "TIMESTAMPTZ"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::TimestampNtz(None)),
            "TIMESTAMP WITHOUT TIME ZONE"
        );
    }

    #[test]
    fn test_normalize_sqlparser_type_datetime() {
        assert_eq!(normalize_sqlparser_type(&DataType::Datetime(None)), "DATETIME");
        assert_eq!(normalize_sqlparser_type(&DataType::Datetime(Some(6))), "DATETIME");
        assert_eq!(normalize_sqlparser_type(&DataType::Datetime64(3, None)), "DATETIME64");
    }

    #[test]
    fn test_normalize_sqlparser_type_unsigned_int_family() {
        assert_eq!(normalize_sqlparser_type(&DataType::TinyIntUnsigned(None)), "TINYINT");
        assert_eq!(normalize_sqlparser_type(&DataType::SmallIntUnsigned(None)), "SMALLINT");
        assert_eq!(normalize_sqlparser_type(&DataType::MediumIntUnsigned(None)), "MEDIUMINT");
        assert_eq!(normalize_sqlparser_type(&DataType::IntUnsigned(None)), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::IntegerUnsigned(None)), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::BigIntUnsigned(None)), "BIGINT");
        assert_eq!(normalize_sqlparser_type(&DataType::BigIntUnsigned(Some(20))), "BIGINT");
        assert_eq!(normalize_sqlparser_type(&DataType::Int2Unsigned(None)), "INT2");
        assert_eq!(normalize_sqlparser_type(&DataType::Int4Unsigned(None)), "INT4");
        assert_eq!(normalize_sqlparser_type(&DataType::Int8Unsigned(None)), "INT8");
        assert_eq!(normalize_sqlparser_type(&DataType::UTinyInt), "TINYINT");
        assert_eq!(normalize_sqlparser_type(&DataType::USmallInt), "SMALLINT");
        assert_eq!(normalize_sqlparser_type(&DataType::UBigInt), "BIGINT");
        assert_eq!(normalize_sqlparser_type(&DataType::UHugeInt), "HUGEINT");
        assert_eq!(normalize_sqlparser_type(&DataType::HugeInt), "HUGEINT");
    }

    #[test]
    fn test_normalize_sqlparser_type_zerofill() {
        use sqlparser::ast::ExactNumberInfo;

        assert_eq!(
            normalize_sqlparser_type(&DataType::Zerofill(Box::new(DataType::Int(Some(5))))),
            "INT"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Zerofill(Box::new(DataType::BigIntUnsigned(
                Some(20)
            )))),
            "BIGINT"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Zerofill(Box::new(DataType::Decimal(
                ExactNumberInfo::PrecisionAndScale(10, 2)
            )))),
            "DECIMAL"
        );
    }

    /// A cast target that names the integer family and nothing narrower folds
    /// into it, the same way an unsigned declaration drops its sign.
    #[test]
    fn test_normalize_sqlparser_type_cast_targets() {
        assert_eq!(normalize_sqlparser_type(&DataType::Signed), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::SignedInteger), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::Unsigned), "INT");
        assert_eq!(normalize_sqlparser_type(&DataType::UnsignedInteger), "INT");
    }

    /// The fixed-width families sqlparser spells out for `ClickHouse` keep the
    /// width they name.
    #[test]
    fn test_normalize_sqlparser_type_fixed_width_families() {
        assert_eq!(normalize_sqlparser_type(&DataType::Int16), "INT16");
        assert_eq!(normalize_sqlparser_type(&DataType::Int32), "INT32");
        assert_eq!(normalize_sqlparser_type(&DataType::Int64), "INT64");
        assert_eq!(normalize_sqlparser_type(&DataType::Int128), "INT128");
        assert_eq!(normalize_sqlparser_type(&DataType::Int256), "INT256");
        assert_eq!(normalize_sqlparser_type(&DataType::UInt8), "UINT8");
        assert_eq!(normalize_sqlparser_type(&DataType::UInt16), "UINT16");
        assert_eq!(normalize_sqlparser_type(&DataType::UInt32), "UINT32");
        assert_eq!(normalize_sqlparser_type(&DataType::UInt64), "UINT64");
        assert_eq!(normalize_sqlparser_type(&DataType::UInt128), "UINT128");
        assert_eq!(normalize_sqlparser_type(&DataType::UInt256), "UINT256");
        assert_eq!(normalize_sqlparser_type(&DataType::Float4), "FLOAT4");
        assert_eq!(normalize_sqlparser_type(&DataType::Float8), "FLOAT8");
        assert_eq!(normalize_sqlparser_type(&DataType::Float32), "FLOAT32");
        assert_eq!(normalize_sqlparser_type(&DataType::Float64), "FLOAT64");
    }

    /// The remaining unsigned spellings of the inexact families.
    #[test]
    fn test_normalize_sqlparser_type_unsigned_inexact_family() {
        use sqlparser::ast::ExactNumberInfo;
        assert_eq!(normalize_sqlparser_type(&DataType::RealUnsigned), "REAL");
        assert_eq!(
            normalize_sqlparser_type(&DataType::FloatUnsigned(ExactNumberInfo::None)),
            "FLOAT"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::DoubleUnsigned(ExactNumberInfo::None)),
            "DOUBLE"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::DoublePrecisionUnsigned),
            "DOUBLE PRECISION"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::DecimalUnsigned(ExactNumberInfo::None)),
            "DECIMAL"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::DecUnsigned(ExactNumberInfo::None)),
            "DECIMAL"
        );
    }

    /// A declaration that named no type reports that rather than an empty
    /// string, which would collapse into a neighbouring column's fingerprint.
    #[test]
    fn test_normalize_sqlparser_type_unspecified_and_any() {
        assert_eq!(normalize_sqlparser_type(&DataType::Unspecified), "UNSPECIFIED");
        assert_eq!(normalize_sqlparser_type(&DataType::AnyType), "ANY TYPE");
    }

    /// Every spelling of the same array type folds to the same token, and a
    /// fixed token stays a borrow so the common case allocates nothing.
    #[test]
    fn test_normalize_sqlparser_type_array() {
        let element = || Box::new(DataType::Int(None));

        assert_eq!(
            normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::SquareBracket(
                element(),
                None
            ))),
            "INT[]"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::SquareBracket(
                element(),
                Some(3)
            ))),
            "INT[]"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::AngleBracket(element()))),
            "INT[]"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::Parenthesis(element()))),
            "INT[]"
        );
        assert_eq!(
            normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::ParenthesisNotNull(
                element()
            ))),
            "INT[]"
        );
        assert_eq!(normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::None)), "ARRAY");
        assert_eq!(
            normalize_sqlparser_type(&DataType::Array(ArrayElemTypeDef::Qualified(
                element(),
                None
            ))),
            "INT[]"
        );

        assert!(matches!(normalize_sqlparser_type(&DataType::Int(None)), Cow::Borrowed("INT")));
    }

    /// A nested array recurses through the element type, so the depth of the
    /// declaration survives normalization.
    #[test]
    fn test_normalize_sqlparser_type_nested_array() {
        let inner = DataType::Array(ArrayElemTypeDef::SquareBracket(
            Box::new(DataType::Timestamp(None, TimezoneInfo::Tz)),
            None,
        ));
        let outer = DataType::Array(ArrayElemTypeDef::SquareBracket(Box::new(inner), Some(2)));

        assert_eq!(normalize_sqlparser_type(&outer), "TIMESTAMPTZ[][]");
    }
}
