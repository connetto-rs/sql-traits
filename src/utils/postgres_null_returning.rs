//! What `pg_proc` does not record about where PostgreSQL builtins get a
//! `NULL` result.

use alloc::vec::Vec;

use crate::utils::{object_name::PG_CATALOG, postgres_types::PgType};

/// A non-strict builtin returning `NULL` when the argument at one of
/// `positions` is `NULL`.
struct NullReturning {
    name: &'static str,
    argument_types: &'static [&'static str],
    positions: &'static [usize],
}

/// The non-strict `pg_catalog` functions known to return `NULL` for a `NULL`
/// argument at some positions.
static NULL_RETURNING: &[NullReturning] = &[
    // `string_to_array(NULL, ',')` is `NULL`, while a `NULL` delimiter splits
    // into characters and a `NULL` third argument matches nothing.
    NullReturning { name: "string_to_array", argument_types: &["text", "text"], positions: &[0] },
    NullReturning {
        name: "string_to_array",
        argument_types: &["text", "text", "text"],
        positions: &[0],
    },
    NullReturning { name: "concat_ws", argument_types: &["text", "\"any\""], positions: &[0] },
];

/// The `pg_catalog` functions, by name and argument types, that return `NULL`
/// only through a `NULL` argument their strictness or [`NULL_RETURNING`]
/// names.
static NULL_ONLY_ON_NULL_INPUT: &[(&str, &[&str])] = &[
    ("abs", &["real"]),
    ("abs", &["double precision"]),
    ("abs", &["bigint"]),
    ("abs", &["integer"]),
    ("abs", &["smallint"]),
    ("abs", &["numeric"]),
    ("btrim", &["text"]),
    ("btrim", &["text", "text"]),
    ("btrim", &["bytea", "bytea"]),
    ("char_length", &["character"]),
    ("char_length", &["text"]),
    ("character_length", &["character"]),
    ("character_length", &["text"]),
    ("clock_timestamp", &[]),
    ("concat", &["\"any\""]),
    ("concat_ws", &["text", "\"any\""]),
    ("count", &[]),
    ("count", &["\"any\""]),
    // An unknown setting raises `42704`.
    ("current_setting", &["text"]),
    ("length", &["text"]),
    ("length", &["character"]),
    ("length", &["lseg"]),
    ("length", &["path"]),
    ("length", &["bit"]),
    ("length", &["bytea", "name"]),
    ("length", &["bytea"]),
    ("length", &["tsvector"]),
    ("lower", &["text"]),
    ("now", &[]),
    ("octet_length", &["bytea"]),
    ("octet_length", &["text"]),
    ("octet_length", &["character"]),
    ("octet_length", &["bit"]),
    ("statement_timestamp", &[]),
    ("transaction_timestamp", &[]),
    ("upper", &["text"]),
];

/// The positions of the entry for the `pg_catalog` function `name` whose
/// argument types, as `format_type` spells them without `VARIADIC`, `matches`
/// accepts.
pub(crate) fn null_returning_positions(
    name: &str,
    matches: impl Fn(&[&str]) -> bool,
) -> Option<&'static [usize]> {
    NULL_RETURNING
        .iter()
        .find(|entry| entry.name == name && matches(entry.argument_types))
        .map(|entry| entry.positions)
}

/// Whether the `pg_catalog` function `name` whose argument types, as
/// `format_type` spells them without `VARIADIC`, `matches` accepts returns
/// `NULL` only through a `NULL` argument.
pub(crate) fn null_only_on_null_input(name: &str, matches: impl Fn(&[&str]) -> bool) -> bool {
    NULL_ONLY_ON_NULL_INPUT
        .iter()
        .any(|(entry, argument_types)| *entry == name && matches(argument_types))
}

/// Returns the argument positions at which a `NULL` makes a call to a
/// non-strict PostgreSQL 18 builtin return `NULL`, though `pg_proc` declares
/// it called on `NULL` input.
///
/// `schema` and `name` name the function as `pg_proc` stores it, and
/// `argument_types` its input argument types, each in any spelling that parses
/// as a type. A strict function, a function without such positions and a
/// function PostgreSQL does not provide answer an empty slice.
///
/// # Example
///
/// ```rust
/// use sql_traits::utils::postgres_null_returning_arguments;
///
/// // `string_to_array(NULL, ',')` is `NULL`, `string_to_array('a', NULL)` is not.
/// let split = ["text", "TEXT"];
/// assert_eq!(postgres_null_returning_arguments("pg_catalog", "string_to_array", &split), &[0]);
/// assert!(postgres_null_returning_arguments("public", "string_to_array", &split).is_empty());
/// assert!(postgres_null_returning_arguments("pg_catalog", "concat", &["\"any\""]).is_empty());
/// // A spelling that reads as no type matches no signature.
/// assert!(
///     postgres_null_returning_arguments("pg_catalog", "string_to_array", &["text", "("])
///         .is_empty()
/// );
/// ```
#[must_use]
pub fn postgres_null_returning_arguments(
    schema: &str,
    name: &str,
    argument_types: &[&str],
) -> &'static [usize] {
    declared_as(schema, argument_types)
        .and_then(|matches| null_returning_positions(name, matches))
        .unwrap_or_default()
}

/// Returns whether a call to the PostgreSQL 18 builtin is `NULL` only when an
/// argument its strictness or [`postgres_null_returning_arguments`] names is
/// `NULL`, a fact `pg_proc` does not record.
///
/// `schema` and `name` name the function as `pg_proc` stores it, and
/// `argument_types` its input argument types without `VARIADIC`, each in any
/// spelling that parses as a type. A function PostgreSQL does not provide,
/// or one that may return `NULL` on non-`NULL` input, answers false.
///
/// # Example
///
/// ```rust
/// use sql_traits::utils::postgres_null_only_on_null_input;
///
/// assert!(postgres_null_only_on_null_input("pg_catalog", "now", &[]));
/// assert!(postgres_null_only_on_null_input("pg_catalog", "current_setting", &["TEXT"]));
/// // `current_setting(name, true)` is `NULL` for an unknown setting.
/// assert!(!postgres_null_only_on_null_input("pg_catalog", "current_setting", &["text", "bool"]));
/// assert!(!postgres_null_only_on_null_input("public", "now", &[]));
/// ```
#[must_use]
pub fn postgres_null_only_on_null_input(schema: &str, name: &str, argument_types: &[&str]) -> bool {
    declared_as(schema, argument_types)
        .is_some_and(|matches| null_only_on_null_input(name, matches))
}

/// What accepts a `pg_catalog` signature declaring `argument_types`, `None`
/// when `schema` is not `pg_catalog` or a type does not parse.
fn declared_as(schema: &str, argument_types: &[&str]) -> Option<impl Fn(&[&str]) -> bool> {
    if schema != PG_CATALOG {
        return None;
    }
    let given =
        argument_types.iter().map(|name| PgType::parse(name)).collect::<Option<Vec<_>>>()?;
    Some(move |declared: &[&str]| {
        declared.len() == given.len()
            && declared
                .iter()
                .zip(&given)
                .all(|(declared, given)| PgType::parse(declared).as_ref() == Some(given))
    })
}
