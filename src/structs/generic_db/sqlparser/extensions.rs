//! Records `CREATE EXTENSION` with the schema the extension lands in, and
//! keeps that schema current through `ALTER SCHEMA ... RENAME` and the drops
//! that take the extension along.
//!
//! The functions an extension's install script creates are registered in that
//! schema for the extensions modelled here, and dropped with the extension.
//! The requirements an extension's control file declares, which `CASCADE`
//! installs alongside it, are not visible in SQL text and are not recorded.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{CreateExtension, CreateFunction, DropExtension, Ident, ObjectName};

use super::{
    ParserDBBuilder, case_idents_match, declared_schema, function_signatures_match,
    postgres_builtins::{
        BuiltinType::{self, Text, Uuid},
        Volatility::{self, Immutable, Volatile},
        declaration, parameter,
    },
};
use crate::{
    errors::{Error, ObjectKind},
    structs::metadata::FunctionMetadata,
    utils::{
        identifier_resolution::{identifiers_match, normalize_identifier},
        last_str,
        object_name::{PG_CATALOG, TEMPORARY_SCHEMA, is_temporary_schema},
    },
};

/// The extension every PostgreSQL database is created with: PL/pgSQL in
/// `pg_catalog`, at the only version it has shipped since PostgreSQL 9.0.
pub(super) fn plpgsql() -> CreateExtension {
    CreateExtension {
        name: Ident::new("plpgsql"),
        if_not_exists: false,
        with_keyword: false,
        cascade: false,
        schema: Some(Ident::new(PG_CATALOG)),
        version: Some(Ident::with_quote('\'', "1.0")),
    }
}

/// One function an extension's install script creates, by name, parameters,
/// result and volatility. Every one modelled here is strict and written in C.
struct ExtensionFunction(
    &'static str,
    &'static [(&'static str, BuiltinType)],
    BuiltinType,
    Volatility,
);

impl ExtensionFunction {
    /// The declaration, in the schema the extension landed in.
    fn function(&self, schema: Option<&Ident>) -> CreateFunction {
        let Self(name, parameters, returns, volatility) = *self;
        let name = schema.cloned().into_iter().chain([Ident::new(name)]).collect::<Vec<_>>();
        declaration(
            ObjectName::from(name),
            parameters.iter().map(|&(name, data_type)| parameter(Some(name), data_type)).collect(),
            returns,
            true,
            volatility,
            "c",
        )
    }
}

/// The functions `uuid-ossp` 1.1 creates, the version PostgreSQL 18 installs.
static UUID_OSSP: &[ExtensionFunction] = &[
    ExtensionFunction("uuid_nil", &[], Uuid, Immutable),
    ExtensionFunction("uuid_ns_dns", &[], Uuid, Immutable),
    ExtensionFunction("uuid_ns_url", &[], Uuid, Immutable),
    ExtensionFunction("uuid_ns_oid", &[], Uuid, Immutable),
    ExtensionFunction("uuid_ns_x500", &[], Uuid, Immutable),
    ExtensionFunction("uuid_generate_v1", &[], Uuid, Volatile),
    ExtensionFunction("uuid_generate_v1mc", &[], Uuid, Volatile),
    ExtensionFunction("uuid_generate_v3", &[("namespace", Uuid), ("name", Text)], Uuid, Immutable),
    ExtensionFunction("uuid_generate_v4", &[], Uuid, Volatile),
    ExtensionFunction("uuid_generate_v5", &[("namespace", Uuid), ("name", Text)], Uuid, Immutable),
];

/// The functions the extension called `name` creates, empty for an extension
/// whose functions are not modelled.
fn extension_functions(name: &Ident) -> &'static [ExtensionFunction] {
    match normalize_identifier(&name.value, name.quote_style.is_some()).as_ref() {
        "uuid-ossp" => UUID_OSSP,
        _ => &[],
    }
}

/// Registers the functions `extension` creates, in the schema it landed in.
///
/// # Errors
///
/// Returns [`Error::FunctionAlreadyExists`] when that schema already holds a
/// function of the same signature, which PostgreSQL refuses as well.
fn create_extension_functions(
    mut builder: ParserDBBuilder,
    extension: &CreateExtension,
) -> Result<ParserDBBuilder, Error> {
    let case = builder.identifier_case();
    for member in extension_functions(&extension.name) {
        let function = member.function(extension.schema.as_ref());
        let taken = builder.functions().iter().any(|(existing, _)| {
            function_signatures_match(
                &existing.name,
                existing.args.as_deref(),
                &function.name,
                function.args.as_deref(),
                case,
            )
        });
        if taken {
            return Err(Error::FunctionAlreadyExists { function_name: member.0.to_string() });
        }
        let mut metadata = FunctionMetadata::default();
        metadata.set_extension(extension.name.clone());
        builder = builder.add_function(Arc::new(function), metadata);
    }
    Ok(builder)
}

/// Drops the functions belonging to the extension called `name`.
///
/// # Errors
///
/// Returns [`Error::FunctionReferenced`] when a check constraint, a policy or
/// a trigger still reaches one, as dropping the function itself does.
fn drop_extension_functions(builder: &mut ParserDBBuilder, name: &Ident) -> Result<(), Error> {
    let case = builder.identifier_case();
    let belongs = |metadata: &FunctionMetadata| {
        metadata.extension().is_some_and(|extension| case_idents_match(extension, name, case))
    };
    if let Some((function, _)) = builder
        .functions()
        .iter()
        .find(|(function, metadata)| belongs(metadata) && builder.is_function_used(function))
    {
        return Err(Error::FunctionReferenced {
            function_name: last_str(&function.name).to_string(),
        });
    }
    builder.functions_mut().retain(|(_, metadata)| !belongs(metadata));
    Ok(())
}

/// Records a `CREATE EXTENSION`, bound to the schema it lands in, together
/// with the functions it creates there.
///
/// # Errors
///
/// Returns [`Error::UnnamedObject`] for an empty name,
/// [`Error::SchemaNotFoundForExtension`] when `WITH SCHEMA` names a schema
/// the input never creates, [`Error::NoSchemaSelectedForExtension`] when no
/// schema is named and the search path selects none,
/// [`Error::ExtensionAlreadyExists`] when the database holds the name and the
/// statement wrote no `IF NOT EXISTS`, and [`Error::FunctionAlreadyExists`]
/// when the schema already holds a function the extension creates.
pub(super) fn create_extension(
    builder: ParserDBBuilder,
    mut extension: CreateExtension,
) -> Result<ParserDBBuilder, Error> {
    if extension.name.value.is_empty() {
        return Err(Error::UnnamedObject { object_kind: ObjectKind::Extension });
    }
    // PostgreSQL looks for the name before it resolves the schema, so a
    // repeated name is skipped or refused whatever schema it asks for.
    let case = builder.identifier_case();
    if builder
        .extensions()
        .iter()
        .any(|(existing, ())| case_idents_match(&existing.name, &extension.name, case))
    {
        if extension.if_not_exists {
            return Ok(builder);
        }
        return Err(Error::ExtensionAlreadyExists { extension_name: extension.name.value });
    }
    extension.schema = match extension.schema.take() {
        Some(schema) => Some(named_schema(&builder, schema, &extension.name)?),
        None => search_path_schema(&builder, &extension.name)?,
    };
    let builder = create_extension_functions(builder, &extension)?;
    Ok(builder.add_extension(Arc::new(extension), ()))
}

/// Applies a `DROP EXTENSION` to every extension it names, dropping the
/// functions each one created.
///
/// What depends on an extension's functions refuses the drop, as it refuses
/// `DROP FUNCTION`, so `CASCADE` and `RESTRICT` drop the same.
///
/// # Errors
///
/// Returns [`Error::DropExtensionNotFound`] for a name the database does not
/// hold, unless the statement wrote `IF EXISTS`, and
/// [`Error::FunctionReferenced`] when something still reaches a function the
/// extension created.
pub(super) fn drop_extensions(
    mut builder: ParserDBBuilder,
    drop: DropExtension,
) -> Result<ParserDBBuilder, Error> {
    let case = builder.identifier_case();
    for name in drop.names {
        let position = builder
            .extensions()
            .iter()
            .position(|(extension, ())| case_idents_match(&extension.name, &name, case));
        match position {
            Some(position) => {
                let (extension, ()) = builder.extensions_mut().remove(position);
                drop_extension_functions(&mut builder, &extension.name)?;
            }
            None if drop.if_exists => {}
            None => return Err(Error::DropExtensionNotFound { extension_name: name.value }),
        }
    }
    Ok(builder)
}

/// Whether the extension was created in this schema.
fn created_in(extension: &CreateExtension, schema_name: &str, schema_quoted: bool) -> bool {
    extension.schema.as_ref().is_some_and(|schema| {
        identifiers_match(&schema.value, schema.quote_style.is_some(), schema_name, schema_quoted)
    })
}

/// Whether any extension was created in this schema, which keeps a `DROP
/// SCHEMA` without `CASCADE` from removing it.
pub(super) fn schema_holds_extension(
    builder: &ParserDBBuilder,
    schema_name: &str,
    schema_quoted: bool,
) -> bool {
    builder
        .extensions()
        .iter()
        .any(|(extension, ())| created_in(extension, schema_name, schema_quoted))
}

/// Drops the extensions created in a schema a `DROP SCHEMA ... CASCADE`
/// removes, with the functions they created.
///
/// # Errors
///
/// Returns [`Error::FunctionReferenced`] when something still reaches a
/// function one of them created.
pub(super) fn drop_extensions_in(
    builder: &mut ParserDBBuilder,
    schema_name: &str,
    schema_quoted: bool,
) -> Result<(), Error> {
    let (doomed, kept) =
        core::mem::take(builder.extensions_mut()).into_iter().partition::<Vec<_>, _>(
            |(extension, ())| created_in(extension, schema_name, schema_quoted),
        );
    *builder.extensions_mut() = kept;
    for (extension, ()) in doomed {
        drop_extension_functions(builder, &extension.name)?;
    }
    Ok(())
}

/// Points the extensions created in a renamed schema at its new name.
pub(super) fn follow_schema_rename(
    builder: &mut ParserDBBuilder,
    (from, from_quoted): (&str, bool),
    (to, to_quoted): (&str, bool),
) {
    for (extension, ()) in builder.extensions_mut() {
        if created_in(extension, from, from_quoted) {
            Arc::make_mut(extension).schema = Some(schema_ident(to, to_quoted));
        }
    }
}

/// The identifier a stored schema name is written with.
fn schema_ident(name: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', name) } else { Ident::new(name) }
}

/// Checks the schema `WITH SCHEMA` names, keeping it as written.
///
/// `public` and `pg_catalog` exist in every database. `pg_temp` is refused,
/// since PostgreSQL resolves the spelling to no schema here.
fn named_schema(
    builder: &ParserDBBuilder,
    schema: Ident,
    extension_name: &Ident,
) -> Result<Ident, Error> {
    let (name, quoted) = (schema.value.as_str(), schema.quote_style.is_some());
    if identifiers_match(name, quoted, "public", false)
        || identifiers_match(name, quoted, PG_CATALOG, false)
        || declared_schema(builder, name, quoted).is_some()
    {
        return Ok(schema);
    }
    Err(Error::SchemaNotFoundForExtension {
        schema_name: schema.value,
        extension_name: extension_name.value.clone(),
    })
}

/// The schema the search path selects for an extension naming none, or
/// [`None`] for the default schema, which this model leaves unwritten.
///
/// PostgreSQL takes the first schema on the path that exists, and unlike a
/// relation an extension may land in `pg_catalog` when the path names it.
fn search_path_schema(
    builder: &ParserDBBuilder,
    extension_name: &Ident,
) -> Result<Option<Ident>, Error> {
    for (entry, quoted) in builder.search_path().filter(|(entry, _)| !entry.is_empty()) {
        if identifiers_match(entry, quoted, "public", false) {
            return Ok(None);
        }
        if is_temporary_schema(entry, quoted) {
            return Ok(Some(Ident::new(TEMPORARY_SCHEMA)));
        }
        if identifiers_match(entry, quoted, PG_CATALOG, false) {
            return Ok(Some(Ident::new(PG_CATALOG)));
        }
        // The catalog spelling wins over the one the path used, since the two
        // only differ where quoting makes them the same name anyway.
        if let Some(schema) = declared_schema(builder, entry, quoted) {
            return Ok(Some(schema_ident(schema.name(), schema.is_quoted())));
        }
    }
    Err(Error::NoSchemaSelectedForExtension { extension_name: extension_name.value.clone() })
}
