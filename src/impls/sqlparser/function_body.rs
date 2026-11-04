//! What a function body reads and calls, read from the body
//! [`FunctionLike::written_body`] hands over, for any implementation.

use alloc::{borrow::Cow, string::ToString, vec::Vec};

use sqlparser::{
    ast::{Expr, Select, Statement},
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
    Return(&'f Expr),
    /// The statements of a `BEGIN ATOMIC` block bound at creation.
    Atomic(&'f [Statement]),
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
    let unparsable = |reason: &dyn ToString| {
        LookupError::FunctionBodyUnparsable {
            function: function.name().to_string(),
            reason: reason.to_string(),
        }
    };
    if procedural {
        let FunctionBody::Text(text) = body else {
            return Err(unparsable(&"a PL/pgSQL body is written as a string"));
        };
        let block = embedded_sql(text).map_err(|error| unparsable(&error))?;
        return Ok((body, SqlBody::PlPgSql(block)));
    }
    let sql = match body {
        FunctionBody::Text(text) => {
            let dialect = database.dialect().sqlparser_dialect();
            let statements =
                Parser::parse_sql(dialect, text).map_err(|error| unparsable(&error))?;
            SqlBody::Statements(Cow::Owned(statements))
        }
        FunctionBody::Expression(expression) => SqlBody::Expression(expression),
        FunctionBody::Select(select) => SqlBody::Select(select),
        FunctionBody::Statements(statements) => SqlBody::Statements(Cow::Borrowed(statements)),
        FunctionBody::Return(expression) => SqlBody::Return(expression),
        FunctionBody::Atomic(statements) => SqlBody::Atomic(statements),
    };
    Ok((body, sql))
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
        SqlBody::Return(expression) => reads_on_path(expression, database, None),
        // A block stores a temporary relation by its bare name, which the
        // default path reaches first.
        SqlBody::Atomic(statements) => {
            let body = RunTimeBody::Sql(statements);
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
    let hex_strings_may_be_integers = hex_strings_may_be_integers(database);
    match sql_body(function, database)? {
        SqlBody::Statements(statements) => {
            let path = run_time_path(function, database)?;
            let body = RunTimeBody::Sql(&statements);
            calls_of_body(function.name(), body, database, path, hex_strings_may_be_integers)
        }
        SqlBody::PlPgSql(block) => {
            let path = run_time_path(function, database)?;
            let body = RunTimeBody::PlPgSql(&block);
            calls_of_body(function.name(), body, database, path, hex_strings_may_be_integers)
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
            Ok(function.bound_body_functions(database)?.collect())
        }
    }
}
