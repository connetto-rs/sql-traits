//! Recording, binding and dropping user-defined types, as `CREATE TYPE`,
//! `ALTER TYPE` and `DROP TYPE` apply them.
//!
//! Every rule here was measured against PostgreSQL 18.6 in Docker rather than
//! read from documentation.
//!
//! A user-defined type lands where a relation would, in the schema it names or
//! in the first schema on the search path the input creates. Its name comes
//! from the same pool of type names as a domain, which holds every other
//! domain and user-defined type, the row type of every table, view and
//! materialized view, and a range's multirange. A composite type takes a
//! relation name too, which an enum, a range and a base type do not, so a new
//! relation may not take a composite's name while it may take an enum's.
//!
//! The bare `CREATE TYPE name;` records a shell, which holds the name until a
//! later definition fills it or a domain or relation takes it over, and which
//! a declared value may not name. `DROP TYPE` drops a domain as `DROP DOMAIN`
//! does, and the user-defined types this model holds as well, refusing what
//! uses them without `CASCADE` and dropping the columns, composite
//! attributes, domains and ranges that use them, and the functions whose
//! signatures name them, with it. A range brings the constructor functions
//! [`range_constructors`] records, which go with it.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use sqlparser::ast::{
    AlterType, AlterTypeOperation, CreateFunction, DataType, Ident, ObjectName, ObjectNamePart,
    UserDefinedTypeCompositeAttributeDef, UserDefinedTypeRangeOption,
    UserDefinedTypeRepresentation, UserDefinedTypeSqlDefinitionOption,
};

use super::{
    CollationResolution, CreatedCollationMetadata, DefaultPrivileges, ParserDB, ParserDBBuilder,
    SchemaQualifier, bind_reference, collation_resolution_for_name,
    domains::{
        DropSpelling, Reached, bind_type, custom_name, custom_name_mut, reach_in_schema,
        reach_type, require_existing_type,
    },
    object_name_last_identifier, place_relation,
    postgres_catalog::{CatalogDependent, PostgresCatalog},
    range_constructors, relation_name_holder, require_named_in_catalog,
    signature_types::{retarget_signatures, signature_types},
    type_arrays::TypeArrays,
};
use crate::{
    errors::{Error, LookupError, ObjectKind},
    impls::{domain_in_schema, user_type_in_schema, user_type_named},
    structs::{AttributeCollation, IdentifierCase, UserDefinedType, metadata::ExpressionCalls},
    traits::{DomainLike, UserDefinedTypeKind, UserDefinedTypeLike},
    utils::{
        identifier_resolution::stored_identifier,
        object_name::{Qualifier, object_name_identifiers, qualifier_of},
    },
};

/// The schema a type stored without a qualifier lives in.
const DEFAULT_SCHEMA: &str = "public";

/// An identifier as its value and whether it was quoted.
fn ident_parts(ident: &Ident) -> (&str, bool) {
    (ident.value.as_str(), ident.quote_style.is_some())
}

/// The identifier a stored schema name is written with.
fn schema_ident(name: &str, quoted: bool) -> Ident {
    if quoted { Ident::with_quote('"', name) } else { Ident::new(name) }
}

/// A stored object a `DROP TYPE` or `DROP DOMAIN` statement dooms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Doomed {
    /// The domain at this position among the stored domains.
    Domain(usize),
    /// The user-defined type at this position.
    UserType(usize),
}

/// Whether `doomed` holds the domain at `position`.
fn doomed_domain(doomed: &[Doomed], position: usize) -> bool {
    doomed.contains(&Doomed::Domain(position))
}

/// Whether `doomed` holds the user-defined type at `position`.
fn doomed_user_type(doomed: &[Doomed], position: usize) -> bool {
    doomed.contains(&Doomed::UserType(position))
}

/// The type identities and functions removed by a dependency cascade.
#[derive(Default)]
pub(super) struct TypeDrop {
    /// Removed owners and their stored arrays, captured before removal.
    pub(super) types: Vec<ObjectName>,
    /// Removed constructors, signature dependents, and reader functions.
    pub(super) functions: Vec<Arc<CreateFunction>>,
}

/// Whether a composite user type holds `name` in `schema`, which the relation
/// pool a composite takes a name from.
pub(super) fn composite_name_holder(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> Option<ObjectKind> {
    composites_named(builder, name, schema).next().map(|_| ObjectKind::Type)
}

/// The composite user types holding `name` in `schema`.
fn composites_named<'builder>(
    builder: &'builder ParserDBBuilder,
    name: &'builder Ident,
    schema: SchemaQualifier<'builder>,
) -> impl Iterator<Item = &'builder UserDefinedType> {
    let case = builder.identifier_case();
    let schema = schema.unwrap_or((DEFAULT_SCHEMA, false));
    builder.user_defined_types().iter().map(|(type_, _)| type_.as_ref()).filter(move |type_| {
        type_.kind() == UserDefinedTypeKind::Composite
            && user_type_named(type_, ident_parts(name), case)
            && user_type_in_schema(type_, schema, case)
    })
}

/// Checks that the composite type `name` in `schema` has the attribute
/// `attribute`, for a `COMMENT ON COLUMN` naming it.
///
/// # Errors
///
/// Returns [`LookupError::ColumnNotFound`] when the composite has no
/// attribute of that name.
pub(super) fn require_composite_attribute(
    builder: &ParserDBBuilder,
    name: &Ident,
    schema: (&str, bool),
    attribute: &Ident,
) -> Result<(), Error> {
    let case = builder.identifier_case();
    let held = composites_named(builder, name, Some(schema))
        .flat_map(|composite| composite.composite_attributes().unwrap_or_default())
        .any(|held| {
            case.identifiers_match(
                &held.name.value,
                held.name.quote_style.is_some(),
                &attribute.value,
                attribute.quote_style.is_some(),
            )
        });
    if held {
        return Ok(());
    }
    Err(LookupError::ColumnNotFound {
        table_name: name.value.clone(),
        column_name: attribute.value.clone(),
    }
    .into())
}

/// Removes a shell of `name` in `schema`, so a domain or relation of that
/// name takes the shell over, and answers whether it did.
pub(super) fn take_over_shell(
    builder: &mut ParserDBBuilder,
    name: &Ident,
    schema: SchemaQualifier<'_>,
) -> bool {
    let case = builder.identifier_case();
    let schema = schema.unwrap_or((DEFAULT_SCHEMA, false));
    let positions: Vec<usize> = builder
        .user_defined_types()
        .iter()
        .enumerate()
        .filter(|(_, (type_, _))| {
            type_.is_shell()
                && user_type_named(type_, ident_parts(name), case)
                && user_type_in_schema(type_, schema, case)
        })
        .map(|(position, _)| position)
        .collect();
    if positions.is_empty() {
        return false;
    }
    let mut position = 0;
    builder.user_defined_types_mut().retain(|_| {
        let kept = !positions.contains(&position);
        position += 1;
        kept
    });
    true
}

/// Whether a user-defined type, or its multirange, lives in `schema`.
pub(super) fn schema_holds_type(
    builder: &ParserDBBuilder,
    schema_name: &str,
    schema_quoted: bool,
) -> bool {
    let case = builder.identifier_case();
    builder
        .user_defined_types()
        .iter()
        .any(|(type_, _)| user_type_in_schema(type_, (schema_name, schema_quoted), case))
}

/// The multirange a range type brings, named after the range's stored
/// (case-folded) name, replacing the first case-sensitive `range` with
/// `multirange` or appending `_multirange` when it writes none, under the
/// range's quoting.
fn multirange_from_range_name((value, quoted): (&str, bool)) -> ObjectName {
    let stored = stored_identifier(value, quoted);
    let multirange = if let Some(index) = stored.find("range") {
        let mut name = String::with_capacity(stored.len() + 3);
        name.push_str(&stored[..index]);
        name.push_str("multirange");
        name.push_str(&stored[index + "range".len()..]);
        name
    } else {
        alloc::format!("{stored}_multirange")
    };
    ObjectName::from(vec![schema_ident(&multirange, quoted)])
}

/// A function a type option declares, with the argument count PostgreSQL
/// resolves that option by.
type NamedFunction = (ObjectName, usize);

/// The function names the options of a base type carry, each with the
/// argument count PostgreSQL resolves the option by.
fn sql_definition_functions_of(
    options: &[UserDefinedTypeSqlDefinitionOption],
) -> Vec<NamedFunction> {
    options
        .iter()
        .filter_map(|option| {
            match option {
                UserDefinedTypeSqlDefinitionOption::Input(name)
                | UserDefinedTypeSqlDefinitionOption::Output(name)
                | UserDefinedTypeSqlDefinitionOption::Receive(name)
                | UserDefinedTypeSqlDefinitionOption::Send(name)
                | UserDefinedTypeSqlDefinitionOption::TypmodOut(name)
                | UserDefinedTypeSqlDefinitionOption::Analyze(name)
                | UserDefinedTypeSqlDefinitionOption::Subscript(name)
                | UserDefinedTypeSqlDefinitionOption::TypmodIn(name) => Some((name.clone(), 1)),
                _ => None,
            }
        })
        .collect()
}

/// The function names the options of a range carry, each with the argument
/// count PostgreSQL resolves the option by.
fn range_functions_of(options: &[UserDefinedTypeRangeOption]) -> Vec<NamedFunction> {
    options
        .iter()
        .filter_map(|option| {
            match option {
                UserDefinedTypeRangeOption::Canonical(name) => Some((name.clone(), 1)),
                UserDefinedTypeRangeOption::SubtypeDiff(name) => Some((name.clone(), 2)),
                _ => None,
            }
        })
        .collect()
}

/// The functions a type definition names, resolved to the functions they
/// reach when the type is written.
fn definition_function_calls(
    builder: &ParserDBBuilder,
    names: &[NamedFunction],
) -> ExpressionCalls<CreateFunction> {
    let functions = builder.function_arc_vec();
    let search_path: Vec<(&str, bool)> = builder.search_path().collect();
    let scope = super::calls_in_expression::CallScope {
        functions: &functions,
        builtins: builder.builtins(),
        search_path: &search_path,
        case: builder.identifier_case(),
    };
    let sites = names
        .iter()
        .map(|(name, arguments)| {
            scope
                .candidates(name)
                .filter(|function| scope.reaches(function, name, *arguments))
                .cloned()
                .collect()
        })
        .collect();
    ExpressionCalls::new(sites)
}

/// What a `CREATE TYPE` name reaches among the stored relations, and never the
/// catalog, whose schema `place_relation` refuses for a type before this
/// lookup.
enum CreationTarget {
    /// The user-defined type at this position.
    UserType(usize),
    /// A domain.
    Domain,
    /// The row type of a relation of this kind.
    RowType(ObjectKind),
}

/// The target a `CREATE TYPE` name reaches, or nothing for a free name; the
/// pg_catalog schema is refused before this lookup, so a name reaches no
/// catalog type.
fn creation_reaches(
    builder: &ParserDBBuilder,
    name: &Ident,
    (schema, schema_quoted): (&str, bool),
) -> Option<CreationTarget> {
    match reach_in_schema(builder, name, (schema, schema_quoted)) {
        Some(Reached::UserType(position, _)) => Some(CreationTarget::UserType(position)),
        Some(Reached::Domain(_)) => Some(CreationTarget::Domain),
        Some(Reached::RowType(kind)) => Some(CreationTarget::RowType(kind)),
        Some(Reached::Catalog) | None => None,
    }
}

/// Records a `CREATE TYPE`, binding the types and functions its definition
/// names, computing the multirange a range brings and recording a range's
/// constructors.
///
/// # Errors
///
/// Refuses a name another type, a domain or the row type of a relation holds
/// in the schema, a name a relation holds for a composite, a definition over
/// a shell or over a type it never creates, a range without a `SUBTYPE`, a
/// multirange name another type holds, and a range whose schema already
/// holds a function of a constructor's signature.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is ingestion state the creation reads or updates"
)]
pub(super) fn create_user_defined_type(
    builder: ParserDBBuilder,
    mut name: ObjectName,
    representation: Option<UserDefinedTypeRepresentation>,
    role: Option<&str>,
    defaults: &DefaultPrivileges,
    collations: &[CreatedCollationMetadata],
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<ParserDBBuilder, Error> {
    require_named_in_catalog(&mut name, ObjectKind::Type, builder.catalog_name())?;
    place_relation(&builder, &mut name, false, ObjectKind::Type, role)?;
    let name_ident = object_name_last_identifier(&name)
        .ok_or(Error::UnnamedObject { object_kind: ObjectKind::Type })?;
    let schema = qualifier_of(&name).named().unwrap_or((DEFAULT_SCHEMA, false));
    let is_composite =
        matches!(representation, Some(UserDefinedTypeRepresentation::Composite { .. }));
    // A composite takes a relation name, which the other kinds do not.
    if is_composite {
        let holder = relation_name_holder(&builder, name_ident, Some(schema))
            .or_else(|| composite_name_holder(&builder, name_ident, Some(schema)));
        if let Some(conflicting_kind) = holder {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind,
                object_name: name_ident.value.clone(),
            });
        }
    }
    // Only a free name or a shell this creation fills is taken, never a shell
    // over a shell.
    let shell_position = match creation_reaches(&builder, name_ident, schema) {
        Some(CreationTarget::UserType(position))
            if builder.user_defined_types()[position].0.is_shell() && representation.is_some() =>
        {
            Some(position)
        }
        Some(CreationTarget::UserType(_)) => {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Type,
                object_name: name_ident.value.clone(),
            });
        }
        Some(CreationTarget::Domain) => {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Domain,
                object_name: name_ident.value.clone(),
            });
        }
        Some(CreationTarget::RowType(conflicting_kind)) => {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind,
                object_name: name_ident.value.clone(),
            });
        }
        None => None,
    };
    if let Some((previous, current)) = Arc::make_mut(arrays).displace(&builder, &name) {
        Arc::make_mut(catalog).rename_type_identity(&previous, current);
    }
    let mut representation = representation;
    let (multirange_name, named_functions) = if let Some(representation) = representation.as_mut() {
        bind_representation(&builder, name_ident, schema, representation, arrays)?
    } else {
        (None, Vec::new())
    };
    if let Some(multirange) = &multirange_name
        && let Some((previous, current)) = Arc::make_mut(arrays).displace(&builder, multirange)
    {
        Arc::make_mut(catalog).rename_type_identity(&previous, current);
    }
    let calls = definition_function_calls(&builder, &named_functions);
    let attribute_collations =
        attribute_collations(&builder, representation.as_ref(), collations, catalog);
    let node =
        Arc::new(UserDefinedType { name, representation, multirange_name, attribute_collations });
    let mut builder = builder;
    if let Some(position) = shell_position {
        let mut seen = 0;
        builder.user_defined_types_mut().retain(|_| {
            let kept = seen != position;
            seen += 1;
            kept
        });
    }
    let builder = range_constructors::create_range_constructors(builder, &node, defaults)?;
    let builder = builder.add_user_defined_type(Arc::clone(&node), calls);
    if !node.is_shell() {
        let arrays = Arc::make_mut(arrays);
        arrays.create(&builder, &node.name);
        if let Some(multirange) = &node.multirange_name {
            arrays.create(&builder, multirange);
        }
    }
    Ok(builder)
}

/// The facts of the collation each composite attribute's `COLLATE` names,
/// resolved against the search path in force as the type is created, as a
/// column's are when its table is.
fn attribute_collations(
    builder: &ParserDBBuilder,
    representation: Option<&UserDefinedTypeRepresentation>,
    collations: &[CreatedCollationMetadata],
    catalog: &PostgresCatalog,
) -> Vec<AttributeCollation> {
    let Some(UserDefinedTypeRepresentation::Composite { attributes }) = representation else {
        return Vec::new();
    };
    let mut search_path = None;
    attributes
        .iter()
        .filter_map(|attribute| {
            let collation = attribute.collation.as_ref()?;
            let search_path = search_path.get_or_insert_with(|| {
                builder
                    .search_path()
                    .map(|(schema, quoted)| (schema.to_string(), quoted))
                    .collect::<Vec<_>>()
            });
            let metadata =
                collation_resolution_for_name(collation, collations, catalog, search_path)
                    .map(CollationResolution::into_column_metadata)?;
            Some(AttributeCollation {
                attribute: attribute.name.clone(),
                postgres_deterministic: metadata.postgres_deterministic(),
                mysql_padding: metadata.mysql_padding(),
            })
        })
        .collect()
}

/// Refuses a composite that writes an attribute name twice, compared as
/// PostgreSQL folds identifiers.
fn refuse_duplicate_composite_attributes(
    case: IdentifierCase,
    attributes: &[UserDefinedTypeCompositeAttributeDef],
    type_name: &str,
) -> Result<(), Error> {
    for (index, attribute) in attributes.iter().enumerate() {
        if attributes.iter().take(index).any(|other| {
            case.identifiers_match(
                &other.name.value,
                other.name.quote_style.is_some(),
                &attribute.name.value,
                attribute.name.quote_style.is_some(),
            )
        }) {
            return Err(Error::DuplicateCompositeAttribute {
                type_name: type_name.to_string(),
                attribute: attribute.name.value.clone(),
            });
        }
    }
    Ok(())
}

/// Binds the types a representation names, and computes the multirange a range
/// brings and the functions a base type or range option declares.
fn bind_representation(
    builder: &ParserDBBuilder,
    name_ident: &Ident,
    schema: (&str, bool),
    representation: &mut UserDefinedTypeRepresentation,
    arrays: &TypeArrays,
) -> Result<(Option<ObjectName>, Vec<NamedFunction>), Error> {
    let mut multirange_name = None;
    let mut named_functions = Vec::new();
    match representation {
        UserDefinedTypeRepresentation::Composite { attributes } => {
            refuse_duplicate_composite_attributes(
                builder.identifier_case(),
                attributes,
                &name_ident.value,
            )?;
            for attribute in attributes {
                bind_type(builder, &mut attribute.data_type, arrays)?;
                require_existing_type(builder, &attribute.data_type)?;
            }
        }
        UserDefinedTypeRepresentation::Enum { labels } => {
            // A label written twice is refused.
            for (index, label) in labels.iter().enumerate() {
                if labels.iter().take(index).any(|other| other.value == label.value) {
                    return Err(Error::DuplicateEnumLabel {
                        type_name: name_ident.value.clone(),
                        label: label.value.clone(),
                    });
                }
            }
        }
        UserDefinedTypeRepresentation::Range { options } => {
            let explicit_name = options.iter().find_map(|option| {
                match option {
                    UserDefinedTypeRangeOption::MultirangeTypeName(name) => Some(name.clone()),
                    _ => None,
                }
            });
            let Some(subtype) = options.iter_mut().find_map(|option| {
                match option {
                    UserDefinedTypeRangeOption::Subtype(data_type) => Some(data_type),
                    _ => None,
                }
            }) else {
                return Err(Error::RangeSubtypeRequired { type_name: name_ident.value.clone() });
            };
            bind_type(builder, subtype, arrays)?;
            require_existing_type(builder, subtype)?;
            named_functions = range_functions_of(options);
            let mut multirange = match explicit_name {
                Some(explicit) => explicit,
                None => multirange_from_range_name(ident_parts(name_ident)),
            };
            if qualifier_of(&multirange).named().is_none() {
                bind_reference(&mut multirange, Some(schema_ident(schema.0, schema.1)));
            }
            // The multirange names the pool of type names too.
            let multirange_ident = object_name_last_identifier(&multirange).unwrap_or(name_ident);
            let schema = qualifier_of(&multirange).named().unwrap_or(schema);
            match reach_in_schema(builder, multirange_ident, schema) {
                Some(Reached::RowType(kind)) => {
                    return Err(Error::RelationNameAlreadyTaken {
                        object_kind: ObjectKind::Type,
                        conflicting_kind: kind,
                        object_name: multirange_ident.value.clone(),
                    });
                }
                Some(Reached::Domain(_)) => {
                    return Err(Error::RelationNameAlreadyTaken {
                        object_kind: ObjectKind::Type,
                        conflicting_kind: ObjectKind::Domain,
                        object_name: multirange_ident.value.clone(),
                    });
                }
                Some(Reached::UserType(_, _)) => {
                    return Err(Error::RelationNameAlreadyTaken {
                        object_kind: ObjectKind::Type,
                        conflicting_kind: ObjectKind::Type,
                        object_name: multirange_ident.value.clone(),
                    });
                }
                Some(Reached::Catalog) | None => {}
            }
            multirange_name = Some(multirange);
        }
        UserDefinedTypeRepresentation::SqlDefinition { options } => {
            for option in &mut *options {
                if let UserDefinedTypeSqlDefinitionOption::Element(data_type) = option {
                    bind_type(builder, data_type, arrays)?;
                    require_existing_type(builder, data_type)?;
                }
            }
            named_functions = sql_definition_functions_of(options);
        }
    }
    Ok((multirange_name, named_functions))
}

/// The name of a doomed type, for a refusal that names it.
fn doomed_type_name(builder: &ParserDBBuilder, doomed: &Doomed) -> String {
    match doomed {
        Doomed::Domain(position) => builder.domains()[*position].0.name().to_string(),
        Doomed::UserType(position) => builder.user_defined_types()[*position].0.name().to_string(),
    }
}

/// The refusal a dependent raises, a domain naming one of its domains or a
/// type naming one of its.
fn dependents_error(
    builder: &ParserDBBuilder,
    used: &Doomed,
    dependent_kind: ObjectKind,
    dependent_name: String,
) -> Error {
    let name = doomed_type_name(builder, used);
    match used {
        Doomed::Domain(_) => {
            Error::DomainHasDependents { domain_name: name, dependent_kind, dependent_name }
        }
        Doomed::UserType(_) => {
            Error::TypeHasDependents { type_name: name, dependent_kind, dependent_name }
        }
    }
}

/// The doomed type a declared type, or its innermost array element, names, if
/// one is doomed.
fn declared_type_reaches_doomed(
    builder: &ParserDBBuilder,
    data_type: &DataType,
    doomed: &[Doomed],
) -> Option<Doomed> {
    let name = custom_name(data_type)?;
    let Ok((schema, ident)) = object_name_identifiers(name) else {
        return None;
    };
    let reached = match schema {
        Some(schema) => reach_in_schema(builder, ident, ident_parts(schema)),
        None => reach_type(builder, ident),
    };
    match reached {
        Some(Reached::Domain(position)) if doomed_domain(doomed, position) => {
            Some(Doomed::Domain(position))
        }
        Some(Reached::UserType(position, _)) if doomed_user_type(doomed, position) => {
            Some(Doomed::UserType(position))
        }
        _ => None,
    }
}

/// The identities an owner holds: its stored name and the stored array the
/// ownership record names, a range's multirange and the multirange's array.
fn type_identities(
    arrays: &TypeArrays,
    owner: &ObjectName,
    multirange: Option<&ObjectName>,
) -> Vec<ObjectName> {
    let mut identities = vec![owner.clone()];
    if let Some(array) = arrays.array(owner) {
        identities.push(array.clone());
    }
    if let Some(multirange) = multirange {
        identities.push(multirange.clone());
        if let Some(array) = arrays.array(multirange) {
            identities.push(array.clone());
        }
    }
    identities
}

/// The identities a held type or domain holds, which an operator naming any
/// of them keeps the type from dropping.
fn held_type_identities(
    builder: &ParserDBBuilder,
    arrays: &TypeArrays,
    held: Option<Doomed>,
) -> Vec<ObjectName> {
    let Some(held) = held else {
        return Vec::new();
    };
    match held {
        Doomed::Domain(position) => {
            let domain = &builder.domains()[position].0;
            type_identities(arrays, &domain.name, None)
        }
        Doomed::UserType(position) => {
            let type_ = &builder.user_defined_types()[position].0;
            type_identities(arrays, &type_.name, type_.multirange_name.as_ref())
        }
    }
}

/// The identities every doomed type holds, each owner with its stored array
/// and a range's multirange with its array.
fn doomed_type_identities(
    builder: &ParserDBBuilder,
    arrays: &TypeArrays,
    doomed: &[Doomed],
) -> Vec<ObjectName> {
    doomed
        .iter()
        .copied()
        .flat_map(|held| held_type_identities(builder, arrays, Some(held)))
        .collect()
}

/// Applies a `DROP DOMAIN` or a `DROP TYPE`, dropping the domains and
/// user-defined types the names reach, the domains and types that depend on
/// them, and the functions a dropped type takes with it.
///
/// Returns the builder alongside the identities and functions the drop took,
/// each doomed owner with its stored array, read before the removal, which
/// the operator facts the statement drops follow.
///
/// # Errors
///
/// Refuses a `DROP DOMAIN` naming no domain without `IF EXISTS`, one naming a
/// user-defined type or the row type of a relation, a `DROP TYPE` naming the
/// multirange of a range, the array of a type, or the row type of a relation,
/// one naming a type of the database system, and one naming nothing without
/// `IF EXISTS`. Once a name resolves to a droppable type, without `CASCADE` it
/// refuses a type a column, a composite attribute, a domain, a range or a base
/// type's element uses, and a type, its stored array, or a range's multirange
/// that `dependent_holding` names. With it, those go too.
pub(super) fn drop_types(
    builder: ParserDBBuilder,
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
    spelling: DropSpelling,
    arrays: &mut Arc<TypeArrays>,
    dependent_holding: &impl Fn(&ObjectName) -> Option<CatalogDependent>,
) -> Result<(ParserDBBuilder, TypeDrop), Error> {
    let mut doomed: Vec<Doomed> = Vec::new();
    for name in names {
        let (schema, ident) = object_name_identifiers(name)?;
        let schema_parts = schema.map(ident_parts);
        // An array type goes only with the type it is the array of.
        if spelling == DropSpelling::Type
            && let Some(base) = arrays.element(&builder, name)
        {
            return Err(Error::TypeDropRequiredBy {
                type_name: format!("{}[]", base.name()),
                requiring_kind: ObjectKind::Type,
                requiring_name: base.name().to_string(),
            });
        }
        let reached = match schema_parts {
            Some(schema) => reach_in_schema(&builder, ident, schema),
            None => reach_type(&builder, ident),
        };
        let skipped = if_exists && reached.is_none();
        let held = match (reached, spelling) {
            (Some(Reached::Domain(position)), _) => {
                if !doomed_domain(&doomed, position) {
                    doomed.push(Doomed::Domain(position));
                }
                Some(Doomed::Domain(position))
            }
            (Some(Reached::UserType(_position, _is_multirange)), DropSpelling::Domain) => {
                return Err(Error::RelationKindMismatch {
                    object_name: name.to_string(),
                    expected_kind: ObjectKind::Domain,
                    actual_kind: ObjectKind::Type,
                });
            }
            (Some(Reached::UserType(position, false)), DropSpelling::Type) => {
                if !doomed_user_type(&doomed, position) {
                    doomed.push(Doomed::UserType(position));
                }
                Some(Doomed::UserType(position))
            }
            // A multirange goes only with its range.
            (Some(Reached::UserType(position, true)), DropSpelling::Type) => {
                let range_name = builder.user_defined_types()[position].0.name().to_string();
                return Err(Error::TypeDropRequiredBy {
                    type_name: name.to_string(),
                    requiring_kind: ObjectKind::Type,
                    requiring_name: range_name,
                });
            }
            (Some(Reached::RowType(kind)), DropSpelling::Domain) => {
                return Err(Error::RelationKindMismatch {
                    object_name: name.to_string(),
                    expected_kind: ObjectKind::Domain,
                    actual_kind: kind,
                });
            }
            // A row type goes only with its relation.
            (Some(Reached::RowType(kind)), DropSpelling::Type) => {
                return Err(Error::TypeDropRequiredBy {
                    type_name: name.to_string(),
                    requiring_kind: kind,
                    requiring_name: name.to_string(),
                });
            }
            (Some(Reached::Catalog), DropSpelling::Type) => {
                return Err(Error::TypeRequiredBySystem { type_name: name.to_string() });
            }
            (Some(Reached::Catalog) | None, DropSpelling::Domain) if !skipped => {
                return Err(Error::DropDomainNotFound { domain_name: name.to_string() });
            }
            (None, DropSpelling::Type) if !skipped => {
                return Err(Error::TypeNotFound { type_name: name.to_string() });
            }
            _ => None,
        };
        if !cascade {
            for candidate in held_type_identities(&builder, arrays, held) {
                if let Some(dependent) = dependent_holding(&candidate) {
                    return Err(Error::TypeDropRequiredBy {
                        type_name: name.to_string(),
                        requiring_kind: dependent.kind,
                        requiring_name: dependent.name,
                    });
                }
            }
        }
    }
    drop_positions(builder, &doomed, cascade, &[], arrays)
}

/// The name a stored table is stored under, with the default schema written
/// out so a lookup reaches it whatever the session's path holds.
fn qualified_table_name(name: &ObjectName) -> ObjectName {
    let mut qualified = name.clone();
    if matches!(qualifier_of(name), Qualifier::Absent) {
        bind_reference(&mut qualified, Some(Ident::new(DEFAULT_SCHEMA)));
    }
    qualified
}

/// The doomed type a user type depends on as a range subtype or an element,
/// if one is doomed.
fn user_type_dependency_on_doomed(
    builder: &ParserDBBuilder,
    type_: &UserDefinedType,
    doomed: &[Doomed],
) -> Option<Doomed> {
    [type_.range_subtype(), type_.element()]
        .into_iter()
        .flatten()
        .find_map(|data_type| declared_type_reaches_doomed(builder, data_type, doomed))
}

/// Extends `doomed` with every domain declared over a doomed type and every
/// type that depends on one as a range subtype or an element, refusing a
/// non-cascade drop that would leave a dependent behind.
fn extend_doomed_by_dependents(
    builder: &ParserDBBuilder,
    doomed: &mut Vec<Doomed>,
    cascade: bool,
) -> Result<(), Error> {
    let mut grew = true;
    while grew {
        grew = false;
        for (position, (domain, _)) in builder.domains().iter().enumerate() {
            if doomed_domain(doomed, position) {
                continue;
            }
            let Some(used) = declared_type_reaches_doomed(builder, &domain.data_type, doomed)
            else {
                continue;
            };
            if !cascade {
                return Err(dependents_error(
                    builder,
                    &used,
                    ObjectKind::Domain,
                    domain.name.to_string(),
                ));
            }
            doomed.push(Doomed::Domain(position));
            grew = true;
        }
        for (position, (type_, _)) in builder.user_defined_types().iter().enumerate() {
            if doomed_user_type(doomed, position) {
                continue;
            }
            let Some(used) = user_type_dependency_on_doomed(builder, type_, doomed) else {
                continue;
            };
            if !cascade {
                return Err(dependents_error(
                    builder,
                    &used,
                    ObjectKind::Type,
                    type_.name().to_string(),
                ));
            }
            doomed.push(Doomed::UserType(position));
            grew = true;
        }
    }
    Ok(())
}

/// Drops the objects at `doomed`, refusing what uses them unless `cascade`,
/// in which case the columns, composite attributes, domains, ranges and
/// functions using them go too. `alongside` holds the functions the same
/// statement drops later, whose calls hold nothing in place.
///
/// Returns the builder alongside the identities the doomed types hold, each
/// with its stored array, and the functions the drop removes, read before the
/// removal.
fn drop_positions(
    mut builder: ParserDBBuilder,
    doomed: &[Doomed],
    cascade: bool,
    alongside: &[Arc<CreateFunction>],
    arrays: &mut Arc<TypeArrays>,
) -> Result<(ParserDBBuilder, TypeDrop), Error> {
    if doomed.is_empty() {
        return Ok((builder, TypeDrop { types: Vec::new(), functions: Vec::new() }));
    }
    // Domains, ranges and element-holding types over a doomed type, to the end
    // of the chain.
    let mut doomed_owned: Vec<Doomed> = doomed.to_vec();
    extend_doomed_by_dependents(&builder, &mut doomed_owned, cascade)?;

    // Without `CASCADE` a column or attribute of a doomed type or multirange
    // blocks the drop.
    if !cascade {
        for (table, _, _) in builder.tables() {
            for column in &table.columns {
                if let Some(used) =
                    declared_type_reaches_doomed(&builder, &column.data_type, &doomed_owned)
                {
                    return Err(dependents_error(
                        &builder,
                        &used,
                        ObjectKind::Column,
                        alloc::format!("{}.{}", table.name, column.name),
                    ));
                }
            }
        }
        for (type_, _) in builder.user_defined_types() {
            let Some(attributes) = type_.composite_attributes() else {
                continue;
            };
            for attribute in attributes {
                if let Some(used) =
                    declared_type_reaches_doomed(&builder, &attribute.data_type, &doomed_owned)
                {
                    return Err(dependents_error(
                        &builder,
                        &used,
                        ObjectKind::Column,
                        alloc::format!("{}.{}", type_.name(), attribute.name.value),
                    ));
                }
            }
        }
    }

    let mut removed = if cascade {
        let (next, removed) = drop_dependents_cascade(builder, &doomed_owned, arrays)?;
        builder = next;
        removed
    } else {
        TypeDrop::default()
    };
    // A range takes its constructors along, after a cascade has dropped the
    // columns of its type and the checks on them.
    removed.functions.extend(range_constructors::drop_range_constructors(
        &mut builder,
        |position| doomed_user_type(&doomed_owned, position),
        alongside,
    )?);
    // Any other function whose signature names a doomed type holds it in
    // place, and goes with it under `CASCADE`.
    let naming: Vec<(Arc<CreateFunction>, Doomed)> = builder
        .functions()
        .iter()
        .filter_map(|(function, _)| {
            signature_types(function)
                .find_map(|data_type| {
                    declared_type_reaches_doomed(&builder, data_type, &doomed_owned)
                })
                .map(|used| (Arc::clone(function), used))
        })
        .collect();
    if !cascade && let Some((function, used)) = naming.first() {
        return Err(dependents_error(
            &builder,
            used,
            ObjectKind::Function,
            function.name.to_string(),
        ));
    }
    let naming: Vec<Arc<CreateFunction>> =
        naming.into_iter().map(|(function, _)| function).collect();
    builder.drop_function_nodes(&naming, alongside)?;
    removed.functions.extend(naming);
    removed.types.extend(doomed_type_identities(&builder, arrays, &doomed_owned));

    // Remove the doomed, domains and user-defined types alike.
    let mut domain_position = 0;
    builder.domains_mut().retain(|(domain, _)| {
        let kept = !doomed_domain(&doomed_owned, domain_position);
        domain_position += 1;
        if !kept {
            Arc::make_mut(arrays).remove(&domain.name);
        }
        kept
    });
    let mut type_position = 0;
    builder.user_defined_types_mut().retain(|(type_, _)| {
        let kept = !doomed_user_type(&doomed_owned, type_position);
        type_position += 1;
        if !kept {
            let arrays = Arc::make_mut(arrays);
            arrays.remove(&type_.name);
            if let Some(multirange) = &type_.multirange_name {
                arrays.remove(multirange);
            }
        }
        kept
    });
    Ok((builder, removed))
}

/// Drops the columns and composite attributes that use the doomed types, for a
/// cascading drop, the columns each taking what dropping a column takes.
fn drop_dependents_cascade(
    mut builder: ParserDBBuilder,
    doomed: &[Doomed],
    arrays: &mut Arc<TypeArrays>,
) -> Result<(ParserDBBuilder, TypeDrop), Error> {
    let mut removed = TypeDrop::default();
    // An inherited column leaves with the parent's, so only declared columns
    // are dropped.
    loop {
        let next = builder.tables().iter().find_map(|(table, _, _)| {
            table
                .columns
                .iter()
                .find(|column| {
                    declared_type_reaches_doomed(&builder, &column.data_type, doomed).is_some()
                        && !super::inheritance::is_inherited_column(&builder, table, &column.name)
                })
                .map(|column| (qualified_table_name(&table.name), column.name.clone()))
        });
        let Some((table_name, column_name)) = next else {
            break;
        };
        let (next, columns) = ParserDB::alter_table_drop_columns(
            builder,
            &table_name,
            super::AlterTableScope { if_exists: false, only: false },
            &[column_name],
            false,
            true,
            arrays,
        )?;
        builder = next;
        removed.types.extend(columns.types);
        removed.functions.extend(columns.functions);
    }
    // The composite stays, possibly with no attribute left.
    let kept: Vec<(usize, Vec<UserDefinedTypeCompositeAttributeDef>)> = builder
        .user_defined_types()
        .iter()
        .enumerate()
        .filter_map(|(position, (type_, _))| {
            let UserDefinedTypeRepresentation::Composite { attributes } =
                type_.representation.as_ref()?
            else {
                return None;
            };
            let doomed_indices: Vec<usize> = attributes
                .iter()
                .enumerate()
                .filter(|(_, attribute)| {
                    declared_type_reaches_doomed(&builder, &attribute.data_type, doomed).is_some()
                })
                .map(|(index, _)| index)
                .collect();
            if doomed_indices.is_empty() {
                return None;
            }
            let attributes = attributes
                .iter()
                .enumerate()
                .filter(|(index, _)| !doomed_indices.contains(index))
                .map(|(_, attribute)| attribute.clone())
                .collect();
            Some((position, attributes))
        })
        .collect();
    for (position, attributes) in kept {
        let (type_, _) = &mut builder.user_defined_types_mut()[position];
        let type_ = Arc::make_mut(type_);
        type_.representation = Some(UserDefinedTypeRepresentation::Composite { attributes });
    }
    Ok((builder, removed))
}

/// Drops every type of a schema `DROP SCHEMA ... CASCADE` removes, with what a
/// cascading drop of each takes, `alongside` holding the functions the schema
/// drop takes afterwards.
///
/// Returns the builder alongside the identities the doomed types hold, each
/// with its stored array, and the functions the drop removes, read before the
/// removal.
///
/// # Errors
///
/// Returns the errors dropping a column a type declares returns, and
/// [`Error::FunctionReferenced`] for a function naming one of the types that
/// something outside the drop still calls.
pub(super) fn drop_types_in(
    builder: ParserDBBuilder,
    schema: (&str, bool),
    alongside: &[Arc<CreateFunction>],
    arrays: &mut Arc<TypeArrays>,
) -> Result<(ParserDBBuilder, TypeDrop), Error> {
    let case = builder.identifier_case();
    let doomed_domains: Vec<Doomed> = builder
        .domains()
        .iter()
        .enumerate()
        .filter(|(_, (domain, _))| domain_in_schema(domain, schema, case))
        .map(|(position, _)| Doomed::Domain(position))
        .collect();
    let doomed_types: Vec<Doomed> = builder
        .user_defined_types()
        .iter()
        .enumerate()
        .filter(|(_, (type_, _))| user_type_in_schema(type_, schema, case))
        .map(|(position, _)| Doomed::UserType(position))
        .collect();
    let mut doomed = doomed_domains;
    doomed.extend(doomed_types);
    drop_positions(builder, &doomed, true, alongside, arrays)
}

/// Retargets a matching type reference while preserving qualification within
/// its schema.
fn retarget_type_name(
    name: &mut ObjectName,
    from: (&str, bool),
    from_schema: (&str, bool),
    to: &ObjectName,
    case: IdentifierCase,
) {
    if names_type(name, from, from_schema, case) {
        let destination_schema = qualifier_of(to).named().unwrap_or((DEFAULT_SCHEMA, false));
        if case.identifiers_match(
            from_schema.0,
            from_schema.1,
            destination_schema.0,
            destination_schema.1,
        ) {
            if let Some(ident) = object_name_last_identifier(to) {
                rewrite_last_ident(name, ident);
            }
        } else {
            name.clone_from(to);
        }
    }
}

/// Whether a column's declared type names the type a rename re-points, so its
/// table is rebuilt.
fn column_renames_type(
    data_type: &DataType,
    (from_name, from_name_quoted): (&str, bool),
    (from_schema, from_schema_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    custom_name(data_type).is_some_and(|name| {
        object_name_identifiers(name).is_ok_and(|(_, _)| {
            names_type(name, (from_name, from_name_quoted), (from_schema, from_schema_quoted), case)
        })
    })
}

/// Repoints declared types and signatures to a renamed type identity.
pub(super) fn repoint_type_references(
    mut builder: ParserDBBuilder,
    previous: &ObjectName,
    to: &ObjectName,
) -> Result<ParserDBBuilder, Error> {
    let (schema, ident) = object_name_identifiers(previous)?;
    let (from_name, from_name_quoted) = ident_parts(ident);
    let (from_schema, from_schema_quoted) = schema.map_or((DEFAULT_SCHEMA, false), ident_parts);
    let case = builder.identifier_case();
    let from = (from_name, from_name_quoted);
    // Every table whose a column names the type, rebuilt.
    let hosts: Vec<super::StoredTable> = builder
        .tables()
        .iter()
        .filter(|(table, _, _)| {
            table.columns.iter().any(|column| {
                column_renames_type(
                    &column.data_type,
                    (from_name, from_name_quoted),
                    (from_schema, from_schema_quoted),
                    case,
                )
            })
        })
        .map(|(table, _, _)| super::StoredTable::of(table))
        .collect();
    for host in &hosts {
        builder = ParserDB::replace_table_node(builder, host, |_, node| {
            for column in &mut node.columns {
                if let Some(name) = custom_name_mut(&mut column.data_type) {
                    retarget_type_name(name, from, (from_schema, from_schema_quoted), to, case);
                }
            }
            Ok(())
        })?;
    }
    // Every domain over the type.
    for (domain, _) in builder.domains_mut() {
        let domain = Arc::make_mut(domain);
        if let Some(name) = custom_name_mut(&mut domain.data_type) {
            retarget_type_name(name, from, (from_schema, from_schema_quoted), to, case);
        }
    }
    // Attributes, subtypes and elements of every user-defined type.
    for (type_, _) in builder.user_defined_types_mut() {
        let representation = Arc::make_mut(type_).representation.as_mut();
        let Some(representation) = representation else {
            continue;
        };
        match representation {
            UserDefinedTypeRepresentation::Composite { attributes } => {
                for attribute in attributes {
                    if let Some(name) = custom_name_mut(&mut attribute.data_type) {
                        retarget_type_name(name, from, (from_schema, from_schema_quoted), to, case);
                    }
                }
            }
            UserDefinedTypeRepresentation::Range { options } => {
                for option in options {
                    if let UserDefinedTypeRangeOption::Subtype(data_type) = option
                        && let Some(name) = custom_name_mut(data_type)
                    {
                        retarget_type_name(name, from, (from_schema, from_schema_quoted), to, case);
                    }
                }
            }
            UserDefinedTypeRepresentation::SqlDefinition { options } => {
                for option in options {
                    if let UserDefinedTypeSqlDefinitionOption::Element(data_type) = option
                        && let Some(name) = custom_name_mut(data_type)
                    {
                        retarget_type_name(name, from, (from_schema, from_schema_quoted), to, case);
                    }
                }
            }
            UserDefinedTypeRepresentation::Enum { .. } => {}
        }
    }
    // Every function signature naming the type.
    retarget_signatures(
        &mut builder,
        |name| names_type(name, from, (from_schema, from_schema_quoted), case),
        |name| retarget_type_name(name, from, (from_schema, from_schema_quoted), to, case),
    );
    Ok(builder)
}

/// Whether the stored type name `name` is the name `from` holds in
/// `from_schema`.
fn names_type(
    name: &ObjectName,
    (from_name, from_name_quoted): (&str, bool),
    (from_schema, from_schema_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    let schema_ok = if name.0.len() >= 2 {
        matches!(
            &name.0[name.0.len() - 2],
            sqlparser::ast::ObjectNamePart::Identifier(schema)
                if case.identifiers_match(
                    &schema.value,
                    schema.quote_style.is_some(),
                    from_schema,
                    from_schema_quoted,
                )
        )
    } else {
        case.identifiers_match(DEFAULT_SCHEMA, false, from_schema, from_schema_quoted)
    };
    name.0.last().and_then(sqlparser::ast::ObjectNamePart::as_ident).is_some_and(|ident| {
        case.identifiers_match(
            &ident.value,
            ident.quote_style.is_some(),
            from_name,
            from_name_quoted,
        )
    }) && schema_ok
}

/// Applies an `ALTER TYPE` statement.
///
/// # Errors
///
/// Refuses a rename of the row type of a relation, of a shell, of a type the
/// input never creates or of a type of the database system, a rename onto a
/// name the pool of type names holds, and, for a composite, onto a name the
/// relation pool holds, and an `ADD VALUE` or `RENAME VALUE` on a type that is
/// no enum, of a label the enum does not hold, or onto a label it holds.
pub(super) fn alter_type(
    builder: ParserDBBuilder,
    alter: AlterType,
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<ParserDBBuilder, Error> {
    match alter.operation {
        AlterTypeOperation::Rename(rename) => {
            alter_type_rename(builder, &alter.name, &rename.new_name, arrays, catalog)
        }
        AlterTypeOperation::AddValue(add) => {
            let builder = alter_type_add_value(builder, &alter.name, add)?;
            Ok(builder)
        }
        AlterTypeOperation::RenameValue(renamed) => {
            let builder = alter_type_rename_value(builder, &alter.name, renamed)?;
            Ok(builder)
        }
    }
}

/// The type a rename or enum alteration names, or the refusal that name does
/// not reach.
fn resolve_named_type(
    builder: &ParserDBBuilder,
    name: &ObjectName,
) -> Result<(Doomed, bool), Error> {
    let (schema, ident) = object_name_identifiers(name)?;
    let reached = match schema {
        Some(schema) => reach_in_schema(builder, ident, ident_parts(schema)),
        None => reach_type(builder, ident),
    };
    match reached {
        Some(Reached::Domain(position)) => Ok((Doomed::Domain(position), false)),
        Some(Reached::UserType(position, is_multirange)) => {
            if builder.user_defined_types()[position].0.is_shell() {
                return Err(Error::TypeIsOnlyShell {
                    type_name: builder.user_defined_types()[position].0.name().to_string(),
                });
            }
            Ok((Doomed::UserType(position), is_multirange))
        }
        Some(Reached::RowType(kind)) => {
            Err(Error::TypeIsRowType { type_name: name.to_string(), relation_kind: kind })
        }
        Some(Reached::Catalog) | None => Err(Error::TypeNotFound { type_name: name.to_string() }),
    }
}

/// Replaces the last identifier of `stored` with `new_name`.
pub(super) fn rewrite_last_ident(stored: &mut ObjectName, new_name: &Ident) {
    stored.0.pop();
    stored.0.push(ObjectNamePart::Identifier(new_name.clone()));
}

fn alter_type_rename(
    builder: ParserDBBuilder,
    name: &ObjectName,
    new_name: &Ident,
    arrays: &mut Arc<TypeArrays>,
    catalog: &mut Arc<PostgresCatalog>,
) -> Result<ParserDBBuilder, Error> {
    // The array type of a stored type is altered only through its type.
    if let Some(base) = arrays.element(&builder, name) {
        return Err(Error::CannotAlterArrayType { array_name: format!("{}[]", base.name()) });
    }
    let (target, is_multirange) = resolve_named_type(&builder, name)?;
    let old_name = match target {
        Doomed::Domain(position) => builder.domains()[position].0.name.clone(),
        Doomed::UserType(position) => {
            let type_ = &builder.user_defined_types()[position].0;
            if is_multirange {
                type_
                    .multirange_name
                    .clone()
                    .ok_or_else(|| Error::TypeNotFound { type_name: name.to_string() })?
            } else {
                type_.name.clone()
            }
        }
    };
    let schema = qualifier_of(&old_name).named().unwrap_or((DEFAULT_SCHEMA, false));
    match reach_in_schema(&builder, new_name, schema) {
        Some(Reached::RowType(kind)) => {
            return Err(Error::TypeIsRowType {
                type_name: new_name.value.clone(),
                relation_kind: kind,
            });
        }
        Some(Reached::UserType(_, _)) => {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Type,
                object_name: new_name.value.clone(),
            });
        }
        Some(Reached::Domain(_)) => {
            return Err(Error::RelationNameAlreadyTaken {
                object_kind: ObjectKind::Type,
                conflicting_kind: ObjectKind::Domain,
                object_name: new_name.value.clone(),
            });
        }
        Some(Reached::Catalog) | None => {}
    }
    // A composite takes a relation name, which the other kinds do not.
    let is_composite = match target {
        Doomed::UserType(position) => {
            builder.user_defined_types()[position].0.kind() == UserDefinedTypeKind::Composite
        }
        Doomed::Domain(_) => false,
    };
    if let Some(conflicting_kind) =
        is_composite.then(|| relation_name_holder(&builder, new_name, Some(schema))).flatten()
    {
        return Err(Error::RelationNameAlreadyTaken {
            object_kind: ObjectKind::Type,
            conflicting_kind,
            object_name: new_name.value.clone(),
        });
    }
    let mut destination = old_name.clone();
    rewrite_last_ident(&mut destination, new_name);
    let array_was_displaced = arrays.array_is_named(&old_name, &destination);
    if let Some((previous, current)) = Arc::make_mut(arrays).displace(&builder, &destination) {
        Arc::make_mut(catalog).rename_type_identity(&previous, current);
    }
    // Re-point the references, then rewrite the stored name.
    let mut builder = repoint_type_references(builder, &old_name, &destination)?;
    match target {
        Doomed::Domain(position) => {
            let (domain, _) = &mut builder.domains_mut()[position];
            let domain = Arc::make_mut(domain);
            rewrite_last_ident(&mut domain.name, new_name);
        }
        Doomed::UserType(position) if !is_multirange => {
            range_constructors::follow_range_rename(&mut builder, position, new_name);
            let (type_, _) = &mut builder.user_defined_types_mut()[position];
            let type_ = Arc::make_mut(type_);
            rewrite_last_ident(&mut type_.name, new_name);
        }
        Doomed::UserType(position) => {
            let (type_, _) = &mut builder.user_defined_types_mut()[position];
            let type_ = Arc::make_mut(type_);
            // The name matched the multirange's spelling, which only a type
            // that declares one holds.
            let multirange = type_
                .multirange_name
                .as_mut()
                .ok_or(Error::TypeNotFound { type_name: name.to_string() })?;
            rewrite_last_ident(multirange, new_name);
        }
    }
    if let Some((previous, current)) =
        Arc::make_mut(arrays).rename(&builder, &old_name, &destination, array_was_displaced)
    {
        Arc::make_mut(catalog).rename_type_identity(&previous, current);
    }
    Arc::make_mut(catalog).rename_type_identity(&old_name, &destination);
    Ok(builder)
}

/// The index the label called `value` holds among `labels`, under `case`.
fn label_position(labels: &[Ident], value: &Ident, case: IdentifierCase) -> Option<usize> {
    labels.iter().position(|label| {
        case.identifiers_match(
            &label.value,
            label.quote_style.is_some(),
            &value.value,
            value.quote_style.is_some(),
        )
    })
}

/// Whether an enum label is called `value`, under `case`.
fn label_named(labels: &[Ident], value: &Ident, case: IdentifierCase) -> bool {
    labels.iter().any(|label| {
        case.identifiers_match(
            &label.value,
            label.quote_style.is_some(),
            &value.value,
            value.quote_style.is_some(),
        )
    })
}

fn alter_type_add_value(
    mut builder: ParserDBBuilder,
    name: &ObjectName,
    add: sqlparser::ast::AlterTypeAddValue,
) -> Result<ParserDBBuilder, Error> {
    let (target, _is_multirange) = resolve_named_type(&builder, name)?;
    let (position, is_user_type) = match target {
        Doomed::UserType(position) => (position, true),
        Doomed::Domain(_) => (0, false),
    };
    if !is_user_type {
        return Err(Error::NotAnEnum { type_name: name.to_string() });
    }
    let case = builder.identifier_case();
    let type_name = name.to_string();
    let (type_, _) = &mut builder.user_defined_types_mut()[position];
    let type_ = Arc::make_mut(type_);
    let Some(UserDefinedTypeRepresentation::Enum { labels }) = type_.representation.as_mut() else {
        return Err(Error::NotAnEnum { type_name });
    };
    // A held label is refused, or skipped with IF NOT EXISTS, before its
    // position is checked.
    if label_named(labels, &add.value, case) {
        if add.if_not_exists {
            return Ok(builder);
        }
        return Err(Error::EnumLabelExists { type_name, label: add.value.value.clone() });
    }
    // A position relative to an unknown label is refused.
    let insert_at = if let Some(position_ref) = &add.position {
        let neighbor = match position_ref {
            sqlparser::ast::AlterTypeAddValuePosition::Before(neighbor)
            | sqlparser::ast::AlterTypeAddValuePosition::After(neighbor) => neighbor,
        };
        let index = label_position(labels, neighbor, case)
            .ok_or_else(|| Error::EnumLabelNotFound { type_name, label: neighbor.value.clone() })?;
        match position_ref {
            sqlparser::ast::AlterTypeAddValuePosition::Before(_) => index,
            sqlparser::ast::AlterTypeAddValuePosition::After(_) => index + 1,
        }
    } else {
        labels.len()
    };
    labels.insert(insert_at, add.value);
    Ok(builder)
}

fn alter_type_rename_value(
    mut builder: ParserDBBuilder,
    name: &ObjectName,
    renamed: sqlparser::ast::AlterTypeRenameValue,
) -> Result<ParserDBBuilder, Error> {
    let (target, _is_multirange) = resolve_named_type(&builder, name)?;
    let (position, is_user_type) = match target {
        Doomed::UserType(position) => (position, true),
        Doomed::Domain(_) => (0, false),
    };
    if !is_user_type {
        return Err(Error::NotAnEnum { type_name: name.to_string() });
    }
    let case = builder.identifier_case();
    let type_name = name.to_string();
    let (type_, _) = &mut builder.user_defined_types_mut()[position];
    let type_ = Arc::make_mut(type_);
    let Some(UserDefinedTypeRepresentation::Enum { labels }) = type_.representation.as_mut() else {
        return Err(Error::NotAnEnum { type_name });
    };
    let Some(index) = label_position(labels, &renamed.from, case) else {
        return Err(Error::EnumLabelNotFound { type_name, label: renamed.from.value.clone() });
    };
    if label_named(labels, &renamed.to, case) {
        return Err(Error::EnumLabelExists { type_name, label: renamed.to.value.clone() });
    }
    labels[index] = renamed.to;
    Ok(builder)
}

/// Points the user-defined types of a renamed schema, and every declared type
/// naming one of them, at the new name.
///
/// # Errors
///
/// Returns the errors rebuilding a table whose column names such a type
/// returns.
pub(super) fn follow_schema_rename(
    mut builder: ParserDBBuilder,
    (from, from_quoted): (&str, bool),
    (to, to_quoted): (&str, bool),
) -> Result<ParserDBBuilder, Error> {
    let case = builder.identifier_case();
    let mut moved: Vec<(String, bool)> = Vec::new();
    for (type_, _) in builder.user_defined_types() {
        if !user_type_in_schema(type_, (from, from_quoted), case) {
            continue;
        }
        moved.push((type_.name().to_string(), type_.name_is_quoted()));
        if let Some(multirange) = &type_.multirange_name
            && let Some(ident) = object_name_last_identifier(multirange)
        {
            moved.push((ident.value.clone(), ident.quote_style.is_some()));
        }
    }
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
                        moved.as_str(),
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
    for (type_, _) in builder.user_defined_types_mut() {
        let renamed = reaches_moved(&type_.name)
            || type_.multirange_name.as_ref().is_some_and(reaches_moved)
            || type_
                .representation
                .as_ref()
                .is_some_and(|representation| type_refs_moved(representation, &reaches_moved));
        if !renamed {
            continue;
        }
        let type_ = Arc::make_mut(type_);
        retarget(&mut type_.name);
        if let Some(multirange) = &mut type_.multirange_name {
            retarget(multirange);
        }
        if let Some(representation) = representation_mut_refs(type_.representation.as_mut()) {
            for name in representation {
                retarget(name);
            }
        }
    }
    // The constructors of a moved range follow it.
    for (_, metadata) in builder.functions_mut() {
        if let Some(constructor) = metadata.range_constructor_mut() {
            retarget(&mut constructor.range);
        }
    }
    // So does every function signature naming a moved type.
    retarget_signatures(&mut builder, reaches_moved, retarget);
    // A domain over a moved type follows it to the new schema.
    for (domain, _) in builder.domains_mut() {
        let domain = Arc::make_mut(domain);
        if let Some(name) = custom_name_mut(&mut domain.data_type)
            && reaches_moved(name)
        {
            retarget(name);
        }
    }
    let hosts: Vec<super::StoredTable> = builder
        .tables()
        .iter()
        .filter(|(table, _, _)| {
            table
                .columns
                .iter()
                .any(|column| custom_name(&column.data_type).is_some_and(reaches_moved))
        })
        .map(|(table, _, _)| super::StoredTable::of(table))
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

/// Whether any type reference in a representation reaches a moved type.
fn type_refs_moved(
    representation: &UserDefinedTypeRepresentation,
    reaches_moved: &impl Fn(&ObjectName) -> bool,
) -> bool {
    representation_type_refs(representation).iter().any(|name| reaches_moved(name))
}

/// The type references a representation holds, for respelling.
fn representation_type_refs(representation: &UserDefinedTypeRepresentation) -> Vec<&ObjectName> {
    let mut names = Vec::new();
    match representation {
        UserDefinedTypeRepresentation::Composite { attributes } => {
            for attribute in attributes {
                if let Some(name) = custom_name(&attribute.data_type) {
                    names.push(name);
                }
            }
        }
        UserDefinedTypeRepresentation::Range { options } => {
            for option in options {
                if let UserDefinedTypeRangeOption::Subtype(data_type) = option
                    && let Some(name) = custom_name(data_type)
                {
                    names.push(name);
                }
            }
        }
        UserDefinedTypeRepresentation::SqlDefinition { options } => {
            for option in options {
                if let UserDefinedTypeSqlDefinitionOption::Element(data_type) = option
                    && let Some(name) = custom_name(data_type)
                {
                    names.push(name);
                }
            }
        }
        UserDefinedTypeRepresentation::Enum { .. } => {}
    }
    names
}

/// The mutable type references a representation holds, for respelling.
fn representation_mut_refs(
    representation: Option<&mut UserDefinedTypeRepresentation>,
) -> Option<Vec<&mut ObjectName>> {
    let representation = representation?;
    let mut names = Vec::new();
    match representation {
        UserDefinedTypeRepresentation::Composite { attributes } => {
            for attribute in attributes {
                if let Some(name) = custom_name_mut(&mut attribute.data_type) {
                    names.push(name);
                }
            }
        }
        UserDefinedTypeRepresentation::Range { options } => {
            for option in options {
                if let UserDefinedTypeRangeOption::Subtype(data_type) = option
                    && let Some(name) = custom_name_mut(data_type)
                {
                    names.push(name);
                }
            }
        }
        UserDefinedTypeRepresentation::SqlDefinition { options } => {
            for option in options {
                if let UserDefinedTypeSqlDefinitionOption::Element(data_type) = option
                    && let Some(name) = custom_name_mut(data_type)
                {
                    names.push(name);
                }
            }
        }
        UserDefinedTypeRepresentation::Enum { .. } => {}
    }
    Some(names)
}
