//! The functions an engine provides without a declaration.
//!
//! Every database built for the engine shares one table of them, and a row is
//! declared the first time any database in the process reaches it, so building
//! a database costs nothing for the functions it never names.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

use crate::{
    structs::{IdentifierCase, Meta, generic_db::Stored},
    traits::FunctionLike,
    utils::object_name::RelationKey,
};

/// A table of the functions an engine provides, all in one schema and sorted
/// by folded name, so the rows one name folds to are adjacent.
pub(crate) trait BuiltinTable<F: FunctionLike>: Sync {
    /// The schema every function of the table lives in, as stored.
    fn schema(&self) -> &'static str;

    /// How many rows the table holds.
    fn len(&self) -> usize;

    /// The rows whose stored name folds to `folded`.
    fn rows_folding_to(&self, folded: &str) -> Range<usize>;

    /// The stored name of the function at `row`.
    fn name(&self, row: usize) -> &'static str;

    /// The function at `row` with its metadata, declared on first use and
    /// [`None`] for a row its source cannot declare.
    fn function(&self, row: usize) -> Option<&'static Stored<F>>;
}

/// The builtins a database or a builder reaches, less the rows its own
/// function store has taken over or dropped.
pub(crate) struct Builtins<F: FunctionLike + 'static> {
    table: Option<&'static dyn BuiltinTable<F>>,
    /// Rows the owning store holds or dropped, ascending.
    taken: Vec<usize>,
}

impl<F: FunctionLike + 'static> Clone for Builtins<F> {
    fn clone(&self) -> Self {
        Self { table: self.table, taken: self.taken.clone() }
    }
}

impl<F: FunctionLike + 'static> Builtins<F> {
    /// No builtins at all.
    pub(crate) const fn none() -> Self {
        Self { table: None, taken: Vec::new() }
    }

    /// Every row of `table`.
    pub(crate) fn of(table: &'static dyn BuiltinTable<F>) -> Self {
        Self { table: Some(table), taken: Vec::new() }
    }

    /// How many rows are still reached through the table.
    pub(crate) fn len(&self) -> usize {
        self.table.map_or(0, |table| table.len() - self.taken.len())
    }

    fn reached(&self, row: usize) -> bool {
        self.taken.binary_search(&row).is_err()
    }

    /// The functions of `rows` still reached through the table.
    fn declared(&self, rows: Range<usize>) -> Rows<'_, F> {
        Rows { builtins: self, rows }
    }

    /// Every row of the table.
    fn rows(&self) -> Range<usize> {
        0..self.table.map_or(0, BuiltinTable::len)
    }

    /// Every function still reached through the table, in table order.
    pub(crate) fn functions(&self) -> Rows<'_, F> {
        self.declared(self.rows())
    }

    /// The rows whose stored name folds as `name` does.
    fn rows_named(&self, name: &str) -> Range<usize> {
        self.table.map_or(0..0, |table| {
            table.rows_folding_to(&IdentifierCase::Folded.compared_form(name, false))
        })
    }

    /// The functions whose stored name folds as `name` does, in every
    /// schema, a superset of those any lookup of `name` reaches.
    pub(crate) fn named(&self, name: &str) -> Rows<'_, F> {
        self.declared(self.rows_named(name))
    }

    /// The functions an index probe for `key` reaches: those stored under
    /// exactly `key`, and with `aliases` every one whose key folds to it.
    pub(crate) fn keyed(&self, key: &RelationKey, aliases: bool) -> Rows<'_, F> {
        let rows = match self.table {
            Some(table) if table.schema() == key.schema => {
                let folded = self.rows_named(&key.name);
                if aliases {
                    folded
                } else {
                    // Within one folded name the rows are sorted by stored
                    // name.
                    let end = folded.end;
                    let start = folded.clone().find(|row| table.name(*row) == key.name);
                    let start = start.unwrap_or(end);
                    let stop = (start..end).find(|row| table.name(*row) != key.name);
                    start..stop.unwrap_or(end)
                }
            }
            _ => 0..0,
        };
        self.declared(rows)
    }

    /// The metadata of `function`, when it is one of the table's rows.
    pub(crate) fn metadata(&self, function: &F) -> Option<&Meta<F>> {
        self.named(function.name())
            .find(|(candidate, _)| {
                core::ptr::eq(candidate.as_ref(), function) || candidate.as_ref() == function
            })
            .map(|(_, metadata)| metadata)
    }

    /// Takes the rows named as `name` folds out of the table, handing their
    /// functions over for the caller's store to hold from now on.
    pub(crate) fn take_named(&mut self, name: &str) -> Vec<Stored<F>> {
        self.take(self.rows_named(name))
    }

    /// Takes every row out of the table, as [`Self::take_named`] does.
    pub(crate) fn take_all(&mut self) -> Vec<Stored<F>> {
        self.take(self.rows())
    }

    fn take(&mut self, rows: Range<usize>) -> Vec<Stored<F>> {
        let Some(table) = self.table else { return Vec::new() };
        let mut taken = Vec::new();
        for row in rows {
            let Err(position) = self.taken.binary_search(&row) else { continue };
            self.taken.insert(position, row);
            taken.extend(
                table
                    .function(row)
                    .map(|(function, metadata)| (Arc::clone(function), metadata.clone())),
            );
        }
        taken
    }
}

/// The functions of a range of table rows that a [`Builtins`] still reaches.
///
/// A named type, so the many generic callers it flows into carry a short
/// symbol rather than a nested adapter chain.
pub(crate) struct Rows<'a, F: FunctionLike + 'static> {
    builtins: &'a Builtins<F>,
    rows: Range<usize>,
}

impl<'a, F: FunctionLike + 'static> Iterator for Rows<'a, F> {
    type Item = &'a Stored<F>;

    fn next(&mut self) -> Option<Self::Item> {
        let table = self.builtins.table?;
        self.rows
            .by_ref()
            .filter(|row| self.builtins.reached(*row))
            .find_map(|row| table.function(row))
    }
}
