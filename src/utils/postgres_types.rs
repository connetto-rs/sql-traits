//! PostgreSQL's type identities as a call or a declaration spells them.
//!
//! The type table mirrors `pg_type` of PostgreSQL 18. A type name is read with
//! sqlparser's PostgreSQL dialect, so every spelling the server accepts for a
//! builtin type (`INT`, `int4`, `integer`, `INT ARRAY`) denotes the same entry.
//! A name the table does not hold, an extension's or a schema's own type, is
//! kept by its stored name, which the coercion rules look up in the database.

use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::{
    ast::{ArrayElemTypeDef, DataType, ExactNumberInfo, ObjectName, TimezoneInfo},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use crate::{
    structs::TargetName,
    utils::{identifier_resolution::stored_identifier, object_name::PG_CATALOG},
};

/// One `pg_type` row, with the type's name, `typcategory` and `typispreferred`.
struct TypeRow(&'static str, u8, bool);

/// The non-composite types of PostgreSQL 18's `pg_type`, sorted by name.
static TYPES: &[TypeRow] = &[
    TypeRow("_record", b'P', false),
    TypeRow("aclitem", b'U', false),
    TypeRow("any", b'P', false),
    TypeRow("anyarray", b'P', false),
    TypeRow("anycompatible", b'P', false),
    TypeRow("anycompatiblearray", b'P', false),
    TypeRow("anycompatiblemultirange", b'P', false),
    TypeRow("anycompatiblenonarray", b'P', false),
    TypeRow("anycompatiblerange", b'P', false),
    TypeRow("anyelement", b'P', false),
    TypeRow("anyenum", b'P', false),
    TypeRow("anymultirange", b'P', false),
    TypeRow("anynonarray", b'P', false),
    TypeRow("anyrange", b'P', false),
    TypeRow("bit", b'V', false),
    TypeRow("bool", b'B', true),
    TypeRow("box", b'G', false),
    TypeRow("bpchar", b'S', false),
    TypeRow("bytea", b'U', false),
    TypeRow("char", b'Z', false),
    TypeRow("cid", b'U', false),
    TypeRow("cidr", b'I', false),
    TypeRow("circle", b'G', false),
    TypeRow("cstring", b'P', false),
    TypeRow("date", b'D', false),
    TypeRow("datemultirange", b'R', false),
    TypeRow("daterange", b'R', false),
    TypeRow("event_trigger", b'P', false),
    TypeRow("fdw_handler", b'P', false),
    TypeRow("float4", b'N', false),
    TypeRow("float8", b'N', true),
    TypeRow("gtsvector", b'U', false),
    TypeRow("index_am_handler", b'P', false),
    TypeRow("inet", b'I', true),
    TypeRow("int2", b'N', false),
    TypeRow("int2vector", b'A', false),
    TypeRow("int4", b'N', false),
    TypeRow("int4multirange", b'R', false),
    TypeRow("int4range", b'R', false),
    TypeRow("int8", b'N', false),
    TypeRow("int8multirange", b'R', false),
    TypeRow("int8range", b'R', false),
    TypeRow("internal", b'P', false),
    TypeRow("interval", b'T', true),
    TypeRow("json", b'U', false),
    TypeRow("jsonb", b'U', false),
    TypeRow("jsonpath", b'U', false),
    TypeRow("language_handler", b'P', false),
    TypeRow("line", b'G', false),
    TypeRow("lseg", b'G', false),
    TypeRow("macaddr", b'U', false),
    TypeRow("macaddr8", b'U', false),
    TypeRow("money", b'N', false),
    TypeRow("name", b'S', false),
    TypeRow("numeric", b'N', false),
    TypeRow("nummultirange", b'R', false),
    TypeRow("numrange", b'R', false),
    TypeRow("oid", b'N', true),
    TypeRow("oidvector", b'A', false),
    TypeRow("path", b'G', false),
    TypeRow("pg_brin_bloom_summary", b'Z', false),
    TypeRow("pg_brin_minmax_multi_summary", b'Z', false),
    TypeRow("pg_ddl_command", b'P', false),
    TypeRow("pg_dependencies", b'Z', false),
    TypeRow("pg_lsn", b'U', false),
    TypeRow("pg_mcv_list", b'Z', false),
    TypeRow("pg_ndistinct", b'Z', false),
    TypeRow("pg_node_tree", b'Z', false),
    TypeRow("pg_snapshot", b'U', false),
    TypeRow("point", b'G', false),
    TypeRow("polygon", b'G', false),
    TypeRow("record", b'P', false),
    TypeRow("refcursor", b'U', false),
    TypeRow("regclass", b'N', false),
    TypeRow("regcollation", b'N', false),
    TypeRow("regconfig", b'N', false),
    TypeRow("regdictionary", b'N', false),
    TypeRow("regnamespace", b'N', false),
    TypeRow("regoper", b'N', false),
    TypeRow("regoperator", b'N', false),
    TypeRow("regproc", b'N', false),
    TypeRow("regprocedure", b'N', false),
    TypeRow("regrole", b'N', false),
    TypeRow("regtype", b'N', false),
    TypeRow("table_am_handler", b'P', false),
    TypeRow("text", b'S', true),
    TypeRow("tid", b'U', false),
    TypeRow("time", b'D', false),
    TypeRow("timestamp", b'D', false),
    TypeRow("timestamptz", b'D', true),
    TypeRow("timetz", b'D', false),
    TypeRow("trigger", b'P', false),
    TypeRow("tsm_handler", b'P', false),
    TypeRow("tsmultirange", b'R', false),
    TypeRow("tsquery", b'U', false),
    TypeRow("tsrange", b'R', false),
    TypeRow("tstzmultirange", b'R', false),
    TypeRow("tstzrange", b'R', false),
    TypeRow("tsvector", b'U', false),
    TypeRow("txid_snapshot", b'U', false),
    TypeRow("unknown", b'X', false),
    TypeRow("uuid", b'U', false),
    TypeRow("varbit", b'V', true),
    TypeRow("varchar", b'S', false),
    TypeRow("void", b'P', false),
    TypeRow("xid", b'U', false),
    TypeRow("xid8", b'U', false),
    TypeRow("xml", b'U', false),
];

/// The types of PostgreSQL 18's `pg_type` whose `typsend` is unset, sorted.
static WITHOUT_SEND: &[&str] = &[
    "aclitem",
    "any",
    "anycompatible",
    "anycompatiblemultirange",
    "anycompatiblenonarray",
    "anycompatiblerange",
    "anyelement",
    "anyenum",
    "anymultirange",
    "anynonarray",
    "anyrange",
    "event_trigger",
    "fdw_handler",
    "gtsvector",
    "index_am_handler",
    "internal",
    "language_handler",
    "table_am_handler",
    "trigger",
    "tsm_handler",
];

/// A type as PostgreSQL identifies it, without the length, precision or
/// dimensions a declaration may add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PgType {
    /// A type of the PostgreSQL 18 catalog.
    Catalog(&'static str),
    /// The array type over an element type. PostgreSQL has one array type per
    /// element type whatever the dimensions.
    Array(Box<PgType>),
    /// A type of one or two name parts the catalog tables do not hold, by
    /// its stored name, compared exactly.
    Named(TargetName<'static>),
    /// A type sqlparser names that no stored name spells.
    Other(String),
}

/// The polymorphic pseudo-types a function may declare in place of a
/// concrete type, which bind to the types of the arguments they take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Polymorphic {
    /// `anyelement`, `anynonarray` and `anyenum`.
    Element { nonarray: bool },
    /// `anyarray`.
    Array,
    /// `anycompatible` and `anycompatiblenonarray`.
    Compatible { nonarray: bool },
    /// `anycompatiblearray`.
    CompatibleArray,
    /// `anyrange`, `anymultirange` and their `anycompatible` forms, which
    /// need range types this model does not hold.
    Range,
}

impl PgType {
    /// The unknown type of an untyped literal such as `NULL` or `'text'`.
    pub(crate) const UNKNOWN: Self = Self::Catalog("unknown");
    /// `text`, what PostgreSQL makes of arguments that are all unknown.
    pub(crate) const TEXT: Self = Self::Catalog("text");
    /// `record`, the row type of no table in particular.
    pub(crate) const RECORD: Self = Self::Catalog("record");
    /// `record[]`, which `pg_type` holds as the pseudo-type `_record`.
    pub(crate) const RECORD_ARRAY: Self = Self::Catalog("_record");

    /// The type `name` denotes, or [`None`] for a name that does not parse as
    /// a type.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        let data_type = parse_data_type(name)?;
        Some(Self::of(&data_type))
    }

    /// The type a parsed declaration denotes.
    pub(crate) fn of(data_type: &DataType) -> Self {
        let catalog = |name: &'static str| Self::Catalog(name);
        match data_type {
            DataType::Array(
                ArrayElemTypeDef::AngleBracket(element)
                | ArrayElemTypeDef::SquareBracket(element, _)
                | ArrayElemTypeDef::Parenthesis(element)
                | ArrayElemTypeDef::ParenthesisNotNull(element)
                | ArrayElemTypeDef::Qualified(element, _),
            ) => Self::of(element).array(),
            DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => catalog("int4"),
            DataType::SmallInt(_) | DataType::Int2(_) => catalog("int2"),
            DataType::BigInt(_) | DataType::Int8(_) => catalog("int8"),
            DataType::Real | DataType::Float4 => catalog("float4"),
            DataType::Float(ExactNumberInfo::Precision(precision)) if *precision <= 24 => {
                catalog("float4")
            }
            DataType::DoublePrecision | DataType::Float8 | DataType::Float(_) => catalog("float8"),
            DataType::Numeric(_) | DataType::Decimal(_) | DataType::Dec(_) => catalog("numeric"),
            DataType::Bool | DataType::Boolean => catalog("bool"),
            DataType::Text => catalog("text"),
            DataType::Varchar(_)
            | DataType::CharacterVarying(_)
            | DataType::CharVarying(_)
            | DataType::NationalCharacterVarying(_)
            | DataType::NationalCharVarying(_)
            | DataType::NcharVarying(_) => catalog("varchar"),
            DataType::Char(_)
            | DataType::Character(_)
            | DataType::NationalCharacter(_)
            | DataType::NationalChar(_)
            | DataType::Nchar(_) => catalog("bpchar"),
            DataType::Bytea => catalog("bytea"),
            DataType::Uuid => catalog("uuid"),
            DataType::Date => catalog("date"),
            DataType::Time(_, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
                catalog("time")
            }
            DataType::Time(_, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz) => catalog("timetz"),
            DataType::Timestamp(_, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone) => {
                catalog("timestamp")
            }
            DataType::Timestamp(_, TimezoneInfo::WithTimeZone | TimezoneInfo::Tz) => {
                catalog("timestamptz")
            }
            DataType::Interval { .. } => catalog("interval"),
            DataType::JSON => catalog("json"),
            DataType::JSONB => catalog("jsonb"),
            DataType::Bit(_) => catalog("bit"),
            DataType::BitVarying(_) | DataType::VarBit(_) => catalog("varbit"),
            DataType::Regclass => catalog("regclass"),
            DataType::TsVector => catalog("tsvector"),
            DataType::TsQuery => catalog("tsquery"),
            DataType::Trigger => catalog("trigger"),
            DataType::GeometricType(kind) => {
                Self::Catalog(match kind {
                    sqlparser::ast::GeometricTypeKind::Point => "point",
                    sqlparser::ast::GeometricTypeKind::Line => "line",
                    sqlparser::ast::GeometricTypeKind::LineSegment => "lseg",
                    sqlparser::ast::GeometricTypeKind::GeometricBox => "box",
                    sqlparser::ast::GeometricTypeKind::GeometricPath => "path",
                    sqlparser::ast::GeometricTypeKind::Polygon => "polygon",
                    sqlparser::ast::GeometricTypeKind::Circle => "circle",
                })
            }
            DataType::Custom(name, _) => Self::named(name),
            other => Self::Other(crate::utils::normalize_sqlparser_type(other).into_owned()),
        }
    }

    /// The type a written name denotes, the catalog's when it holds the name
    /// in `pg_catalog`.
    fn named(name: &ObjectName) -> Self {
        let stored: Option<Vec<Cow<'_, str>>> = name
            .0
            .iter()
            .map(|part| {
                part.as_ident()
                    .map(|ident| stored_identifier(&ident.value, ident.quote_style.is_some()))
            })
            .collect();
        match stored.as_deref() {
            Some([name]) => Self::stored(None, name),
            Some([schema, name]) => Self::stored(Some(schema), name),
            _ => Self::Other(name.to_string()),
        }
    }

    /// The type a database names `name`.
    pub(crate) fn from_target(name: &TargetName<'_>) -> Self {
        let schema = name.schema().map(|schema| stored_identifier(schema, name.schema_is_quoted()));
        Self::stored(schema.as_deref(), &stored_identifier(name.name(), name.name_is_quoted()))
    }

    /// The type the stored `name` denotes in `schema`, or on the search path.
    fn stored(schema: Option<&str>, name: &str) -> Self {
        if schema.is_none_or(|schema| schema == PG_CATALOG) {
            if let Some(row) = catalog_row(serial_type(name).unwrap_or(name)) {
                return Self::Catalog(row.0);
            }
            // `_int4` is the catalog's own name for `int4[]`.
            if let Some(row) = name.strip_prefix('_').and_then(catalog_row) {
                return Self::Catalog(row.0).array();
            }
        }
        let name = TargetName::new(name, true);
        let name = match schema {
            Some(schema) => name.with_schema(schema, true),
            None => name,
        };
        Self::Named(name.into_owned())
    }

    /// The array type over this one. An array's array is the array itself,
    /// since PostgreSQL gives dimensions no type of their own.
    pub(crate) fn array(self) -> Self {
        match self {
            Self::Array(_) => self,
            element if element == Self::RECORD => Self::RECORD_ARRAY,
            element => Self::Array(Box::new(element)),
        }
    }

    /// The element type of an array type.
    pub(crate) fn element(&self) -> Option<&Self> {
        match self {
            Self::Array(element) => Some(element),
            _ if *self == Self::RECORD_ARRAY => Some(&RECORD),
            Self::Catalog(_) | Self::Named(_) | Self::Other(_) => None,
        }
    }

    /// The polymorphic pseudo-type this is, when a declaration uses it to
    /// stand for the type of an argument.
    pub(crate) fn polymorphic(&self) -> Option<Polymorphic> {
        let Self::Catalog(name) = self else { return None };
        Some(match *name {
            "anyelement" | "anyenum" => Polymorphic::Element { nonarray: false },
            "anynonarray" => Polymorphic::Element { nonarray: true },
            "anyarray" => Polymorphic::Array,
            "anycompatible" => Polymorphic::Compatible { nonarray: false },
            "anycompatiblenonarray" => Polymorphic::Compatible { nonarray: true },
            "anycompatiblearray" => Polymorphic::CompatibleArray,
            "anyrange" | "anymultirange" | "anycompatiblerange" | "anycompatiblemultirange" => {
                Polymorphic::Range
            }
            _ => return None,
        })
    }

    /// Whether this is `"any"`, which takes a value of any type as it is and
    /// binds nothing.
    pub(crate) fn is_any(&self) -> bool {
        *self == Self::Catalog("any")
    }

    /// Whether this is the unknown type of an untyped literal.
    pub(crate) fn is_unknown(&self) -> bool {
        *self == Self::UNKNOWN
    }

    /// A spelling that reads back as this type with the PostgreSQL dialect.
    pub(crate) fn spelling(&self) -> Cow<'static, str> {
        match self {
            // A bare name sqlparser reads as another type, `char` among
            // them, is qualified.
            Self::Catalog(name) if Self::parse(name).as_ref() == Some(self) => Cow::Borrowed(name),
            Self::Catalog(name) => Cow::Owned(format!("{PG_CATALOG}.{name}")),
            Self::Array(element) => Cow::Owned(format!("{}[]", element.spelling())),
            Self::Named(name) => Cow::Owned(name.to_string()),
            Self::Other(name) => Cow::Owned(name.clone()),
        }
    }

    /// Whether a value of this type is an array, which `anynonarray` refuses.
    pub(crate) fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }

    /// Whether the type has a binary send function of its own, or `None` for a
    /// type the catalog tables do not hold.
    pub(crate) fn send_function(&self) -> Option<bool> {
        match self {
            Self::Catalog(name) => {
                catalog_row(name).map(|_| WITHOUT_SEND.binary_search(name).is_err())
            }
            Self::Array(_) => Some(true),
            Self::Named(_) | Self::Other(_) => None,
        }
    }
}

/// The element of `record[]`.
static RECORD: PgType = PgType::RECORD;

/// The catalog row named `name`.
fn catalog_row(name: &str) -> Option<&'static TypeRow> {
    TYPES.binary_search_by(|row| row.0.cmp(name)).ok().and_then(|index| TYPES.get(index))
}

/// The stored name, `typcategory` and `typispreferred` of the PostgreSQL 18
/// type stored as `name` in `pg_catalog`.
pub(crate) fn built_in_type(name: &str) -> Option<(&'static str, u8, bool)> {
    catalog_row(name).map(|row| (row.0, row.1, row.2))
}

/// The integer type a `serial` spelling declares.
fn serial_type(name: &str) -> Option<&'static str> {
    match name {
        "smallserial" | "serial2" => Some("int2"),
        "serial" | "serial4" => Some("int4"),
        "bigserial" | "serial8" => Some("int8"),
        _ => None,
    }
}

/// Whether an unqualified type name, as PostgreSQL stores it, reaches a type
/// of `pg_catalog`, which an unqualified name finds before any schema on the
/// path that does not list `pg_catalog` itself. A `serial` spelling counts,
/// since `CREATE TABLE` reads it before looking for a type of that name.
pub(crate) fn names_catalog_type(name: &str) -> bool {
    catalog_row(serial_type(name).unwrap_or(name)).is_some()
        || name.strip_prefix('_').and_then(catalog_row).is_some()
}

/// Reads `name` as a type with the PostgreSQL dialect, refusing trailing text.
pub(crate) fn parse_data_type(name: &str) -> Option<DataType> {
    let mut parser = Parser::new(&PostgreSqlDialect {}).try_with_sql(name).ok()?;
    let data_type = parser.parse_data_type().ok()?;
    matches!(parser.peek_token_ref().token, sqlparser::tokenizer::Token::EOF).then_some(data_type)
}

#[cfg(test)]
mod tests {
    use sqlparser::ast::{
        DataType, GeometricTypeKind, Ident, ObjectName, ObjectNamePart, ObjectNamePartFunction,
    };

    use super::PgType;

    /// Every spelling PostgreSQL accepts for a catalog type denotes its row.
    #[test]
    fn spellings_denote_their_catalog_types() {
        for (spelling, catalog) in [
            ("FLOAT(10)", "float4"),
            ("FLOAT(30)", "float8"),
            ("BOOL", "bool"),
            ("CHAR(3)", "bpchar"),
            ("NATIONAL CHARACTER(3)", "bpchar"),
            ("NATIONAL CHAR", "bpchar"),
            ("NATIONAL CHARACTER VARYING(3)", "varchar"),
            ("NATIONAL CHAR VARYING", "varchar"),
            ("NCHAR(3)", "bpchar"),
            ("NCHAR VARYING(3)", "varchar"),
            ("TIME", "time"),
            ("TIME WITH TIME ZONE", "timetz"),
            ("TIMETZ", "timetz"),
            ("TIMESTAMP(3)", "timestamp"),
            ("JSON", "json"),
            ("JSONB", "jsonb"),
            ("BIT(3)", "bit"),
            ("BIT VARYING(3)", "varbit"),
            ("REGCLASS", "regclass"),
            ("TSVECTOR", "tsvector"),
            ("TSQUERY", "tsquery"),
            ("TRIGGER", "trigger"),
            ("SMALLSERIAL", "int2"),
            ("SERIAL", "int4"),
            ("BIGSERIAL", "int8"),
            ("pg_catalog.int4", "int4"),
            ("anycompatiblenonarray", "anycompatiblenonarray"),
            ("anyrange", "anyrange"),
        ] {
            assert_eq!(PgType::parse(spelling), Some(PgType::Catalog(catalog)), "{spelling}");
        }
        let int4_array = Some(PgType::Catalog("int4").array());
        assert_eq!(PgType::parse("_int4"), int4_array);
        assert_eq!(PgType::parse("INT[][]"), int4_array);
        assert_eq!(PgType::parse("TINYINT"), Some(PgType::Other("TINYINT".to_owned())));
        assert_eq!(PgType::parse("not a type ("), None);
    }

    /// The geometric types sqlparser names by kind are the catalog's.
    #[test]
    fn geometric_kinds_denote_their_catalog_types() {
        for (kind, catalog) in [
            (GeometricTypeKind::Point, "point"),
            (GeometricTypeKind::Line, "line"),
            (GeometricTypeKind::LineSegment, "lseg"),
            (GeometricTypeKind::GeometricBox, "box"),
            (GeometricTypeKind::GeometricPath, "path"),
            (GeometricTypeKind::Polygon, "polygon"),
            (GeometricTypeKind::Circle, "circle"),
        ] {
            assert_eq!(PgType::of(&DataType::GeometricType(kind)), PgType::Catalog(catalog));
        }
    }

    /// A name part built at run time names a type of its own.
    #[test]
    fn a_run_time_name_part_is_a_type_of_its_own() {
        let name = ObjectName(vec![ObjectNamePart::Function(ObjectNamePartFunction {
            name: Ident::new("identifier"),
            args: vec![],
        })]);
        assert!(matches!(PgType::of(&DataType::Custom(name, vec![])), PgType::Other(_)));
    }

    /// The send facts agree with PostgreSQL 18's `pg_type.typsend`.
    #[test]
    fn catalog_types_answer_their_send_function() {
        assert!(super::WITHOUT_SEND.is_sorted());
        for (spelling, send) in [
            ("aclitem", Some(false)),
            ("gtsvector", Some(false)),
            ("pg_catalog.aclitem", Some(false)),
            ("_aclitem", Some(true)),
            ("aclitem[]", Some(true)),
            ("INT", Some(true)),
            ("int4range", Some(true)),
            ("LINE", Some(true)),
            ("box2d", None),
            ("TINYINT", None),
        ] {
            let pg_type = PgType::parse(spelling).expect("parses");
            assert_eq!(pg_type.send_function(), send, "{spelling}");
        }
    }

    /// Every type's spelling reads back as the type, a catalog name sqlparser
    /// reads as another type included.
    #[test]
    fn spellings_read_back_as_their_types() {
        for spelling in ["int4", "\"char\"", "_int4", "app.\"Mood\"", "app.mood[]", "TINYINT"] {
            let pg_type = PgType::parse(spelling).expect("parses");
            assert_eq!(PgType::parse(&pg_type.spelling()).as_ref(), Some(&pg_type), "{spelling}");
        }
        assert_eq!(PgType::parse("\"char\"").expect("parses").spelling(), "pg_catalog.char");
    }

    /// `record[]` is the catalog's own `_record`, whose element is `record`.
    #[test]
    fn record_arrays_are_the_catalog_record_array() {
        let record_array = PgType::parse("record[]").expect("parses");
        assert_eq!(Some(&record_array), PgType::parse("_record").as_ref());
        assert_eq!(record_array.element(), PgType::parse("record").as_ref());
    }
}
