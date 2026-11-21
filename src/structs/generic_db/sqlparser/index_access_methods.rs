//! PostgreSQL built-in index access method capabilities.

use alloc::string::ToString;

use sqlparser::ast::{CreateIndex, Ident, IndexType, TableConstraint};

use super::ParserDBBuilder;
use crate::{errors::Error, impls::SqlparserDialect};

/// Resolves a written method against the supplied built-in names.
fn matching_name(
    builder: &ParserDBBuilder,
    identifier: &Ident,
    names: &[&'static str],
) -> Option<&'static str> {
    let case = builder.identifier_case();
    names.iter().copied().find(|name| {
        case.identifiers_match(&identifier.value, identifier.quote_style.is_some(), name, false)
    })
}

/// Refuses unique indexes whose built-in method cannot enforce uniqueness.
pub(super) fn refuse_in_index(builder: &ParserDBBuilder, index: &CreateIndex) -> Result<(), Error> {
    if *builder.dialect() != SqlparserDialect::PostgreSql || !index.unique {
        return Ok(());
    }
    let method = match index.using.as_ref() {
        Some(IndexType::Hash) => Some("hash"),
        Some(IndexType::GiST) => Some("gist"),
        Some(IndexType::GIN) => Some("gin"),
        Some(IndexType::BRIN) => Some("brin"),
        Some(IndexType::SPGiST) => Some("spgist"),
        Some(IndexType::Custom(identifier)) => {
            matching_name(builder, identifier, &["hash", "gist", "gin", "brin", "spgist"])
        }
        Some(IndexType::BTree | IndexType::Bloom) | None => None,
    };
    if let Some(access_method) = method {
        return Err(Error::AccessMethodWithoutUniqueSupport {
            access_method: access_method.to_string(),
        });
    }
    Ok(())
}

/// Refuses exclusions whose built-in method cannot enforce exclusion.
pub(super) fn refuse_in_constraint(
    builder: &ParserDBBuilder,
    constraint: &TableConstraint,
) -> Result<(), Error> {
    if *builder.dialect() == SqlparserDialect::PostgreSql
        && let TableConstraint::Exclude(exclusion) = constraint
        && let Some(method) = &exclusion.index_method
        && let Some(access_method) = matching_name(builder, method, &["gin", "brin"])
    {
        return Err(Error::AccessMethodWithoutExclusionSupport {
            access_method: access_method.to_string(),
        });
    }
    Ok(())
}
