//! Builder for constructing a `GenericDB` instance.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use crate::{
    errors::LookupError,
    structs::{
        ColumnIdentity, GenericDB, IdentifierCase, IdentityCounter, Meta, RelationIdentity,
        SchemaProfile,
        generic_db::{Identified, RelationSlot, Stored},
    },
    traits::{
        ExtensionLike, FunctionLike, PolicyLike, RoleLike, RuleLike, SchemaLike, TableLike,
        TriggerLike, ViewLike,
    },
    utils::object_name::{
        RelationKey, render_table_candidate, stored_function_key, stored_sequence_key,
        stored_table_key, stored_view_key, table_matches_key, view_matches_key,
    },
};

fn table_names_match<T: TableLike>(left: &T, right: &T, case: IdentifierCase) -> bool {
    case.identifiers_match(
        left.table_name(),
        left.table_name_is_quoted(),
        right.table_name(),
        right.table_name_is_quoted(),
    )
}

fn table_schema_is_public<T: TableLike>(table: &T, case: IdentifierCase) -> bool {
    table.table_schema().is_some_and(|schema_name| {
        case.identifiers_match(schema_name, table.table_schema_is_quoted(), "public", false)
    })
}

fn tables_share_identity<T: TableLike>(left: &T, right: &T, case: IdentifierCase) -> bool {
    table_names_match(left, right, case)
        && match (left.table_schema(), right.table_schema()) {
            (None, None) => true,
            (Some(left_schema), Some(right_schema)) => {
                case.identifiers_match(
                    left_schema,
                    left.table_schema_is_quoted(),
                    right_schema,
                    right.table_schema_is_quoted(),
                )
            }
            _ => false,
        }
}

fn creates_implicit_public_ambiguity<T: TableLike>(
    left: &T,
    right: &T,
    case: IdentifierCase,
) -> bool {
    table_names_match(left, right, case)
        && ((left.table_schema().is_none() && table_schema_is_public(right, case))
            || (right.table_schema().is_none() && table_schema_is_public(left, case)))
}

/// The stored views one of `keys` names, each keyed as written.
fn views_named_by<V: ViewLike>(
    views: &[Identified<V, RelationIdentity>],
    keys: &[RelationKey],
) -> Vec<Identified<V, RelationIdentity>> {
    views
        .iter()
        .filter(|(view, _, _)| {
            keys.iter().any(|key| view_matches_key(view.as_ref(), key, IdentifierCase::AsWritten))
        })
        .cloned()
        .collect()
}

/// Builder for constructing a `GenericDB` instance.
pub struct GenericDBBuilder<P: SchemaProfile> {
    /// SQL dialect of the database.
    dialect: P::Dialect,
    /// Catalog name of the database.
    catalog_name: String,
    /// Timezone of the database.
    timezone: Option<String>,
    /// List of tables in the database.
    tables: Vec<Identified<P::Table, RelationIdentity>>,
    /// List of plain views in the database.
    views: Vec<Identified<P::View, RelationIdentity>>,
    /// List of materialized views in the database.
    materialized_views: Vec<Identified<P::MaterializedView, RelationIdentity>>,
    /// List of sequences in the database.
    sequences: Vec<Stored<P::Sequence>>,
    /// Sequences the statement being applied has dropped, kept until the
    /// column defaults still drawing from them are settled.
    dropped_sequences: Vec<Arc<P::Sequence>>,
    /// List of columns in the database.
    columns: Vec<Identified<P::Column, ColumnIdentity>>,
    /// List of indices in the database.
    indices: Vec<Stored<P::Index>>,
    /// List of unique indices in the database.
    unique_indices: Vec<Stored<P::UniqueIndex>>,
    /// List of foreign keys in the database.
    foreign_keys: Vec<Stored<P::ForeignKey>>,
    /// List of functions created in the database.
    functions: Vec<Stored<P::Function>>,
    /// List of triggers created in the database.
    triggers: Vec<Stored<P::Trigger>>,
    /// List of rewrite rules created in the database.
    rules: Vec<Stored<P::Rule>>,
    /// List of extensions installed in the database.
    extensions: Vec<Stored<P::Extension>>,
    /// List of policies created in the database.
    policies: Vec<Stored<P::Policy>>,
    /// List of check constraints in the database.
    check_constraints: Vec<Stored<P::CheckConstraint>>,
    /// List of roles in the database.
    roles: Vec<Stored<P::Role>>,
    /// List of role memberships in the database, in the order granted.
    role_memberships: Vec<Stored<P::RoleMembership>>,
    /// List of schemas in the database.
    schemas: Vec<Stored<P::Schema>>,
    /// List of table grants in the database.
    table_grants: Vec<Stored<P::TableGrant>>,
    /// List of column grants in the database.
    column_grants: Vec<Stored<P::ColumnGrant>>,
    /// Schemas an unqualified name is resolved against, in order.
    ///
    /// Defaults to `public` alone, and `SET search_path` replaces it wholesale
    /// rather than extending it, which is what the database does.
    search_path: Vec<(String, bool)>,
    /// Comparison deciding whether a creation takes a name something already
    /// holds, which each engine answers its own way.
    identifier_case: IdentifierCase,
    /// Where the next identity is drawn from.
    identities: IdentityCounter,
}

impl<P: SchemaProfile> Clone for GenericDBBuilder<P> {
    fn clone(&self) -> Self {
        Self {
            dialect: self.dialect.clone(),
            catalog_name: self.catalog_name.clone(),
            timezone: self.timezone.clone(),
            tables: self.tables.clone(),
            views: self.views.clone(),
            materialized_views: self.materialized_views.clone(),
            sequences: self.sequences.clone(),
            dropped_sequences: self.dropped_sequences.clone(),
            columns: self.columns.clone(),
            indices: self.indices.clone(),
            unique_indices: self.unique_indices.clone(),
            foreign_keys: self.foreign_keys.clone(),
            functions: self.functions.clone(),
            triggers: self.triggers.clone(),
            rules: self.rules.clone(),
            extensions: self.extensions.clone(),
            policies: self.policies.clone(),
            check_constraints: self.check_constraints.clone(),
            roles: self.roles.clone(),
            role_memberships: self.role_memberships.clone(),
            schemas: self.schemas.clone(),
            table_grants: self.table_grants.clone(),
            column_grants: self.column_grants.clone(),
            search_path: self.search_path.clone(),
            identifier_case: self.identifier_case,
            identities: self.identities,
        }
    }
}

impl<P: SchemaProfile> GenericDBBuilder<P> {
    /// Returns a database carrying the given ingestion state, without
    /// consuming the current builder.
    pub(crate) fn snapshot_with(&self, ingestion: P::Ingestion) -> GenericDB<P> {
        self.clone().into_database(ingestion)
    }

    /// Returns a database holding only the tables, views and materialized
    /// views `keys` names, and their columns, `column_table` naming each
    /// column's table.
    pub(crate) fn relations_database(
        &self,
        keys: &[RelationKey],
        column_table: impl Fn(&P::Column) -> &P::Table,
    ) -> GenericDB<P> {
        let case = IdentifierCase::AsWritten;
        let tables: Vec<_> = self
            .tables
            .iter()
            .filter(|(table, _, _)| {
                keys.iter().any(|key| table_matches_key(table.as_ref(), key, case))
            })
            .cloned()
            .collect();
        let columns = self
            .columns
            .iter()
            .filter(|(column, _, _)| {
                let owner = column_table(column);
                tables.iter().any(|(table, _, _)| core::ptr::eq(table.as_ref(), owner))
            })
            .cloned()
            .collect();
        Self {
            dialect: self.dialect.clone(),
            catalog_name: self.catalog_name.clone(),
            timezone: self.timezone.clone(),
            views: views_named_by(&self.views, keys),
            materialized_views: views_named_by(&self.materialized_views, keys),
            sequences: Vec::new(),
            dropped_sequences: Vec::new(),
            tables,
            columns,
            indices: Vec::new(),
            unique_indices: Vec::new(),
            foreign_keys: Vec::new(),
            functions: Vec::new(),
            triggers: Vec::new(),
            rules: Vec::new(),
            extensions: Vec::new(),
            policies: Vec::new(),
            check_constraints: Vec::new(),
            roles: Vec::new(),
            role_memberships: Vec::new(),
            schemas: self.schemas.clone(),
            table_grants: Vec::new(),
            column_grants: Vec::new(),
            search_path: self.search_path.clone(),
            identifier_case: self.identifier_case,
            identities: self.identities,
        }
        .into_database(P::default_ingestion(&self.dialect))
    }

    /// Returns a mutable reference to the tables list.
    pub(crate) fn tables_mut(&mut self) -> &mut Vec<Identified<P::Table, RelationIdentity>> {
        &mut self.tables
    }

    /// Returns a mutable reference to the plain views list.
    pub(crate) fn views_mut(&mut self) -> &mut Vec<Identified<P::View, RelationIdentity>> {
        &mut self.views
    }

    /// Returns the plain views recorded so far, with their metadata.
    pub(crate) fn views(&self) -> &[Identified<P::View, RelationIdentity>] {
        &self.views
    }

    /// Returns a mutable reference to the materialized views list.
    pub(crate) fn materialized_views_mut(
        &mut self,
    ) -> &mut Vec<Identified<P::MaterializedView, RelationIdentity>> {
        &mut self.materialized_views
    }

    /// Returns the materialized views recorded so far, with their metadata.
    pub(crate) fn materialized_views(
        &self,
    ) -> &[Identified<P::MaterializedView, RelationIdentity>] {
        &self.materialized_views
    }

    /// Returns the sequences recorded so far, with their metadata.
    pub(crate) fn sequences(&self) -> &[Stored<P::Sequence>] {
        &self.sequences
    }

    /// Returns a mutable reference to the sequences list.
    pub(crate) fn sequences_mut(&mut self) -> &mut Vec<Stored<P::Sequence>> {
        &mut self.sequences
    }

    /// Records sequences the statement being applied dropped.
    pub(crate) fn record_dropped_sequences(
        &mut self,
        dropped: impl IntoIterator<Item = Arc<P::Sequence>>,
    ) {
        self.dropped_sequences.extend(dropped);
    }

    /// Hands over the sequences dropped since the last call.
    pub(crate) fn take_dropped_sequences(&mut self) -> Vec<Arc<P::Sequence>> {
        core::mem::take(&mut self.dropped_sequences)
    }

    /// Returns how many relations hold a name in some schema's pool: tables,
    /// views of both kinds, sequences and indexes.
    pub(crate) fn relation_count(&self) -> usize {
        self.tables.len()
            + self.views.len()
            + self.materialized_views.len()
            + self.sequences.len()
            + self.indices.len()
            + self.unique_indices.len()
    }

    /// Returns `saved` as the builder a rollback leaves, keeping this
    /// builder's identity counter so no identity is handed out twice.
    pub(crate) fn rolled_back_to(&self, mut saved: Self) -> Self {
        saved.identities = self.identities;
        saved
    }

    /// Hands out a relation identity no object of this ingestion has held.
    pub(crate) fn draw_relation_identity(&mut self) -> RelationIdentity {
        self.identities.relation()
    }

    /// Hands out a column identity no object of this ingestion has held.
    pub(crate) fn draw_column_identity(&mut self) -> ColumnIdentity {
        self.identities.column()
    }

    /// Returns the schemas an unqualified name resolves against, in order,
    /// each with whether it was quoted.
    pub(crate) fn search_path(&self) -> impl Iterator<Item = (&str, bool)> {
        self.search_path.iter().map(|(name, quoted)| (name.as_str(), *quoted))
    }

    /// Replaces the schemas an unqualified name resolves against.
    pub(crate) fn set_search_path(&mut self, search_path: Vec<(String, bool)>) {
        self.search_path = search_path;
    }

    /// Returns the SQL dialect recorded for this builder.
    pub(crate) fn dialect(&self) -> &P::Dialect {
        &self.dialect
    }

    /// Returns the catalog this builder is filling, which a qualified
    /// reference may name outermost.
    pub(crate) fn catalog_name(&self) -> &str {
        &self.catalog_name
    }

    /// Returns the path a database starts with, and the one `RESET` restores.
    pub(crate) fn default_search_path() -> Vec<(String, bool)> {
        crate::utils::object_name::DEFAULT_SEARCH_PATH
            .iter()
            .map(|&(schema, quoted)| (schema.to_string(), quoted))
            .collect()
    }

    /// Returns a mutable reference to the table grants list.
    pub(crate) fn table_grants_mut(&mut self) -> &mut Vec<Stored<P::TableGrant>> {
        &mut self.table_grants
    }

    /// Returns a mutable reference to the column grants list.
    pub(crate) fn column_grants_mut(&mut self) -> &mut Vec<Stored<P::ColumnGrant>> {
        &mut self.column_grants
    }

    /// Returns a mutable reference to the functions list.
    pub(crate) fn functions_mut(&mut self) -> &mut Vec<Stored<P::Function>> {
        &mut self.functions
    }

    /// Returns a slice of check constraint Arc references with their metadata.
    pub(crate) fn check_constraints(&self) -> &[Stored<P::CheckConstraint>] {
        &self.check_constraints
    }

    /// Returns a slice of policy Arc references with their metadata.
    pub(crate) fn policies(&self) -> &[Stored<P::Policy>] {
        &self.policies
    }

    /// Returns a slice of trigger Arc references with their metadata.
    pub(crate) fn triggers(&self) -> &[Stored<P::Trigger>] {
        &self.triggers
    }

    /// Returns a slice of rule Arc references with their metadata.
    pub(crate) fn rules(&self) -> &[Stored<P::Rule>] {
        &self.rules
    }

    /// Returns a slice of extension Arc references with their metadata.
    pub(crate) fn extensions(&self) -> &[Stored<P::Extension>] {
        &self.extensions
    }

    /// Returns a slice of foreign key Arc references with their metadata.
    pub(crate) fn foreign_keys(&self) -> &[Stored<P::ForeignKey>] {
        &self.foreign_keys
    }

    /// Returns a slice of index Arc references with their metadata.
    pub(crate) fn indices(&self) -> &[Stored<P::Index>] {
        &self.indices
    }

    /// Returns a slice of unique index Arc references with their metadata.
    pub(crate) fn unique_indices(&self) -> &[Stored<P::UniqueIndex>] {
        &self.unique_indices
    }

    /// Returns a slice of function Arc references with their metadata.
    pub(crate) fn functions(&self) -> &[Stored<P::Function>] {
        &self.functions
    }

    /// Returns a slice of table grant Arc references with their metadata.
    pub(crate) fn table_grants(&self) -> &[Stored<P::TableGrant>] {
        &self.table_grants
    }

    /// Returns a slice of column grant Arc references with their metadata.
    pub(crate) fn column_grants(&self) -> &[Stored<P::ColumnGrant>] {
        &self.column_grants
    }

    /// Returns a slice of column Arc references with their metadata.
    pub(crate) fn columns(&self) -> &[Identified<P::Column, ColumnIdentity>] {
        &self.columns
    }

    /// Returns a mutable reference to the columns list.
    pub(crate) fn columns_mut(&mut self) -> &mut Vec<Identified<P::Column, ColumnIdentity>> {
        &mut self.columns
    }

    /// Returns a mutable reference to the indices list.
    pub(crate) fn indices_mut(&mut self) -> &mut Vec<Stored<P::Index>> {
        &mut self.indices
    }

    /// Returns a mutable reference to the unique indices list.
    pub(crate) fn unique_indices_mut(&mut self) -> &mut Vec<Stored<P::UniqueIndex>> {
        &mut self.unique_indices
    }

    /// Returns a mutable reference to the foreign keys list.
    pub(crate) fn foreign_keys_mut(&mut self) -> &mut Vec<Stored<P::ForeignKey>> {
        &mut self.foreign_keys
    }

    /// Returns a mutable reference to the check constraints list.
    pub(crate) fn check_constraints_mut(&mut self) -> &mut Vec<Stored<P::CheckConstraint>> {
        &mut self.check_constraints
    }

    /// Returns a mutable reference to the triggers list.
    pub(crate) fn triggers_mut(&mut self) -> &mut Vec<Stored<P::Trigger>> {
        &mut self.triggers
    }

    /// Returns a mutable reference to the rules list.
    pub(crate) fn rules_mut(&mut self) -> &mut Vec<Stored<P::Rule>> {
        &mut self.rules
    }

    /// Returns a mutable reference to the extensions list.
    pub(crate) fn extensions_mut(&mut self) -> &mut Vec<Stored<P::Extension>> {
        &mut self.extensions
    }

    /// Returns a mutable reference to the policies list.
    pub(crate) fn policies_mut(&mut self) -> &mut Vec<Stored<P::Policy>> {
        &mut self.policies
    }

    /// Returns a mutable reference to the roles list.
    pub(crate) fn roles_mut(&mut self) -> &mut Vec<Stored<P::Role>> {
        &mut self.roles
    }

    /// Returns a slice of role membership Arc references with their metadata,
    /// in the order granted.
    #[must_use]
    pub fn role_memberships(&self) -> &[Stored<P::RoleMembership>] {
        &self.role_memberships
    }

    /// Returns a mutable reference to the role memberships list.
    pub(crate) fn role_memberships_mut(&mut self) -> &mut Vec<Stored<P::RoleMembership>> {
        &mut self.role_memberships
    }

    /// Returns a slice of schema Arc references with their metadata.
    pub(crate) fn schemas(&self) -> &[Stored<P::Schema>] {
        &self.schemas
    }

    /// Returns a mutable reference to the schemas list.
    pub(crate) fn schemas_mut(&mut self) -> &mut Vec<Stored<P::Schema>> {
        &mut self.schemas
    }

    #[must_use]
    /// Creates a new `GenericDBBuilder` instance.
    pub fn new(catalog_name: String, dialect: P::Dialect) -> Self {
        Self {
            dialect,
            catalog_name,
            timezone: None,
            tables: Vec::new(),
            views: Vec::new(),
            materialized_views: Vec::new(),
            sequences: Vec::new(),
            dropped_sequences: Vec::new(),
            columns: Vec::new(),
            indices: Vec::new(),
            unique_indices: Vec::new(),
            foreign_keys: Vec::new(),
            functions: Vec::new(),
            triggers: Vec::new(),
            rules: Vec::new(),
            extensions: Vec::new(),
            policies: Vec::new(),
            check_constraints: Vec::new(),
            roles: Vec::new(),
            role_memberships: Vec::new(),
            schemas: Vec::new(),
            table_grants: Vec::new(),
            column_grants: Vec::new(),
            search_path: Self::default_search_path(),
            identifier_case: IdentifierCase::AsWritten,
            identities: IdentityCounter::default(),
        }
    }
}

impl<P: SchemaProfile> GenericDBBuilder<P> {
    /// The comparison this input takes a name under.
    pub(crate) const fn identifier_case(&self) -> IdentifierCase {
        self.identifier_case
    }

    /// Sets the comparison deciding whether a creation takes a name already
    /// held, which is PostgreSQL's rule unless the caller says otherwise.
    #[must_use]
    #[inline]
    pub fn with_identifier_case(mut self, case: IdentifierCase) -> Self {
        self.identifier_case = case;
        self
    }

    fn ensure_table_lookup_invariants(&self, table: &P::Table) -> Result<(), LookupError> {
        let case = self.identifier_case;
        for (existing, _, _) in self.tables() {
            let existing = existing.as_ref();
            if tables_share_identity(existing, table, case)
                || creates_implicit_public_ambiguity(existing, table, case)
            {
                return Err(LookupError::TableLookupConflict {
                    table: render_table_candidate(table),
                    conflicting_table: render_table_candidate(existing),
                    case,
                });
            }
        }
        Ok(())
    }

    /// Sets the timezone for the database.
    #[must_use]
    #[inline]
    pub fn timezone(mut self, timezone: String) -> Self {
        self.timezone = Some(timezone);
        self
    }

    /// Adds a table with its metadata to the builder, under an identity no
    /// object of this builder has held.
    ///
    /// # Errors
    ///
    /// Returns an error if adding the table would introduce semantic lookup
    /// ambiguity.
    pub fn add_table(
        mut self,
        table: Arc<P::Table>,
        metadata: Meta<P::Table>,
    ) -> Result<Self, LookupError> {
        let identity = self.draw_relation_identity();
        self.add_table_with_identity(table, metadata, identity)
    }

    /// Adds a table under the identity it already holds, for a statement that
    /// replaces the table's node.
    pub(crate) fn add_table_with_identity(
        mut self,
        table: Arc<P::Table>,
        metadata: Meta<P::Table>,
        identity: RelationIdentity,
    ) -> Result<Self, LookupError> {
        self.ensure_table_lookup_invariants(table.as_ref())?;
        self.tables.push((table, metadata, identity));
        Ok(self)
    }

    /// Adds multiple tables with their metadata to the builder.
    ///
    /// # Errors
    ///
    /// Returns an error as soon as one of the tables would introduce semantic
    /// lookup ambiguity.
    pub fn add_tables(
        self,
        tables: impl IntoIterator<Item = (Arc<P::Table>, Meta<P::Table>)>,
    ) -> Result<Self, LookupError> {
        tables
            .into_iter()
            .try_fold(self, |builder, (table, metadata)| builder.add_table(table, metadata))
    }

    /// Adds a column with its metadata to the builder, under an identity no
    /// object of this builder has held.
    #[must_use]
    #[inline]
    pub fn add_column(mut self, column: Arc<P::Column>, metadata: Meta<P::Column>) -> Self {
        let identity = self.draw_column_identity();
        self.add_column_with_identity(column, metadata, identity)
    }

    /// Adds a column under the identity it already holds, for a statement
    /// that replaces its table's node.
    #[must_use]
    pub(crate) fn add_column_with_identity(
        mut self,
        column: Arc<P::Column>,
        metadata: Meta<P::Column>,
        identity: ColumnIdentity,
    ) -> Self {
        self.columns.push((column, metadata, identity));
        self
    }

    /// Adds multiple columns with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_columns(
        self,
        columns: impl IntoIterator<Item = (Arc<P::Column>, Meta<P::Column>)>,
    ) -> Self {
        columns
            .into_iter()
            .fold(self, |builder, (column, metadata)| builder.add_column(column, metadata))
    }

    /// Adds an index with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_index(mut self, index: Arc<P::Index>, metadata: Meta<P::Index>) -> Self {
        self.indices.push((index, metadata));
        self
    }

    /// Adds multiple indices with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_indices(
        mut self,
        indices: impl IntoIterator<Item = (Arc<P::Index>, Meta<P::Index>)>,
    ) -> Self {
        self.indices.extend(indices);
        self
    }

    /// Adds a unique index with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_unique_index(
        mut self,
        index: Arc<P::UniqueIndex>,
        metadata: Meta<P::UniqueIndex>,
    ) -> Self {
        self.unique_indices.push((index, metadata));
        self
    }

    /// Adds multiple unique indices with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_unique_indices(
        mut self,
        indices: impl IntoIterator<Item = (Arc<P::UniqueIndex>, Meta<P::UniqueIndex>)>,
    ) -> Self {
        self.unique_indices.extend(indices);
        self
    }

    /// Adds a foreign key with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_foreign_key(
        mut self,
        key: Arc<P::ForeignKey>,
        metadata: Meta<P::ForeignKey>,
    ) -> Self {
        self.foreign_keys.push((key, metadata));
        self
    }

    /// Adds multiple foreign keys with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_foreign_keys(
        mut self,
        keys: impl IntoIterator<Item = (Arc<P::ForeignKey>, Meta<P::ForeignKey>)>,
    ) -> Self {
        self.foreign_keys.extend(keys);
        self
    }

    /// Adds a function with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_function(mut self, function: Arc<P::Function>, metadata: Meta<P::Function>) -> Self {
        self.functions.push((function, metadata));
        self
    }

    /// Adds a trigger with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_trigger(mut self, trigger: Arc<P::Trigger>, metadata: Meta<P::Trigger>) -> Self {
        self.triggers.push((trigger, metadata));
        self
    }

    /// Adds a rewrite rule with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_rule(mut self, rule: Arc<P::Rule>, metadata: Meta<P::Rule>) -> Self {
        self.rules.push((rule, metadata));
        self
    }

    /// Adds an extension with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_extension(
        mut self,
        extension: Arc<P::Extension>,
        metadata: Meta<P::Extension>,
    ) -> Self {
        self.extensions.push((extension, metadata));
        self
    }

    /// Adds a policy with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_policy(mut self, policy: Arc<P::Policy>, metadata: Meta<P::Policy>) -> Self {
        self.policies.push((policy, metadata));
        self
    }

    /// Adds multiple policies with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_policies(
        mut self,
        policies: impl IntoIterator<Item = (Arc<P::Policy>, Meta<P::Policy>)>,
    ) -> Self {
        self.policies.extend(policies);
        self
    }

    /// Adds multiple functions with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_functions(
        mut self,
        functions: impl IntoIterator<Item = (Arc<P::Function>, Meta<P::Function>)>,
    ) -> Self {
        self.functions.extend(functions);
        self
    }

    /// Returns a vector of function Arc references.
    #[must_use]
    pub fn function_arc_vec(&self) -> Vec<Arc<P::Function>> {
        self.functions.iter().map(|(func_arc, _)| func_arc.clone()).collect()
    }

    /// Returns a slice of table Arc references with their metadata and
    /// identities.
    #[must_use]
    pub fn tables(&self) -> &[Identified<P::Table, RelationIdentity>] {
        &self.tables
    }

    /// Returns a slice of role Arc references with their metadata.
    #[must_use]
    pub fn roles(&self) -> &[Stored<P::Role>] {
        &self.roles
    }

    /// Adds a check constraint with its metadata to the builder.
    #[must_use]
    pub fn add_check_constraint(
        mut self,
        constraint: Arc<P::CheckConstraint>,
        metadata: Meta<P::CheckConstraint>,
    ) -> Self {
        self.check_constraints.push((constraint, metadata));
        self
    }

    /// Adds a role with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_role(mut self, role: Arc<P::Role>, metadata: Meta<P::Role>) -> Self {
        self.roles.push((role, metadata));
        self
    }

    /// Adds multiple roles with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_roles(
        mut self,
        roles: impl IntoIterator<Item = (Arc<P::Role>, Meta<P::Role>)>,
    ) -> Self {
        self.roles.extend(roles);
        self
    }

    /// Adds a schema with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_schema(mut self, schema: Arc<P::Schema>, metadata: Meta<P::Schema>) -> Self {
        self.schemas.push((schema, metadata));
        self
    }

    /// Adds multiple schemas with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_schemas(
        mut self,
        schemas: impl IntoIterator<Item = (Arc<P::Schema>, Meta<P::Schema>)>,
    ) -> Self {
        self.schemas.extend(schemas);
        self
    }

    /// Adds a table grant with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_table_grant(
        mut self,
        grant: Arc<P::TableGrant>,
        metadata: Meta<P::TableGrant>,
    ) -> Self {
        self.table_grants.push((grant, metadata));
        self
    }

    /// Adds multiple table grants with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_table_grants(
        mut self,
        grants: impl IntoIterator<Item = (Arc<P::TableGrant>, Meta<P::TableGrant>)>,
    ) -> Self {
        self.table_grants.extend(grants);
        self
    }

    /// Adds a column grant with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_column_grant(
        mut self,
        grant: Arc<P::ColumnGrant>,
        metadata: Meta<P::ColumnGrant>,
    ) -> Self {
        self.column_grants.push((grant, metadata));
        self
    }

    /// Adds multiple column grants with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_column_grants(
        mut self,
        grants: impl IntoIterator<Item = (Arc<P::ColumnGrant>, Meta<P::ColumnGrant>)>,
    ) -> Self {
        self.column_grants.extend(grants);
        self
    }

    /// Adds a plain view with its metadata to the builder, under an identity
    /// no object of this builder has held.
    #[must_use]
    #[inline]
    pub fn add_view(mut self, view: Arc<P::View>, metadata: Meta<P::View>) -> Self {
        let identity = self.draw_relation_identity();
        self.add_view_with_identity(view, metadata, identity)
    }

    /// Adds a plain view under the identity it already holds, for a statement
    /// that replaces the view.
    #[must_use]
    pub(crate) fn add_view_with_identity(
        mut self,
        view: Arc<P::View>,
        metadata: Meta<P::View>,
        identity: RelationIdentity,
    ) -> Self {
        self.views.push((view, metadata, identity));
        self
    }

    /// Adds multiple plain views with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_views(self, views: impl IntoIterator<Item = (Arc<P::View>, Meta<P::View>)>) -> Self {
        views.into_iter().fold(self, |builder, (view, metadata)| builder.add_view(view, metadata))
    }

    /// Adds a materialized view with its metadata to the builder, under an
    /// identity no object of this builder has held.
    #[must_use]
    #[inline]
    pub fn add_materialized_view(
        mut self,
        view: Arc<P::MaterializedView>,
        metadata: Meta<P::MaterializedView>,
    ) -> Self {
        let identity = self.draw_relation_identity();
        self.materialized_views.push((view, metadata, identity));
        self
    }

    /// Adds multiple materialized views with their metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_materialized_views(
        self,
        views: impl IntoIterator<Item = (Arc<P::MaterializedView>, Meta<P::MaterializedView>)>,
    ) -> Self {
        views
            .into_iter()
            .fold(self, |builder, (view, metadata)| builder.add_materialized_view(view, metadata))
    }

    /// Adds a sequence with its metadata to the builder.
    #[must_use]
    #[inline]
    pub fn add_sequence(mut self, sequence: Arc<P::Sequence>, metadata: Meta<P::Sequence>) -> Self {
        self.sequences.push((sequence, metadata));
        self
    }

    /// Reopens a built database for further ingestion, handing its
    /// continuation state back beside the builder.
    ///
    /// The relation and identity indexes are dropped and rebuilt when the
    /// builder finishes again, while the identity counter carries on.
    pub(crate) fn from_database(database: GenericDB<P>) -> (Self, P::Ingestion) {
        let GenericDB {
            dialect,
            catalog_name,
            timezone,
            tables,
            views,
            materialized_views,
            sequences,
            relation_index: _,
            folded_relation_index: _,
            columns,
            relation_identities: _,
            column_identities: _,
            identities,
            indices,
            unique_indices,
            foreign_keys,
            functions,
            function_index: _,
            folded_function_index: _,
            triggers,
            rules,
            extensions,
            policies,
            check_constraints,
            roles,
            role_memberships,
            table_grants,
            column_grants,
            schemas,
            search_path,
            identifier_case,
            ingestion,
        } = database;
        let builder = Self {
            dialect,
            catalog_name,
            timezone,
            tables,
            views,
            materialized_views,
            sequences,
            dropped_sequences: Vec::new(),
            columns,
            indices,
            unique_indices,
            foreign_keys,
            functions,
            triggers,
            rules,
            extensions,
            policies,
            check_constraints,
            roles,
            role_memberships,
            schemas,
            table_grants,
            column_grants,
            search_path,
            identifier_case,
            identities,
        };
        (builder, ingestion)
    }

    /// Sorts every store into the order lookups on the built database rely
    /// on.
    fn sort_stores(&mut self) {
        self.tables.sort_unstable_by_key(|(table, _, _)| {
            (
                table.table_schema().map(alloc::string::ToString::to_string),
                table.table_name().to_string(),
            )
        });

        self.columns.sort_unstable_by(|(a, _, _), (b, _, _)| a.as_ref().cmp(b.as_ref()));
        self.indices.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        self.unique_indices.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        self.foreign_keys.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        // Same-named declarations differ only in their arguments, so the node
        // itself breaks the tie and a lookup answers the same one every time.
        self.functions.sort_unstable_by(|(a, _), (b, _)| {
            a.name().cmp(b.name()).then_with(|| a.as_ref().cmp(b.as_ref()))
        });
        self.triggers.sort_unstable_by(|(a, _), (b, _)| a.name().cmp(b.name()));
        // A rule name is unique per relation, so the relation breaks the tie.
        self.rules.sort_unstable_by(|(a, _), (b, _)| {
            a.name()
                .cmp(b.name())
                .then_with(|| a.target_relation_name().cmp(&b.target_relation_name()))
        });
        // An extension name is unique in the database.
        self.extensions.sort_unstable_by(|(a, _), (b, _)| a.stored_name().cmp(&b.stored_name()));
        self.policies.sort_unstable_by(|(a, _), (b, _)| a.name().cmp(b.name()));
        self.check_constraints.sort_unstable_by(|(a, _), (b, _)| a.as_ref().cmp(b.as_ref()));
        self.roles
            .sort_unstable_by(|(left, _), (right, _)| left.stored_name().cmp(&right.stored_name()));
        self.schemas.sort_unstable_by(|(a, _), (b, _)| a.name().cmp(b.name()));
        self.views.sort_unstable_by(|(a, _, _), (b, _, _)| {
            stored_view_key(a.as_ref(), IdentifierCase::AsWritten)
                .cmp(&stored_view_key(b.as_ref(), IdentifierCase::AsWritten))
        });
        self.materialized_views.sort_unstable_by(|(a, _, _), (b, _, _)| {
            stored_view_key(a.as_ref(), IdentifierCase::AsWritten)
                .cmp(&stored_view_key(b.as_ref(), IdentifierCase::AsWritten))
        });
        self.sequences.sort_unstable_by(|(a, _), (b, _)| {
            stored_sequence_key(a.as_ref(), IdentifierCase::AsWritten)
                .cmp(&stored_sequence_key(b.as_ref(), IdentifierCase::AsWritten))
        });
        // Grants are not sorted as their order may be significant
    }

    /// Sorts, indexes, and seals the built objects into a database carrying
    /// the given ingestion state.
    pub(crate) fn into_database(mut self, ingestion: P::Ingestion) -> GenericDB<P> {
        self.sort_stores();
        let catalog_name = self.catalog_name;

        // Tables, views, materialized views and sequences share one pool of
        // names, so one index answers all four. Slots of a kind stay ascending
        // because each kind is walked in storage order.
        let mut relation_index: BTreeMap<_, Vec<RelationSlot>> = BTreeMap::new();
        let mut relation_identities = Vec::with_capacity(
            self.tables.len() + self.views.len() + self.materialized_views.len(),
        );
        for (position, (table, _, identity)) in self.tables.iter().enumerate() {
            relation_index
                .entry(stored_table_key(table.as_ref(), IdentifierCase::AsWritten))
                .or_default()
                .push(RelationSlot::Table(position));
            relation_identities.push((*identity, RelationSlot::Table(position)));
        }
        for (position, (view, _, identity)) in self.views.iter().enumerate() {
            relation_index
                .entry(stored_view_key(view.as_ref(), IdentifierCase::AsWritten))
                .or_default()
                .push(RelationSlot::View(position));
            relation_identities.push((*identity, RelationSlot::View(position)));
        }
        for (position, (view, _, identity)) in self.materialized_views.iter().enumerate() {
            relation_index
                .entry(stored_view_key(view.as_ref(), IdentifierCase::AsWritten))
                .or_default()
                .push(RelationSlot::MaterializedView(position));
            relation_identities.push((*identity, RelationSlot::MaterializedView(position)));
        }
        for (position, (sequence, _)) in self.sequences.iter().enumerate() {
            relation_index
                .entry(stored_sequence_key(sequence.as_ref(), IdentifierCase::AsWritten))
                .or_default()
                .push(RelationSlot::Sequence(position));
        }
        relation_identities.sort_unstable_by_key(|(identity, _)| *identity);
        let mut column_identities: Vec<_> = self
            .columns
            .iter()
            .enumerate()
            .map(|(position, (_, _, identity))| (*identity, position))
            .collect();
        column_identities.sort_unstable_by_key(|(identity, _)| *identity);

        // Functions keep their own pool of names, and a name carrying several
        // argument lists holds several slots, ascending in storage order.
        let mut function_index: BTreeMap<_, Vec<usize>> = BTreeMap::new();
        for (position, (function, _)) in self.functions.iter().enumerate() {
            function_index
                .entry(stored_function_key(function.as_ref(), IdentifierCase::AsWritten))
                .or_default()
                .push(position);
        }

        let folded_relation_index = folded_key_index(relation_index.keys());
        let folded_function_index = folded_key_index(function_index.keys());

        GenericDB {
            dialect: self.dialect,
            catalog_name,
            timezone: self.timezone,
            tables: self.tables,
            views: self.views,
            materialized_views: self.materialized_views,
            sequences: self.sequences,
            relation_index,
            folded_relation_index,
            columns: self.columns,
            relation_identities,
            column_identities,
            identities: self.identities,
            indices: self.indices,
            unique_indices: self.unique_indices,
            foreign_keys: self.foreign_keys,
            functions: self.functions,
            function_index,
            folded_function_index,
            triggers: self.triggers,
            rules: self.rules,
            extensions: self.extensions,
            policies: self.policies,
            check_constraints: self.check_constraints,
            roles: self.roles,
            role_memberships: self.role_memberships,
            schemas: self.schemas,
            table_grants: self.table_grants,
            column_grants: self.column_grants,
            search_path: self.search_path,
            identifier_case: self.identifier_case,
            ingestion,
        }
    }
}

/// Indexes the stored keys a folded lookup reaches, skipping every key that
/// already equals its folded form.
fn folded_key_index<'a>(
    keys: impl Iterator<Item = &'a RelationKey>,
) -> BTreeMap<RelationKey, Vec<RelationKey>> {
    let mut folded: BTreeMap<RelationKey, Vec<RelationKey>> = BTreeMap::new();
    for key in keys {
        let folded_key = key.folded();
        if folded_key != *key {
            folded.entry(folded_key).or_default().push(key.clone());
        }
    }
    folded
}

impl<P: SchemaProfile> From<GenericDBBuilder<P>> for GenericDB<P> {
    /// Seals the builder under its dialect's default ingestion state, the
    /// state a database built without [`ParseOptions`] resumes from.
    ///
    /// [`ParseOptions`]: crate::structs::ParseOptions
    fn from(builder: GenericDBBuilder<P>) -> Self {
        let ingestion = P::default_ingestion(&builder.dialect);
        builder.into_database(ingestion)
    }
}

#[cfg(test)]
mod tests {

    use crate::{
        impls::SqlparserDialect,
        structs::{ParserDBBuilder, SqlparserProfile},
    };

    #[test]
    fn empty_bulk_additions_preserve_every_collection() {
        let builder = ParserDBBuilder::new("catalog".to_string(), SqlparserDialect::Generic)
            .add_tables(core::iter::empty())
            .expect("empty tables are accepted")
            .add_columns(core::iter::empty())
            .add_indices(core::iter::empty())
            .add_unique_indices(core::iter::empty())
            .add_foreign_keys(core::iter::empty())
            .add_policies(core::iter::empty())
            .add_functions(core::iter::empty())
            .add_roles(core::iter::empty())
            .add_schemas(core::iter::empty())
            .add_table_grants(core::iter::empty())
            .add_column_grants(core::iter::empty());

        let _: super::GenericDB<SqlparserProfile> = builder.into();
    }
}
