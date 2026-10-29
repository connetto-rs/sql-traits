//! PostgreSQL's type categories, preferred types and implicit casts, and the
//! common-type rule built on them.
//!
//! The tables mirror `pg_type` and `pg_cast` of PostgreSQL 18. A type name is
//! read with sqlparser's PostgreSQL dialect, so every spelling the server
//! accepts for a builtin type (`INT`, `int4`, `integer`, `INT ARRAY`) denotes
//! the same entry. A name the tables do not hold, an extension's or a schema's
//! own type, is a type of its own in the user-defined category, equal only to
//! itself and reached by no implicit cast. Domains are not modelled, so a
//! domain is such a type rather than its base type.

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};

use sqlparser::{
    ast::{ArrayElemTypeDef, DataType, ExactNumberInfo, ObjectName, TimezoneInfo},
    dialect::PostgreSqlDialect,
    parser::Parser,
};

use crate::utils::identifier_resolution::normalize_identifier;

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

/// The casts PostgreSQL 18's `pg_cast` marks implicit, as source and target,
/// sorted. A type's cast to itself, which only applies a length, is left out.
static IMPLICIT_CASTS: &[(&str, &str)] = &[
    ("bit", "varbit"),
    ("bpchar", "name"),
    ("bpchar", "text"),
    ("bpchar", "varchar"),
    ("char", "text"),
    ("cidr", "inet"),
    ("date", "timestamp"),
    ("date", "timestamptz"),
    ("float4", "float8"),
    ("int2", "float4"),
    ("int2", "float8"),
    ("int2", "int4"),
    ("int2", "int8"),
    ("int2", "numeric"),
    ("int2", "oid"),
    ("int2", "regclass"),
    ("int2", "regcollation"),
    ("int2", "regconfig"),
    ("int2", "regdictionary"),
    ("int2", "regnamespace"),
    ("int2", "regoper"),
    ("int2", "regoperator"),
    ("int2", "regproc"),
    ("int2", "regprocedure"),
    ("int2", "regrole"),
    ("int2", "regtype"),
    ("int4", "float4"),
    ("int4", "float8"),
    ("int4", "int8"),
    ("int4", "numeric"),
    ("int4", "oid"),
    ("int4", "regclass"),
    ("int4", "regcollation"),
    ("int4", "regconfig"),
    ("int4", "regdictionary"),
    ("int4", "regnamespace"),
    ("int4", "regoper"),
    ("int4", "regoperator"),
    ("int4", "regproc"),
    ("int4", "regprocedure"),
    ("int4", "regrole"),
    ("int4", "regtype"),
    ("int8", "float4"),
    ("int8", "float8"),
    ("int8", "numeric"),
    ("int8", "oid"),
    ("int8", "regclass"),
    ("int8", "regcollation"),
    ("int8", "regconfig"),
    ("int8", "regdictionary"),
    ("int8", "regnamespace"),
    ("int8", "regoper"),
    ("int8", "regoperator"),
    ("int8", "regproc"),
    ("int8", "regprocedure"),
    ("int8", "regrole"),
    ("int8", "regtype"),
    ("macaddr", "macaddr8"),
    ("macaddr8", "macaddr"),
    ("name", "text"),
    ("numeric", "float4"),
    ("numeric", "float8"),
    ("oid", "regclass"),
    ("oid", "regcollation"),
    ("oid", "regconfig"),
    ("oid", "regdictionary"),
    ("oid", "regnamespace"),
    ("oid", "regoper"),
    ("oid", "regoperator"),
    ("oid", "regproc"),
    ("oid", "regprocedure"),
    ("oid", "regrole"),
    ("oid", "regtype"),
    ("pg_dependencies", "bytea"),
    ("pg_dependencies", "text"),
    ("pg_mcv_list", "bytea"),
    ("pg_mcv_list", "text"),
    ("pg_ndistinct", "bytea"),
    ("pg_ndistinct", "text"),
    ("pg_node_tree", "text"),
    ("regclass", "oid"),
    ("regcollation", "oid"),
    ("regconfig", "oid"),
    ("regdictionary", "oid"),
    ("regnamespace", "oid"),
    ("regoper", "oid"),
    ("regoper", "regoperator"),
    ("regoperator", "oid"),
    ("regoperator", "regoper"),
    ("regproc", "oid"),
    ("regproc", "regprocedure"),
    ("regprocedure", "oid"),
    ("regprocedure", "regproc"),
    ("regrole", "oid"),
    ("regtype", "oid"),
    ("text", "bpchar"),
    ("text", "name"),
    ("text", "regclass"),
    ("text", "varchar"),
    ("time", "interval"),
    ("time", "timetz"),
    ("timestamp", "timestamptz"),
    ("varbit", "bit"),
    ("varchar", "bpchar"),
    ("varchar", "name"),
    ("varchar", "regclass"),
    ("varchar", "text"),
];

/// `typcategory` of an array type.
const ARRAY: u8 = b'A';
/// `typcategory` of the string types.
const STRING: u8 = b'S';
/// `typcategory` of a user-defined type.
const USER_DEFINED: u8 = b'U';

/// A type as PostgreSQL identifies it, without the length, precision or
/// dimensions a declaration may add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PgType {
    /// A type of the PostgreSQL 18 catalog.
    Catalog(&'static str),
    /// The array type over an element type. PostgreSQL has one array type per
    /// element type whatever the dimensions.
    Array(Box<PgType>),
    /// A type the catalog tables do not hold, by its folded qualified name.
    Other(String),
}

/// The pseudo-types a function may declare in place of a concrete type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Polymorphic {
    /// `"any"`, which takes a value of any type and binds nothing.
    Any,
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
            DataType::Varchar(_) | DataType::CharacterVarying(_) | DataType::CharVarying(_) => {
                catalog("varchar")
            }
            DataType::Char(_) | DataType::Character(_) => catalog("bpchar"),
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
        let parts: Vec<Cow<'_, str>> = name
            .0
            .iter()
            .map(|part| {
                part.as_ident().map_or_else(
                    || Cow::Owned(part.to_string()),
                    |ident| normalize_identifier(&ident.value, ident.quote_style.is_some()),
                )
            })
            .collect();
        let terminal = match parts.as_slice() {
            [only] => Some(only.as_ref()),
            [schema, terminal] if schema == "pg_catalog" => Some(terminal.as_ref()),
            _ => None,
        };
        if let Some(terminal) = terminal {
            if let Some(row) = catalog_row(serial_type(terminal).unwrap_or(terminal)) {
                return Self::Catalog(row.0);
            }
            // `_int4` is the catalog's own name for `int4[]`.
            if let Some(row) = terminal.strip_prefix('_').and_then(catalog_row) {
                return Self::Catalog(row.0).array();
            }
        }
        Self::Other(parts.join("."))
    }

    /// The array type over this one. An array's array is the array itself,
    /// since PostgreSQL gives dimensions no type of their own.
    pub(crate) fn array(self) -> Self {
        match self {
            Self::Array(_) => self,
            element => Self::Array(Box::new(element)),
        }
    }

    /// The element type of an array type.
    pub(crate) fn element(&self) -> Option<&Self> {
        match self {
            Self::Array(element) => Some(element),
            Self::Catalog(_) | Self::Other(_) => None,
        }
    }

    /// The pseudo-type this is, when a declaration uses it to stand for the
    /// type of an argument.
    pub(crate) fn polymorphic(&self) -> Option<Polymorphic> {
        let Self::Catalog(name) = self else { return None };
        Some(match *name {
            "any" => Polymorphic::Any,
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

    /// Whether this is the unknown type of an untyped literal.
    pub(crate) fn is_unknown(&self) -> bool {
        *self == Self::UNKNOWN
    }

    /// `typcategory`.
    pub(crate) fn category(&self) -> u8 {
        match self {
            Self::Catalog(name) => catalog_row(name).map_or(USER_DEFINED, |row| row.1),
            Self::Array(_) => ARRAY,
            Self::Other(_) => USER_DEFINED,
        }
    }

    /// `typispreferred`.
    pub(crate) fn is_preferred(&self) -> bool {
        match self {
            Self::Catalog(name) => catalog_row(name).is_some_and(|row| row.2),
            Self::Array(_) | Self::Other(_) => false,
        }
    }

    /// Whether the category is that of the string types, which an untyped
    /// literal leans towards.
    pub(crate) fn is_string_category(&self) -> bool {
        self.category() == STRING
    }

    /// Whether PostgreSQL converts a value of this type to `target` without
    /// being asked, mirroring `can_coerce_type` under `COERCION_IMPLICIT` for
    /// a declared, non-polymorphic target.
    pub(crate) fn coerces_implicitly_to(&self, target: &Self) -> bool {
        if self == target
            || self.is_unknown()
            || matches!(target.polymorphic(), Some(Polymorphic::Any))
        {
            return true;
        }
        match (self, target) {
            (Self::Catalog(source), Self::Catalog(target)) => {
                IMPLICIT_CASTS.binary_search(&(*source, *target)).is_ok()
            }
            (Self::Array(source), Self::Array(target)) => source.coerces_implicitly_to(target),
            _ => false,
        }
    }

    /// Whether a value of this type is an array, which `anynonarray` refuses.
    pub(crate) fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
    }
}

/// The catalog row named `name`.
fn catalog_row(name: &str) -> Option<&'static TypeRow> {
    TYPES.binary_search_by(|row| row.0.cmp(name)).ok().and_then(|index| TYPES.get(index))
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

/// Reads `name` as a type with the PostgreSQL dialect, refusing trailing text.
pub(crate) fn parse_data_type(name: &str) -> Option<DataType> {
    let mut parser = Parser::new(&PostgreSqlDialect {}).try_with_sql(name).ok()?;
    let data_type = parser.parse_data_type().ok()?;
    matches!(parser.peek_token_ref().token, sqlparser::tokenizer::Token::EOF).then_some(data_type)
}

/// The type PostgreSQL's `select_common_type` and `verify_common_type` settle
/// on for `items`, or [`None`] when they have no common type.
pub(crate) fn select_common_type<T>(
    items: &[T],
    type_of: impl Fn(&T) -> &PgType,
) -> Option<CommonType> {
    let mut chosen: Option<(usize, &PgType)> = None;
    for (index, candidate) in items.iter().map(&type_of).enumerate() {
        if candidate.is_unknown() {
            continue;
        }
        let Some((_, current)) = chosen else {
            chosen = Some((index, candidate));
            continue;
        };
        if candidate == current {
            continue;
        }
        if candidate.category() != current.category() {
            return None;
        }
        if !current.is_preferred()
            && current.coerces_implicitly_to(candidate)
            && !candidate.coerces_implicitly_to(current)
        {
            chosen = Some((index, candidate));
        }
    }
    let Some((position, common)) = chosen else { return Some(CommonType::AllUnknown) };
    items
        .iter()
        .map(type_of)
        .all(|candidate| candidate.coerces_implicitly_to(common))
        .then_some(CommonType::At(position))
}

/// The type [`select_common_type`] settles on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommonType {
    /// The type at this position of the input.
    At(usize),
    /// Every input was an untyped literal, so the type is `text`.
    AllUnknown,
}

/// Answers the type PostgreSQL gives a set of values that must share one, as
/// in `COALESCE`, `CASE`, `ARRAY[...]`, `VALUES` and `UNION`, by
/// `select_common_type`.
///
/// Each entry is a type name, `unknown` for an untyped literal such as `NULL`
/// or `'abc'`, or [`None`] for a value whose type the caller does not know.
/// The answer is the winning name as given, `TEXT` when every value is an
/// untyped literal, and [`None`] when a value's type is not known or no
/// common type exists, where PostgreSQL refuses the statement.
///
/// # Example
///
/// ```rust
/// use sql_traits::utils::common_type_name;
///
/// assert_eq!(common_type_name(&[Some("BYTEA"), Some("unknown")]).as_deref(), Some("BYTEA"));
/// assert_eq!(common_type_name(&[Some("INT"), Some("BIGINT")]).as_deref(), Some("BIGINT"));
/// // `double precision` is the preferred numeric type.
/// assert_eq!(common_type_name(&[Some("FLOAT8"), Some("NUMERIC")]).as_deref(), Some("FLOAT8"));
/// assert_eq!(common_type_name(&[Some("unknown"), Some("unknown")]).as_deref(), Some("TEXT"));
/// // Binary strings and text belong to different categories.
/// assert_eq!(common_type_name(&[Some("BYTEA"), Some("TEXT")]), None);
/// assert_eq!(common_type_name(&[Some("INT"), None]), None);
/// ```
#[must_use]
pub fn common_type_name<'a>(types: &[Option<&'a str>]) -> Option<Cow<'a, str>> {
    let names: Vec<&'a str> = types.iter().copied().collect::<Option<_>>()?;
    let parsed: Vec<PgType> =
        names.iter().map(|name| PgType::parse(name)).collect::<Option<_>>()?;
    match select_common_type(&parsed, |pg_type| pg_type)? {
        CommonType::At(position) => names.get(position).map(|name| Cow::Borrowed(*name)),
        CommonType::AllUnknown => Some(Cow::Borrowed("TEXT")),
    }
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
}
