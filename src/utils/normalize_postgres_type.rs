//! Submodule providing a function for normalizing `PostgreSQL` data types.

use alloc::{borrow::Cow, string::String, vec::Vec};

/// Normalizes a `PostgreSQL` type name, so every spelling of one type answers
/// one name.
///
/// A declaration and PostgreSQL's own print of the type (`format_type`)
/// normalize alike.
///
/// - A builtin answers the crate's upper-case token, so `int4`, `integer` and
///   `INT` all answer `INT`, `character varying` answers `VARCHAR`, and
///   `bpchar`, `character`, `nchar` and `national character` answer `CHAR`.
/// - A builtin's type modifiers are dropped wherever they stand, so
///   `character(4)` answers `CHAR`, `timestamp(3) with time zone` answers
///   `TIMESTAMP WITH TIME ZONE` and `interval day to second(3)` answers
///   `INTERVAL`, as their declarations do.
/// - Any other unquoted name folds, so `void` and `VOID` both answer `VOID`.
/// - A quoted name is read as it stands. It answers bare when an unquoted name
///   means the same type, as `"mytype"` does, and stays quoted otherwise, as
///   `"MyType"` and the internal `"char"` do.
/// - `SETOF` and array brackets wrap the normalized element. Every array
///   answers a single `[]`, since PostgreSQL does not enforce dimensions.
/// - Each part of a schema-qualified name is read the same way, and a
///   `pg_catalog` qualifier is dropped.
/// - `float(p)` answers `REAL` up to 24 bits of precision and `DOUBLE
///   PRECISION` beyond, and a bare `float` is double precision, as PostgreSQL
///   reads both. The modifier list of any type outside the builtins is kept as
///   written.
///
/// # Examples
///
/// ```rust
/// use sql_traits::utils::normalize_postgres_type;
///
/// assert_eq!(normalize_postgres_type("int4"), "INT");
/// assert_eq!(normalize_postgres_type("character varying[]"), "VARCHAR[]");
/// assert_eq!(normalize_postgres_type("SETOF integer"), "SETOF INT");
/// assert_eq!(normalize_postgres_type("void"), "VOID");
/// assert_eq!(normalize_postgres_type("\"mytype\""), "MYTYPE");
/// assert_eq!(normalize_postgres_type("\"MyType\""), "\"MyType\"");
/// assert_eq!(normalize_postgres_type("app.points"), "APP.POINTS");
/// ```
#[must_use]
#[inline]
pub fn normalize_postgres_type(pg_type: &str) -> Cow<'_, str> {
    canonical(pg_type).unwrap_or(Cow::Borrowed(pg_type))
}

/// Normalizes a `PostgreSQL` data type without disturbing its ownership: a
/// type already in its normalized spelling is handed straight back.
#[must_use]
#[inline]
pub(crate) fn normalize_postgres_type_cow(pg_type: Cow<'_, str>) -> Cow<'_, str> {
    canonical(&pg_type).unwrap_or(pg_type)
}

/// Builtin spellings and the token each answers, keyed by the folded spelling.
///
/// Spellings made of several words match whatever whitespace separates them.
const ALIASES: &[(&str, &str)] = &[
    ("int2", "SMALLINT"),
    ("smallint", "SMALLINT"),
    ("smallserial", "SMALLINT"),
    ("serial2", "SMALLINT"),
    ("int4", "INT"),
    ("int", "INT"),
    ("integer", "INT"),
    ("serial", "INT"),
    ("serial4", "INT"),
    ("int8", "BIGINT"),
    ("bigint", "BIGINT"),
    ("bigserial", "BIGINT"),
    ("serial8", "BIGINT"),
    ("float4", "REAL"),
    ("real", "REAL"),
    ("float8", "DOUBLE PRECISION"),
    ("float", "DOUBLE PRECISION"),
    ("double precision", "DOUBLE PRECISION"),
    ("numeric", "NUMERIC"),
    ("decimal", "NUMERIC"),
    ("dec", "NUMERIC"),
    ("bool", "BOOLEAN"),
    ("boolean", "BOOLEAN"),
    ("varchar", "VARCHAR"),
    ("character varying", "VARCHAR"),
    ("char varying", "VARCHAR"),
    ("nchar varying", "VARCHAR"),
    ("national character varying", "VARCHAR"),
    ("national char varying", "VARCHAR"),
    ("bpchar", "CHAR"),
    ("char", "CHAR"),
    ("character", "CHAR"),
    ("nchar", "CHAR"),
    ("national character", "CHAR"),
    ("national char", "CHAR"),
    ("bit", "BIT"),
    ("varbit", "VARBIT"),
    ("bit varying", "VARBIT"),
    ("timestamp", "TIMESTAMP WITHOUT TIME ZONE"),
    ("timestamp without time zone", "TIMESTAMP WITHOUT TIME ZONE"),
    ("timestamptz", "TIMESTAMP WITH TIME ZONE"),
    ("timestamp with time zone", "TIMESTAMP WITH TIME ZONE"),
    ("time", "TIME WITHOUT TIME ZONE"),
    ("time without time zone", "TIME WITHOUT TIME ZONE"),
    ("timetz", "TIME WITH TIME ZONE"),
    ("time with time zone", "TIME WITH TIME ZONE"),
    ("interval", "INTERVAL"),
];

/// The fields `interval` may be restricted to, which PostgreSQL reads as part
/// of its type modifier.
const INTERVAL_FIELDS: &[&str] = &["year", "month", "day", "hour", "minute", "second", "to"];

/// Keywords that name a type only when written bare and unqualified.
///
/// Quoted, each is an ordinary lookup of that name, which finds a different
/// type (`"char"` is the internal one-byte type) or none at all, so a quoted
/// spelling of one keeps its quotes.
const TYPE_KEYWORDS: &[&str] = &[
    "bigint",
    "boolean",
    "char",
    "character",
    "dec",
    "decimal",
    "double",
    "float",
    "int",
    "integer",
    "national",
    "nchar",
    "real",
    "smallint",
    "table",
];

/// Returns the normalized spelling of `pg_type`, or `None` when it already is
/// one.
fn canonical(pg_type: &str) -> Option<Cow<'static, str>> {
    if is_single_unquoted_name(pg_type) {
        if let Some(token) = builtin(pg_type) {
            return (token != pg_type).then_some(Cow::Borrowed(token));
        }
        return (!is_folded(pg_type)).then(|| Cow::Owned(fold_words(pg_type)));
    }
    let mut normalized = String::with_capacity(pg_type.len());
    push_normalized(&mut normalized, pg_type);
    (normalized != pg_type).then_some(Cow::Owned(normalized))
}

/// Appends the normalized spelling of `pg_type` to `out`.
fn push_normalized(out: &mut String, pg_type: &str) {
    let pg_type = pg_type.trim();
    if let Some(element) = strip_setof(pg_type) {
        out.push_str("SETOF ");
        push_normalized(out, element);
        return;
    }
    if let Some(element) = strip_array_brackets(pg_type) {
        push_normalized(out, element);
        out.push_str("[]");
        return;
    }
    let (name, modifiers) = split_modifiers(pg_type);
    if let Some(token) = float_with_precision(name, modifiers).or_else(|| builtin(pg_type)) {
        out.push_str(token);
        return;
    }
    let names_builtin = if let Some(parts) = parse_parts(name) {
        push_name(out, &parts)
    } else {
        out.push_str(&fold_words(name));
        false
    };
    if !names_builtin {
        out.push_str(modifiers);
    }
}

/// Returns the token of the builtin an unquoted, unqualified spelling names
/// once its type modifiers are set aside, or `None` when it names none.
///
/// The modifier list follows the name or, as PostgreSQL prints `timestamp(3)
/// with time zone`, stands between its words. An interval's field
/// restriction is a modifier as well.
fn builtin(pg_type: &str) -> Option<&'static str> {
    let (head, tail) = match pg_type.split_once('(') {
        Some((head, rest)) => (head, rest.split_once(')')?.1),
        None => (pg_type, ""),
    };
    if head.contains(['"', '.']) || tail.contains(['"', '.', '(', ')']) {
        return None;
    }
    let mut words = head.split_ascii_whitespace().chain(tail.split_ascii_whitespace());
    if let Some(token) = alias(&words) {
        return Some(token);
    }
    let restricts_interval = words.next()?.eq_ignore_ascii_case("interval")
        && words.all(|word| INTERVAL_FIELDS.iter().any(|field| word.eq_ignore_ascii_case(field)));
    restricts_interval.then_some("INTERVAL")
}

/// Returns the type `float(p)` selects, `p` counting binary digits of
/// precision.
fn float_with_precision(name: &str, modifiers: &str) -> Option<&'static str> {
    if !name.eq_ignore_ascii_case("float") {
        return None;
    }
    let bits: u64 = modifiers.strip_prefix('(')?.strip_suffix(')')?.trim().parse().ok()?;
    Some(if bits <= 24 { "REAL" } else { "DOUBLE PRECISION" })
}

/// Whether `pg_type` is one unquoted, unqualified name with no wrapper or
/// modifier, which normalizes without being parsed into parts.
fn is_single_unquoted_name(pg_type: &str) -> bool {
    !pg_type.contains(['"', '.', '[', '(']) && strip_setof(pg_type.trim()).is_none()
}

/// Returns the element of a `SETOF` declaration.
fn strip_setof(pg_type: &str) -> Option<&str> {
    let keyword = pg_type.get(..5)?;
    let rest = &pg_type[5..];
    (keyword.eq_ignore_ascii_case("SETOF") && rest.starts_with(|c: char| c.is_ascii_whitespace()))
        .then_some(rest)
}

/// Returns the element of an array type, with every trailing bracket pair
/// removed.
fn strip_array_brackets(pg_type: &str) -> Option<&str> {
    let mut element = pg_type;
    while let Some(open) = element.strip_suffix(']').and_then(|rest| rest.rfind('[')) {
        element = element[..open].trim_end();
    }
    (element.len() != pg_type.len()).then_some(element)
}

/// Splits a trailing modifier list, such as `(Point, 4326)`, from the name it
/// follows.
fn split_modifiers(pg_type: &str) -> (&str, &str) {
    if pg_type.ends_with(')') {
        let mut quoted = false;
        for (index, byte) in pg_type.bytes().enumerate() {
            match byte {
                b'"' => quoted = !quoted,
                b'(' if !quoted => return (pg_type[..index].trim_end(), &pg_type[index..]),
                _ => {}
            }
        }
    }
    (pg_type, "")
}

/// One dot-separated part of a type name.
struct Part<'a> {
    /// The text of the part, without its quotes.
    text: Cow<'a, str>,
    /// Whether the part was written in double quotes.
    quoted: bool,
}

impl Part<'_> {
    /// Returns the name PostgreSQL stores for the part.
    fn stored(&self) -> Cow<'_, str> {
        if self.quoted || !self.text.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Borrowed(&self.text)
        } else {
            Cow::Owned(self.text.to_ascii_lowercase())
        }
    }
}

/// Splits a type name into its dot-separated parts, or returns `None` when it
/// is not a well-formed name.
fn parse_parts(name: &str) -> Option<Vec<Part<'_>>> {
    let mut parts = Vec::new();
    let mut rest = name.trim_start();
    loop {
        if let Some(after_quote) = rest.strip_prefix('"') {
            let mut text = String::new();
            let mut chars = after_quote.char_indices();
            let end = loop {
                match chars.next()? {
                    (index, '"') if after_quote[index + 1..].starts_with('"') => {
                        text.push('"');
                        chars.next();
                    }
                    (index, '"') => break index + 1,
                    (_, character) => text.push(character),
                }
            };
            parts.push(Part { text: Cow::Owned(text), quoted: true });
            rest = after_quote[end..].trim_start();
        } else {
            let end = rest.find(['.', '"']).unwrap_or(rest.len());
            let text = rest[..end].trim();
            // Only a builtin such as `character varying` is spelled in several
            // words, and a builtin is never qualified here.
            let several_words = text.split_ascii_whitespace().nth(1).is_some();
            let qualified = end < rest.len() || !parts.is_empty();
            if rest[end..].starts_with('"') || (several_words && qualified) {
                return None;
            }
            parts.push(Part { text: Cow::Borrowed(text), quoted: false });
            rest = &rest[end..];
        }
        if rest.is_empty() {
            return Some(parts);
        }
        rest = rest.strip_prefix('.')?.trim_start();
    }
}

/// Appends the normalized spelling of a parsed type name to `out`, and
/// returns whether the name is a builtin's.
fn push_name(out: &mut String, parts: &[Part<'_>]) -> bool {
    match parts {
        [only] if !only.quoted => {
            if let Some(token) = alias(&only.text.split_ascii_whitespace()) {
                out.push_str(token);
                true
            } else {
                out.push_str(&fold_words(&only.text));
                false
            }
        }
        // `pg_catalog` is searched first whatever the search path says, so
        // the qualified name is an ordinary lookup of the bare one, exactly
        // as a quoted unqualified name is.
        [schema, name] if schema.stored() == "pg_catalog" => push_looked_up(out, &name.stored()),
        [only] => push_looked_up(out, &only.text),
        qualified => {
            for (index, part) in qualified.iter().enumerate() {
                if index > 0 {
                    out.push('.');
                }
                let stored = part.stored();
                if quoting_matters(&stored, false) {
                    push_quoted(out, &stored);
                } else {
                    out.push_str(&stored.to_ascii_uppercase());
                }
            }
            false
        }
    }
}

/// Appends the normalized spelling of the unqualified type PostgreSQL finds
/// by looking up `stored` as it stands, and returns whether it is a builtin.
fn push_looked_up(out: &mut String, stored: &str) -> bool {
    if quoting_matters(stored, true) {
        push_quoted(out, stored);
        false
    } else if let Some(token) = alias(&stored.split_ascii_whitespace()) {
        out.push_str(token);
        true
    } else {
        out.push_str(&stored.to_ascii_uppercase());
        false
    }
}

/// Whether a stored name must stay quoted to keep naming the same type.
///
/// An unquoted name folds to lower case and reads a type keyword as the
/// keyword, so a name that has an upper-case letter, a character no unquoted
/// identifier may carry, or the spelling of a type keyword is reached only
/// quoted. `unqualified` says whether the name stands alone, the only place a
/// keyword reads as a type.
pub(crate) fn quoting_matters(stored: &str, unqualified: bool) -> bool {
    let mut characters = stored.chars();
    let Some(first) = characters.next() else {
        return true;
    };
    let plain = |character: char| {
        !character.is_ascii()
            || character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '_' | '$')
    };
    !plain(first)
        || first.is_ascii_digit()
        || first == '$'
        || !characters.all(plain)
        || (unqualified && TYPE_KEYWORDS.contains(&stored))
}

/// Appends `stored` in double quotes, doubling each quote it holds.
pub(crate) fn push_quoted(out: &mut String, stored: &str) {
    out.push('"');
    for character in stored.chars() {
        if character == '"' {
            out.push('"');
        }
        out.push(character);
    }
    out.push('"');
}

/// Returns the token the builtin spelled by `words` answers.
fn alias<'a>(words: &(impl Iterator<Item = &'a str> + Clone)) -> Option<&'static str> {
    ALIASES
        .iter()
        .find(|(spelling, _)| words_eq_ignore_ascii_case(spelling, words.clone()))
        .map(|(_, token)| *token)
}

/// Whether a spelling holds `words`, ignoring ASCII case and the whitespace
/// between them.
fn words_eq_ignore_ascii_case<'a>(
    spelling: &str,
    mut words: impl Iterator<Item = &'a str>,
) -> bool {
    let mut spelling = spelling.split_ascii_whitespace();
    loop {
        match (spelling.next(), words.next()) {
            (None, None) => return true,
            (Some(expected), Some(word)) if expected.eq_ignore_ascii_case(word) => {}
            _ => return false,
        }
    }
}

/// Whether an unquoted name is already folded: upper case, with single spaces
/// between its words and none around them.
fn is_folded(name: &str) -> bool {
    !name.bytes().any(|byte| byte.is_ascii_lowercase())
        && !name.starts_with(' ')
        && !name.ends_with(' ')
        && !name.contains("  ")
        && !name.bytes().any(|byte| byte.is_ascii_whitespace() && byte != b' ')
}

/// Folds an unquoted name to upper case, with single spaces between its words.
fn fold_words(name: &str) -> String {
    let mut folded = String::with_capacity(name.len());
    for (index, word) in name.split_ascii_whitespace().enumerate() {
        if index > 0 {
            folded.push(' ');
        }
        folded.push_str(word);
    }
    folded.make_ascii_uppercase();
    folded
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::ToOwned, string::String};

    use super::*;

    #[test]
    fn test_normalize_postgres_type_integers() {
        for spelling in ["int2", "smallint", "smallserial", "serial2", "\"int2\""] {
            assert_eq!(normalize_postgres_type(spelling), "SMALLINT", "{spelling}");
        }
        for spelling in ["int4", "integer", "INT", "serial", "InT4", "pg_catalog.int4"] {
            assert_eq!(normalize_postgres_type(spelling), "INT", "{spelling}");
        }
        for spelling in ["int8", "bigint", "bigserial", "BiGiNt"] {
            assert_eq!(normalize_postgres_type(spelling), "BIGINT", "{spelling}");
        }
    }

    #[test]
    fn test_normalize_postgres_type_floats() {
        assert_eq!(normalize_postgres_type("float4"), "REAL");
        assert_eq!(normalize_postgres_type("real"), "REAL");
        assert_eq!(normalize_postgres_type("float8"), "DOUBLE PRECISION");
        assert_eq!(normalize_postgres_type("double   precision"), "DOUBLE PRECISION");
        assert_eq!(normalize_postgres_type("numeric"), "NUMERIC");
        assert_eq!(normalize_postgres_type("decimal"), "NUMERIC");
    }

    /// `float` names double precision, and a binary precision picks the
    /// single-precision type up to 24 bits.
    #[test]
    fn test_normalize_postgres_type_float_precision() {
        assert_eq!(normalize_postgres_type("float"), "DOUBLE PRECISION");
        assert_eq!(normalize_postgres_type("FLOAT(24)"), "REAL");
        assert_eq!(normalize_postgres_type("float ( 10 )[]"), "REAL[]");
        assert_eq!(normalize_postgres_type("float(25)"), "DOUBLE PRECISION");
        assert_eq!(normalize_postgres_type("\"float\""), "\"float\"");
        assert_eq!(normalize_postgres_type("\"float\"(10)"), "\"float\"(10)");
        assert_eq!(normalize_postgres_type("app.float(10)"), "APP.FLOAT(10)");
    }

    #[test]
    fn test_normalize_postgres_type_strings() {
        assert_eq!(normalize_postgres_type("varchar"), "VARCHAR");
        assert_eq!(normalize_postgres_type("\"varchar\""), "VARCHAR");
        assert_eq!(normalize_postgres_type("character varying"), "VARCHAR");
        assert_eq!(normalize_postgres_type("bpchar"), "CHAR");
        assert_eq!(normalize_postgres_type("character"), "CHAR");
        assert_eq!(normalize_postgres_type("text"), "TEXT");
    }

    /// Every grammar spelling of `bpchar` and `varchar`, the national ones
    /// included, answers the one token of its type.
    #[test]
    fn test_normalize_postgres_type_national_strings() {
        for spelling in ["nchar", "NATIONAL CHARACTER", "national  char", "Nchar(4)"] {
            assert_eq!(normalize_postgres_type(spelling), "CHAR", "{spelling}");
        }
        for spelling in ["nchar varying", "national character varying(3)", "national char varying"]
        {
            assert_eq!(normalize_postgres_type(spelling), "VARCHAR", "{spelling}");
        }
        assert_eq!(normalize_postgres_type("\"nchar\"(4)"), "\"nchar\"(4)");
        assert_eq!(normalize_postgres_type("pg_catalog.nchar"), "\"nchar\"");
    }

    /// A builtin's type modifiers are dropped wherever they stand, as
    /// PostgreSQL prints them inside `timestamp(3) with time zone` and after
    /// `character(4)`.
    #[test]
    fn test_normalize_postgres_type_drops_builtin_modifiers() {
        for (spelling, token) in [
            ("bpchar(4)", "CHAR"),
            ("character(4)[]", "CHAR[]"),
            ("pg_catalog.bpchar(4)", "CHAR"),
            ("\"bpchar\"(4)", "CHAR"),
            ("character varying(10)", "VARCHAR"),
            ("SETOF numeric(10, 2)", "SETOF NUMERIC"),
            ("bit(3)", "BIT"),
            ("bit varying(5)", "VARBIT"),
            ("timestamptz(3)", "TIMESTAMP WITH TIME ZONE"),
            ("timestamp(3) with time zone", "TIMESTAMP WITH TIME ZONE"),
            ("timestamp ( 3 )  without time zone[]", "TIMESTAMP WITHOUT TIME ZONE[]"),
            ("time(2) with time zone", "TIME WITH TIME ZONE"),
            ("interval(3)", "INTERVAL"),
            ("interval year to month", "INTERVAL"),
            ("INTERVAL DAY TO SECOND(3)", "INTERVAL"),
            ("pg_catalog.interval(3)", "INTERVAL"),
        ] {
            assert_eq!(normalize_postgres_type(spelling), token, "{spelling}");
        }
        assert_eq!(normalize_postgres_type("\"char\"(1)"), "\"char\"(1)");
        assert_eq!(normalize_postgres_type("app.bpchar(4)"), "APP.BPCHAR(4)");
        assert_eq!(normalize_postgres_type("interval_set(3)"), "INTERVAL_SET(3)");
    }

    #[test]
    fn test_normalize_postgres_type_times() {
        assert_eq!(normalize_postgres_type("timestamp"), "TIMESTAMP WITHOUT TIME ZONE");
        assert_eq!(normalize_postgres_type("timestamptz"), "TIMESTAMP WITH TIME ZONE");
        assert_eq!(normalize_postgres_type("time with time zone"), "TIME WITH TIME ZONE");
        assert_eq!(normalize_postgres_type("TIMETZ"), "TIME WITH TIME ZONE");
    }

    /// A name outside the alias table folds rather than passing through.
    #[test]
    fn test_normalize_postgres_type_folds_unknown_names() {
        assert_eq!(normalize_postgres_type("void"), "VOID");
        assert_eq!(normalize_postgres_type("trigger"), "TRIGGER");
        assert_eq!(normalize_postgres_type(" custom_type "), "CUSTOM_TYPE");
        assert_eq!(normalize_postgres_type("\"custom_type\""), "CUSTOM_TYPE");
    }

    /// A quoted name answers bare only where an unquoted one names the same
    /// type.
    #[test]
    fn test_normalize_postgres_type_quoted_names() {
        assert_eq!(normalize_postgres_type("\"MyType\""), "\"MyType\"");
        assert_eq!(normalize_postgres_type("\"my type\""), "\"my type\"");
        assert_eq!(normalize_postgres_type("\"say \"\"hi\"\"\""), "\"say \"\"hi\"\"\"");
        assert_eq!(normalize_postgres_type("\"char\""), "\"char\"");
        assert_eq!(normalize_postgres_type("pg_catalog.char"), "\"char\"");
        assert_eq!(normalize_postgres_type("\"\""), "\"\"");
    }

    #[test]
    fn test_normalize_postgres_type_qualified_names() {
        assert_eq!(normalize_postgres_type("App.Points"), "APP.POINTS");
        assert_eq!(normalize_postgres_type("app.\"Points\""), "APP.\"Points\"");
        assert_eq!(normalize_postgres_type("app.int"), "APP.INT");
        assert_eq!(normalize_postgres_type("\"\".my_type"), "\"\".MY_TYPE");
    }

    #[test]
    fn test_normalize_postgres_type_wrappers() {
        assert_eq!(normalize_postgres_type("SETOF integer"), "SETOF INT");
        assert_eq!(normalize_postgres_type("setof\tapp.points"), "SETOF APP.POINTS");
        assert_eq!(normalize_postgres_type("integer[]"), "INT[]");
        assert_eq!(normalize_postgres_type("INT4[3][]"), "INT[]");
        assert_eq!(normalize_postgres_type("SETOF \"MyType\"[]"), "SETOF \"MyType\"[]");
        assert_eq!(normalize_postgres_type("setofthings"), "SETOFTHINGS");
    }

    #[test]
    fn test_normalize_postgres_type_keeps_modifiers() {
        assert_eq!(normalize_postgres_type("geometry(Point, 3857)"), "GEOMETRY(Point, 3857)");
        assert_eq!(normalize_postgres_type("geometry(Point, 3857)[]"), "GEOMETRY(Point, 3857)[]");
        assert_eq!(normalize_postgres_type("\"Geo(x\"(Point)"), "\"Geo(x\"(Point)");
        assert_eq!(normalize_postgres_type("\"geo\"(Point)"), "GEO(Point)");
    }

    /// A name of several words outside the alias table folds word by word,
    /// and so does a spelling that is not a well-formed name.
    #[test]
    fn test_normalize_postgres_type_folds_words() {
        assert_eq!(normalize_postgres_type("any \t type"), "ANY TYPE");
        assert_eq!(normalize_postgres_type("app.my  type"), "APP.MY TYPE");
        assert_eq!(normalize_postgres_type("APP.MY TYPE"), "APP.MY TYPE");
    }

    /// Normalizing a normalized name changes nothing.
    #[test]
    fn test_normalize_postgres_type_is_idempotent() {
        for spelling in [
            "int4",
            "SETOF integer[]",
            "\"MyType\"",
            "\"char\"",
            "app.\"Points\"",
            "double precision",
            "void",
            "geometry(Point, 3857)",
            "timestamp(3) with time zone",
            "national character(4)[]",
        ] {
            let once = normalize_postgres_type(spelling).into_owned();
            assert_eq!(normalize_postgres_type(&once), once, "{spelling}");
        }
    }

    /// A type with a canonical alias becomes that alias, and one already in
    /// its normalized spelling is handed back untouched rather than
    /// reallocated.
    #[test]
    fn test_normalize_postgres_type_cow_preserves_ownership() {
        assert_eq!(normalize_postgres_type_cow(Cow::Borrowed("int4")), Cow::Borrowed("INT"));
        assert!(matches!(
            normalize_postgres_type_cow(Cow::Owned(String::from("serial"))),
            Cow::Borrowed("INT")
        ));
        assert!(matches!(normalize_postgres_type("VOID"), Cow::Borrowed("VOID")));

        let array = "TEXT[]".to_owned();
        let pointer = array.as_ptr();
        let normalized = normalize_postgres_type_cow(Cow::Owned(array));
        assert!(
            matches!(&normalized, Cow::Owned(owned) if owned.as_ptr() == pointer),
            "a normalized owned type must stay owned and in place: {normalized:?}"
        );
    }
}
