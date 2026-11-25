//! The functions a deployment's acting role may call by bare name, and the
//! `search_path` a request looks those names up on.
//!
//! Each listed or refused function matches what PostgreSQL 18.7 answered for
//! the same schema with `has_function_privilege`, `has_schema_privilege` and a
//! bare call after `SET ROLE`.
#![allow(clippy::expect_used)]

use sql_traits::{
    errors::{CallableFunctionsError, DeploymentError},
    prelude::*,
};
use sqlparser::dialect::PostgreSqlDialect;

const ROLES: &str = "CREATE ROLE authenticator LOGIN NOINHERIT;
     CREATE ROLE app NOLOGIN;
     GRANT app TO authenticator;";

const API: &str = "CREATE SCHEMA api;
     GRANT USAGE ON SCHEMA api TO app;
     CREATE SCHEMA internal;
     CREATE FUNCTION api.search(term text) RETURNS SETOF text
         LANGUAGE sql STABLE AS $$ SELECT term $$;
     CREATE FUNCTION api.audit(term text) RETURNS SETOF text
         LANGUAGE sql STABLE AS $$ SELECT term $$;
     REVOKE EXECUTE ON FUNCTION api.audit(text) FROM PUBLIC;
     CREATE FUNCTION internal.rebuild() RETURNS SETOF text
         LANGUAGE sql STABLE AS $$ SELECT 'x' $$;";

fn parse(sql: &str) -> ParserDB {
    ParserDB::parse::<PostgreSqlDialect>(&format!("{ROLES} {sql}")).expect("schema builds")
}

/// A deployment logging in as `authenticator` and acting as `app`, each
/// request setting `path` itself when given.
fn deployment(path: Option<&str>) -> Deployment {
    let deployment = Deployment::new("authenticator", ["app"], []).expect("a valid description");
    match path {
        Some(path) => deployment.with_search_path(path).expect("a path PostgreSQL reads"),
        None => deployment,
    }
}

/// The bare names `app` may call on `path`, or why the listing refuses.
fn callable(
    database: &ParserDB,
    path: Option<&str>,
) -> Result<Vec<String>, CallableFunctionsError> {
    let deployment = deployment(path);
    let resolved = deployment.resolve(database).expect("every role is declared");
    Ok(resolved
        .callable_functions("app")?
        .into_iter()
        .map(|function| function.name().to_string())
        .collect())
}

#[test]
fn execute_and_schema_usage_decide_what_is_listed() {
    let database = parse(API);
    assert_eq!(callable(&database, Some("api")), Ok(vec!["search".to_string()]));
}

#[test]
fn execute_reaches_the_role_through_an_inherited_membership() {
    let database = parse(&format!(
        "{API}
         CREATE ROLE auditors NOLOGIN;
         GRANT auditors TO app;
         GRANT EXECUTE ON FUNCTION api.audit(text) TO auditors;"
    ));
    assert_eq!(
        callable(&database, Some("api")),
        Ok(vec!["audit".to_string(), "search".to_string()])
    );
}

#[test]
fn functions_the_listing_never_counts_are_left_out() {
    let database = parse(&format!(
        "{API}
         CREATE FUNCTION api.touch() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN RETURN NEW; END $$;
         CREATE PROCEDURE api.run() LANGUAGE sql AS $$ SELECT 1 $$;
         CREATE AGGREGATE api.total(integer) (SFUNC = int4pl, STYPE = integer);
         CREATE EXTENSION \"uuid-ossp\" SCHEMA api;"
    ));
    assert_eq!(callable(&database, Some("api")), Ok(vec!["search".to_string()]));
}

#[test]
fn a_bare_name_shared_by_callable_functions_is_refused() {
    let overloads = parse(&format!(
        "{API}
         CREATE FUNCTION api.search(term text, lim integer) RETURNS SETOF text
             LANGUAGE sql STABLE AS $$ SELECT term LIMIT lim $$;"
    ));
    assert_eq!(
        callable(&overloads, Some("api")),
        Err(CallableFunctionsError::SharedBareName {
            name: "search".into(),
            functions: vec!["api.search(text)".into(), "api.search(text, integer)".into()],
        })
    );

    let two_schemas = parse(&format!(
        "{API}
         CREATE SCHEMA reports;
         GRANT USAGE ON SCHEMA reports TO app;
         CREATE FUNCTION reports.search(term text) RETURNS SETOF text
             LANGUAGE sql STABLE AS $$ SELECT term $$;"
    ));
    assert_eq!(
        callable(&two_schemas, Some("api, reports")),
        Err(CallableFunctionsError::SharedBareName {
            name: "search".into(),
            functions: vec!["api.search(text)".into(), "reports.search(text)".into()],
        })
    );

    // PostgreSQL keeps both, and a client compares function names without case.
    let spellings = parse(&format!(
        "{API}
         CREATE FUNCTION api.\"Search\"() RETURNS SETOF text
             LANGUAGE sql STABLE AS $$ SELECT 'x' $$;"
    ));
    assert_eq!(
        callable(&spellings, Some("api")),
        Err(CallableFunctionsError::SharedBareName {
            name: "Search".into(),
            functions: vec!["api.\"Search\"()".into(), "api.search(text)".into()],
        })
    );
}

#[test]
fn a_function_not_called_by_its_bare_name_is_refused() {
    let database = parse(API);
    let unreached = callable(&database, None);
    assert_eq!(
        unreached,
        Err(CallableFunctionsError::UnreachedByBareName {
            function: "api.search(text)".into(),
            search_path: "public".into(),
            reaches: Vec::new(),
        })
    );
    assert_eq!(
        unreached.expect_err("refused").to_string(),
        "a call of `api.search(text)` by its bare name on search_path `public` reaches no function"
    );

    // `pg_catalog` comes first, so `lower` reaches the catalog's own.
    let shadowed = parse(&format!(
        "{API}
         CREATE FUNCTION api.lower(value text) RETURNS text LANGUAGE sql AS $$ SELECT value $$;"
    ));
    assert!(matches!(
        callable(&shadowed, Some("api")),
        Err(CallableFunctionsError::UnreachedByBareName { function, reaches, .. })
            if function == "api.lower(text)"
                && reaches.contains(&"pg_catalog.lower(text)".to_string())
                && !reaches.contains(&function)
    ));

    // A schema the role holds no `USAGE` on is skipped on the path.
    let unusable = parse(&format!(
        "{API}
         GRANT EXECUTE ON FUNCTION internal.rebuild() TO app;
         CREATE FUNCTION api.rebuild() RETURNS SETOF text LANGUAGE sql AS $$ SELECT 'y' $$;"
    ));
    assert_eq!(
        callable(&unusable, Some("internal, api")),
        Ok(vec!["rebuild".to_string(), "search".to_string()])
    );
}

#[test]
fn user_on_the_path_names_the_acting_role_schema() {
    let database = parse(&format!(
        "{API}
         CREATE SCHEMA app;
         GRANT USAGE ON SCHEMA app TO app;
         CREATE FUNCTION app.mine() RETURNS SETOF text LANGUAGE sql AS $$ SELECT 'm' $$;"
    ));
    assert_eq!(
        callable(&database, Some("\"$user\", api")),
        Ok(vec!["mine".to_string(), "search".to_string()])
    );
}

#[test]
fn the_listing_takes_one_acting_role() {
    let database = parse(API);
    let deployment = deployment(Some("api"));
    let resolved = deployment.resolve(&database).expect("every role is declared");
    assert_eq!(
        resolved.callable_functions("authenticator"),
        Err(CallableFunctionsError::Deployment(DeploymentError::NotAnActingRole {
            role: "authenticator".into()
        }))
    );
}

/// The path a request acting as `app` looks names up on.
fn path(
    database: &ParserDB,
    deployment: &Deployment,
) -> Result<Vec<(String, bool)>, DeploymentError> {
    let resolved = deployment.resolve(database).expect("every role is declared");
    Ok(resolved
        .search_path("app")?
        .into_iter()
        .map(|(schema, quoted)| (schema.to_string(), quoted))
        .collect())
}

fn entries(path: &[(&str, bool)]) -> Vec<(String, bool)> {
    path.iter().map(|&(schema, quoted)| (schema.to_string(), quoted)).collect()
}

#[test]
fn the_request_path_follows_the_role_setting_chain() {
    let chain = "CREATE SCHEMA login_path; CREATE SCHEMA every_role; CREATE SCHEMA acting;
         ALTER ROLE ALL SET search_path = every_role;
         ALTER ROLE authenticator SET search_path = 'login_path';
         ALTER ROLE app SET search_path = acting;";
    let database = parse(chain);
    let plain = deployment(None);
    assert_eq!(path(&database, &plain), Ok(entries(&[("login_path", false)])));
    let postgrest = deployment(None).with_acting_role_settings();
    assert_eq!(path(&database, &postgrest), Ok(entries(&[("acting", false)])));
    let declared = deployment(Some("\"$user\", Public"));
    assert_eq!(path(&database, &declared), Ok(entries(&[("$user", true), ("public", false)])));

    let database = parse("CREATE SCHEMA every_role; ALTER ROLE ALL SET search_path = every_role;");
    assert_eq!(path(&database, &plain), Ok(entries(&[("every_role", false)])));
    assert_eq!(path(&parse(""), &plain), Ok(entries(&[("public", false)])));

    let in_database = parse("ALTER ROLE authenticator IN DATABASE d SET search_path = api;");
    assert_eq!(
        path(&in_database, &plain),
        Err(DeploymentError::NoDatabase {
            role: Some("authenticator".into()),
            setting: "search_path".into()
        })
    );
    assert_eq!(
        path(&in_database, &deployment(None).connected_to("d")),
        Ok(entries(&[("api", false)]))
    );
}

#[test]
fn a_declared_path_postgresql_refuses_is_refused() {
    let refused = Deployment::new("authenticator", ["app"], [])
        .expect("a valid description")
        .with_search_path("api, \"open");
    assert_eq!(refused, Err(DeploymentError::InvalidSearchPath { path: "api, \"open".into() }));
}
