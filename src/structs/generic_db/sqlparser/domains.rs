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
//! name a column or a domain is declared with is bound to the domain it reached
//! when the statement ran, so neither a later path nor a later domain of the
//! same name changes which domain it names.
//!
//! Dropping a domain is refused while a column or another domain is declared
//! with it, an array of it included. `CASCADE` drops those columns, taking
//! along what dropping a column takes, and those domains, with their own
//! columns and domains in turn. `DROP TYPE` drops a domain as `DROP DOMAIN`
//! does, and passes over a name reaching anything else, since the other kinds
//! of type are not modelled.

use alloc::{string::ToString, sync::Arc, vec::Vec};

use sqlparser::ast::{ArrayElemTypeDef, CreateDomain, DataType, Ident, ObjectName};

use super::{
    AlterTableScope, ParserDB, ParserDBBuilder, SchemaQualifier, StoredTable, bind_reference,
    inheritance, object_name_last_identifier, place_relation, relation_name_holder,
    require_named_in_catalog,
};
use crate::{
    errors::{Error, ObjectKind},
    impls::{bound_domain, domain_in_schema, domain_named},
    traits::DomainLike,
    utils::{
        identifier_resolution::{identifiers_match, is_temporary_schema, normalize_identifier},
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
    /// model does not hold.
    Type,
}

/// What a type name reaches in the first schema holding a type of that name.
enum Reached {
    /// The domain at this position among the stored domains.
    Domain(usize),
    /// The row type of a relation of this kind.
    RowType(ObjectKind),
    /// A type of `pg_catalog`.
    Catalog,
}

/// Records a `CREATE DOMAIN`, binding the type it is declared over.
///
/// # Errors
///
/// Refuses a name another domain or the row type of a relation holds in the
/// schema, and a schema the input never creates.
pub(super) fn create_domain(
    builder: ParserDBBuilder,
    mut domain: CreateDomain,
    role: Option<&str>,
) -> Result<ParserDBBuilder, Error> {
    require_named_in_catalog(&mut domain.name, ObjectKind::Domain, builder.catalog_name())?;
    place_relation(&builder, &mut domain.name, false, ObjectKind::Domain, role)?;
    let name = object_name_last_identifier(&domain.name)
        .ok_or(Error::UnnamedObject { object_kind: ObjectKind::Domain })?;
    let schema = qualifier_of(&domain.name).named();
    let holder = domain_name_holder(&builder, name, schema).or_else(|| {
        relation_name_holder(&builder, name, schema).filter(|kind| has_row_type(*kind))
    });
    if let Some(conflicting_kind) = holder {
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::Domain,
            conflicting_kind,
            object_name: name.value.clone(),
        });
    }
    bind_type(&builder, &mut domain.data_type);
    let calls = builder.calls_in(&domain);
    Ok(builder.add_domain(Arc::new(domain), calls))
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
fn reach_type(builder: &ParserDBBuilder, name: &Ident) -> Option<Reached> {
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
        .find_map(|schema| reach_in_schema(builder, name, schema))
}

/// What a type name reaches in one schema, if the schema holds a type of that
/// name.
fn reach_in_schema(
    builder: &ParserDBBuilder,
    name: &Ident,
    (schema, schema_quoted): (&str, bool),
) -> Option<Reached> {
    let case = builder.identifier_case();
    if identifiers_match(schema, schema_quoted, PG_CATALOG, false)
        && names_catalog_type(&normalize_identifier(&name.value, name.quote_style.is_some()))
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
        fn $name(data_type: &$($mutability)? DataType) -> Option<&$($mutability)? ObjectName> {
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

/// Binds a declared type naming a domain, or an array of one, to the domain
/// the session's path reaches now, writing that domain's schema into the
/// name.
///
/// A domain of the default schema stays bare, unless `pg_catalog` holds a type
/// of its name, which a bare name would reach instead.
pub(super) fn bind_type(builder: &ParserDBBuilder, data_type: &mut DataType) {
    if builder.domains().is_empty() {
        return;
    }
    let Some(name) = custom_name_mut(data_type) else {
        return;
    };
    let Ok((None, ident)) = object_name_identifiers(name) else {
        return;
    };
    let Some(Reached::Domain(position)) = reach_type(builder, ident) else {
        return;
    };
    let domain = &builder.domains()[position].0;
    let qualifier = match domain.schema() {
        Some(schema) => Some(schema_ident(schema, domain.schema_is_quoted())),
        None if names_catalog_type(&domain.stored_name()) => Some(Ident::new(DEFAULT_SCHEMA)),
        None => None,
    };
    bind_reference(name, qualifier);
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

/// Applies a `DROP DOMAIN` or a `DROP TYPE`.
///
/// # Errors
///
/// Refuses a `DROP DOMAIN` naming no domain without `IF EXISTS`, one naming
/// the row type of a relation, and, without `CASCADE`, a domain a column or
/// another domain is declared with.
pub(super) fn drop_domains(
    builder: ParserDBBuilder,
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
    spelling: DropSpelling,
) -> Result<ParserDBBuilder, Error> {
    let mut doomed = Vec::with_capacity(names.len());
    for name in names {
        let (schema, ident) = object_name_identifiers(name)?;
        let reached = match schema {
            Some(schema) => reach_in_schema(&builder, ident, ident_parts(schema)),
            None => reach_type(&builder, ident),
        };
        // PostgreSQL refuses a name reaching a type that is no domain,
        // `IF EXISTS` or not, and skips only a name reaching nothing.
        let skipped = if_exists && reached.is_none();
        match (reached, spelling) {
            (Some(Reached::Domain(position)), _) => doomed.push(position),
            (Some(Reached::RowType(actual_kind)), DropSpelling::Domain) => {
                return Err(Error::RelationKindMismatch {
                    object_name: name.to_string(),
                    expected_kind: ObjectKind::Domain,
                    actual_kind,
                });
            }
            (Some(Reached::Catalog) | None, DropSpelling::Domain) if !skipped => {
                return Err(Error::DropDomainNotFound { domain_name: name.to_string() });
            }
            _ => {}
        }
    }
    drop_positions(builder, doomed, cascade)
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

/// Drops every domain of a schema `DROP SCHEMA ... CASCADE` removes, with what
/// a cascading drop of each takes.
///
/// # Errors
///
/// Returns the errors dropping a column a domain declares returns.
pub(super) fn drop_domains_in(
    builder: ParserDBBuilder,
    schema_name: &str,
    schema_quoted: bool,
) -> Result<ParserDBBuilder, Error> {
    let case = builder.identifier_case();
    let doomed = builder
        .domains()
        .iter()
        .enumerate()
        .filter(|(_, (domain, _))| domain_in_schema(domain, (schema_name, schema_quoted), case))
        .map(|(position, _)| position)
        .collect();
    drop_positions(builder, doomed, true)
}

/// Drops the domains at `doomed`, refusing what uses them unless `cascade`,
/// in which case the domains and columns using them go too.
fn drop_positions(
    mut builder: ParserDBBuilder,
    mut doomed: Vec<usize>,
    cascade: bool,
) -> Result<ParserDBBuilder, Error> {
    if doomed.is_empty() {
        return Ok(builder);
    }
    let domain_name =
        |builder: &ParserDBBuilder, position: usize| builder.domains()[position].0.name.to_string();

    // Every domain declared over a doomed one, followed to the end of the
    // chain.
    let mut grew = true;
    while grew {
        grew = false;
        for (position, (domain, _)) in builder.domains().iter().enumerate() {
            if doomed.contains(&position) {
                continue;
            }
            let Some(used) = named_domain(&builder, &domain.data_type) else {
                continue;
            };
            if !doomed.contains(&used) {
                continue;
            }
            if !cascade {
                return Err(Error::DomainHasDependents {
                    domain_name: domain_name(&builder, used),
                    dependent_kind: ObjectKind::Domain,
                    dependent_name: domain.name.to_string(),
                });
            }
            doomed.push(position);
            grew = true;
        }
    }

    let uses_doomed = |builder: &ParserDBBuilder, data_type: &DataType| {
        named_domain(builder, data_type).filter(|used| doomed.contains(used))
    };

    if !cascade {
        for (table, _, _) in builder.tables() {
            for column in &table.columns {
                if let Some(used) = uses_doomed(&builder, &column.data_type) {
                    return Err(Error::DomainHasDependents {
                        domain_name: domain_name(&builder, used),
                        dependent_kind: ObjectKind::Column,
                        dependent_name: alloc::format!("{}.{}", table.name, column.name),
                    });
                }
            }
        }
    }

    // A column a parent passes down leaves with the parent's, so only the
    // columns a table declares itself are dropped, one at a time, each drop
    // taking what dropping a column takes.
    loop {
        let next = builder.tables().iter().find_map(|(table, _, _)| {
            table
                .columns
                .iter()
                .find(|column| {
                    uses_doomed(&builder, &column.data_type).is_some()
                        && !inheritance::is_inherited_column(&builder, table, &column.name)
                })
                .map(|column| (qualified_table_name(&table.name), column.name.clone()))
        });
        let Some((table_name, column_name)) = next else {
            break;
        };
        builder = ParserDB::alter_table_drop_columns(
            builder,
            &table_name,
            AlterTableScope { if_exists: false, only: false },
            &[column_name],
            false,
            true,
        )?;
    }

    let mut position = 0;
    builder.domains_mut().retain(|_| {
        let kept = !doomed.contains(&position);
        position += 1;
        kept
    });
    Ok(builder)
}

/// A stored table name with the default schema written out, so a lookup
/// reaches that table whatever the session's path holds.
fn qualified_table_name(name: &ObjectName) -> ObjectName {
    let mut qualified = name.clone();
    if matches!(qualifier_of(name), Qualifier::Absent) {
        bind_reference(&mut qualified, Some(Ident::new(DEFAULT_SCHEMA)));
    }
    qualified
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
