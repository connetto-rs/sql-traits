//! Implementation of the `DomainLike` trait for sqlparser's `CreateDomain`
//! type.

use sqlparser::ast::{CreateDomain, DataType, ObjectName};

use crate::{
    structs::{IdentifierCase, ParserDB},
    traits::{DomainLike, Metadata},
    utils::{
        identifier_resolution::normalize_identifier,
        object_name::{object_name_identifiers, object_name_last_part, qualifier_of},
        postgres_types::names_catalog_type,
    },
};

/// The schema a domain stored without a qualifier lives in.
const DEFAULT_SCHEMA: &str = "public";

impl Metadata for CreateDomain {
    /// The calls of the domain's default and checks, bound when it was
    /// written.
    type Meta = crate::structs::metadata::ExpressionCalls<sqlparser::ast::CreateFunction>;
}

/// Ingestion qualifies `name` with the schema the domain landed in, leaving
/// it bare for the default schema, so both parts are read as stored.
impl DomainLike for CreateDomain {
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
    fn data_type<'db>(&'db self, _database: &'db Self::DB) -> &'db DataType {
        &self.data_type
    }
}

/// Whether `domain` lives in the schema `schema` names, under `case`.
pub(crate) fn domain_in_schema(
    domain: &CreateDomain,
    (schema, schema_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    let (stored, stored_quoted) = domain
        .schema()
        .map_or((DEFAULT_SCHEMA, false), |stored| (stored, domain.schema_is_quoted()));
    case.identifiers_match(stored, stored_quoted, schema, schema_quoted)
}

/// Whether `domain` is called `name`, under `case`.
pub(crate) fn domain_named(
    domain: &CreateDomain,
    (name, name_quoted): (&str, bool),
    case: IdentifierCase,
) -> bool {
    case.identifiers_match(domain.name(), domain.name_is_quoted(), name, name_quoted)
}

/// The domain a type name stored in the schema names.
///
/// Ingestion rewrites every type name reaching a domain to the qualifier of
/// that domain, as it does for every other stored reference, so the name is
/// read as written with no search path, and a bare name names the default
/// schema. A bare name of a type `pg_catalog` holds reaches that catalog type,
/// and ingestion writes `public` out for a domain of such a name, so a bare
/// one never reaches a domain.
pub(crate) fn bound_domain<'a>(
    domains: impl IntoIterator<Item = &'a CreateDomain>,
    reference: &ObjectName,
    case: IdentifierCase,
) -> Option<&'a CreateDomain> {
    let (schema, name) = object_name_identifiers(reference).ok()?;
    let name = (name.value.as_str(), name.quote_style.is_some());
    let schema = match schema {
        Some(schema) => (schema.value.as_str(), schema.quote_style.is_some()),
        None if names_catalog_type(&normalize_identifier(name.0, name.1)) => return None,
        None => (DEFAULT_SCHEMA, false),
    };
    domains
        .into_iter()
        .find(|domain| domain_named(domain, name, case) && domain_in_schema(domain, schema, case))
}
