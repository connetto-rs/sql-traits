//! The functions the calls of a function body may reach when it runs.
//!
//! A call resolves where PostgreSQL resolves it, under the path the body runs
//! with as [`walk_body`] tracks it, comparing names under the catalog's
//! [`DatabaseLike::identifier_case`], and reaches every overload accepting
//! its argument count, as [`call_reaches`] decides. Only the calls the body
//! writes count, so a view the body reads does not add the calls of its
//! definition.

use alloc::vec::Vec;
use core::ops::ControlFlow;

use sqlparser::ast::Visit;

use super::{
    callee_path::calls_path_changer,
    data_statement::{BodyPath, BodyWalk, RunTimeBody, add_unqualified, walk_body},
};
use crate::{
    errors::LookupError,
    structs::{EmbeddedSql, UnqualifiedName},
    traits::DatabaseLike,
    utils::{
        call_sites::{Call, call_reaches, for_each_call},
        object_name::{
            Qualifier, object_name_last_part, qualifier_of, target_name_from_object_name,
        },
    },
};

/// Gathers the functions each call a body makes may reach, under the path in
/// force where the call runs.
pub(super) struct CallCollector<'db, 'path, DB: DatabaseLike> {
    database: &'db DB,
    path: BodyPath<'path>,
    /// Every function some call may reach, deduplicated in first-reached
    /// order.
    functions: Vec<&'db DB::Function>,
    /// The calls written without a schema, each with every function it may
    /// reach, or [`None`] for a walk that does not gather them.
    pub(super) unqualified: Option<Vec<UnqualifiedName<'db, DB>>>,
}

impl<'db, 'path, DB: DatabaseLike> CallCollector<'db, 'path, DB> {
    fn new(database: &'db DB, path: BodyPath<'path>) -> Self {
        Self { database, path, functions: Vec::new(), unqualified: None }
    }

    /// A walk starting under `path` that also gathers the calls written
    /// without a schema.
    pub(super) fn gathering_unqualified(database: &'db DB, path: BodyPath<'path>) -> Self {
        Self { unqualified: Some(Vec::new()), ..Self::new(database, path) }
    }

    /// Records every function `call` may reach under the walk's path.
    fn reach(&mut self, call: Call<'_>) -> Result<(), LookupError> {
        let (name, database) = (call.name, self.database);
        let unqualified = qualifier_of(name) == Qualifier::Absent;
        if let BodyPath::Unknown(error) = &self.path
            && unqualified
        {
            return Err(error.clone());
        }
        let Some((called, _)) = object_name_last_part(name) else { return Ok(()) };
        let names = self.unqualified.as_mut().filter(|_| unqualified);
        let mut functions = Vec::new();
        for function in database
            .functions_named(called)
            .filter(|function| reaches_on_path(database, function, call, &self.path))
        {
            if !self.functions.iter().any(|seen| core::ptr::eq(*seen, function)) {
                self.functions.push(function);
            }
            if names.is_some() {
                functions.push(function);
            }
        }
        if let Some(names) = names
            && let Some(written) = target_name_from_object_name(name)
        {
            add_unqualified(
                names,
                UnqualifiedName::Function { name: written.into_owned(), functions },
            );
        }
        Ok(())
    }

    /// Records every function the calls in `node` may reach.
    pub(super) fn collect<N: Visit + ?Sized>(&mut self, node: &N) -> Result<(), LookupError> {
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
    type SavedPath = BodyPath<'s>;

    fn save_path(&self) -> Self::SavedPath {
        self.path.clone()
    }

    fn restore_path(&mut self, saved: Self::SavedPath) {
        self.path = saved;
    }

    fn set_path(&mut self, path: BodyPath<'s>) {
        self.path = path;
    }

    fn walk(&mut self, sql: EmbeddedSql<'_>) -> Result<(), LookupError> {
        self.collect(&sql)
    }

    fn calls_path_changer(&self, sql: EmbeddedSql<'_>) -> bool {
        calls_path_changer(&sql, self.database)
    }
}

/// Whether `call` may run `function` under `path`, a name written without a
/// schema reaching nothing under a path that is not known.
pub(crate) fn reaches_on_path<DB: DatabaseLike>(
    database: &DB,
    function: &DB::Function,
    call: Call<'_>,
    path: &BodyPath<'_>,
) -> bool {
    let (name, arguments, case) = (call.name, call.arguments, database.identifier_case());
    match path {
        BodyPath::Database => {
            call_reaches(function, name, arguments, || database.search_path(), case)
        }
        BodyPath::Set(entries) => {
            let entries = || entries.iter().map(|(schema, quoted)| (schema.as_ref(), *quoted));
            call_reaches(function, name, arguments, entries, case)
        }
        BodyPath::Unknown(_) => call_reaches(function, name, arguments, core::iter::empty, case),
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

/// Resolves every function the calls in the statements of a function body
/// may reach, one statement after another as [`walk_body`] hands them over,
/// starting under `path`.
///
/// # Errors
///
/// Returns the errors of [`walk_body`], an unqualified call after a run-time
/// path change among them.
pub(crate) fn calls_of_body<'db, DB: DatabaseLike>(
    function: &str,
    body: RunTimeBody<'_>,
    database: &'db DB,
    path: BodyPath<'_>,
    hex_strings_may_be_integers: bool,
) -> Result<Vec<&'db DB::Function>, LookupError> {
    let mut collector = CallCollector::new(database, path);
    walk_body(function, body, &mut collector, hex_strings_may_be_integers)?;
    Ok(collector.functions)
}
