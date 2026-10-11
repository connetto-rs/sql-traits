//! Implementation of the `TriggerLike` trait for sqlparser's `CreateTrigger`
//! type.

use sqlparser::ast::CreateTrigger;

use crate::{
    errors::LookupError,
    structs::{ParserDB, TargetName},
    traits::{DatabaseLike, FunctionLike, Metadata, TriggerLike},
    utils::{
        identifier_resolution::identifiers_match,
        last_str,
        object_name::{
            object_name_last_part, resolve_required_bound_table, target_name_of_object_name,
        },
    },
};

impl Metadata for CreateTrigger {
    type Meta = ();
}

impl TriggerLike for CreateTrigger {
    type DB = ParserDB;

    #[inline]
    fn name(&self) -> &str {
        last_str(&self.name)
    }

    #[inline]
    fn table<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<&'db <Self::DB as DatabaseLike>::Table, LookupError>
    where
        Self: 'db,
    {
        resolve_required_bound_table(&self.table_name, database)
    }

    #[inline]
    fn target_table_name(&self) -> TargetName<'_> {
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

    #[inline]
    fn function<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Option<&'db <Self::DB as DatabaseLike>::Function>
    where
        Self: 'db,
    {
        let (function_name, function_quoted) = self.function_name_ident()?;
        database.functions().find(|function| {
            identifiers_match(
                function.name(),
                function.name_is_quoted(),
                function_name,
                function_quoted,
            )
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
