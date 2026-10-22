//! Submodule defining a generic `TableMetadata` struct.

use alloc::{string::String, sync::Arc, vec::Vec};

use crate::traits::{ColumnLike, DatabaseLike, TableLike};

#[derive(Debug, Clone)]
/// Metadata about a database table.
pub struct TableMetadata<T: TableLike> {
    /// The columns of the table.
    columns: Vec<Arc<<T::DB as DatabaseLike>::Column>>,
    /// The check constraints of the table.
    check_constraints: Vec<Arc<<T::DB as DatabaseLike>::CheckConstraint>>,
    /// The indices of the table.
    indices: Vec<Arc<<T::DB as DatabaseLike>::Index>>,
    /// The unique indices of the table.
    unique_indices: Vec<Arc<<T::DB as DatabaseLike>::UniqueIndex>>,
    /// The foreign keys of the table.
    foreign_keys: Vec<Arc<<T::DB as DatabaseLike>::ForeignKey>>,
    /// The columns composing the primary key of the table.
    primary_key: Vec<Arc<<T::DB as DatabaseLike>::Column>>,
    /// The names of the columns the table takes from a parent rather than
    /// declaring itself.
    ///
    /// Mirrors `pg_attribute.attislocal`: a column the child also declares is
    /// local and so absent here, even though it merged with a parent's. Held
    /// as names rather than handles because replacing the table node rebuilds
    /// every column, and the distinction has to survive that.
    inherited_column_names: Vec<String>,
    /// The rendering of each table constraint the table receives from a parent
    /// rather than declaring itself.
    ///
    /// Mirrors `pg_constraint.conislocal`: a constraint the child also declares
    /// is local and so absent here, which is what spares it when the parent
    /// drops its own. How many parents still pass one down is not held, because
    /// it follows from their nodes and is read when it is asked for. Held as
    /// renderings for the same reason the columns above are held as names.
    inherited_constraints: Vec<String>,
    /// Whether Row Level Security is enabled for the table.
    rls_enabled: bool,
    /// Whether Row Level Security is forced for the table (applies to table
    /// owners too).
    rls_forced: bool,
    /// The role the input names as the table's owner, if it names one.
    owner: Option<String>,
    /// The comment block written above the table.
    documentation: Option<String>,
}

impl<T: TableLike> Default for TableMetadata<T> {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            check_constraints: Vec::new(),
            indices: Vec::new(),
            unique_indices: Vec::new(),
            foreign_keys: Vec::new(),
            primary_key: Vec::new(),
            inherited_column_names: Vec::new(),
            inherited_constraints: Vec::new(),
            rls_enabled: false,
            rls_forced: false,
            owner: None,
            documentation: None,
        }
    }
}

impl<T: TableLike> TableMetadata<T> {
    /// Returns whether Row Level Security is enabled for the table.
    #[inline]
    #[must_use]
    pub fn rls_enabled(&self) -> bool {
        self.rls_enabled
    }

    /// Sets whether Row Level Security is enabled for the table.
    ///
    /// # Arguments
    ///
    /// * `rls_enabled` - Whether Row Level Security is enabled.
    #[inline]
    pub fn set_rls_enabled(&mut self, rls_enabled: bool) {
        self.rls_enabled = rls_enabled;
    }

    /// Returns whether Row Level Security is forced for the table.
    ///
    /// When RLS is forced, the policies apply even to the table owner,
    /// unlike regular RLS where the owner bypasses policies.
    #[inline]
    #[must_use]
    pub fn rls_forced(&self) -> bool {
        self.rls_forced
    }

    /// Sets whether Row Level Security is forced for the table.
    ///
    /// # Arguments
    ///
    /// * `rls_forced` - Whether Row Level Security is forced.
    #[inline]
    pub fn set_rls_forced(&mut self, rls_forced: bool) {
        self.rls_forced = rls_forced;
    }

    /// Returns the role the input names as the table's owner.
    #[inline]
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// Sets the role the input names as the table's owner.
    ///
    /// # Arguments
    ///
    /// * `owner` - The owning role, or [`None`] when the input names no role.
    #[inline]
    pub fn set_owner(&mut self, owner: Option<String>) {
        self.owner = owner;
    }

    /// Returns an iterator over the references of columns of the table.
    #[inline]
    pub fn columns(&self) -> impl Iterator<Item = &<T::DB as DatabaseLike>::Column> {
        self.columns.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns an iterator over the Arc of columns of the table.
    #[inline]
    pub fn column_arcs(&self) -> impl Iterator<Item = &Arc<<T::DB as DatabaseLike>::Column>> {
        self.columns.iter()
    }

    /// Returns a slice of the Arc of columns of the table.
    #[must_use]
    #[inline]
    pub fn column_arc_slice(&self) -> &[Arc<<T::DB as DatabaseLike>::Column>] {
        &self.columns
    }

    /// Returns an iterator over the columns the table declares itself.
    ///
    /// A column the table declares and a parent also declares counts as local,
    /// matching `pg_attribute.attislocal`.
    #[inline]
    pub fn local_columns(&self) -> impl Iterator<Item = &<T::DB as DatabaseLike>::Column> {
        self.columns
            .iter()
            .filter(|column| !self.is_inherited(column))
            .map(core::convert::AsRef::as_ref)
    }

    fn is_inherited(&self, column: &Arc<<T::DB as DatabaseLike>::Column>) -> bool {
        self.inherited_column_names.iter().any(|name| name == column.column_name())
    }

    /// Returns the names of the columns the table takes from a parent.
    #[must_use]
    #[inline]
    pub fn inherited_column_names(&self) -> &[String] {
        &self.inherited_column_names
    }

    /// Records the names of the columns the table takes from a parent.
    #[inline]
    pub fn set_inherited_column_names(&mut self, inherited_column_names: Vec<String>) {
        self.inherited_column_names = inherited_column_names;
    }

    /// Returns the rendering of each table constraint the table takes from a
    /// parent.
    #[must_use]
    #[inline]
    pub fn inherited_constraints(&self) -> &[String] {
        &self.inherited_constraints
    }

    /// Records the rendering of each table constraint the table takes from a
    /// parent.
    #[inline]
    pub fn set_inherited_constraints(&mut self, inherited_constraints: Vec<String>) {
        self.inherited_constraints = inherited_constraints;
    }

    /// Returns an iterator over the check constraints of the table.
    #[inline]
    pub fn check_constraints(
        &self,
    ) -> impl Iterator<Item = &<T::DB as DatabaseLike>::CheckConstraint> {
        self.check_constraints.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns an iterator over the Arc of check constraints of the table.
    #[inline]
    pub fn check_constraint_arcs(
        &self,
    ) -> impl Iterator<Item = &Arc<<T::DB as DatabaseLike>::CheckConstraint>> {
        self.check_constraints.iter()
    }

    /// Returns an iterator over the indices of the table.
    #[inline]
    pub fn indices(&self) -> impl Iterator<Item = &<T::DB as DatabaseLike>::Index> {
        self.indices.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns an iterator over the Arc of indices of the table.
    #[inline]
    pub fn index_arcs(&self) -> impl Iterator<Item = &Arc<<T::DB as DatabaseLike>::Index>> {
        self.indices.iter()
    }

    /// Returns an iterator over the unique indices of the table.
    #[inline]
    pub fn unique_indices(&self) -> impl Iterator<Item = &<T::DB as DatabaseLike>::UniqueIndex> {
        self.unique_indices.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns an iterator over the Arc of unique indices of the table.
    #[inline]
    pub fn unique_index_arcs(
        &self,
    ) -> impl Iterator<Item = &Arc<<T::DB as DatabaseLike>::UniqueIndex>> {
        self.unique_indices.iter()
    }

    /// Returns an iterator over the foreign keys of the table.
    #[inline]
    pub fn foreign_keys(&self) -> impl Iterator<Item = &<T::DB as DatabaseLike>::ForeignKey> {
        self.foreign_keys.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns an iterator over the Arc of foreign keys of the table.
    #[inline]
    pub fn foreign_key_arcs(
        &self,
    ) -> impl Iterator<Item = &Arc<<T::DB as DatabaseLike>::ForeignKey>> {
        self.foreign_keys.iter()
    }

    /// Returns an iterator over the columns composing the primary key of the
    /// table.
    #[inline]
    pub fn primary_key_columns(&self) -> impl Iterator<Item = &<T::DB as DatabaseLike>::Column> {
        self.primary_key.iter().map(core::convert::AsRef::as_ref)
    }

    /// Returns the comment block written above the table.
    #[inline]
    #[must_use]
    pub fn documentation(&self) -> Option<&str> {
        self.documentation.as_deref()
    }

    /// Sets the comment block written above the table.
    #[inline]
    pub fn set_documentation(&mut self, documentation: Option<String>) {
        self.documentation = documentation;
    }

    /// Moves the documentation out, leaving the table undocumented.
    #[inline]
    pub(crate) fn take_documentation(&mut self) -> Option<String> {
        self.documentation.take()
    }

    /// Adds a column to the table metadata.
    ///
    /// # Arguments
    ///
    /// * `column` - The column to add.
    #[inline]
    pub fn add_column(&mut self, column: Arc<<T::DB as DatabaseLike>::Column>) {
        self.columns.push(column);
    }

    /// Adds a check constraint to the table metadata.
    ///
    /// # Arguments
    ///
    /// * `constraint` - The check constraint to add.
    #[inline]
    pub fn add_check_constraint(
        &mut self,
        constraint: Arc<<T::DB as DatabaseLike>::CheckConstraint>,
    ) {
        self.check_constraints.push(constraint);
    }

    /// Adds an index to the table metadata.
    ///
    /// # Arguments
    ///
    /// * `index` - The index to add.
    #[inline]
    pub fn add_index(&mut self, index: Arc<<T::DB as DatabaseLike>::Index>) {
        self.indices.push(index);
    }

    /// Adds a unique index to the table metadata.
    ///
    /// # Arguments
    ///
    /// * `index` - The unique index to add.
    #[inline]
    pub fn add_unique_index(&mut self, index: Arc<<T::DB as DatabaseLike>::UniqueIndex>) {
        self.unique_indices.push(index);
    }

    /// Adds a foreign key to the table metadata.
    ///
    /// # Arguments
    ///
    /// * `fk` - The foreign key to add.
    #[inline]
    pub fn add_foreign_key(&mut self, fk: Arc<<T::DB as DatabaseLike>::ForeignKey>) {
        self.foreign_keys.push(fk);
    }

    /// Sets the columns composing the primary key of the table.
    ///
    /// # Arguments
    ///
    /// * `pk_columns` - The columns composing the primary key.
    pub fn set_primary_key(&mut self, pk_columns: Vec<Arc<<T::DB as DatabaseLike>::Column>>) {
        self.primary_key = pk_columns;
    }

    /// Removes indices that don't match the predicate.
    ///
    /// # Arguments
    ///
    /// * `f` - A predicate function that returns `true` for indices to keep.
    pub fn retain_indices<F>(&mut self, f: F)
    where
        F: FnMut(&Arc<<T::DB as DatabaseLike>::Index>) -> bool,
    {
        self.indices.retain(f);
    }

    /// Swaps a stored index handle for another, for the parse-time rewrite
    /// that follows an `ALTER INDEX ... RENAME`.
    ///
    /// The same handle sits here and in the index store of the database, so a
    /// rename that touched only one of the two would leave the table reporting
    /// the old name.
    pub(crate) fn replace_index(
        &mut self,
        previous: &Arc<<T::DB as DatabaseLike>::Index>,
        replacement: &Arc<<T::DB as DatabaseLike>::Index>,
    ) {
        for index in &mut self.indices {
            if Arc::ptr_eq(index, previous) {
                *index = replacement.clone();
            }
        }
    }
}
