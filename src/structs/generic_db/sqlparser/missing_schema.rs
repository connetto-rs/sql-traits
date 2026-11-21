//! Stored names of explicitly referenced schemas absent from the model.

use alloc::string::String;

use sqlparser::ast::ObjectName;

use super::{ParserDBBuilder, collation_schema_is_declared};
use crate::utils::{identifier_resolution::identifiers_match, object_name::qualifier_of};

/// The stored name of an explicitly referenced schema that does not exist.
pub(super) fn missing_schema_name(builder: &ParserDBBuilder, name: &ObjectName) -> Option<String> {
    let (schema, quoted) = qualifier_of(name).named()?;
    let exists = collation_schema_is_declared(builder, schema, quoted)
        || ["information_schema", "pg_toast"]
            .iter()
            .any(|builtin| identifiers_match(schema, quoted, builtin, false));
    (!exists).then(|| builder.dialect().stored_spelling(schema, quoted))
}
