//! Implementation of the `ViewLike` trait for the two view kinds.

use alloc::string::{String, ToString};

use sqlparser::ast::{Action, Query};

use crate::{
    errors::{LookupError, ObjectKind},
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

    fn is_temporary(&self) -> bool {
        self.declaration().is_temporary()
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

    fn is_security_invoker(&self) -> bool {
        self.security_invoker()
    }

    fn is_security_barrier(&self) -> bool {
        self.security_barrier()
    }

    fn owner<'db>(&self, database: &'db ParserDB) -> Result<Option<&'db str>, LookupError> {
        Ok(view_metadata(self, database)?.owner())
    }

    fn owner_privileges<'db>(
        &'db self,
        database: &'db ParserDB,
    ) -> Result<impl Iterator<Item = &'db Action>, LookupError> {
        Ok(view_metadata(self, database)?.owner_privileges())
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

    fn owner<'db>(&self, database: &'db ParserDB) -> Result<Option<&'db str>, LookupError> {
        Ok(materialized_view_metadata(self, database)?.owner())
    }

    fn owner_privileges<'db>(
        &'db self,
        database: &'db ParserDB,
    ) -> Result<impl Iterator<Item = &'db Action>, LookupError> {
        Ok(materialized_view_metadata(self, database)?.owner_privileges())
    }

    fn declared_column_names(&self) -> &[(String, bool)] {
        self.declaration().columns()
    }
}

/// The metadata `database` holds for `view`.
fn view_metadata<'db>(
    view: &View,
    database: &'db ParserDB,
) -> Result<&'db ViewMetadata, LookupError> {
    database
        .view_metadata(view)
        .ok_or_else(|| ObjectKind::View.not_in_database(&view.target_name().to_string()))
}

/// The metadata `database` holds for the materialized view `view`.
fn materialized_view_metadata<'db>(
    view: &MaterializedView,
    database: &'db ParserDB,
) -> Result<&'db ViewMetadata, LookupError> {
    database.materialized_view_metadata(view).ok_or_else(|| {
        ObjectKind::MaterializedView.not_in_database(&view.target_name().to_string())
    })
}
