//! PostgreSQL type names in diagnostics.

use alloc::string::{String, ToString};

use super::{postgres_types::PgType, quote_identifier::quote_identifier};
use crate::structs::TargetName;

/// Formats arrays and the names their element types carry, omitting the
/// schema of a user type `visible` accepts.
pub(crate) fn format_with_visibility(
    data_type: &PgType,
    visible: &impl Fn(&TargetName<'_>) -> bool,
) -> String {
    match data_type {
        PgType::Catalog(name) => {
            match *name {
                "bool" => "boolean",
                "bpchar" => "character",
                "float4" => "real",
                "float8" => "double precision",
                "int2" => "smallint",
                "int4" => "integer",
                "int8" => "bigint",
                "time" => "time without time zone",
                "timetz" => "time with time zone",
                "timestamp" => "timestamp without time zone",
                "timestamptz" => "timestamp with time zone",
                "varbit" => "bit varying",
                "varchar" => "character varying",
                "any" => "\"any\"",
                "char" => "\"char\"",
                "_record" => "record[]",
                name => name,
            }
            .to_string()
        }
        PgType::Array(element) => format!("{}[]", format_with_visibility(element, visible)),
        PgType::Named(name) => {
            let identifier = quote_identifier(name.name());
            match name.schema().filter(|_| !visible(name)) {
                Some(schema) => format!("{}.{identifier}", quote_identifier(schema)),
                None => identifier,
            }
        }
        PgType::Other(name) => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_diagnostic_names() {
        for (stored, printed) in [
            ("int2", "smallint"),
            ("int4", "integer"),
            ("int8", "bigint"),
            ("bool", "boolean"),
            ("float4", "real"),
            ("float8", "double precision"),
            ("bpchar", "character"),
            ("varchar", "character varying"),
            ("time", "time without time zone"),
            ("timetz", "time with time zone"),
            ("timestamp", "timestamp without time zone"),
            ("timestamptz", "timestamp with time zone"),
            ("varbit", "bit varying"),
            ("bit", "bit"),
            ("char", "\"char\""),
            ("any", "\"any\""),
            ("text", "text"),
            ("record", "record"),
            ("_record", "record[]"),
        ] {
            assert_eq!(format_with_visibility(&PgType::Catalog(stored), &|_| false), printed);
        }
        assert_eq!(
            format_with_visibility(&PgType::Catalog("int4").array(), &|_| false),
            "integer[]"
        );
    }
}
