//! Maps SQL type strings to canonical type tokens for schema fingerprinting.
//!
//! The canonical tokens ensure that equivalent types across different SQL
//! dialects produce the same fingerprint.

use alloc::{borrow::ToOwned, string::String};

use super::scalar_family::scalar_family;

/// Returns a canonical type token for the given SQL data type string.
///
/// Known families map to fixed uppercase tokens (`INT`, `STRING`, …).
/// Unknown types are emitted as `OTHER:<lowercase-normalized-db-type>`
/// with internal whitespace collapsed to a single ASCII space, per
/// FINGERPRINT_SPEC §7.2.1.
///
/// # Examples
///
/// ```rust
/// use sql_traits::utils::fingerprint_type_token::canonical_type_token;
///
/// assert_eq!(canonical_type_token("INT"), "INT");
/// assert_eq!(canonical_type_token("integer"), "INT");
/// assert_eq!(canonical_type_token("VARCHAR"), "STRING");
/// assert_eq!(canonical_type_token("geometry"), "OTHER:geometry");
/// assert_eq!(
///     canonical_type_token("TIMESTAMP  WITH  LOCAL  TIME  ZONE"),
///     "OTHER:timestamp with local time zone"
/// );
/// assert_eq!(canonical_type_token("INTERVAL YEAR TO MONTH"), "INTERVAL");
/// assert_eq!(canonical_type_token("TIMESTAMP WITH TIME ZONE"), "TIMESTAMPTZ");
/// assert_eq!(canonical_type_token("JSONB"), "JSONB");
/// ```
#[must_use]
pub fn canonical_type_token(sql_type: &str) -> String {
    let trimmed = sql_type.trim();
    if let Some(family) = scalar_family(trimmed) {
        return family.token().to_owned();
    }

    // Unknown type: emit `OTHER:<lowercase-normalized-db-type>` with
    // internal whitespace runs collapsed to a single ASCII space.
    let mut result = String::with_capacity(6 + trimmed.len());
    result.push_str("OTHER:");
    let mut prev_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
        } else {
            for lower in ch.to_lowercase() {
                result.push(lower);
            }
            prev_was_space = false;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::canonical_type_token;

    #[test]
    fn test_integer_family() {
        for ty in &[
            "INT",
            "integer",
            "SMALLINT",
            "bigint",
            "TINYINT",
            "MEDIUMINT",
            "int2",
            "INT4",
            "INT8",
            "SERIAL",
            "smallserial",
            "BIGSERIAL",
        ] {
            assert_eq!(canonical_type_token(ty), "INT", "failed for {ty}");
        }
    }

    #[test]
    fn test_integer_family_new_tokens() {
        for ty in &[
            "HUGEINT", "hugeint", "INT16", "INT32", "INT64", "INT128", "INT256", "UINT8", "uint8",
            "UINT16", "UINT32", "UINT64", "UINT128", "UINT256",
        ] {
            assert_eq!(canonical_type_token(ty), "INT", "failed for {ty}");
        }
    }

    #[test]
    fn test_float_family() {
        for ty in &["FLOAT", "REAL", "DOUBLE", "DOUBLE PRECISION", "float4", "float8"] {
            assert_eq!(canonical_type_token(ty), "FLOAT", "failed for {ty}");
        }
    }

    #[test]
    fn test_float_family_new_tokens() {
        assert_eq!(canonical_type_token("FLOAT32"), "FLOAT");
        assert_eq!(canonical_type_token("float64"), "FLOAT");
    }

    #[test]
    fn test_decimal_family() {
        for ty in &["DECIMAL", "NUMERIC", "number", "MONEY"] {
            assert_eq!(canonical_type_token(ty), "DECIMAL", "failed for {ty}");
        }
    }

    #[test]
    fn test_bool_family() {
        for ty in &["BOOL", "boolean"] {
            assert_eq!(canonical_type_token(ty), "BOOL", "failed for {ty}");
        }
    }

    #[test]
    fn test_string_family() {
        for ty in &[
            "TEXT",
            "VARCHAR",
            "CHAR",
            "CHARACTER",
            "CHARACTER VARYING",
            "nvarchar",
            "NCHAR",
            "CLOB",
            "STRING",
        ] {
            assert_eq!(canonical_type_token(ty), "STRING", "failed for {ty}");
        }
    }

    #[test]
    fn test_string_family_new_tokens() {
        for ty in &["TINYTEXT", "tinytext", "MEDIUMTEXT", "LONGTEXT", "FIXEDSTRING", "fixedstring"]
        {
            assert_eq!(canonical_type_token(ty), "STRING", "failed for {ty}");
        }
    }

    #[test]
    fn test_bytes_family() {
        for ty in &["BYTEA", "BLOB", "binary", "VARBINARY", "BYTES"] {
            assert_eq!(canonical_type_token(ty), "BYTES", "failed for {ty}");
        }
    }

    #[test]
    fn test_bytes_family_new_tokens() {
        for ty in &["TINYBLOB", "tinyblob", "MEDIUMBLOB", "LONGBLOB"] {
            assert_eq!(canonical_type_token(ty), "BYTES", "failed for {ty}");
        }
    }

    #[test]
    fn test_date() {
        assert_eq!(canonical_type_token("DATE"), "DATE");
        assert_eq!(canonical_type_token("date"), "DATE");
        assert_eq!(canonical_type_token("DATE32"), "DATE");
        assert_eq!(canonical_type_token("date32"), "DATE");
    }

    #[test]
    fn test_time_family() {
        for ty in &["TIME", "TIME WITHOUT TIME ZONE", "TIME WITH TIME ZONE", "timetz"] {
            assert_eq!(canonical_type_token(ty), "TIME", "failed for {ty}");
        }
    }

    #[test]
    fn test_timestamp_family() {
        for ty in &["TIMESTAMP", "TIMESTAMP WITHOUT TIME ZONE", "DATETIME"] {
            assert_eq!(canonical_type_token(ty), "TIMESTAMP", "failed for {ty}");
        }
    }

    #[test]
    fn test_timestamp_family_new_tokens() {
        assert_eq!(canonical_type_token("DATETIME64"), "TIMESTAMP");
        assert_eq!(canonical_type_token("datetime64"), "TIMESTAMP");
    }

    #[test]
    fn test_timestamptz_family() {
        for ty in &[
            "TIMESTAMPTZ",
            "timestamptz",
            "TIMESTAMP WITH TIME ZONE",
            "timestamp with time zone",
            "TIMESTAMP WITH TIMEZONE",
            "timestamp with timezone",
        ] {
            assert_eq!(canonical_type_token(ty), "TIMESTAMPTZ", "failed for {ty}");
        }
    }

    #[test]
    fn test_uuid() {
        assert_eq!(canonical_type_token("UUID"), "UUID");
        assert_eq!(canonical_type_token("uuid"), "UUID");
    }

    #[test]
    fn test_json_family() {
        assert_eq!(canonical_type_token("JSON"), "JSON");
        assert_eq!(canonical_type_token("json"), "JSON");
    }

    #[test]
    fn test_jsonb_family() {
        assert_eq!(canonical_type_token("JSONB"), "JSONB");
        assert_eq!(canonical_type_token("jsonb"), "JSONB");
    }

    #[test]
    fn test_unknown_type() {
        // Spec §7.2.1: OTHER tokens are lowercase.
        assert_eq!(canonical_type_token("geometry"), "OTHER:geometry");
        assert_eq!(canonical_type_token("HSTORE"), "OTHER:hstore");
        assert_eq!(canonical_type_token("citext"), "OTHER:citext");
        // Mixed case folds to lowercase too.
        assert_eq!(canonical_type_token("Geometry"), "OTHER:geometry");
    }

    #[test]
    fn test_unknown_type_collapses_whitespace() {
        // Spec §7.2.1: internal whitespace runs collapse to a single ASCII
        // space.
        assert_eq!(
            canonical_type_token("TIMESTAMP  WITH  LOCAL  TIME  ZONE"),
            "OTHER:timestamp with local time zone"
        );
        // Tabs and other whitespace also collapse.
        assert_eq!(canonical_type_token("foo\tbar  \t baz"), "OTHER:foo bar baz");
    }

    #[test]
    fn test_whitespace_trimming() {
        assert_eq!(canonical_type_token("  INT  "), "INT");
        assert_eq!(canonical_type_token(" text "), "STRING");
    }

    #[test]
    fn canonical_outputs_remain_stable() {
        for (declared_type, expected) in [
            ("INT", "INT"),
            ("DOUBLE PRECISION", "FLOAT"),
            ("NUMERIC", "DECIMAL"),
            ("BOOLEAN", "BOOL"),
            ("CHARACTER VARYING", "STRING"),
            ("VARBINARY", "BYTES"),
            ("UUID", "UUID"),
            ("DATE", "DATE"),
            ("TIME WITH TIME ZONE", "TIME"),
            ("TIMESTAMP WITHOUT TIME ZONE", "TIMESTAMP"),
            ("TIMESTAMP WITH TIME ZONE", "TIMESTAMPTZ"),
            ("JSON", "JSON"),
            ("JSONB", "JSONB"),
            (" Geometry ", "OTHER:geometry"),
            ("INTERVAL  YEAR  TO  MONTH", "INTERVAL"),
            ("TIMESTAMP  WITH  LOCAL  TIME  ZONE", "OTHER:timestamp with local time zone"),
        ] {
            assert_eq!(canonical_type_token(declared_type), expected);
        }
    }
}
