//! The functions a deployment's acting role may call by their bare names.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};
use core::ptr;

use sqlparser::ast::ArgMode;

use super::ResolvedDeployment;
use crate::{
    errors::{CallableFunctionsError, LookupError},
    structs::{FunctionKind, IdentifierCase, TargetName},
    traits::{DatabaseLike, FunctionLike, RoleLike, SchemaLike},
    utils::{
        call_typing::{input_modes, reached_by_name},
        identifier_resolution::{identifiers_match, stored_identifier},
        object_name::{PG_CATALOG, RoutinePath},
        postgres_format_type::format_type_in,
        postgres_types::PgType,
        quote_identifier::quote_identifier,
        search_path_text::display_search_path,
    },
};

impl<'db, DB: DatabaseLike> ResolvedDeployment<'db, DB> {
    /// The functions a request acting as `acting_role` may call by their
    /// bare names, in the order the database holds them.
    ///
    /// A function is listed when the schema declares it outside `pg_catalog`
    /// and every extension, it is a plain function returning neither
    /// `trigger` nor `event_trigger`, the role may
    /// [execute](FunctionLike::can_execute) it, `PUBLIC`'s default grant and
    /// inherited memberships included, and the role holds
    /// [`USAGE`](SchemaLike::has_usage) on its schema. A function in a schema
    /// the database does not declare is not listed.
    ///
    /// Every listed function must be what a call of its bare name reaches,
    /// so a client can call each by that name alone. The bare names are
    /// looked up on [`Self::search_path`] as PostgreSQL looks them up,
    /// `pg_catalog` first unless the path places it, `"$user"` read as the
    /// acting role, and each schema the role holds no `USAGE` on skipped.
    ///
    /// # Errors
    ///
    /// Returns [`CallableFunctionsError::Deployment`] for a role the
    /// deployment does not act as or a `search_path` it cannot settle,
    /// [`CallableFunctionsError::Lookup`] when a function's privileges cannot
    /// be read, [`CallableFunctionsError::SharedBareName`] when the role may
    /// call two functions whose names compare equal without case, as
    /// overloads and one name in two schemas do, and
    /// [`CallableFunctionsError::UnreachedByBareName`] when a bare call taking
    /// as many arguments as a listed function accepts reaches another
    /// function as well, or not that one.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::{errors::CallableFunctionsError, prelude::*};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// # fn main() -> Result<(), CallableFunctionsError> {
    /// let database = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE ROLE authenticator LOGIN NOINHERIT;
    ///      CREATE ROLE app NOLOGIN;
    ///      CREATE SCHEMA api;
    ///      GRANT USAGE ON SCHEMA api TO app;
    ///      CREATE SCHEMA internal;
    ///      CREATE FUNCTION api.search(term text) RETURNS SETOF text
    ///          LANGUAGE sql STABLE AS $$ SELECT term $$;
    ///      CREATE FUNCTION api.audit(term text) RETURNS SETOF text
    ///          LANGUAGE sql STABLE AS $$ SELECT term $$;
    ///      REVOKE EXECUTE ON FUNCTION api.audit(text) FROM PUBLIC;
    ///      CREATE FUNCTION internal.rebuild() RETURNS SETOF text
    ///          LANGUAGE sql STABLE AS $$ SELECT 'x' $$;",
    /// )
    /// .expect("the schema parses");
    /// let deployment = Deployment::new("authenticator", ["app"], [])?.with_search_path("api")?;
    /// let resolved = deployment.resolve(&database)?;
    ///
    /// // `PUBLIC` lost `EXECUTE` on `audit`, and `app` lacks `USAGE` on `internal`.
    /// let callable: Vec<&str> =
    ///     resolved.callable_functions("app")?.into_iter().map(FunctionLike::name).collect();
    /// assert_eq!(callable, ["search"]);
    ///
    /// // On the default path a bare `search` reaches nothing.
    /// let default_path = Deployment::new("authenticator", ["app"], [])?;
    /// assert!(matches!(
    ///     default_path.resolve(&database)?.callable_functions("app"),
    ///     Err(CallableFunctionsError::UnreachedByBareName { .. })
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    pub fn callable_functions(
        &self,
        acting_role: &str,
    ) -> Result<Vec<&'db DB::Function>, CallableFunctionsError> {
        let role = self.acting_role(acting_role)?;
        let path = self.search_path(acting_role)?;
        let database = self.database;
        let case = database.identifier_case();
        let mut callable = Vec::new();
        for function in database.functions() {
            if is_listed(function, database)? && may_call(function, role, database, case)? {
                callable.push(function);
            }
        }
        refuse_shared_bare_names(&callable, database)?;
        let lookup = lookup_path(database, role, &path, case);
        for &function in &callable {
            refuse_unreached(function, database, &lookup, &path, case)?;
        }
        Ok(callable)
    }
}

/// Whether the listing counts `function` at all, as a plain function the
/// schema declares outside `pg_catalog` and every extension, which a `FROM`
/// item or a scalar subquery can call.
///
/// # Errors
///
/// Returns the [`LookupError`] reading the function's kind or extension
/// raises.
fn is_listed<DB: DatabaseLike>(
    function: &DB::Function,
    database: &DB,
) -> Result<bool, LookupError> {
    let target = function.target_name();
    if target.schema().is_some_and(|schema| {
        identifiers_match(schema, target.schema_is_quoted(), PG_CATALOG, false)
    }) {
        return Ok(false);
    }
    if function.extension(database)?.is_some() || function.kind(database)? != FunctionKind::Normal {
        return Ok(false);
    }
    let returned = function.return_type_name(database);
    Ok(!matches!(
        returned.as_deref().and_then(PgType::parse),
        Some(PgType::Catalog("trigger" | "event_trigger"))
    ))
}

/// Whether `role` may execute `function` and holds `USAGE` on its schema.
///
/// # Errors
///
/// Returns the [`LookupError`] reading the function's privileges raises.
fn may_call<DB: DatabaseLike>(
    function: &DB::Function,
    role: &DB::Role,
    database: &DB,
    case: IdentifierCase,
) -> Result<bool, LookupError> {
    if !function.can_execute(role, database)? {
        return Ok(false);
    }
    let target = function.target_name();
    let (schema, quoted) =
        target.schema().map_or(("public", false), |schema| (schema, target.schema_is_quoted()));
    Ok(declared_schema(database, schema, quoted, case)
        .is_some_and(|declared| declared.has_usage(role, database)))
}

/// The schema `database` declares under `name`.
fn declared_schema<'db, DB: DatabaseLike>(
    database: &'db DB,
    name: &str,
    quoted: bool,
    case: IdentifierCase,
) -> Option<&'db DB::Schema> {
    database
        .schemas()
        .find(|schema| case.identifiers_match(schema.name(), schema.name_is_quoted(), name, quoted))
}

/// The schemas a bare name is looked up in for `role` on `path`, as
/// PostgreSQL's `recomputeNamespacePath` builds them, with `"$user"` read as
/// the role, and a schema the database does not declare or `role` holds no
/// `USAGE` on left out.
fn lookup_path<'db, DB: DatabaseLike>(
    database: &'db DB,
    role: &'db DB::Role,
    path: &[(&'db str, bool)],
    case: IdentifierCase,
) -> Vec<(Cow<'db, str>, bool)> {
    path.iter()
        .filter_map(|&(schema, quoted)| {
            let (schema, quoted) = if schema == "$user" {
                (role.stored_name(), true)
            } else {
                (Cow::Borrowed(schema), quoted)
            };
            let usable = identifiers_match(&schema, quoted, PG_CATALOG, false)
                || declared_schema(database, &schema, quoted, case)
                    .is_some_and(|declared| declared.has_usage(role, database));
            usable.then_some((schema, quoted))
        })
        .collect()
}

/// Refuses a name two functions of `callable` share, compared without case,
/// as a client calling each by its bare name cannot tell them apart and
/// SQLite compares function names without case.
///
/// # Errors
///
/// Returns [`CallableFunctionsError::SharedBareName`] naming every function
/// sharing the first name shared.
fn refuse_shared_bare_names<DB: DatabaseLike>(
    callable: &[&DB::Function],
    database: &DB,
) -> Result<(), CallableFunctionsError> {
    for function in callable {
        let name = function.stored_name();
        let sharing: Vec<String> = callable
            .iter()
            .filter(|other| other.stored_name().eq_ignore_ascii_case(&name))
            .map(|other| signature(*other, database))
            .collect();
        if sharing.len() > 1 {
            return Err(CallableFunctionsError::SharedBareName {
                name: name.into_owned(),
                functions: sharing,
            });
        }
    }
    Ok(())
}

/// Refuses `function` unless a call of its bare name, taking any number of
/// positional arguments it accepts, reaches it alone on `lookup`, the path
/// `path` gives.
///
/// # Errors
///
/// Returns [`CallableFunctionsError::UnreachedByBareName`] naming what the
/// first such call reaches.
fn refuse_unreached<DB: DatabaseLike>(
    function: &DB::Function,
    database: &DB,
    lookup: &[(Cow<'_, str>, bool)],
    path: &[(&str, bool)],
    case: IdentifierCase,
) -> Result<(), CallableFunctionsError> {
    let target = TargetName::new(function.name(), function.name_is_quoted());
    let inputs = input_modes(function).len();
    for arguments in (0..=inputs).filter(|count| function.accepts_argument_count(*count)) {
        let written = vec![None; arguments];
        let reached = reached_by_name(database, &target, &written, RoutinePath::Set(lookup), case);
        if !matches!(reached.as_slice(), [(only, _)] if ptr::eq(*only, function)) {
            return Err(CallableFunctionsError::UnreachedByBareName {
                function: signature(function, database),
                search_path: display_search_path(path.iter().copied()),
                reaches: reached.iter().map(|(other, _)| signature(*other, database)).collect(),
            });
        }
    }
    Ok(())
}

/// `function` schema-qualified, with the types of its input arguments as
/// PostgreSQL's `format_type` prints them.
fn signature<F: FunctionLike>(function: &F, database: &F::DB) -> String {
    let target = function.target_name();
    let schema = target.schema().map_or(Cow::Borrowed("public"), |schema| {
        stored_identifier(schema, target.schema_is_quoted())
    });
    let inputs: Vec<String> = function
        .argument_modes()
        .zip(function.argument_type_names(database))
        .filter(|(mode, _)| *mode != ArgMode::Out)
        .map(|(_, type_name)| {
            PgType::parse(&type_name)
                .map_or_else(|| type_name.to_string(), |parsed| format_type_in(database, &parsed))
        })
        .collect();
    format!(
        "{}.{}({})",
        quote_identifier(&schema),
        quote_identifier(&function.stored_name()),
        inputs.join(", ")
    )
}
