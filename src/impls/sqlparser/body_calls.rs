//! The functions the calls of a SQL function body may reach when it runs.
//!
//! A call resolves where PostgreSQL resolves it, under the path the body runs
//! with as [`walk_body`] tracks it, and reaches every overload accepting its
//! argument count, as [`call_reaches`] decides. Only the calls the body
//! writes count, so a view the body reads does not add the calls of its
//! definition.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::{Statement, Visit};

use super::data_statement::{BodyPath, BodyWalk, walk_body};
use crate::{
    errors::LookupError,
    structs::IdentifierCase,
    traits::DatabaseLike,
    utils::{
        call_sites::{Call, call_reaches, for_each_call},
        object_name::{Qualifier, qualifier_of},
    },
};

/// Gathers the functions each call a body makes may reach, under the path in
/// force where the call runs.
struct CallCollector<'db, 'path, DB: DatabaseLike> {
    database: &'db DB,
    path: BodyPath<'path>,
    /// Every function some call may reach, deduplicated in first-reached
    /// order.
    functions: Vec<&'db DB::Function>,
}

impl<'db, 'path, DB: DatabaseLike> CallCollector<'db, 'path, DB> {
    fn new(database: &'db DB, path: BodyPath<'path>) -> Self {
        Self { database, path, functions: Vec::new() }
    }

    /// Records every function `call` may reach under the walk's path.
    fn reach(&mut self, call: Call<'_>) -> Result<(), LookupError> {
        let (name, arguments, case) = (call.name, call.arguments, IdentifierCase::AsWritten);
        let database = self.database;
        let path = &self.path;
        if let BodyPath::Unknown(error) = path
            && qualifier_of(name) == Qualifier::Absent
        {
            return Err(error.clone());
        }
        let reaches = |function: &DB::Function| {
            match path {
                BodyPath::Database => {
                    call_reaches(function, name, arguments, || database.search_path(), case)
                }
                BodyPath::Set(entries) => {
                    let entries =
                        || entries.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
                    call_reaches(function, name, arguments, entries, case)
                }
                // Only a qualified name gets here, which reaches its schema
                // whatever the path holds.
                BodyPath::Unknown(_) => {
                    call_reaches(function, name, arguments, core::iter::empty, case)
                }
            }
        };
        for function in database.functions().filter(|function| reaches(function)) {
            if !self.functions.iter().any(|seen| core::ptr::eq(*seen, function)) {
                self.functions.push(function);
            }
        }
        Ok(())
    }

    /// Records every function the calls in `node` may reach.
    fn collect<N: Visit + ?Sized>(&mut self, node: &N) -> Result<(), LookupError> {
        let walk = for_each_call(node, |call| {
            match self.reach(call) {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        });
        walk.break_value().map_or(Ok(()), Err)
    }
}

impl<'s, DB: DatabaseLike> BodyWalk<'s> for CallCollector<'_, 's, DB> {
    fn set_path(&mut self, path: BodyPath<'s>) {
        self.path = path;
    }

    fn walk(&mut self, statement: &Statement) -> Result<(), LookupError> {
        self.collect(statement)
    }
}

/// Resolves every function the calls of `node`, a body read as a whole when
/// it runs, may reach under `path`.
///
/// # Errors
///
/// Returns the error an unknown `path` holds for an unqualified call.
pub(crate) fn calls_on_path<'db, N: Visit + ?Sized, DB: DatabaseLike>(
    node: &N,
    database: &'db DB,
    path: BodyPath<'_>,
) -> Result<Vec<&'db DB::Function>, LookupError> {
    let mut collector = CallCollector::new(database, path);
    collector.collect(node)?;
    Ok(collector.functions)
}

/// Resolves every function the calls in the statements of a SQL function
/// body may reach, one statement after another as [`walk_body`] hands them
/// over, starting under `path`.
///
/// # Errors
///
/// Returns the errors of [`walk_body`], an unqualified call after a run-time
/// path change among them.
pub(crate) fn calls_of_body<'db, DB: DatabaseLike>(
    function: &str,
    statements: &[Statement],
    database: &'db DB,
    path: BodyPath<'_>,
    hex_strings_may_be_integers: bool,
) -> Result<Vec<&'db DB::Function>, LookupError> {
    let mut collector = CallCollector::new(database, path);
    walk_body(function, statements, &mut collector, hex_strings_may_be_integers)?;
    Ok(collector.functions)
}
