//! Implementation of the `TriggerLike` trait for sqlparser's `CreateTrigger`
//! type.

use alloc::borrow::Cow;

use sqlparser::ast::{ArgMode, CreateTrigger};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{ParserDB, TargetName, TriggerEnablement, metadata::TriggerMetadata},
    traits::{DatabaseLike, FunctionLike, Metadata, Relation, TableLike, TriggerLike},
    utils::{
        identifier_resolution::stored_identifier,
        last_str,
        object_name::{
            object_name_last_part, qualifier_of, resolve_bound_relation_object_name,
            resolve_bound_table_or_foreign_table, target_name_of_object_name,
        },
    },
};

/// The schema a stored or bound name lives in, `public` when it writes none.
fn in_schema(schema: Option<(&str, bool)>) -> Cow<'_, str> {
    schema.map_or(Cow::Borrowed("public"), |(schema, quoted)| stored_identifier(schema, quoted))
}

/// Resolves the metadata `database` holds for `trigger`.
fn trigger_metadata<'db>(
    trigger: &CreateTrigger,
    database: &'db ParserDB,
) -> Result<&'db TriggerMetadata, LookupError> {
    database
        .trigger_metadata(trigger)
        .ok_or_else(|| ObjectKind::Trigger.not_in_database(trigger.name()))
}

impl Metadata for CreateTrigger {
    type Meta = TriggerMetadata;
}

impl TriggerLike for CreateTrigger {
    type DB = ParserDB;

    #[inline]
    fn name(&self) -> &str {
        last_str(&self.name)
    }

    /// Ingestion bound the target to the relation it reached, so the name is
    /// read as written with no search path. Only a table, a foreign table or
    /// a plain view carries a trigger.
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db,
    {
        match resolve_bound_relation_object_name(&self.table_name, database)? {
            Some(
                relation @ (Relation::Table(_) | Relation::ForeignTable(_) | Relation::View(_)),
            ) => Ok(relation),
            Some(Relation::MaterializedView(_) | Relation::Sequence(_)) | None => {
                Err(LookupError::RelationNotFound { object_name: self.table_name.to_string() })
            }
        }
    }

    #[inline]
    fn target_relation_name(&self) -> TargetName<'_> {
        target_name_of_object_name(&self.table_name)
    }

    #[inline]
    fn events(&self) -> &[sqlparser::ast::TriggerEvent] {
        &self.events
    }

    #[inline]
    fn timing(&self) -> Option<sqlparser::ast::TriggerPeriod> {
        self.period
    }

    #[inline]
    fn orientation(&self) -> Option<sqlparser::ast::TriggerObjectKind> {
        self.trigger_object
    }

    /// The condition inside the parentheses `WHEN` writes around it, which
    /// the parser keeps as a nested expression.
    #[inline]
    fn condition(&self) -> Option<&sqlparser::ast::Expr> {
        self.condition.as_ref().map(|condition| {
            match condition {
                sqlparser::ast::Expr::Nested(inner) => inner.as_ref(),
                condition => condition,
            }
        })
    }

    /// The trigger of the same name on the table directly above the
    /// partition, for a trigger ingestion recorded as its copy.
    fn parent_trigger<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<&'db <Self::DB as DatabaseLike>::Trigger>, LookupError>
    where
        Self: 'db,
    {
        if !trigger_metadata(self, database)?.is_clone() {
            return Ok(None);
        }
        // A copy lives on a partition, which is a table or a foreign table.
        let partition = resolve_bound_table_or_foreign_table(&self.table_name, database)?;
        let parent_table =
            partition.map(|partition| partition.partition_root(database)).transpose()?.flatten();
        Ok(parent_table.and_then(|parent_table| database.trigger(self.name(), parent_table)))
    }

    fn enablement(&self, database: &Self::DB) -> Result<TriggerEnablement, LookupError> {
        Ok(trigger_metadata(self, database)?.enablement())
    }

    /// The function ingestion bound the trigger to, which takes no arguments
    /// and lives in the schema the bound name carries, `public` when bare.
    #[inline]
    fn function<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>
    where
        Self: 'db,
    {
        let called = &self.exec_body.as_ref()?.func_desc.name;
        let (name, quoted) = object_name_last_part(called)?;
        let name = stored_identifier(name, quoted);
        let schema = in_schema(qualifier_of(called).named());
        database.functions_named(&name).find(|function| {
            let target = function.target_name();
            function
                .args
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|argument| argument.mode == Some(ArgMode::Out))
                && function.stored_name() == name
                && in_schema(target.schema().map(|schema| (schema, target.schema_is_quoted())))
                    == schema
        })
    }

    #[inline]
    fn function_name(&self) -> Option<&str> {
        self.function_name_ident().map(|(name, _)| name)
    }

    #[inline]
    fn function_name_ident(&self) -> Option<(&str, bool)> {
        let body = self.exec_body.as_ref()?;
        object_name_last_part(&body.func_desc.name)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::{
        ast::{Ident, ObjectNamePart, ObjectNamePartFunction},
        dialect::PostgreSqlDialect,
    };

    use crate::{
        prelude::ParserDB,
        traits::{DatabaseLike, FunctionLike, TriggerLike},
    };

    #[test]
    fn dynamic_and_empty_trigger_function_names_are_exposed() {
        let database = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE TABLE docs(id INT);
             CREATE FUNCTION touch() RETURNS TRIGGER AS 'BEGIN END' LANGUAGE plpgsql;
             CREATE TRIGGER docs_touch AFTER INSERT ON docs
             FOR EACH ROW EXECUTE FUNCTION touch();",
        )
        .expect("schema parses");
        let mut trigger = database.triggers().next().expect("trigger exists").clone();
        let function_name = &mut trigger.exec_body.as_mut().expect("execution body").func_desc.name;
        function_name.0 = vec![ObjectNamePart::Function(ObjectNamePartFunction {
            name: Ident::with_quote('"', "IDENTIFIER"),
            args: Vec::new(),
        })];
        // Which function the trigger will call is decided when it fires, so
        // the trigger names none and reaches none.
        assert_eq!(trigger.function_name(), None);
        assert_eq!(trigger.function(&database).map(FunctionLike::name), None);
        trigger.exec_body.as_mut().expect("execution body").func_desc.name.0.clear();
        assert_eq!(trigger.function_name(), None);
    }
}
