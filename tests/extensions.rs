//! Tests that `CREATE EXTENSION` is recorded with the schema it lands in, and
//! that the extension follows that schema through renames and drops.
//!
//! The expected outcomes are the ones PostgreSQL 17 and 18 give for the same
//! statements.
#![allow(clippy::expect_used)]

use sql_traits::{errors::Error, prelude::*};
use sqlparser::dialect::PostgreSqlDialect;

fn parse(sql: &str) -> Result<ParserDB, Error> {
    ParserDB::parse::<PostgreSqlDialect>(sql)
}

/// An extension as `name@schema version`, with `-` for the default schema and
/// for no version, read the way generic consumer code reads it.
fn describe(extension: impl ExtensionLike) -> String {
    format!(
        "{}@{} {}",
        extension.name(),
        extension.schema().unwrap_or("-"),
        extension.version().unwrap_or("-")
    )
}

/// Every extension as [`describe`] spells it, sorted.
fn extensions(database: &ParserDB) -> Vec<String> {
    let mut listed: Vec<String> = database.extensions().map(describe).collect();
    listed.sort();
    listed
}

fn listed(extensions: &[&str]) -> Vec<String> {
    extensions.iter().map(ToString::to_string).collect()
}

/// The extensions besides the `plpgsql` every PostgreSQL database holds.
fn created(database: &ParserDB) -> Vec<String> {
    extensions(database)
        .into_iter()
        .filter(|extension| !extension.starts_with("plpgsql@"))
        .collect()
}

#[test]
fn a_database_starts_with_plpgsql_in_pg_catalog() {
    let database = parse("").expect("empty input parses");
    assert_eq!(extensions(&database), listed(&["plpgsql@pg_catalog 1.0"]));
}

#[test]
fn only_postgresql_starts_with_plpgsql() {
    let database = ParserDB::parse::<GenericDialect>("").expect("empty input parses");
    assert_eq!(database.extensions().count(), 0);
}

#[test]
fn with_schema_records_the_named_schema() {
    let database = parse("CREATE SCHEMA exts; CREATE EXTENSION pgcrypto WITH SCHEMA exts;")
        .expect("the extension is created");
    assert_eq!(created(&database), listed(&["pgcrypto@exts -"]));
}

#[test]
fn version_is_recorded() {
    let database = parse(
        "CREATE SCHEMA exts; CREATE EXTENSION hstore WITH SCHEMA exts VERSION '1.4' CASCADE;",
    )
    .expect("the extension is created");
    assert_eq!(created(&database), listed(&["hstore@exts 1.4"]));
}

#[test]
fn without_schema_the_extension_lands_in_the_default_schema() {
    let database = parse("CREATE EXTENSION pgcrypto;").expect("the extension is created");
    assert_eq!(created(&database), listed(&["pgcrypto@- -"]));
}

#[test]
fn without_schema_the_first_existing_schema_on_the_path_is_taken() {
    let database = parse(
        "CREATE SCHEMA s2;
         SET search_path = nonexistent, s2, public;
         CREATE EXTENSION pgcrypto;",
    )
    .expect("the extension is created");
    assert_eq!(created(&database), listed(&["pgcrypto@s2 -"]));
}

#[test]
fn without_schema_pg_catalog_on_the_path_is_taken() {
    let database = parse("SET search_path = pg_catalog, public; CREATE EXTENSION pgcrypto;")
        .expect("the extension is created");
    assert_eq!(created(&database), listed(&["pgcrypto@pg_catalog -"]));
}

#[test]
fn without_schema_the_temporary_schema_on_the_path_is_taken() {
    let database = parse("SET search_path = pg_temp, public; CREATE EXTENSION pgcrypto;")
        .expect("the extension is created");
    assert_eq!(created(&database), listed(&["pgcrypto@pg_temp -"]));
}

#[test]
fn a_path_selecting_no_schema_is_refused() {
    for sql in [
        "SET search_path = ''; CREATE EXTENSION hstore;",
        "SET search_path = nonexistent; CREATE EXTENSION hstore;",
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::NoSchemaSelectedForExtension { extension_name }) if extension_name == "hstore"
            ),
            "{sql}"
        );
    }
}

#[test]
fn a_schema_the_input_never_creates_is_refused() {
    for (sql, schema) in [
        ("CREATE EXTENSION pgcrypto WITH SCHEMA missing;", "missing"),
        ("CREATE EXTENSION pgcrypto WITH SCHEMA missing CASCADE;", "missing"),
        ("CREATE EXTENSION pgcrypto WITH SCHEMA pg_temp;", "pg_temp"),
        ("CREATE SCHEMA exts; CREATE EXTENSION pgcrypto WITH SCHEMA \"Exts\";", "Exts"),
    ] {
        assert!(
            matches!(
                parse(sql),
                Err(Error::SchemaNotFoundForExtension { schema_name, extension_name })
                    if schema_name == schema && extension_name == "pgcrypto"
            ),
            "{sql}"
        );
    }
}

#[test]
fn public_and_pg_catalog_need_no_create_schema() {
    let database = parse(
        "CREATE EXTENSION pgcrypto WITH SCHEMA public;
         CREATE EXTENSION hstore WITH SCHEMA pg_catalog;",
    )
    .expect("both extensions are created");
    assert_eq!(created(&database), listed(&["hstore@pg_catalog -", "pgcrypto@public -"]));
}

/// The stored name and schema of an extension, and whether each was quoted.
fn stored_identity(extension: impl ExtensionLike) -> (String, Option<String>, bool, bool) {
    (
        extension.stored_name().into_owned(),
        extension.stored_schema().map(std::borrow::Cow::into_owned),
        extension.name_is_quoted(),
        extension.schema_is_quoted(),
    )
}

#[test]
fn stored_names_fold_unquoted_identifiers_and_keep_quoted_ones() {
    let database = parse(
        "CREATE SCHEMA \"Tools\";
         CREATE SCHEMA Lab;
         CREATE EXTENSION \"UUID-ossp\" WITH SCHEMA \"Tools\";
         CREATE EXTENSION PgCrypto WITH SCHEMA Lab;",
    )
    .expect("both extensions are created");
    let mut identities: Vec<_> = database
        .extensions()
        .filter(|extension| extension.name() != "plpgsql")
        .map(stored_identity)
        .collect();
    identities.sort();
    assert_eq!(
        identities,
        vec![
            (String::from("UUID-ossp"), Some(String::from("Tools")), true, true),
            (String::from("pgcrypto"), Some(String::from("lab")), false, false),
        ]
    );
}

#[test]
fn a_second_create_of_the_same_name_is_refused() {
    for sql in [
        "CREATE EXTENSION pgcrypto; CREATE EXTENSION pgcrypto;",
        "CREATE EXTENSION pgcrypto; CREATE EXTENSION PGCRYPTO;",
        "CREATE EXTENSION plpgsql;",
    ] {
        assert!(matches!(parse(sql), Err(Error::ExtensionAlreadyExists { .. })), "{sql}");
    }
}

#[test]
fn a_quoted_name_differing_in_case_is_another_extension() {
    let database = parse("CREATE EXTENSION pgcrypto; CREATE EXTENSION \"PGCRYPTO\";")
        .expect("the names differ");
    assert_eq!(created(&database), listed(&["PGCRYPTO@- -", "pgcrypto@- -"]));
}

#[test]
fn an_existing_name_is_reported_before_the_schema_is_checked() {
    for sql in [
        "CREATE EXTENSION plpgsql WITH SCHEMA missing;",
        "SET search_path = ''; CREATE EXTENSION plpgsql;",
    ] {
        assert!(matches!(parse(sql), Err(Error::ExtensionAlreadyExists { .. })), "{sql}");
    }
    for sql in [
        "CREATE EXTENSION IF NOT EXISTS plpgsql WITH SCHEMA missing;",
        "SET search_path = ''; CREATE EXTENSION IF NOT EXISTS plpgsql;",
    ] {
        assert_eq!(
            extensions(&parse(sql).expect("the repeated creation is skipped")),
            listed(&["plpgsql@pg_catalog 1.0"]),
            "{sql}"
        );
    }
}

#[test]
fn if_not_exists_keeps_the_first_extension() {
    let database = parse(
        "CREATE SCHEMA exts;
         CREATE EXTENSION pgcrypto WITH SCHEMA exts;
         CREATE EXTENSION IF NOT EXISTS pgcrypto WITH SCHEMA public VERSION '1.3';
         CREATE EXTENSION IF NOT EXISTS plpgsql WITH SCHEMA pg_catalog;",
    )
    .expect("the repeated creations are skipped");
    assert_eq!(extensions(&database), listed(&["pgcrypto@exts -", "plpgsql@pg_catalog 1.0"]));
}

#[test]
fn drop_extension_removes_every_named_extension() {
    let database = parse(
        "CREATE EXTENSION pgcrypto;
         CREATE EXTENSION hstore;
         CREATE EXTENSION citext;
         DROP EXTENSION pgcrypto, HSTORE CASCADE;
         DROP EXTENSION plpgsql;",
    )
    .expect("the extensions are dropped");
    assert_eq!(extensions(&database), listed(&["citext@- -"]));
}

#[test]
fn drop_extension_of_an_absent_name_is_refused_unless_if_exists() {
    assert!(matches!(
        parse("CREATE EXTENSION pgcrypto; DROP EXTENSION pgcrypto, nope;"),
        Err(Error::DropExtensionNotFound { extension_name }) if extension_name == "nope"
    ));
    let database = parse("CREATE EXTENSION pgcrypto; DROP EXTENSION IF EXISTS nope, pgcrypto;")
        .expect("the absent name is skipped");
    assert_eq!(created(&database), listed(&[]));
}

#[test]
fn drop_schema_holding_an_extension_needs_cascade() {
    assert!(matches!(
        parse("CREATE SCHEMA exts; CREATE EXTENSION pgcrypto WITH SCHEMA exts; DROP SCHEMA exts;"),
        Err(Error::SchemaNotEmpty { schema_name }) if schema_name == "exts"
    ));
    let database = parse(
        "CREATE SCHEMA exts;
         CREATE EXTENSION pgcrypto WITH SCHEMA exts;
         CREATE EXTENSION hstore;
         DROP SCHEMA exts CASCADE;",
    )
    .expect("the cascade takes the extension along");
    assert_eq!(created(&database), listed(&["hstore@- -"]));
}

#[test]
fn an_extension_follows_its_schema_through_a_rename() {
    let database = parse(
        "CREATE SCHEMA exts;
         CREATE EXTENSION pgcrypto WITH SCHEMA exts;
         ALTER SCHEMA exts RENAME TO tools;",
    )
    .expect("the schema is renamed");
    assert_eq!(created(&database), listed(&["pgcrypto@tools -"]));
    assert!(matches!(
        parse(
            "CREATE SCHEMA exts;
             CREATE EXTENSION pgcrypto WITH SCHEMA exts;
             ALTER SCHEMA exts RENAME TO tools;
             DROP SCHEMA tools;",
        ),
        Err(Error::SchemaNotEmpty { schema_name }) if schema_name == "tools"
    ));
}

/// The function `name` resolves to from a bare reference, as
/// `schema.name returns type`.
fn resolved(database: &ParserDB, target: TargetName<'_>) -> Option<String> {
    database.resolve_target_function(target, IdentifierCase::AsWritten).expect("unambiguous").map(
        |function| {
            format!(
                "{} returns {}",
                function.target_name(),
                function.return_type_name(database).unwrap_or_default()
            )
        },
    )
}

#[test]
fn uuid_ossp_creates_its_functions_where_it_lands() {
    let database = parse("CREATE EXTENSION \"uuid-ossp\";").expect("the extension is created");
    let bare = |name| resolved(&database, TargetName::new(name, false));
    for name in [
        "uuid_nil",
        "uuid_ns_dns",
        "uuid_ns_url",
        "uuid_ns_oid",
        "uuid_ns_x500",
        "uuid_generate_v1",
        "uuid_generate_v1mc",
        "uuid_generate_v3",
        "uuid_generate_v4",
        "uuid_generate_v5",
    ] {
        assert_eq!(bare(name), Some(format!("{name} returns UUID")));
    }

    let elsewhere = parse("CREATE SCHEMA ext; CREATE EXTENSION \"uuid-ossp\" WITH SCHEMA ext;")
        .expect("the extension is created");
    let v4 = TargetName::new("uuid_generate_v4", false);
    assert_eq!(resolved(&elsewhere, v4.clone()), None);
    assert_eq!(
        resolved(&elsewhere, v4.with_schema("ext", false)),
        Some("ext.uuid_generate_v4 returns UUID".to_owned())
    );
}

#[test]
fn uuid_ossp_functions_go_with_the_extension() {
    let v4 = || TargetName::new("uuid_generate_v4", false);
    let dropped = parse("CREATE EXTENSION \"uuid-ossp\"; DROP EXTENSION \"uuid-ossp\";")
        .expect("the extension is dropped");
    assert_eq!(resolved(&dropped, v4()), None);
    let cascaded = parse(
        "CREATE SCHEMA ext; CREATE EXTENSION \"uuid-ossp\" WITH SCHEMA ext;
         DROP SCHEMA ext CASCADE;",
    )
    .expect("the schema is dropped");
    assert_eq!(resolved(&cascaded, v4().with_schema("ext", false)), None);

    assert!(matches!(
        parse("CREATE EXTENSION \"uuid-ossp\"; DROP FUNCTION uuid_generate_v4();"),
        Err(Error::FunctionRequiredByExtension { function_name, extension_name })
            if function_name == "uuid_generate_v4" && extension_name == "uuid-ossp"
    ));
    assert!(matches!(
        parse(
            "CREATE EXTENSION \"uuid-ossp\";
             CREATE TABLE t (id UUID CHECK (id <> uuid_nil()));
             DROP EXTENSION \"uuid-ossp\";",
        ),
        Err(Error::FunctionReferenced { function_name }) if function_name == "uuid_nil"
    ));
    assert!(matches!(
        parse(
            "CREATE FUNCTION uuid_nil() RETURNS UUID LANGUAGE sql AS 'SELECT NULL::uuid';
             CREATE EXTENSION \"uuid-ossp\";",
        ),
        Err(Error::FunctionAlreadyExists { function_name }) if function_name == "uuid_nil"
    ));
}

#[test]
fn decode_is_a_builtin_returning_bytea() {
    let database = parse("").expect("empty input parses");
    assert_eq!(
        resolved(&database, TargetName::new("decode", false)),
        Some("pg_catalog.decode returns BYTEA".to_owned())
    );
}
