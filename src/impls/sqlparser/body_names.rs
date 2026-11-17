//! The relation and function names a function body read when it runs writes
//! without a schema, each with what it reached.
//!
//! The walk is the one [`reads_of_body`](super::data_statement::reads_of_body)
//! and [`calls_of_body`](super::body_calls::calls_of_body) make, statement by
//! statement under the path in force for each, so a name resolves here
//! exactly as those readers resolve it.

use alloc::vec::Vec;

use sqlparser::ast::Visit;

use super::{
    body_calls::CallCollector,
    data_statement::{BodyPath, BodyWalk, RelationCollector, add_unqualified},
};
use crate::{
    errors::LookupError,
    structs::{EmbeddedSql, UnqualifiedName},
    traits::DatabaseLike,
};

/// Walks a body for its relations and its calls together, gathering the
/// names each writes without a schema in the order of the statements naming
/// them.
pub(super) struct NameCollector<'db, 'path, DB: DatabaseLike> {
    relations: RelationCollector<'db, 'path, DB>,
    calls: CallCollector<'db, 'path, DB>,
    /// Every name gathered so far, deduplicated in first-met order.
    names: Vec<UnqualifiedName<'db, DB>>,
}

impl<'db, 'path, DB: DatabaseLike> NameCollector<'db, 'path, DB> {
    /// A walk starting under `path`.
    pub(super) fn new(database: &'db DB, path: BodyPath<'path>) -> Self {
        Self {
            relations: RelationCollector::gathering_unqualified(database, path.clone()),
            calls: CallCollector::gathering_unqualified(database, path),
            names: Vec::new(),
        }
    }

    /// Gathers the names in `node`, a body read as a whole when it runs.
    pub(super) fn collect<N: Visit>(&mut self, node: &N) -> Result<(), LookupError> {
        self.relations.collect(node, &[])?;
        self.calls.collect(node)?;
        self.gather();
        Ok(())
    }

    /// The names gathered, each once.
    pub(super) fn into_names(self) -> Vec<UnqualifiedName<'db, DB>> {
        self.names
    }

    /// Moves what the last node's walks gathered after what came before,
    /// its relations ahead of its calls.
    fn gather(&mut self) {
        for collected in [&mut self.relations.unqualified, &mut self.calls.unqualified] {
            for name in collected.iter_mut().flat_map(|names| names.drain(..)) {
                add_unqualified(&mut self.names, name);
            }
        }
    }
}

impl<'s, DB: DatabaseLike> BodyWalk<'s> for NameCollector<'_, 's, DB> {
    type SavedPath = BodyPath<'s>;

    fn save_path(&self) -> Self::SavedPath {
        self.calls.save_path()
    }

    fn restore_path(&mut self, saved: Self::SavedPath) {
        self.relations.restore_path(Some(saved.clone()));
        self.calls.restore_path(saved);
    }

    fn set_path(&mut self, path: BodyPath<'s>) {
        self.relations.set_path(path.clone());
        self.calls.set_path(path);
    }

    fn walk(&mut self, sql: EmbeddedSql<'_>) -> Result<(), LookupError> {
        self.relations.walk(sql)?;
        self.calls.walk(sql)?;
        self.gather();
        Ok(())
    }

    fn calls_path_changer(&self, sql: EmbeddedSql<'_>) -> bool {
        self.calls.calls_path_changer(sql)
    }
}
