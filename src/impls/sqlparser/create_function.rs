//! Implementation of the `FunctionLike` trait for sqlparser's `CreateFunction`
//! type.

use alloc::{borrow::Cow, format, string::ToString};

use sqlparser::ast::{
    CreateFunction, CreateFunctionBody, DataType, Expr, FunctionCalledOnNull,
    FunctionDefinitionSetParam, FunctionReturnType, FunctionSecurity, Value, ValueWithSpan,
};

use crate::{
    errors::{LookupError, ObjectKind},
    structs::{FunctionMetadata, ParserDB, TargetName},
    traits::{FunctionLike, Metadata},
    utils::{
        last_str, normalize_sqlparser_type,
        object_name::{object_name_last_part, target_name_of_object_name},
    },
};

impl Metadata for CreateFunction {
    type Meta = FunctionMetadata;
}

/// Resolves the metadata `database` holds for `function`.
///
/// A [`CreateFunction`] node and a [`ParserDB`] are independent values, so a
/// node the database does not hold (dropped, replaced, or parsed from different
/// input) has no metadata to report.
fn function_metadata<'db>(
    function: &CreateFunction,
    database: &'db ParserDB,
) -> Result<&'db FunctionMetadata, LookupError> {
    database
        .function_metadata(function)
        .ok_or_else(|| ObjectKind::Function.not_in_database(&function.name.to_string()))
}

impl FunctionLike for CreateFunction {
    type DB = ParserDB;

    #[inline]
    fn name(&self) -> &str {
        last_str(&self.name)
    }

    #[inline]
    fn target_name(&self) -> TargetName<'_> {
        target_name_of_object_name(&self.name)
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        object_name_last_part(&self.name).is_some_and(|(_, quoted)| quoted)
    }

    #[inline]
    fn argument_type_names<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = Cow<'db, str>> {
        self.args
            .iter()
            .flat_map(|args| args.iter().map(|arg| normalize_sqlparser_type(&arg.data_type)))
    }

    #[inline]
    fn argument_names<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> impl Iterator<Item = Option<TargetName<'db>>> {
        self.args.iter().flat_map(|args| {
            args.iter().map(|arg| {
                arg.name
                    .as_ref()
                    .map(|name| TargetName::new(&name.value, name.quote_style.is_some()))
            })
        })
    }

    #[inline]
    fn language(&self) -> Option<&str> {
        self.language.as_ref().map(|language| language.value.as_str())
    }

    #[inline]
    fn language_is_quoted(&self) -> bool {
        self.language.as_ref().is_some_and(|language| language.quote_style.is_some())
    }

    #[inline]
    fn return_type_name<'db>(&'db self, _database: &'db Self::DB) -> Option<Cow<'db, str>> {
        self.return_type.as_ref().map(|rt| {
            match rt {
                FunctionReturnType::DataType(dt) => normalize_sqlparser_type(dt),
                // The SETOF marker survives the way an array's `[]` does:
                // whether the declaration names one value or a set of them is
                // part of the answer.
                FunctionReturnType::SetOf(dt) => {
                    Cow::Owned(format!("SETOF {}", normalize_sqlparser_type(dt)))
                }
            }
        })
    }

    #[inline]
    fn returns_set(&self) -> bool {
        match &self.return_type {
            Some(FunctionReturnType::SetOf(_)) => true,
            // A declared row shape is a set: PostgreSQL records `RETURNS
            // TABLE (...)` with `pg_proc.proretset = true`, exactly like
            // `SETOF`, and MSSQL's named-table return declares a table too.
            Some(FunctionReturnType::DataType(dt)) => {
                matches!(dt, DataType::Table(_) | DataType::NamedTable { .. })
            }
            None => false,
        }
    }

    #[inline]
    fn body(&self) -> Option<&str> {
        let Some(CreateFunctionBody::AsBeforeOptions { body: body_expr, .. }) = &self.function_body
        else {
            return None;
        };

        match body_expr {
            Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(s), .. }) => Some(s),
            Expr::Value(ValueWithSpan { value: Value::DollarQuotedString(s), .. }) => {
                Some(&s.value)
            }
            _ => None,
        }
    }

    #[inline]
    fn body_expression(&self) -> Option<&Expr> {
        match &self.function_body {
            Some(CreateFunctionBody::Return(expr)) => Some(expr),
            _ => None,
        }
    }

    #[inline]
    fn configuration_parameters(&self) -> &[FunctionDefinitionSetParam] {
        &self.set_params
    }

    #[inline]
    fn null_input_behavior(&self) -> FunctionCalledOnNull {
        self.called_on_null.clone().unwrap_or(FunctionCalledOnNull::CalledOnNullInput)
    }

    #[inline]
    fn security_mode(&self) -> FunctionSecurity {
        self.security.clone().unwrap_or(FunctionSecurity::Invoker)
    }

    #[inline]
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        Ok(function_metadata(self, database)?.owner())
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
        structs::{IdentifierCase, TargetName},
        traits::{DatabaseLike, FunctionLike},
    };

    #[test]
    fn dynamic_and_empty_function_names_report_their_quote_state() {
        let database = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE FUNCTION ping() RETURNS BOOLEAN AS 'SELECT TRUE';",
        )
        .expect("schema parses");
        let mut function = database
            .function_by_target(TargetName::new("ping", false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function exists")
            .clone();
        function.name.0 = vec![ObjectNamePart::Function(ObjectNamePartFunction {
            name: Ident::with_quote('"', "IDENTIFIER"),
            args: Vec::new(),
        })];
        // The name arrives only when the statement runs, so there is no
        // identifier here to call quoted, and none to display either.
        assert!(!function.name_is_quoted());
        assert_eq!(function.name(), "");
        function.name.0.clear();
        assert!(!function.name_is_quoted());
    }
}
