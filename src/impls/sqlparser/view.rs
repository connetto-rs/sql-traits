//! Implementation of the `ViewLike` trait for the two view kinds.

use alloc::string::String;

use sqlparser::ast::Query;

use crate::{
    structs::{MaterializedView, ParserDB, View, metadata::ViewMetadata},
    traits::{Metadata, ViewLike},
};

impl Metadata for View {
    type Meta = ViewMetadata;
}

impl ViewLike for View {
    type DB = ParserDB;

    fn view_name(&self) -> &str {
        self.declaration().name()
    }

    fn view_name_is_quoted(&self) -> bool {
        self.declaration().name_is_quoted()
    }

    fn view_schema(&self) -> Option<&str> {
        self.declaration().schema()
    }

    fn view_schema_is_quoted(&self) -> bool {
        self.declaration().schema_is_quoted()
    }

    fn is_materialized(&self) -> bool {
        false
    }

    fn definition(&self) -> &Query {
        self.declaration().query()
    }

    fn selects_wildcard(&self) -> bool {
        self.declaration().selects_wildcard()
    }

    fn declared_column_names(&self) -> &[(String, bool)] {
        self.declaration().columns()
    }
}

impl Metadata for MaterializedView {
    type Meta = ViewMetadata;
}

impl ViewLike for MaterializedView {
    type DB = ParserDB;

    fn view_name(&self) -> &str {
        self.declaration().name()
    }

    fn view_name_is_quoted(&self) -> bool {
        self.declaration().name_is_quoted()
    }

    fn view_schema(&self) -> Option<&str> {
        self.declaration().schema()
    }

    fn view_schema_is_quoted(&self) -> bool {
        self.declaration().schema_is_quoted()
    }

    fn is_materialized(&self) -> bool {
        true
    }

    fn definition(&self) -> &Query {
        self.declaration().query()
    }

    fn selects_wildcard(&self) -> bool {
        self.declaration().selects_wildcard()
    }

    fn declared_column_names(&self) -> &[(String, bool)] {
        self.declaration().columns()
    }
}
