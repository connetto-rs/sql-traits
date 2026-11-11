//! Recording, binding and dropping domains.
//!
//! Every rule here was measured against PostgreSQL 18.6 in Docker rather than
//! read from documentation.
//!
//! A domain lands where a relation would, in the schema it names or in the
//! first schema on the search path the input creates. Its name comes from the
//! pool of type names of that schema, which holds every other domain and the
//! row type of every table, view and materialized view. A sequence has no row
//! type, yet a new sequence may not take a name a domain holds, while an index
//! shares nothing with a type.
//!
//! A type name written bare is looked for in the temporary schema, then in
//! `pg_catalog`, then along the search path, unless the path lists either of
//! the first two where it wants them. The first schema holding a type of that
//! name ends the walk, whether the type is a domain or a row type. Every type
//! name a column, a domain or a function's signature is declared with is
//! bound to the domain it reached when the statement ran, so neither a later
//! path nor a later domain of the same name changes which domain it names.
//!
//! Dropping a domain is refused while a column, another domain or a
//! function's signature is declared with it, an array of it included.
//! `CASCADE` drops those columns, taking along what dropping a column takes,
//! those domains, with their own columns and domains in turn, and those
//! functions. `DROP TYPE` drops a domain as `DROP DOMAIN`
//! does, and passes over a name reaching anything else, since the other kinds
//! of type are not modelled.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{ArrayElemTypeDef, CreateDomain, DataType, Ident, ObjectName};

use super::{
    ParserDB, ParserDBBuilder, PostgresCatalog, SchemaQualifier, StoredTable, bind_reference,
    object_name_last_identifier, place_relation, relation_name_holder, require_named_in_catalog,
    type_arrays::TypeArrays,
};
use crate::{
    errors::{Error, ObjectKind},
    impls::{bound_domain, bound_user_type, domain_in_schema, domain_named, user_type_holds},
    structs::UserDefinedType,
    traits::{DomainLike, UserDefinedTypeLike},
    utils::{
        identifier_resolution::{identifiers_match, is_temporary_schema, stored_identifier},
        object_name::{
            PG_CATALOG, Qualifier, TEMPORARY_SCHEMA, object_name_identifiers, qualifier_of,
        },
        postgres_types::names_catalog_type,
    },
};

/// The schema a domain or a relation stored without a qualifier lives in.
const DEFAULT_SCHEMA: &str = "public";

/// Which statement spelled a drop, which decides what a name reaching
/// something other than a domain does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DropSpelling {
    /// `DROP DOMAIN`, which drops nothing but a domain.
    Domain,
    /// `DROP TYPE`, which drops a domain as well as the kinds of type this
    /// model holds.
    Type,
}

/// What a type name reaches in the first schema holding a type of that name.
pub(super) enum Reached {
    /// The domain at this position among the stored domains.
    Domain(usize),
    /// The user-defined type at this position, with whether the name is its
    /// multirange's.
    UserType(usize, bool),
    /// The row type of a relation of this kind.
    RowType(ObjectKind),
    /// A type of `pg_catalog`.
    Catalog,
}

/// Records a `CREATE DOMAIN`, binding the type it is declared over. A
/// generated array holding the name is displaced before the domain fills it,
/// and the domain's own array is registered once it enters the namespace.
///
/// # Errors
///
/// Refuses a name another domain or the row type of a relation holds in the
/// schema, and a schema the input never creates.
pub(super) fn create_domain(
    mut builder: ParserDBBuilder,
    mut domain: CreateDomain,
    role: Option<&str>,
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<ParserDBBuilder, Error> {
    require_named_in_catalog(&mut domain.name, ObjectKind::Domain, builder.catalog_name())?;
    place_relation(&builder, &mut domain.name, false, ObjectKind::Domain, role)?;
    let name = object_name_last_identifier(&domain.name)
        .ok_or(Error::UnnamedObject { object_kind: ObjectKind::Domain })?;
    let schema = qualifier_of(&domain.name).named();
    // A domain takes a shell of its name over, filling the type name.
    super::user_defined_types::take_over_shell(&mut builder, name, schema);
    let holder = type_name_holder(&builder, name, schema);
    if let Some(conflicting_kind) = holder {
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::Domain,
            conflicting_kind,
            object_name: name.value.clone(),
        });
    }
    if let Some((previous, current)) = Arc::make_mut(arrays).displace(&builder, &domain.name) {
        Arc::make_mut(catalog).rename_type_identity(&previous, current);
    }
    bind_type(&builder, &mut domain.data_type, arrays)?;
    let calls = builder.calls_in(&domain);
    let node = Arc::new(domain);
    let builder = builder.add_domain(Arc::clone(&node), calls);
    Arc::make_mut(arrays).create(&builder, &node.name);
    Ok(builder)
}

/// Returns [`ObjectKind::Domain`] when a domain holds `name` in `schema`.
pub(super) fn domain_name_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    let case = builder.identifier_case();
    let schema = schema.unwrap_or((DEFAULT_SCHEMA, false));
    builder
        .domains()
        .iter()
        .any(|(domain, _)| {
            domain_named(domain, ident_parts(name), case) && domain_in_schema(domain, schema, case)
        })
        .then_some(ObjectKind::Domain)
}

/// Returns the kind of relation or domain already holding `name` in
/// `schema`, which neither a view nor a sequence may take.
pub(super) fn relation_or_domain_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    relation_name_holder(builder, name, schema)
        .or_else(|| domain_name_holder(builder, name, schema))
}

/// Returns the kind of type a relation may not take, a domain or a filled
/// user-defined type the relation's own name would clash with in the pool of
/// type names. A row type is left out, since another relation holding it is a
/// relation name the relation pool already names.
pub(super) fn domain_or_user_type_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    let case = builder.identifier_case();
    let schema_tuple = schema.unwrap_or((DEFAULT_SCHEMA, false));
    domain_name_holder(builder, name, schema).or_else(|| {
        builder
            .user_defined_types()
            .iter()
            .any(|(type_, _)| {
                !type_.is_shell()
                    && user_type_holds(type_, ident_parts(name), schema_tuple, case).is_some()
            })
            .then_some(ObjectKind::Type)
    })
}

/// Returns the kind of type a relation rename may not take, a domain or a
/// user-defined type, its multirange included, which a rename cannot take
/// over the way a creation fills a shell.
pub(super) fn rename_type_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    if let Some(kind) = domain_name_holder(builder, name, schema) {
        return Some(kind);
    }
    let case = builder.identifier_case();
    let schema_tuple = schema.unwrap_or((DEFAULT_SCHEMA, false));
    builder.user_defined_types().iter().find_map(|(type_, _)| {
        user_type_holds(type_, ident_parts(name), schema_tuple, case).map(|_| ObjectKind::Type)
    })
}

/// Returns the kind of type already holding `name` in the pool of type names
/// of `schema`, a domain, a filled user-defined type, or the row type of a
/// relation, which a new type, domain or relation may not take.
///
/// A shell is left out, since a creation of any kind takes it over or fills
/// it rather than colliding with it.
pub(super) fn type_name_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    domain_or_user_type_holder(builder, name, schema)
        .or_else(|| relation_name_holder(builder, name, schema).filter(|kind| has_row_type(*kind)))
}

/// Whether a relation of this kind carries a row type, whose name is taken in
/// the pool of type names.
const fn has_row_type(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView)
}

/// An identifier as its value and whether it was quoted.
fn ident_parts(ident: &Ident) -> (&str, bool) {
    (ident.value.as_str(), ident.quote_style.is_some())
}

/// The identifier a stored schema name is written with.
fn schema_ident(name: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', name) } else { Ident::new(name) }
}

/// What a bare type name reaches through the session's path.
pub(super) fn reach_type(builder: &ParserDBBuilder, name: &Ident) -> Option<Reached> {
    reach_type_in(builder, name).map(|(reached, _)| reached)
}

/// What a bare type name reaches through the session's path, with the schema
/// that holds it.
fn reach_type_in<'builder>(
    builder: &'builder ParserDBBuilder,
    name: &Ident,
) -> Option<(Reached, (&'builder str, bool))> {
    let temporary =
        (!builder.search_path().any(|(schema, quoted)| is_temporary_schema(schema, quoted)))
            .then_some((TEMPORARY_SCHEMA, false));
    let catalog = (!builder
        .search_path()
        .any(|(schema, quoted)| identifiers_match(schema, quoted, PG_CATALOG, false)))
    .then_some((PG_CATALOG, false));
    temporary
        .into_iter()
        .chain(catalog)
        .chain(builder.search_path())
        .filter(|(schema, _)| !schema.is_empty())
        .find_map(|schema| reach_in_schema(builder, name, schema).map(|reached| (reached, schema)))
}

/// What a type name reaches in one schema, if the schema holds a type of that
/// name.
pub(super) fn reach_in_schema(
    builder: &ParserDBBuilder,
    name: &Ident,
    (schema, schema_quoted): (&str, bool),
) -> Option<Reached> {
    let case = builder.identifier_case();
    if identifiers_match(schema, schema_quoted, PG_CATALOG, false)
        && names_catalog_type(&stored_identifier(&name.value, name.quote_style.is_some()))
    {
        return Some(Reached::Catalog);
    }
    let schema = if is_temporary_schema(schema, schema_quoted) {
        (TEMPORARY_SCHEMA, false)
    } else {
        (schema, schema_quoted)
    };
    if let Some(position) = builder.domains().iter().position(|(domain, _)| {
        domain_named(domain, ident_parts(name), case) && domain_in_schema(domain, schema, case)
    }) {
        return Some(Reached::Domain(position));
    }
    if let Some((position, is_multirange)) =
        builder.user_defined_types().iter().enumerate().find_map(|(position, (type_, _))| {
            user_type_holds(type_, ident_parts(name), schema, case)
                .map(|is_multirange| (position, is_multirange))
        })
    {
        return Some(Reached::UserType(position, is_multirange));
    }
    relation_name_holder(builder, name, Some(schema))
        .filter(|kind| has_row_type(*kind))
        .map(Reached::RowType)
}

/// Defines `$name`, which answers the type name a declared type names, or
/// its innermost array element names, through a shared reference, or through
/// a mutable one when given `mut`.
macro_rules! innermost_type_name {
    ($(#[$doc:meta])* $name:ident $($mutability:tt)?) => {
        $(#[$doc])*
        pub(super) fn $name(
            data_type: &$($mutability)? DataType,
        ) -> Option<&$($mutability)? ObjectName> {
            match data_type {
                DataType::Custom(name, _) => Some(name),
                DataType::Array(
                    ArrayElemTypeDef::AngleBracket(element)
                    | ArrayElemTypeDef::SquareBracket(element, _)
                    | ArrayElemTypeDef::Parenthesis(element)
                    | ArrayElemTypeDef::ParenthesisNotNull(element)
                    | ArrayElemTypeDef::Qualified(element, _),
                ) => $name(element),
                _ => None,
            }
        }
    };
}

innermost_type_name!(
    /// The type name a type, or its innermost array element, names.
    custom_name
);
innermost_type_name!(
    /// [`custom_name`], for rewriting.
    custom_name_mut mut
);

/// Binds a declared type naming a domain or a user-defined type, or an array
/// of either, to the type the session's path reaches now, writing that type's
/// schema into the name. A name reaching a generated array binds to its
/// element first, qualified with the schema the element holds in.
///
/// A type of the default schema stays bare, unless `pg_catalog` holds a type of
/// its name, which a bare name would reach instead.
///
/// # Errors
///
/// Returns [`Error::TypeIsOnlyShell`] for a type that reaches a shell, a name
/// held by a bare `CREATE TYPE name;` and never filled.
pub(super) fn bind_type(
    builder: &ParserDBBuilder,
    data_type: &mut DataType,
    arrays: &TypeArrays,
) -> Result<(), Error> {
    arrays.bind_type(builder, data_type);
    if builder.domains().is_empty() && builder.user_defined_types().is_empty() {
        return Ok(());
    }
    let Some(name) = custom_name_mut(data_type) else {
        return Ok(());
    };
    let Ok((None, ident)) = object_name_identifiers(name) else {
        return Ok(());
    };
    let Some(reached) = reach_type(builder, ident) else {
        return Ok(());
    };
    let qualifier = match reached {
        Reached::Domain(position) => domain_qualifier(&builder.domains()[position].0),
        Reached::UserType(position, _is_multirange) => {
            let type_ = &builder.user_defined_types()[position].0;
            if type_.is_shell() {
                return Err(Error::TypeIsOnlyShell { type_name: type_.name().to_string() });
            }
            user_type_qualifier(type_)
        }
        Reached::RowType(_) | Reached::Catalog => return Ok(()),
    };
    bind_reference(name, qualifier);
    Ok(())
}

/// Binds a type a function's signature declares, as [`bind_type`] binds a
/// column's, reaching the row type of a relation too and keeping a shell,
/// which a signature may name.
pub(super) fn bind_signature_type(
    builder: &ParserDBBuilder,
    data_type: &mut DataType,
    arrays: &TypeArrays,
) {
    arrays.bind_type(builder, data_type);
    let Some(name) = custom_name_mut(data_type) else {
        return;
    };
    let Ok((None, ident)) = object_name_identifiers(name) else {
        return;
    };
    let Some((reached, (schema, quoted))) = reach_type_in(builder, ident) else {
        return;
    };
    let qualifier = match reached {
        Reached::Domain(position) => domain_qualifier(&builder.domains()[position].0),
        Reached::UserType(position, _is_multirange) => {
            user_type_qualifier(&builder.user_defined_types()[position].0)
        }
        Reached::RowType(_) => {
            let default = identifiers_match(schema, quoted, DEFAULT_SCHEMA, false)
                && !names_catalog_type(&stored_identifier(
                    &ident.value,
                    ident.quote_style.is_some(),
                ));
            (!default).then(|| schema_ident(schema, quoted))
        }
        Reached::Catalog => return,
    };
    bind_reference(name, qualifier);
}

/// The qualifier a name binding to `domain` takes, none for the default
/// schema unless `pg_catalog` holds a type of its name.
fn domain_qualifier(domain: &CreateDomain) -> Option<Ident> {
    match domain.schema() {
        Some(schema) => Some(schema_ident(schema, domain.schema_is_quoted())),
        None if names_catalog_type(&domain.stored_name()) => Some(Ident::new(DEFAULT_SCHEMA)),
        None => None,
    }
}

/// The qualifier a name binding to the user-defined type `type_` takes.
fn user_type_qualifier(type_: &UserDefinedType) -> Option<Ident> {
    type_.schema().map(|schema| schema_ident(schema, type_.schema_is_quoted()))
}

/// Refuses a type a definition declares over when it names a shell, keeping
/// an unknown name the input never creates, as a column and a domain do.
///
/// # Errors
///
/// Returns [`Error::TypeIsOnlyShell`] for a name that reaches a shell.
pub(super) fn require_existing_type(
    builder: &ParserDBBuilder,
    data_type: &DataType,
) -> Result<(), Error> {
    let Some(name) = custom_name(data_type) else {
        return Ok(());
    };
    let Ok((_, ident)) = object_name_identifiers(name) else {
        return Ok(());
    };
    if names_catalog_type(&stored_identifier(&ident.value, ident.quote_style.is_some())) {
        return Ok(());
    }
    // A shell types no value. A bare name reaches a shell of another schema
    // on the path only through [`bind_type`], which runs before this.
    let types = builder.user_defined_types().iter().map(|(type_, _)| type_.as_ref());
    if let Some((type_, _multirange)) = bound_user_type(types, name, builder.identifier_case())
        && type_.is_shell()
    {
        return Err(Error::TypeIsOnlyShell { type_name: name.to_string() });
    }
    Ok(())
}

/// The position among the stored domains of the domain a declared type names,
/// directly or as the element of an array.
pub(super) fn named_domain(builder: &ParserDBBuilder, data_type: &DataType) -> Option<usize> {
    let name = custom_name(data_type)?;
    let domain = bound_domain(
        builder.domains().iter().map(|(domain, _)| domain.as_ref()),
        name,
        builder.identifier_case(),
    )?;
    builder.domains().iter().position(|(stored, _)| core::ptr::eq(stored.as_ref(), domain))
}

/// Whether a domain lives in the schema.
pub(super) fn schema_holds_domain(
    builder: &ParserDBBuilder,
    schema_name: &str,
    schema_quoted: bool,
) -> bool {
    let case = builder.identifier_case();
    builder
        .domains()
        .iter()
        .any(|(domain, _)| domain_in_schema(domain, (schema_name, schema_quoted), case))
}

/// Points the domains of a renamed schema, and every declared type naming one
/// of them, at the new name.
///
/// # Errors
///
/// Returns the errors rebuilding a table whose column names such a domain
/// returns.
pub(super) fn follow_schema_rename(
    mut builder: ParserDBBuilder,
    (from, from_quoted): (&str, bool),
    (to, to_quoted): (&str, bool),
) -> Result<ParserDBBuilder, Error> {
    let case = builder.identifier_case();
    let moved: Vec<(alloc::string::String, bool)> = builder
        .domains()
        .iter()
        .filter(|(domain, _)| domain_in_schema(domain, (from, from_quoted), case))
        .map(|(domain, _)| (domain.name().to_string(), domain.name_is_quoted()))
        .collect();
    if moved.is_empty() {
        return Ok(builder);
    }
    let reaches_moved = |name: &ObjectName| {
        let Qualifier::Named(schema, schema_quoted) = qualifier_of(name) else {
            return false;
        };
        object_name_last_identifier(name).is_some_and(|ident| {
            case.identifiers_match(schema, schema_quoted, from, from_quoted)
                && moved.iter().any(|(moved, moved_quoted)| {
                    case.identifiers_match(
                        moved,
                        *moved_quoted,
                        &ident.value,
                        ident.quote_style.is_some(),
                    )
                })
        })
    };
    let retarget = |name: &mut ObjectName| {
        if reaches_moved(name) {
            bind_reference(name, Some(schema_ident(to, to_quoted)));
        }
    };

    for (domain, _) in builder.domains_mut() {
        let renamed = reaches_moved(&domain.name)
            || custom_name(&domain.data_type).is_some_and(reaches_moved);
        if !renamed {
            continue;
        }
        let domain = Arc::make_mut(domain);
        retarget(&mut domain.name);
        if let Some(name) = custom_name_mut(&mut domain.data_type) {
            retarget(name);
        }
    }
    super::signature_types::retarget_signatures(&mut builder, reaches_moved, retarget);

    let hosts: Vec<StoredTable> = builder
        .tables()
        .iter()
        .filter(|(table, _, _)| {
            table
                .columns
                .iter()
                .any(|column| custom_name(&column.data_type).is_some_and(reaches_moved))
        })
        .map(|(table, _, _)| StoredTable::of(table))
        .collect();
    for host in &hosts {
        builder = ParserDB::replace_table_node(builder, host, |_, node| {
            for column in &mut node.columns {
                if let Some(name) = custom_name_mut(&mut column.data_type) {
                    retarget(name);
                }
            }
            Ok(())
        })?;
    }
    Ok(builder)
}
