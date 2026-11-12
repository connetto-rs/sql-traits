//! Cast catalog facts and their lifecycle across the objects they name.
#![expect(clippy::unwrap_used, reason = "Fixtures must expose their declared catalog entities.")]

use sql_traits::{
    errors::{Error, ObjectKind},
    prelude::*,
};
use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};

fn pg(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("pg_catalog", false)
}

fn app(name: &str) -> TargetName<'_> {
    TargetName::new(name, false).with_schema("app", false)
}

fn built_in<'db>(db: &'db ParserDB, source: &str, target: &str) -> &'db PostgresCatalogCast {
    db.casts()
        .unwrap()
        .find(|cast| cast.source_type(db) == pg(source) && cast.target_type(db) == pg(target))
        .unwrap()
}

/// Parses `sql` with the default catalog and one configured cast from
/// `source` to `app.fahrenheit` through `app.to_f(source)`.
fn configured(source: TargetName<'_>, sql: &str) -> Result<ParserDB, Error> {
    let cast = PostgresCatalogCast::through_function(
        source.clone(),
        app("fahrenheit"),
        CastContext::Implicit,
        app("to_f"),
        [source],
    );
    ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::default().with_cast(cast))
        .parse::<PostgreSqlDialect>(sql)
}

/// The schema every lifecycle fixture starts from.
const TYPES: &str = "CREATE SCHEMA app;
     CREATE TYPE app.celsius AS ENUM ('c');
     CREATE TYPE app.fahrenheit AS ENUM ('f');
     CREATE FUNCTION app.to_f(app.celsius) RETURNS app.fahrenheit LANGUAGE SQL AS 'SELECT ''f''::app.fahrenheit';";

fn with_types(sql: &str) -> String {
    format!("{TYPES} {sql}")
}

/// The configured cast, the only one targeting `app.fahrenheit`.
fn the_cast(db: &ParserDB) -> Option<&PostgresCatalogCast> {
    db.casts().unwrap().find(|cast| cast.target_type(db).name() == "fahrenheit")
}

#[test]
fn the_postgres_18_catalog_holds_every_built_in_cast_once() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let mut pairs: Vec<_> = db
        .casts()
        .unwrap()
        .map(|cast| (cast.source_type(&db).to_string(), cast.target_type(&db).to_string()))
        .collect();
    assert_eq!(pairs.len(), 235);
    pairs.sort();
    pairs.dedup();
    assert_eq!(pairs.len(), 235);
}

#[test]
fn casts_are_unavailable_outside_postgresql_and_empty_in_an_empty_catalog() {
    let sqlite = ParserDB::parse::<SQLiteDialect>("").unwrap();
    assert!(sqlite.casts().is_none());
    let empty = ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::empty())
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    assert!(empty.casts().is_some_and(|mut casts| casts.next().is_none()));
}

#[test]
fn built_in_casts_keep_their_context_method_and_exact_function() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();

    let widening = built_in(&db, "int4", "numeric");
    assert_eq!(widening.context(), CastContext::Implicit);
    assert_eq!(widening.method(), CastMethod::Function);
    let function = widening.function(&db).unwrap();
    assert_eq!(function.target_name(), pg("numeric"));
    assert_eq!(function.argument_type_names(&db).collect::<Vec<_>>(), ["INT"]);

    let narrowing = built_in(&db, "int8", "int4");
    assert_eq!(narrowing.context(), CastContext::Assignment);
    assert_eq!(narrowing.function(&db).unwrap().target_name(), pg("int4"));

    let relabel = built_in(&db, "varchar", "text");
    assert_eq!(relabel.context(), CastContext::Implicit);
    assert_eq!(relabel.method(), CastMethod::Binary);
    assert!(relabel.function(&db).is_none());

    let io = built_in(&db, "json", "jsonb");
    assert_eq!(io.context(), CastContext::Assignment);
    assert_eq!(io.method(), CastMethod::InOut);
    assert!(io.function(&db).is_none());

    // The function of a cast can take a type the source is binary-coercible
    // to, as `xml(text)` implements the cast from `varchar`.
    let explicit = built_in(&db, "varchar", "xml");
    assert_eq!(explicit.context(), CastContext::Explicit);
    let function = explicit.function(&db).unwrap();
    assert_eq!(function.target_name(), pg("xml"));
    assert_eq!(function.argument_type_names(&db).collect::<Vec<_>>(), ["TEXT"]);

    let length = built_in(&db, "varchar", "varchar");
    assert_eq!(
        length.function(&db).unwrap().argument_type_names(&db).collect::<Vec<_>>(),
        ["VARCHAR", "INT", "BOOLEAN"]
    );
}

#[test]
fn every_built_in_function_cast_links_its_function() {
    let db = ParserDB::parse::<PostgreSqlDialect>("").unwrap();
    let unlinked: Vec<_> = db
        .casts()
        .unwrap()
        .filter(|cast| (cast.method() == CastMethod::Function) != cast.function(&db).is_some())
        .map(|cast| format!("{} AS {}", cast.source_type(&db), cast.target_type(&db)))
        .collect();
    assert!(unlinked.is_empty(), "{unlinked:?}");
}

#[test]
fn a_configured_cast_replaces_the_cast_between_the_same_types() {
    let catalog = PostgresCatalog::default().with_cast(PostgresCatalogCast::binary_coercible(
        pg("int4"),
        pg("int8"),
        CastContext::Explicit,
    ));
    let db = ParseOptions::default()
        .with_postgres_catalog(catalog)
        .parse::<PostgreSqlDialect>("")
        .unwrap();
    assert_eq!(db.casts().unwrap().count(), 235);
    let replaced = built_in(&db, "int4", "int8");
    assert_eq!(replaced.context(), CastContext::Explicit);
    assert_eq!(replaced.method(), CastMethod::Binary);
}

#[test]
fn a_configured_cast_links_only_its_exact_function_signature() {
    let db = configured(
        app("celsius"),
        &with_types(
            "CREATE FUNCTION app.to_f(TEXT) RETURNS app.fahrenheit LANGUAGE SQL AS 'SELECT ''f''::app.fahrenheit';",
        ),
    )
    .unwrap();
    let cast = the_cast(&db).unwrap();
    assert_eq!(cast.method(), CastMethod::Function);
    let function = cast.function(&db).unwrap();
    assert_eq!(function.target_name(), app("to_f"));
    assert_eq!(function.argument_type_names(&db).collect::<Vec<_>>(), ["app.celsius"]);

    let io = ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::empty().with_cast(PostgresCatalogCast::through_io(
            app("celsius"),
            pg("text"),
            CastContext::Assignment,
        )))
        .parse::<PostgreSqlDialect>(TYPES)
        .unwrap();
    let cast = io.casts().unwrap().next().unwrap();
    assert_eq!(cast.method(), CastMethod::InOut);
    assert!(cast.function(&io).is_none());
}

#[test]
fn a_cast_with_no_declared_function_has_no_link() {
    let db = configured(app("celsius"), "CREATE SCHEMA app;").unwrap();
    assert!(the_cast(&db).unwrap().function(&db).is_none());
}

#[test]
fn type_and_rowtype_renames_repoint_the_cast() {
    let db = configured(app("celsius"), &with_types("ALTER TYPE app.celsius RENAME TO kelvin;"))
        .unwrap();
    let cast = the_cast(&db).unwrap();
    assert_eq!(cast.source_type(&db), app("kelvin"));
    assert_eq!(cast.function(&db).unwrap().target_name(), app("to_f"));

    let array =
        configured(app("_celsius"), &with_types("ALTER TYPE app.celsius RENAME TO kelvin;"))
            .unwrap();
    assert_eq!(the_cast(&array).unwrap().source_type(&array), app("_kelvin"));

    let rowtype = configured(
        app("readings"),
        "CREATE SCHEMA app;
         CREATE TABLE app.readings (a INT);
         ALTER TABLE app.readings RENAME TO samples;",
    )
    .unwrap();
    assert_eq!(the_cast(&rowtype).unwrap().source_type(&rowtype), app("samples"));
}

#[test]
fn a_restricted_drop_of_a_named_type_is_refused_naming_the_cast() {
    for drop in ["DROP TYPE app.celsius;", "DROP TYPE app.fahrenheit;"] {
        // No function names either type, so only the cast holds them.
        let result = configured(
            app("celsius"),
            &format!(
                "CREATE SCHEMA app;
                 CREATE TYPE app.celsius AS ENUM ('c');
                 CREATE TYPE app.fahrenheit AS ENUM ('f');
                 {drop}"
            ),
        );
        assert!(
            matches!(
                &result,
                Err(Error::TypeDropRequiredBy { requiring_kind: ObjectKind::Cast, requiring_name, .. })
                    if requiring_name == "(app.celsius AS app.fahrenheit)"
            ),
            "{drop}: {result:?}"
        );
        assert!(
            result.unwrap_err().to_string().contains("Cast `(app.celsius AS app.fahrenheit)`"),
            "{drop}"
        );
    }
    let table = configured(
        app("readings"),
        "CREATE SCHEMA app;
         CREATE TABLE app.readings (a INT);
         DROP TABLE app.readings;",
    );
    assert!(matches!(
        table,
        Err(Error::RelationHasDependents { dependent_kind: ObjectKind::Cast, .. })
    ));
    let view = configured(
        app("readings"),
        "CREATE SCHEMA app;
         CREATE VIEW app.readings AS SELECT 1 AS a;
         DROP VIEW app.readings;",
    );
    assert!(matches!(
        view,
        Err(Error::RelationHasDependents { dependent_kind: ObjectKind::Cast, .. })
    ));
}

#[test]
fn a_cascading_drop_of_a_named_object_removes_the_cast() {
    for drop in [
        "DROP TYPE app.celsius CASCADE;",
        "DROP TYPE app.fahrenheit CASCADE;",
        "DROP SCHEMA app CASCADE;",
        "DROP FUNCTION app.to_f(app.celsius) CASCADE;",
    ] {
        let db = configured(app("celsius"), &with_types(drop)).unwrap();
        assert!(the_cast(&db).is_none(), "{drop}");
        assert_eq!(db.casts().unwrap().count(), 235, "{drop}");
    }
    let table = configured(
        app("readings"),
        "CREATE SCHEMA app;
         CREATE TABLE app.readings (a INT);
         DROP TABLE app.readings CASCADE;",
    )
    .unwrap();
    assert!(the_cast(&table).is_none());
}

#[test]
fn a_restricted_drop_of_the_cast_function_is_refused() {
    let result = configured(app("celsius"), &with_types("DROP FUNCTION app.to_f(app.celsius);"));
    assert!(matches!(result, Err(Error::FunctionReferenced { .. })));
}

#[test]
fn function_renames_and_moves_keep_the_link() {
    let renamed = configured(
        app("celsius"),
        &with_types("ALTER FUNCTION app.to_f(app.celsius) RENAME TO convert;"),
    )
    .unwrap();
    assert_eq!(
        the_cast(&renamed).unwrap().function(&renamed).unwrap().target_name(),
        app("convert")
    );

    let moved = configured(
        app("celsius"),
        &with_types(
            "CREATE SCHEMA other;
             ALTER FUNCTION app.to_f(app.celsius) SET SCHEMA other;",
        ),
    )
    .unwrap();
    assert_eq!(
        the_cast(&moved).unwrap().function(&moved).unwrap().target_name(),
        TargetName::new("to_f", false).with_schema("other", false)
    );
}

#[test]
fn a_schema_rename_repoints_every_identity_of_the_cast() {
    let db = configured(app("celsius"), &with_types("ALTER SCHEMA app RENAME TO moved;")).unwrap();
    let moved = |name| TargetName::new(name, false).with_schema("moved", false);
    let cast = the_cast(&db).unwrap();
    assert_eq!(cast.source_type(&db), moved("celsius"));
    assert_eq!(cast.target_type(&db), moved("fahrenheit"));
    assert_eq!(cast.function(&db).unwrap().target_name(), moved("to_f"));

    // A cast without a function follows the rename too.
    let io = ParseOptions::default()
        .with_postgres_catalog(PostgresCatalog::empty().with_cast(PostgresCatalogCast::through_io(
            app("celsius"),
            app("fahrenheit"),
            CastContext::Explicit,
        )))
        .parse::<PostgreSqlDialect>(&with_types("ALTER SCHEMA app RENAME TO moved;"))
        .unwrap();
    let cast = io.casts().unwrap().next().unwrap();
    assert_eq!(cast.source_type(&io), moved("celsius"));
    assert_eq!(cast.target_type(&io), moved("fahrenheit"));
}

#[test]
fn a_type_rename_repoints_only_the_identities_naming_it() {
    let cast = PostgresCatalogCast::through_function(
        app("celsius"),
        app("fahrenheit"),
        CastContext::Assignment,
        app("to_f"),
        [app("celsius"), pg("int4"), pg("bool")],
    );
    let options = || {
        ParseOptions::default()
            .with_postgres_catalog(PostgresCatalog::empty().with_cast(cast.clone()))
    };
    let schema = "CREATE SCHEMA app;
         CREATE TYPE app.celsius AS ENUM ('c');
         CREATE TYPE app.fahrenheit AS ENUM ('f');
         CREATE FUNCTION app.to_f(app.celsius, INT, BOOLEAN) RETURNS app.fahrenheit LANGUAGE SQL AS 'SELECT ''f''::app.fahrenheit';";

    let target = options()
        .parse::<PostgreSqlDialect>(&format!("{schema} ALTER TYPE app.fahrenheit RENAME TO hot;"))
        .unwrap();
    let renamed = target.casts().unwrap().next().unwrap();
    assert_eq!(renamed.source_type(&target), app("celsius"));
    assert_eq!(renamed.target_type(&target), app("hot"));
    assert_eq!(renamed.function(&target).unwrap().target_name(), app("to_f"));

    let source = options()
        .parse::<PostgreSqlDialect>(&format!("{schema} ALTER TYPE app.celsius RENAME TO cold;"))
        .unwrap();
    let renamed = source.casts().unwrap().next().unwrap();
    assert_eq!(renamed.source_type(&source), app("cold"));
    let function = renamed.function(&source).unwrap();
    assert_eq!(
        function.argument_type_names(&source).collect::<Vec<_>>(),
        ["app.cold", "INT", "BOOLEAN"]
    );
}

#[test]
fn a_dropped_schema_takes_casts_naming_its_undeclared_objects() {
    for drop in ["DROP SCHEMA app;", "DROP SCHEMA app CASCADE;"] {
        let db = configured(app("celsius"), &format!("CREATE SCHEMA app; {drop}")).unwrap();
        assert!(the_cast(&db).is_none(), "{drop}");
    }
}

#[test]
fn unrelated_drops_preserve_the_cast() {
    let db = configured(
        app("celsius"),
        &with_types(
            "CREATE TYPE app.other AS ENUM ('o');
             CREATE FUNCTION app.to_f(TEXT) RETURNS app.fahrenheit LANGUAGE SQL AS 'SELECT ''f''::app.fahrenheit';
             DROP TYPE app.other;
             DROP FUNCTION app.to_f(TEXT);",
        ),
    )
    .unwrap();
    let cast = the_cast(&db).unwrap();
    assert_eq!(cast.source_type(&db), app("celsius"));
    assert_eq!(cast.function(&db).unwrap().target_name(), app("to_f"));
}

#[test]
fn rollback_restores_a_cast_a_cascade_removed() {
    let db = configured(
        app("celsius"),
        &with_types(
            "BEGIN;
             DROP TYPE app.celsius CASCADE;
             ROLLBACK;",
        ),
    )
    .unwrap();
    let cast = the_cast(&db).unwrap();
    assert_eq!(cast.source_type(&db), app("celsius"));
    assert_eq!(cast.function(&db).unwrap().target_name(), app("to_f"));
}
