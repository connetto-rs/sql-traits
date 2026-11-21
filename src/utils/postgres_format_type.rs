//! PostgreSQL type names in diagnostics.

use alloc::string::{String, ToString};

use super::{
    postgres_types::{PgType, names_catalog_type},
    quote_identifier::quote_identifier,
};
use crate::{
    structs::TargetName,
    traits::DatabaseLike,
    utils::{
        identifier_resolution::{identifiers_match, is_temporary_schema},
        object_name::{PG_CATALOG, TEMPORARY_SCHEMA},
    },
};

/// The diagnostic spelling of a type, retaining its explicit qualification.
pub(crate) fn format_type(data_type: &PgType) -> String {
    format_with_visibility(data_type, &|_| false)
}

/// The diagnostic spelling of a type in the database's search path, which
/// qualifies a user type unless its bare name reaches it. A bare type name is
/// looked for in the temporary schema and then in `pg_catalog`, each unless
/// the path lists it, then along the path.
pub(crate) fn format_type_in(database: &impl DatabaseLike, data_type: &PgType) -> String {
    format_with_visibility(data_type, &|name| {
        let Some(schema) = name.schema() else { return true };
        let temporary = (!database
            .search_path()
            .any(|(candidate, quoted)| is_temporary_schema(candidate, quoted)))
        .then_some((TEMPORARY_SCHEMA, false));
        let catalog = (!database
            .search_path()
            .any(|(candidate, quoted)| identifiers_match(candidate, quoted, PG_CATALOG, false)))
        .then_some((PG_CATALOG, false));
        temporary
            .into_iter()
            .chain(catalog)
            .chain(database.search_path())
            .find(|(candidate, quoted)| holds_type(database, name.name(), candidate, *quoted))
            .is_some_and(|(candidate, quoted)| identifiers_match(candidate, quoted, schema, true))
    })
}

/// Whether `schema` holds a type stored as `name`, the catalog types
/// counting as held by `pg_catalog`.
fn holds_type(database: &impl DatabaseLike, name: &str, schema: &str, quoted: bool) -> bool {
    (identifiers_match(schema, quoted, PG_CATALOG, false) && names_catalog_type(name))
        || database.type_facts(TargetName::new(name, true).with_schema(schema, quoted)).is_some()
}

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
            assert_eq!(format_type(&PgType::Catalog(stored)), printed);
        }
        assert_eq!(format_type(&PgType::Catalog("int4").array()), "integer[]");
    }

    #[test]
    fn user_types_qualified_unless_their_bare_name_reaches_them() {
        let database = crate::prelude::ParserDB::parse::<sqlparser::dialect::PostgreSqlDialect>(
            "CREATE DOMAIN public.int4 AS integer; CREATE DOMAIN public.d AS integer; \
             CREATE TEMP TABLE tt (id int); CREATE DOMAIN public.tt AS integer;",
        )
        .unwrap();
        for (name, printed) in [("int4", "public.int4"), ("d", "d"), ("tt", "public.tt")] {
            let data_type = PgType::Named(TargetName::new(name, true).with_schema("public", true));
            assert_eq!(format_type_in(&database, &data_type), printed);
        }
    }
}
