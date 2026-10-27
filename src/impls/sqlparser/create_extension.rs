//! Implementation of the `ExtensionLike` trait for sqlparser's
//! `CreateExtension` type.

use sqlparser::ast::CreateExtension;

use crate::{
    structs::ParserDB,
    traits::{ExtensionLike, Metadata},
};

impl Metadata for CreateExtension {
    type Meta = ();
}

/// Ingestion binds `schema` to the schema the extension landed in, so it is
/// read as stored.
impl ExtensionLike for CreateExtension {
    type DB = ParserDB;

    #[inline]
    fn name(&self) -> &str {
        &self.name.value
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        self.name.quote_style.is_some()
    }

    #[inline]
    fn schema(&self) -> Option<&str> {
        self.schema.as_ref().map(|schema| schema.value.as_str())
    }

    #[inline]
    fn schema_is_quoted(&self) -> bool {
        self.schema.as_ref().is_some_and(|schema| schema.quote_style.is_some())
    }

    #[inline]
    fn version(&self) -> Option<&str> {
        self.version.as_ref().map(|version| version.value.as_str())
    }
}
