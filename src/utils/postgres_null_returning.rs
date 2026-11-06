//! The non-strict PostgreSQL builtins that still return `NULL` for a `NULL`
//! argument at some positions, a fact `pg_proc` does not record.

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
];

/// The positions of the entry for the `pg_catalog` function `name` whose
/// argument types, as `format_type` spells them, `matches` accepts.
pub(crate) fn null_returning_positions(
    name: &str,
    matches: impl Fn(&[&str]) -> bool,
) -> Option<&'static [usize]> {
    NULL_RETURNING
        .iter()
        .find(|entry| entry.name == name && matches(entry.argument_types))
        .map(|entry| entry.positions)
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
    if schema != PG_CATALOG {
        return &[];
    }
    let Some(given) =
        argument_types.iter().map(|name| PgType::parse(name)).collect::<Option<Vec<_>>>()
    else {
        return &[];
    };
    null_returning_positions(name, |declared| {
        declared.len() == given.len()
            && declared
                .iter()
                .zip(&given)
                .all(|(declared, given)| PgType::parse(declared).as_ref() == Some(given))
    })
    .unwrap_or_default()
}
