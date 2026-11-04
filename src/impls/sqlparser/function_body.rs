//! What a function body reads and calls, read from the body
//! [`FunctionLike::written_body`] hands over, for any implementation.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use sqlparser::{
    ast::{CreateFunctionBody, Expr, Select, Statement},
    parser::Parser,
};
use sqlparser_plpgsql::{embedded::Block, embedded_sql};

use super::{
    body_calls::{calls_of_body, calls_on_path},
    data_statement::{BodyPath, Reads, RunTimeBody, reads_of_body, reads_on_path},
};
use crate::{
    errors::LookupError,
    structs::FunctionBody,
    traits::{DatabaseLike, DialectLike, FunctionLike},
};

/// The SQL a function runs, in the shape its body is written.
#[expect(
    clippy::large_enum_variant,
    reason = "one value lives on the stack per reader call, so boxing would only add an allocation"
)]
pub(super) enum SqlBody<'f> {
    /// Statements read one after another each time the function runs, parsed
    /// from the body's text or already parsed by the input.
    Statements(Cow<'f, [Statement]>),
    /// The SQL a PL/pgSQL body embeds, read each time the function runs.
    PlPgSql(Block),
    /// An expression read each time the function runs.
    Expression(&'f Expr),
    /// A query read each time the function runs.
    Select(&'f Select),
    /// A `RETURN` expression bound at creation.
    Return(Cow<'f, Expr>),
    /// The statements of a `BEGIN ATOMIC` block bound at creation.
    Atomic(Cow<'f, [Statement]>),
}

/// Whether the dialect of `database` parses `0x1F` into the hex string
/// variant `X'1F'` also parses to, so that variant may hold an integer.
fn hex_strings_may_be_integers<DB: DatabaseLike>(database: &DB) -> bool {
    !database.dialect().sqlparser_dialect().supports_non_decimal_integer_literals()
}

/// The SQL body of `function`, refusing one whose reads and calls cannot be
/// known, as [`FunctionLike::referenced_tables`] describes.
pub(super) fn sql_body<'f, F: FunctionLike>(
    function: &'f F,
    database: &F::DB,
) -> Result<SqlBody<'f>, LookupError> {
    Ok(written_sql_body(function, database)?.1)
}

/// The body of `function` as written, with the SQL [`sql_body`] reads from it.
pub(super) fn written_sql_body<'f, F: FunctionLike>(
    function: &'f F,
    database: &F::DB,
) -> Result<(FunctionBody<'f>, SqlBody<'f>), LookupError> {
    let (body, procedural) = checked_body(function)?;
    Ok((body, parsed_body(function, body, procedural, database)?))
}

/// The body of `function` as written, with whether it is PL/pgSQL, refusing
/// a body in another language than SQL and a function with no body.
fn checked_body<F: FunctionLike>(function: &F) -> Result<(FunctionBody<'_>, bool), LookupError> {
    let language = function.stored_language();
    let procedural = language.as_deref() == Some("plpgsql");
    if let Some(language) = language.filter(|language| language != "sql" && !procedural) {
        return Err(LookupError::FunctionBodyNotSql {
            function: function.name().to_string(),
            language: language.into_owned(),
        });
    }
    let Some(body) = function.written_body() else {
        return Err(LookupError::FunctionBodyUnavailable { function: function.name().to_string() });
    };
    Ok((body, procedural))
}

/// The SQL `body` of `function` holds, parsed from its text where it is
/// handed over as text.
fn parsed_body<'f, F: FunctionLike>(
    function: &F,
    body: FunctionBody<'f>,
    procedural: bool,
    database: &F::DB,
) -> Result<SqlBody<'f>, LookupError> {
    if procedural {
        let FunctionBody::Text(text) = body else {
            return Err(unparsable(function, &"a PL/pgSQL body is written as a string"));
        };
        let block = embedded_sql(text).map_err(|error| unparsable(function, &error))?;
        return Ok(SqlBody::PlPgSql(block));
    }
    Ok(match body {
        FunctionBody::Text(text) => {
            let dialect = database.dialect().sqlparser_dialect();
            let statements =
                Parser::parse_sql(dialect, text).map_err(|error| unparsable(function, &error))?;
            SqlBody::Statements(Cow::Owned(statements))
        }
        FunctionBody::Expression(expression) => SqlBody::Expression(expression),
        FunctionBody::Select(select) => SqlBody::Select(select),
        FunctionBody::Statements(statements) => SqlBody::Statements(Cow::Borrowed(statements)),
        FunctionBody::Return(expression) => SqlBody::Return(Cow::Borrowed(expression)),
        FunctionBody::Atomic(statements) => SqlBody::Atomic(Cow::Borrowed(statements)),
        FunctionBody::BoundDefinition(definition) => {
            bound_definition_body(function, definition, database)?
        }
    })
}

/// The body bound at creation that the `CREATE FUNCTION` statement
/// `definition` holds, parsed under the dialect of `database`.
fn bound_definition_body<F: FunctionLike>(
    function: &F,
    definition: &str,
    database: &F::DB,
) -> Result<SqlBody<'static>, LookupError> {
    let dialect = database.dialect().sqlparser_dialect();
    let statements =
        Parser::parse_sql(dialect, definition).map_err(|error| unparsable(function, &error))?;
    let Ok([Statement::CreateFunction(create)]) = <[Statement; 1]>::try_from(statements) else {
        return Err(unparsable(function, &"the definition is not one CREATE FUNCTION statement"));
    };
    match create.function_body {
        Some(CreateFunctionBody::Return(expression)) => Ok(SqlBody::Return(Cow::Owned(expression))),
        Some(CreateFunctionBody::BeginAtomic(block)) => {
            Ok(SqlBody::Atomic(Cow::Owned(block.statements)))
        }
        _ => Err(unparsable(function, &"the definition holds no body bound at creation")),
    }
}

/// The error naming `function` for a body that does not parse.
fn unparsable<F: FunctionLike>(function: &F, reason: &dyn ToString) -> LookupError {
    LookupError::FunctionBodyUnparsable {
        function: function.name().to_string(),
        reason: reason.to_string(),
    }
}

/// The expression of a body written `RETURN <expression>`, as
/// [`FunctionLike::body_expression`] describes.
pub(crate) fn body_expression<'f, F: FunctionLike>(
    function: &'f F,
    database: &F::DB,
) -> Result<Option<Cow<'f, Expr>>, LookupError> {
    Ok(match function.written_body() {
        Some(FunctionBody::Return(expression)) => Some(Cow::Borrowed(expression)),
        Some(FunctionBody::BoundDefinition(definition)) => {
            match bound_definition_body(function, definition, database)? {
                SqlBody::Return(expression) => Some(expression),
                _ => None,
            }
        }
        _ => None,
    })
}

/// The path a body read each time `function` runs starts under.
fn run_time_path<'f, F: FunctionLike>(
    function: &'f F,
    database: &'f F::DB,
) -> Result<BodyPath<'f>, LookupError> {
    Ok(function.search_path(database)?.map_or(BodyPath::Database, BodyPath::Set))
}

/// Everything the body of `function` reads, as
/// [`FunctionLike::referenced_relations`] describes.
pub(crate) fn body_reads<'db, F: FunctionLike>(
    function: &F,
    database: &'db F::DB,
) -> Result<Reads<'db, F::DB>, LookupError> {
    let hex_strings_may_be_integers = hex_strings_may_be_integers(database);
    match sql_body(function, database)? {
        SqlBody::Statements(statements) => {
            let path = run_time_path(function, database)?;
            let body = RunTimeBody::Sql(&statements);
            reads_of_body(function.name(), body, database, path, hex_strings_may_be_integers)
        }
        SqlBody::PlPgSql(block) => {
            let path = run_time_path(function, database)?;
            let body = RunTimeBody::PlPgSql(&block);
            reads_of_body(function.name(), body, database, path, hex_strings_may_be_integers)
        }
        SqlBody::Expression(expression) => {
            reads_on_path(expression, database, Some(run_time_path(function, database)?))
        }
        SqlBody::Select(select) => {
            reads_on_path(select, database, Some(run_time_path(function, database)?))
        }
        // Bound at creation, so the function's own path plays no part.
        SqlBody::Return(expression) => reads_on_path(expression.as_ref(), database, None),
        // A block stores a temporary relation by its bare name, which the
        // default path reaches first.
        SqlBody::Atomic(statements) => {
            let body = RunTimeBody::Sql(&statements);
            reads_of_body(function.name(), body, database, BodyPath::default_path(), false)
        }
    }
}

/// Every function the calls of the body of `function` may reach, as
/// [`FunctionLike::referenced_functions`] describes.
pub(crate) fn body_calls<'db, F: FunctionLike>(
    function: &F,
    database: &'db F::DB,
) -> Result<Vec<&'db F>, LookupError> {
    let (body, procedural) = checked_body(function)?;
    // A bound body is never parsed here, since the store records its calls.
    let sql = if body.is_bound() && !procedural {
        None
    } else {
        Some(parsed_body(function, body, procedural, database)?)
    };
    let hex_strings_may_be_integers = hex_strings_may_be_integers(database);
    match sql {
        Some(SqlBody::Statements(statements)) => {
            let path = run_time_path(function, database)?;
            let body = RunTimeBody::Sql(&statements);
            calls_of_body(function.name(), body, database, path, hex_strings_may_be_integers)
        }
        Some(SqlBody::PlPgSql(block)) => {
            let path = run_time_path(function, database)?;
            let body = RunTimeBody::PlPgSql(&block);
            calls_of_body(function.name(), body, database, path, hex_strings_may_be_integers)
        }
        Some(SqlBody::Expression(expression)) => {
            calls_on_path(expression, database, run_time_path(function, database)?)
        }
        Some(SqlBody::Select(select)) => {
            calls_on_path(select, database, run_time_path(function, database)?)
        }
        // Bound at creation, under the session's path of the time and
        // reaching the functions held then.
        None | Some(SqlBody::Return(_) | SqlBody::Atomic(_)) => {
            Ok(function.bound_body_functions(database)?.collect())
        }
    }
}
