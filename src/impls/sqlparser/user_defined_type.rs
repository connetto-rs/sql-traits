//! Implementation of the `UserDefinedTypeLike` trait for the stored
//! `UserDefinedType` node, and the resolver that reads a bound type name
//! back to the type it reached.

use sqlparser::ast::CreateFunction;

use crate::{
    structs::{IdentifierCase, ParserDB, UserDefinedType},
    traits::{Metadata, UserDefinedTypeKind, UserDefinedTypeLike},
    utils::{
        identifier_resolution::normalize_identifier,
        object_name::{object_name_identifiers, object_name_last_part, qualifier_of},
        postgres_types::names_catalog_type,
    },
};

/// The schema a user-defined type stored without a qualifier lives in.
const DEFAULT_SCHEMA: &str = "public";

/// The calls a base type's or a range's options bind, resolved when the type
/// was written.
impl Metadata for UserDefinedType {
    /// The functions the type's definition names, bound when it was written.
    type Meta = crate::structs::metadata::ExpressionCalls<CreateFunction>;
}

impl UserDefinedTypeLike for UserDefinedType {
    type DB = ParserDB;

    #[inline]
    fn name(&self) -> &str {
        object_name_last_part(&self.name).map_or("", |(name, _)| name)
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        object_name_last_part(&self.name).is_some_and(|(_, quoted)| quoted)
    }

    #[inline]
    fn schema(&self) -> Option<&str> {
        qualifier_of(&self.name).named().map(|(schema, _)| schema)
    }

    #[inline]
    fn schema_is_quoted(&self) -> bool {
        qualifier_of(&self.name).named().is_some_and(|(_, quoted)| quoted)
    }

    #[inline]
    fn kind(&self) -> UserDefinedTypeKind {
        match &self.representation {
            None => UserDefinedTypeKind::Shell,
            Some(representation) => {
                match representation {
                    sqlparser::ast::UserDefinedTypeRepresentation::Composite { .. } => {
                        UserDefinedTypeKind::Composite
                    }
                    sqlparser::ast::UserDefinedTypeRepresentation::Enum { .. } => {
                        UserDefinedTypeKind::Enum
                    }
                    sqlparser::ast::UserDefinedTypeRepresentation::Range { .. } => {
                        UserDefinedTypeKind::Range
                    }
                    sqlparser::ast::UserDefinedTypeRepresentation::SqlDefinition { .. } => {
                        UserDefinedTypeKind::Base
                    }
                }
            }
        }
    }
}

/// Whether a stored user-defined type's own name is called `name`, under
/// `case`.
pub(crate) fn user_type_named(
    type_: &UserDefinedType,
    (name, name_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    case.identifiers_match(type_.name(), type_.name_is_quoted(), name, name_quoted)
}

/// Whether a stored user-defined type lives in the schema `schema` names,
/// under `case`.
pub(crate) fn user_type_in_schema(
    type_: &UserDefinedType,
    (schema, schema_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    object_name_in_schema(&type_.name, (schema, schema_quoted), case)
}

/// Whether a stored user-defined type holds the name `name` in the schema
/// `schema` names, under `case`, answered with whether the name is the
/// type's own or its multirange's.
pub(crate) fn user_type_holds(
    type_: &UserDefinedType,
    (name, name_quoted): (&str, bool),
    (schema, schema_quoted): (&str, bool),
    case: IdentifierCase,
) -> Option<bool> {
    if !user_type_in_schema(type_, (schema, schema_quoted), case) {
        return None;
    }
    if user_type_named(type_, (name, name_quoted), case) {
        return Some(false);
    }
    type_
        .multirange_name
        .as_ref()
        .is_some_and(|multirange| {
            object_name_named(multirange, (name, name_quoted), case)
                && object_name_in_schema(multirange, (schema, schema_quoted), case)
        })
        .then_some(true)
}

/// The user-defined type a bound type name stored in the schema names, or
/// `None` when the name reaches no stored type.
///
/// A range type is answered with its multirange flagged when the name is the
/// multirange's, since the multirange is not a type the schema lists of its
/// own. Ingestion rewrites every type name reaching a user-defined type to
/// that type's qualifier, as it does for every other stored reference, so a
/// bare name names the default schema, and a bare name of a type `pg_catalog`
/// holds reaches that catalog type, never a stored one.
pub(crate) fn bound_user_type<'a>(
    types: impl IntoIterator<Item = &'a UserDefinedType>,
    reference: &sqlparser::ast::ObjectName,
    case: IdentifierCase,
) -> Option<(&'a UserDefinedType, bool)> {
    let (schema, name) = object_name_identifiers(reference).ok()?;
    let name = (name.value.as_str(), name.quote_style.is_some());
    let schema = match schema {
        Some(schema) => (schema.value.as_str(), schema.quote_style.is_some()),
        None if names_catalog_type(&normalize_identifier(name.0, name.1)) => return None,
        None => (DEFAULT_SCHEMA, false),
    };
    for type_ in types {
        if !user_type_in_schema(type_, schema, case) {
            continue;
        }
        if user_type_named(type_, name, case) {
            return Some((type_, false));
        }
        if let Some(multirange) = &type_.multirange_name
            && object_name_named(multirange, name, case)
            && object_name_in_schema(multirange, schema, case)
        {
            return Some((type_, true));
        }
    }
    None
}

/// Whether the stored name `stored` is called `name`, under `case`.
fn object_name_named(
    stored: &sqlparser::ast::ObjectName,
    (name, name_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    object_name_last_part(stored).is_some_and(|(stored, stored_quoted)| {
        case.identifiers_match(stored, stored_quoted, name, name_quoted)
    })
}

/// Whether the stored name `stored` lives in the schema `schema` names, under
/// `case`, a stored name without a qualifier living in the default schema.
fn object_name_in_schema(
    stored: &sqlparser::ast::ObjectName,
    (schema, schema_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    match qualifier_of(stored).named() {
        Some((stored, stored_quoted)) => {
            case.identifiers_match(stored, stored_quoted, schema, schema_quoted)
        }
        None => case.identifiers_match(DEFAULT_SCHEMA, false, schema, schema_quoted),
    }
}
