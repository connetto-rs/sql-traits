//! Implementation of the `FunctionLike` trait for sqlparser's `CreateFunction`
//! type.

use alloc::{borrow::Cow, format, string::ToString, vec::Vec};

use sqlparser::ast::{
    ArgMode, CreateFunction, CreateFunctionBody, CreateRole, CreateTable, DataType, Expr,
    FunctionBehavior, FunctionCalledOnNull, FunctionDefinitionSetParam, FunctionReturnType,
    FunctionSecurity, FunctionSetValue, Select, Statement, Value, ValueWithSpan,
};

use super::{
    body_calls::{calls_of_body, calls_on_path},
    data_statement::{BodyPath, Reads, reads_of_body, reads_on_path},
};
use crate::{
    errors::{LookupError, ObjectKind},
    structs::{FunctionKind, FunctionMetadata, ParserDB, TargetName},
    traits::{DatabaseLike, FunctionLike, Metadata, Relation, role::privilege_holders},
    utils::{
        identifier_resolution::SearchPath,
        last_str, normalize_sqlparser_type,
        object_name::{
            is_search_path_parameter, object_name_last_part, search_path_entry,
            target_name_of_object_name,
        },
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

/// The SQL a function runs, in the shape its body is written.
enum SqlBody<'f> {
    /// Statements read one after another each time the function runs, parsed
    /// from the body's text or already parsed by the input.
    Statements(Cow<'f, [Statement]>),
    /// An expression read each time the function runs.
    Expression(&'f Expr),
    /// A query read each time the function runs.
    Select(&'f Select),
    /// A `RETURN` expression bound at creation.
    Return(&'f Expr),
    /// The statements of a `BEGIN ATOMIC` block bound at creation.
    Atomic(&'f [Statement]),
}

/// The SQL body of `function`, refusing one whose reads and calls cannot be
/// known, as [`FunctionLike::referenced_tables`] describes.
fn sql_body<'f>(
    function: &'f CreateFunction,
    database: &ParserDB,
) -> Result<SqlBody<'f>, LookupError> {
    if let Some(language) = function.stored_language().filter(|language| language != "sql") {
        return Err(LookupError::FunctionBodyNotSql {
            function: function.name().to_string(),
            language: language.into_owned(),
        });
    }
    let unavailable =
        || LookupError::FunctionBodyUnavailable { function: function.name().to_string() };
    if function.using.is_some() || function.remote_connection.is_some() {
        return Err(unavailable());
    }
    match &function.function_body {
        None => Err(unavailable()),
        Some(CreateFunctionBody::AsBeforeOptions { body: expression, .. }) => {
            let Some(text) = function.body() else {
                return Ok(SqlBody::Expression(expression));
            };
            let statements = database.dialect().parse_statements(text).map_err(|error| {
                LookupError::FunctionBodyUnparsable {
                    function: function.name().to_string(),
                    reason: error.to_string(),
                }
            })?;
            Ok(SqlBody::Statements(Cow::Owned(statements)))
        }
        Some(
            CreateFunctionBody::AsAfterOptions(expression)
            | CreateFunctionBody::AsReturnExpr(expression),
        ) => Ok(SqlBody::Expression(expression)),
        Some(CreateFunctionBody::Return(expression)) => Ok(SqlBody::Return(expression)),
        Some(CreateFunctionBody::BeginAtomic(block)) => Ok(SqlBody::Atomic(&block.statements)),
        Some(CreateFunctionBody::AsBeginEnd(block)) => {
            Ok(SqlBody::Statements(Cow::Borrowed(&block.statements)))
        }
        Some(CreateFunctionBody::AsReturnSelect(select)) => Ok(SqlBody::Select(select)),
    }
}

/// The path a body read each time `function` runs starts under.
fn run_time_path<'f>(
    function: &'f CreateFunction,
    database: &'f ParserDB,
) -> Result<BodyPath<'f>, LookupError> {
    Ok(function.search_path(database)?.map_or(BodyPath::Database, BodyPath::Set))
}

/// Everything the body of `function` reads, as
/// [`FunctionLike::referenced_relations`] describes.
fn body_reads<'db>(
    function: &CreateFunction,
    database: &'db ParserDB,
) -> Result<Reads<'db, ParserDB>, LookupError> {
    let hex_strings_may_be_integers = database.dialect().hex_strings_may_be_integers();
    match sql_body(function, database)? {
        SqlBody::Statements(statements) => {
            let path = run_time_path(function, database)?;
            reads_of_body(function.name(), &statements, database, path, hex_strings_may_be_integers)
        }
        SqlBody::Expression(expression) => {
            reads_on_path(expression, database, Some(run_time_path(function, database)?))
        }
        SqlBody::Select(select) => {
            reads_on_path(select, database, Some(run_time_path(function, database)?))
        }
        // Bound at creation, so the function's own path plays no part.
        SqlBody::Return(expression) => reads_on_path(expression, database, None),
        // A block stores a temporary relation by its bare name, which the
        // default path reaches first.
        SqlBody::Atomic(statements) => {
            reads_of_body(function.name(), statements, database, BodyPath::default_path(), false)
        }
    }
}

/// Every function the calls of the body of `function` may reach, as
/// [`FunctionLike::referenced_functions`] describes.
fn body_calls<'db>(
    function: &CreateFunction,
    database: &'db ParserDB,
) -> Result<Vec<&'db CreateFunction>, LookupError> {
    let hex_strings_may_be_integers = database.dialect().hex_strings_may_be_integers();
    match sql_body(function, database)? {
        SqlBody::Statements(statements) => {
            let path = run_time_path(function, database)?;
            calls_of_body(function.name(), &statements, database, path, hex_strings_may_be_integers)
        }
        SqlBody::Expression(expression) => {
            calls_on_path(expression, database, run_time_path(function, database)?)
        }
        SqlBody::Select(select) => {
            calls_on_path(select, database, run_time_path(function, database)?)
        }
        // Bound at creation, under the session's path of the time and
        // reaching the functions held then.
        SqlBody::Return(_) | SqlBody::Atomic(_) => {
            Ok(function_metadata(function, database)?.body_calls().functions().collect())
        }
    }
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
    fn argument_modes(&self) -> impl Iterator<Item = ArgMode> {
        self.args.iter().flatten().map(|arg| arg.mode.clone().unwrap_or(ArgMode::In))
    }

    #[inline]
    fn argument_default_count(&self) -> usize {
        self.args
            .iter()
            .flatten()
            .filter(|arg| arg.mode != Some(ArgMode::Out) && arg.default_expr.is_some())
            .count()
    }

    fn result_type_name<'db>(&'db self, _database: &'db Self::DB) -> Option<Cow<'db, str>> {
        let row = |columns: &mut dyn Iterator<Item = &DataType>| {
            let first = columns.next()?;
            Some(Cow::Owned(match columns.next() {
                None => first.to_string(),
                Some(_) => "record".to_string(),
            }))
        };
        match &self.return_type {
            Some(FunctionReturnType::DataType(DataType::Table(columns))) => {
                row(&mut columns.iter().flatten().map(|column| &column.data_type))
                    .or(Some(Cow::Borrowed("record")))
            }
            Some(FunctionReturnType::DataType(DataType::NamedTable { columns, .. })) => {
                row(&mut columns.iter().map(|column| &column.data_type))
                    .or(Some(Cow::Borrowed("record")))
            }
            Some(
                FunctionReturnType::DataType(data_type) | FunctionReturnType::SetOf(data_type),
            ) => Some(Cow::Owned(data_type.to_string())),
            None => {
                row(&mut self
                    .args
                    .iter()
                    .flatten()
                    .filter(|arg| matches!(arg.mode, Some(ArgMode::Out | ArgMode::InOut)))
                    .map(|arg| &arg.data_type))
            }
        }
    }

    #[inline]
    fn kind(&self, database: &Self::DB) -> Result<FunctionKind, LookupError> {
        Ok(function_metadata(self, database)?.kind())
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

    fn search_path<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<SearchPath<'db>>, LookupError> {
        let Some(parameter) = self
            .set_params
            .iter()
            .rev()
            .find(|parameter| is_search_path_parameter(&parameter.name))
        else {
            return Ok(None);
        };
        match &parameter.value {
            FunctionSetValue::Values(values) => {
                let hex_strings_may_be_integers = database.dialect().hex_strings_may_be_integers();
                values
                    .iter()
                    .map(|value| search_path_entry(value, hex_strings_may_be_integers))
                    .collect::<Result<_, _>>()
                    .map(Some)
            }
            // Ingestion stores the clause as the path it read, so it reaches
            // here only on a node never ingested, whose current path is the
            // database's.
            FunctionSetValue::FromCurrent => {
                Ok(Some(
                    database
                        .search_path()
                        .map(|(schema, quoted)| (Cow::Borrowed(schema), quoted))
                        .collect(),
                ))
            }
            FunctionSetValue::Default => Ok(None),
        }
    }

    #[inline]
    fn body_is_bound(&self) -> bool {
        matches!(
            self.function_body,
            Some(CreateFunctionBody::Return(_) | CreateFunctionBody::BeginAtomic(_))
        )
    }

    fn referenced_tables<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db CreateTable>, LookupError> {
        Ok(body_reads(self, database)?.tables)
    }

    fn referenced_relations<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<Relation<'db, Self::DB>>, LookupError> {
        Ok(body_reads(self, database)?.relations)
    }

    fn referenced_functions<'db>(
        &self,
        database: &'db Self::DB,
    ) -> Result<Vec<&'db CreateFunction>, LookupError> {
        body_calls(self, database)
    }

    #[inline]
    fn null_input_behavior(&self) -> FunctionCalledOnNull {
        self.called_on_null.clone().unwrap_or(FunctionCalledOnNull::CalledOnNullInput)
    }

    #[inline]
    fn returns_null_on_null_argument(&self, database: &Self::DB, index: usize) -> bool {
        let Some(argument) = self
            .args
            .iter()
            .flatten()
            .filter(|argument| !matches!(argument.mode, Some(ArgMode::Out)))
            .nth(index)
        else {
            return false;
        };
        if matches!(argument.mode, Some(ArgMode::Variadic)) {
            return false;
        }
        match self.null_input_behavior() {
            FunctionCalledOnNull::Strict | FunctionCalledOnNull::ReturnsNullOnNullInput => true,
            FunctionCalledOnNull::CalledOnNullInput => {
                function_metadata(self, database)
                    .is_ok_and(|metadata| metadata.returns_null_on_null_argument(index))
            }
        }
    }

    #[inline]
    fn volatility(&self) -> FunctionBehavior {
        self.behavior.clone().unwrap_or(FunctionBehavior::Volatile)
    }

    #[inline]
    fn security_mode(&self) -> FunctionSecurity {
        self.security.clone().unwrap_or(FunctionSecurity::Invoker)
    }

    #[inline]
    fn owner<'db>(&self, database: &'db Self::DB) -> Result<Option<&'db str>, LookupError> {
        Ok(function_metadata(self, database)?.owner())
    }

    #[inline]
    fn is_executable_by_public(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(function_metadata(self, database)?.execute_privileges().public())
    }

    #[inline]
    fn can_execute(&self, role: &CreateRole, database: &Self::DB) -> Result<bool, LookupError> {
        let metadata = function_metadata(self, database)?;
        Ok(privilege_holders(role, database).any(|holder| metadata.can_execute(holder)))
    }

    #[inline]
    fn can_grant_execute(
        &self,
        role: &CreateRole,
        database: &Self::DB,
    ) -> Result<bool, LookupError> {
        let metadata = function_metadata(self, database)?;
        Ok(privilege_holders(role, database).any(|holder| metadata.can_grant_execute(holder)))
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

    /// The definer functions of one schema, each written so that its own path
    /// or its body's binding differs from the others.
    const DEFINERS: &str = "
        CREATE SCHEMA admin;
        CREATE TABLE admin.docs (id INT);
        SET search_path = admin, public;
        CREATE FUNCTION string_body() RETURNS BIGINT LANGUAGE sql SECURITY DEFINER
            AS 'SELECT count(*) FROM docs';
        CREATE FUNCTION return_body() RETURNS BIGINT LANGUAGE sql SECURITY DEFINER
            RETURN (SELECT count(*) FROM docs);
        CREATE FUNCTION atomic_body() RETURNS BIGINT LANGUAGE sql SECURITY DEFINER
            BEGIN ATOMIC SELECT count(*) FROM docs; END;
        CREATE FUNCTION c_body() RETURNS BIGINT LANGUAGE c SECURITY DEFINER AS 'lib', 'sym';
        CREATE FUNCTION quoted_path() RETURNS INT LANGUAGE sql SECURITY DEFINER
            SET search_path = 'admin', \"pg_temp\" AS 'SELECT 1';
        CREATE FUNCTION from_current() RETURNS INT LANGUAGE sql SECURITY DEFINER
            SET search_path FROM CURRENT AS 'SELECT 1';
        CREATE FUNCTION emptied() RETURNS INT LANGUAGE sql SECURITY DEFINER
            SET search_path = '' AS 'SELECT 1';
        CREATE FUNCTION reset_later() RETURNS INT LANGUAGE sql SECURITY DEFINER
            SET search_path = '' AS 'SELECT 1';
        ALTER FUNCTION reset_later() RESET search_path;
        CREATE FUNCTION reset_all() RETURNS INT LANGUAGE sql SECURITY DEFINER
            SET search_path = admin AS 'SELECT 1';
        ALTER FUNCTION reset_all() RESET ALL;
        CREATE FUNCTION set_later() RETURNS INT LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
        ALTER FUNCTION set_later() SET search_path = PG_TEMP, \"Admin\";
        CREATE FUNCTION set_default() RETURNS INT LANGUAGE sql SECURITY DEFINER
            SET search_path = admin AS 'SELECT 1';
        ALTER FUNCTION set_default() SET search_path TO DEFAULT;
    ";

    fn own_path(database: &ParserDB, name: &str) -> Option<Vec<(String, bool)>> {
        database
            .function_by_target(TargetName::new(name, false), IdentifierCase::AsWritten)
            .expect("unambiguous lookup")
            .expect("function exists")
            .search_path(database)
            .expect("ingested path reads")
            .map(|path| {
                path.into_iter().map(|(schema, quoted)| (schema.into_owned(), quoted)).collect()
            })
    }

    #[test]
    fn own_search_path_follows_every_setting_and_reset() {
        let database = ParserDB::parse::<PostgreSqlDialect>(DEFINERS).expect("schema parses");
        let schema = |name: &str, quoted| (name.to_string(), quoted);
        for unset in ["string_body", "return_body", "atomic_body", "c_body", "reset_later"] {
            assert_eq!(own_path(&database, unset), None, "{unset} runs under the caller's path");
        }
        for unset in ["reset_all", "set_default"] {
            assert_eq!(own_path(&database, unset), None, "{unset} dropped its setting");
        }
        assert_eq!(
            own_path(&database, "quoted_path"),
            Some(vec![schema("admin", true), schema("pg_temp", true)]),
        );
        assert_eq!(
            own_path(&database, "from_current"),
            Some(vec![schema("admin", false), schema("public", false)]),
        );
        assert_eq!(own_path(&database, "emptied"), Some(vec![schema("", true)]));
        assert_eq!(
            own_path(&database, "set_later"),
            Some(vec![schema("PG_TEMP", false), schema("Admin", true)]),
        );
    }

    #[test]
    fn only_return_and_atomic_bodies_are_bound() {
        let database = ParserDB::parse::<PostgreSqlDialect>(DEFINERS).expect("schema parses");
        let mut bound: Vec<&str> = database
            .functions()
            .filter(|function| function.body_is_bound())
            .map(FunctionLike::name)
            .collect();
        bound.sort_unstable();
        assert_eq!(bound, ["atomic_body", "return_body"]);
    }

    #[test]
    fn from_current_on_a_node_never_ingested_reads_the_database_path() {
        let database = ParserDB::parse::<PostgreSqlDialect>(DEFINERS).expect("schema parses");
        let Ok(sqlparser::ast::Statement::CreateFunction(function)) =
            database.dialect().parse_statements(
                "CREATE FUNCTION loose() RETURNS INT LANGUAGE sql SET search_path FROM CURRENT AS 'SELECT 1'",
            ).map(|mut statements| statements.remove(0))
        else {
            panic!("statement parses into a function");
        };
        let path = function.search_path(&database).expect("path reads");
        assert_eq!(
            path.map(|path| {
                path.into_iter()
                    .map(|(schema, quoted)| (schema.into_owned(), quoted))
                    .collect::<Vec<_>>()
            }),
            Some(vec![("admin".to_string(), false), ("public".to_string(), false)]),
        );
    }
}
