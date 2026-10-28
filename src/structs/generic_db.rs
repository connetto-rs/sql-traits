//! Generic database schema representations and utilities.

mod builder;
mod database;
mod profile;
mod sqlparser;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt::Debug;

pub use builder::GenericDBBuilder;
pub(crate) use profile::{Identified, Stored};
pub use profile::{Meta, SchemaProfile};
pub use sqlparser::{
    AccessResolution, ParseOptions, ParserDB, ParserDBBuilder, ParserDBIngestor, ParserIngestion,
    PostgresCatalog, PostgresCatalogCollation, PostgresCatalogType, SqlparserProfile,
    UnresolvedAccessReference,
};

use crate::{
    structs::{ColumnIdentity, IdentifierCase, IdentityCounter, RelationIdentity},
    traits::{PolicyLike, RoleLike, SchemaLike, TableLike, TriggerLike},
    utils::{
        identifier_resolution::stored_identifier_matches_lookup,
        object_name::{RelationKey, stored_view_key},
    },
};

/// Where the relation holding a shared name lives: which kind it is, and its
/// position in that kind's collection.
///
/// PostgreSQL keeps tables, views and materialized views in one pool of
/// names, so one index answers all three and a lookup for a kind keeps only
/// the slots of that kind. Crate-private: it names positions inside private
/// collections, which no caller outside can hold or use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RelationSlot {
    /// A table, at this position in the table collection.
    Table(usize),
    /// A plain view, at this position in the view collection.
    View(usize),
    /// A materialized view, at this position in its collection.
    MaterializedView(usize),
}

/// A generic representation of a database schema.
pub struct GenericDB<P: SchemaProfile> {
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
    /// Relations by normalized `(schema, name)`, values their storage slots.
    ///
    /// PostgreSQL keeps tables, views and materialized views in one pool of
    /// names, so all three share this index and a creation collides with
    /// whichever kind already holds the name. Lists hold every relation a key
    /// matched, ascending within a kind, so lookup answers and ambiguity
    /// reporting agree with scanning the list.
    relation_index: BTreeMap<RelationKey, Vec<RelationSlot>>,
    /// Folded key to the stored keys reaching it, holding only the keys that
    /// fold to something other than themselves, so an all-lowercase catalog
    /// stores nothing here.
    folded_relation_index: BTreeMap<RelationKey, Vec<RelationKey>>,
    /// List of columns in the database.
    columns: Vec<Identified<P::Column, ColumnIdentity>>,
    /// Every relation identity held, ascending, with the slot holding it.
    relation_identities: Vec<(RelationIdentity, RelationSlot)>,
    /// Every column identity held, ascending, with the column's position.
    column_identities: Vec<(ColumnIdentity, usize)>,
    /// Where the next identity is drawn from, carried so a reopened
    /// ingestion never hands out one this database has already used.
    identities: IdentityCounter,
    /// List of indices in the database.
    indices: Vec<Stored<P::Index>>,
    /// List of unique indices in the database.
    unique_indices: Vec<Stored<P::UniqueIndex>>,
    /// List of foreign keys in the database.
    foreign_keys: Vec<Stored<P::ForeignKey>>,
    /// List of functions created in the database.
    functions: Vec<Stored<P::Function>>,
    /// Functions by normalized `(schema, name)`, values their storage slots.
    ///
    /// Functions have their own pool of names, apart from relations, and a
    /// name carrying several argument lists holds several functions, so the
    /// slots of one key stay ascending and lookup answers agree with scanning
    /// the list.
    function_index: BTreeMap<RelationKey, Vec<usize>>,
    /// Folded key to the stored function keys reaching it, on the terms of
    /// [`Self::folded_relation_index`].
    folded_function_index: BTreeMap<RelationKey, Vec<RelationKey>>,
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
    /// List of table grants in the database.
    table_grants: Vec<Stored<P::TableGrant>>,
    /// List of column grants in the database.
    column_grants: Vec<Stored<P::ColumnGrant>>,
    /// List of schemas in the database.
    schemas: Vec<Stored<P::Schema>>,
    /// Schemas an unqualified name resolves against, in order.
    search_path: Vec<(String, bool)>,
    /// Comparison the creations in this catalog were taken under, which each
    /// engine answers its own way. A lookup takes its own comparison.
    identifier_case: IdentifierCase,
    /// Continuation state a resumed ingestion needs beyond the objects.
    ingestion: P::Ingestion,
}

impl<P: SchemaProfile> Debug for GenericDB<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GenericDB")
            .field("dialect", &self.dialect)
            .field("catalog_name", &self.catalog_name)
            .field("timezone", &self.timezone)
            .field("tables", &self.tables.len())
            .field("views", &self.views.len())
            .field("materialized_views", &self.materialized_views.len())
            .field("columns", &self.columns.len())
            .field("relation_identities", &self.relation_identities.len())
            .field("column_identities", &self.column_identities.len())
            .field("indices", &self.indices.len())
            .field("unique_indices", &self.unique_indices.len())
            .field("foreign_keys", &self.foreign_keys.len())
            .field("functions", &self.functions.len())
            .field("triggers", &self.triggers.len())
            .field("rules", &self.rules.len())
            .field("extensions", &self.extensions.len())
            .field("policies", &self.policies.len())
            .field("check_constraints", &self.check_constraints.len())
            .field("roles", &self.roles.len())
            .field("role_memberships", &self.role_memberships.len())
            .field("table_grants", &self.table_grants.len())
            .field("column_grants", &self.column_grants.len())
            .field("schemas", &self.schemas.len())
            .field("search_path", &self.search_path)
            .field("identifier_case", &self.identifier_case)
            .field("ingestion", &self.ingestion)
            .field("relation_index", &self.relation_index.len())
            .field("function_index", &self.function_index.len())
            .field("folded_relation_index", &self.folded_relation_index.len())
            .field("folded_function_index", &self.folded_function_index.len())
            .field("identities", &self.identities)
            .finish()
    }
}

impl<P: SchemaProfile> Clone for GenericDB<P> {
    fn clone(&self) -> Self {
        Self {
            dialect: self.dialect.clone(),
            catalog_name: self.catalog_name.clone(),
            timezone: self.timezone.clone(),
            tables: self.tables.clone(),
            views: self.views.clone(),
            materialized_views: self.materialized_views.clone(),
            columns: self.columns.clone(),
            relation_identities: self.relation_identities.clone(),
            column_identities: self.column_identities.clone(),
            identities: self.identities,
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
            table_grants: self.table_grants.clone(),
            column_grants: self.column_grants.clone(),
            schemas: self.schemas.clone(),
            search_path: self.search_path.clone(),
            identifier_case: self.identifier_case,
            ingestion: self.ingestion.clone(),
            relation_index: self.relation_index.clone(),
            function_index: self.function_index.clone(),
            folded_relation_index: self.folded_relation_index.clone(),
            folded_function_index: self.folded_function_index.clone(),
        }
    }
}

impl<P: SchemaProfile> GenericDB<P> {
    /// Creates a new `GenericDBBuilder` instance.
    #[must_use]
    #[allow(clippy::type_complexity)]
    pub fn new(catalog_name: String, dialect: P::Dialect) -> GenericDBBuilder<P> {
        GenericDBBuilder::new(catalog_name, dialect)
    }

    /// The comparison the creations in this catalog were taken under.
    pub(crate) const fn identifier_case(&self) -> IdentifierCase {
        self.identifier_case
    }

    /// Returns a reference to the metadata of the specified table, if it exists
    /// in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     -- This is a test table
    ///     CREATE TABLE test_table (id INT);
    ///     ",
    /// )?;
    /// let table = db
    ///     .table_by_target(TargetName::new("test_table", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let metadata = db.table_metadata(table).unwrap();
    /// assert_eq!(metadata.documentation(), Some("This is a test table"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn table_metadata(&self, table: &P::Table) -> Option<&Meta<P::Table>> {
        self.tables
            .binary_search_by_key(
                &(
                    table.table_schema().map(alloc::string::ToString::to_string),
                    table.table_name().to_string(),
                ),
                |(t, _, _)| {
                    (
                        t.table_schema().map(alloc::string::ToString::to_string),
                        t.table_name().to_string(),
                    )
                },
            )
            .ok()
            .map(|index| &self.tables[index].1)
    }

    /// Returns a reference to the metadata of the specified view, if it exists
    /// in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;",
    /// )?;
    /// let view = db
    ///     .view_by_target(TargetName::new("v", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// assert!(db.view_metadata(view).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn view_metadata(&self, view: &P::View) -> Option<&Meta<P::View>> {
        self.views
            .binary_search_by(|(candidate, _, _)| {
                stored_view_key(candidate.as_ref(), IdentifierCase::AsWritten)
                    .cmp(&stored_view_key(view, IdentifierCase::AsWritten))
            })
            .ok()
            .map(|index| &self.views[index].1)
    }

    /// Returns a reference to the metadata of the specified materialized view,
    /// if it exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
    /// )?;
    /// let view = db
    ///     .materialized_view_by_target(TargetName::new("m", false), IdentifierCase::AsWritten)?
    ///     .expect("the view is recorded");
    /// assert!(db.materialized_view_metadata(view).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn materialized_view_metadata(
        &self,
        view: &P::MaterializedView,
    ) -> Option<&Meta<P::MaterializedView>> {
        self.materialized_views
            .binary_search_by(|(candidate, _, _)| {
                stored_view_key(candidate.as_ref(), IdentifierCase::AsWritten)
                    .cmp(&stored_view_key(view, IdentifierCase::AsWritten))
            })
            .ok()
            .map(|index| &self.materialized_views[index].1)
    }

    /// Returns a reference to the metadata of the specified column, if it
    /// exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let column = table.column("id", &db, IdentifierCase::AsWritten)?.unwrap();
    /// let metadata = db.column_metadata(column).unwrap();
    /// assert_eq!(metadata.postgres_deterministic(), None);
    /// # Ok(())
    /// # }
    /// ```
    pub fn column_metadata(&self, column: &P::Column) -> Option<&Meta<P::Column>> {
        self.columns
            .binary_search_by(|(c, _, _)| c.as_ref().cmp(column))
            .ok()
            .map(|index| &self.columns[index].1)
    }

    /// Returns a reference to the metadata of the specified unique index, if it
    /// exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT UNIQUE);")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.unique_indices(&db)?.next().unwrap();
    /// // The metadata for unique indices in ParserDB is currently unit ()
    /// // (actually it might be struct depending on impl, let's just check existence)
    /// assert!(db.unique_index_metadata(index).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn unique_index_metadata(&self, index: &P::UniqueIndex) -> Option<&Meta<P::UniqueIndex>> {
        self.unique_indices
            .binary_search_by(|(i, _)| i.as_ref().cmp(index))
            .ok()
            .map(|index| &self.unique_indices[index].1)
    }

    /// Returns a reference to the metadata of the specified check constraint,
    /// if it exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT CHECK (id > 0));")?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let check = table.check_constraints(&db)?.next().unwrap();
    /// assert!(db.check_constraint_metadata(check).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn check_constraint_metadata(
        &self,
        constraint: &P::CheckConstraint,
    ) -> Option<&Meta<P::CheckConstraint>> {
        self.check_constraints
            .binary_search_by(|(c, _)| c.as_ref().cmp(constraint))
            .ok()
            .map(|index| &self.check_constraints[index].1)
    }

    /// Returns a reference to the metadata of the specified foreign key, if it
    /// exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE TABLE parent (id INT PRIMARY KEY);
    ///     CREATE TABLE child (id INT PRIMARY KEY, parent_id INT REFERENCES parent(id));
    ///     ",
    /// )?;
    /// let child =
    ///     db.table_by_target(TargetName::new("child", false), IdentifierCase::AsWritten)?.unwrap();
    /// let fk = child.foreign_keys(&db)?.next().unwrap();
    /// assert!(db.foreign_key_metadata(fk).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn foreign_key_metadata(&self, key: &P::ForeignKey) -> Option<&Meta<P::ForeignKey>> {
        self.foreign_keys
            .binary_search_by(|(k, _)| k.as_ref().cmp(key))
            .ok()
            .map(|index| &self.foreign_keys[index].1)
    }

    /// Returns a reference to the metadata of the specified index, if it exists
    /// in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE INDEX my_idx ON t(id);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.indices(&db)?.next().expect("index should exist");
    /// assert!(db.index_metadata(index).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn index_metadata(&self, index: &P::Index) -> Option<&Meta<P::Index>> {
        self.indices
            .binary_search_by(|(i, _)| i.as_ref().cmp(index))
            .ok()
            .map(|index| &self.indices[index].1)
    }

    /// Returns a reference to the metadata of the specified function, if it
    /// exists in the database.
    ///
    /// # Arguments
    ///
    /// * `function` - The function to retrieve metadata for.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db =
    ///     ParserDB::parse::<GenericDialect>("CREATE FUNCTION my_func() RETURNS INT AS 'SELECT 1';")?;
    /// let func = db
    ///     .function_by_target(TargetName::new("my_func", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// assert!(db.function_metadata(func).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn function_metadata(&self, function: &P::Function) -> Option<&Meta<P::Function>> {
        self.functions
            .iter()
            .find_map(|(candidate, metadata)| (candidate.as_ref() == function).then_some(metadata))
    }

    /// Returns a reference of the trigger by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the trigger to retrieve.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE TABLE t (id INT);
    ///     CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN END' LANGUAGE plpgsql;
    ///     CREATE TRIGGER my_trigger AFTER INSERT ON t FOR EACH ROW EXECUTE PROCEDURE f();
    ///     ",
    /// )?;
    /// let trigger = db.trigger("my_trigger").unwrap();
    /// assert_eq!(trigger.name(), "my_trigger");
    /// assert!(db.trigger("non_existent").is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn trigger(&self, name: &str) -> Option<&P::Trigger> {
        self.triggers
            .binary_search_by(|(t, _)| t.name().cmp(name))
            .ok()
            .map(|index| self.triggers[index].0.as_ref())
    }

    /// Returns a reference to the metadata of the specified trigger, if it
    /// exists in the database.
    ///
    /// # Arguments
    ///
    /// * `trigger` - The trigger to retrieve metadata for.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE TABLE t (id INT);
    ///     CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN END' LANGUAGE plpgsql;
    ///     CREATE TRIGGER my_trigger AFTER INSERT ON t FOR EACH ROW EXECUTE PROCEDURE f();
    ///     ",
    /// )?;
    /// let trigger = db.trigger("my_trigger").unwrap();
    /// assert!(db.trigger_metadata(trigger).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn trigger_metadata(&self, trigger: &P::Trigger) -> Option<&Meta<P::Trigger>> {
        self.triggers
            .binary_search_by(|(t, _)| t.name().cmp(trigger.name()))
            .ok()
            .map(|index| &self.triggers[index].1)
    }

    /// Returns a reference to the metadata of the specified policy, if it
    /// exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE TABLE t (id INT);
    ///     CREATE POLICY my_policy ON t USING (id > 0);
    ///     ",
    /// )?;
    /// let policy = db.policies().next().expect("policy should exist");
    /// assert!(db.policy_metadata(policy).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn policy_metadata(&self, policy: &P::Policy) -> Option<&Meta<P::Policy>> {
        self.policies
            .binary_search_by(|(p, _)| p.name().cmp(policy.name()))
            .ok()
            .map(|index| &self.policies[index].1)
    }

    /// Returns a reference to the role with the canonical stored name.
    ///
    /// # Arguments
    ///
    /// * `name` - The canonical stored name of the role to retrieve.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE ROLE admin SUPERUSER;")?;
    /// let role = db.role("admin").unwrap();
    /// assert_eq!(role.name(), "admin");
    /// assert!(role.is_superuser());
    /// assert!(db.role("non_existent").is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn role(&self, name: &str) -> Option<&P::Role> {
        self.roles
            .binary_search_by(|(role, _)| role.stored_name().as_ref().cmp(name))
            .ok()
            .map(|index| self.roles[index].0.as_ref())
    }

    /// Returns a reference to the metadata of the specified role, if it
    /// exists in the database.
    ///
    /// # Arguments
    ///
    /// * `role` - The role to retrieve metadata for.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE ROLE admin;")?;
    /// let role = db.role("admin").unwrap();
    /// assert!(db.role_metadata(role).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn role_metadata(&self, role: &P::Role) -> Option<&Meta<P::Role>> {
        let stored_name = role.stored_name();
        self.roles
            .binary_search_by(|(candidate, _)| candidate.stored_name().cmp(&stored_name))
            .ok()
            .map(|index| &self.roles[index].1)
    }

    /// Returns a reference to the catalog name.
    ///
    /// The name is the one the schema was started with, and no statement in
    /// the input changes it.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDBIngestor::new::<GenericDialect>("analytics".to_string()).finish();
    /// assert_eq!(db.catalog_name(), "analytics");
    /// ```
    #[must_use]
    #[inline]
    pub fn catalog_name(&self) -> &str {
        &self.catalog_name
    }

    /// Iterates over the table and metadata
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// -- table b
    /// CREATE TABLE b (id INT);
    /// -- table a
    /// CREATE TABLE a (id INT);",
    /// )?;
    ///
    /// let mut parsed: Vec<(&str, Option<&str>)> =
    ///     db.tables_metadata().map(|(t, meta)| (t.table_name(), meta.documentation())).collect();
    ///
    /// parsed.sort_by(|(a, _), (b, _)| a.cmp(b));
    /// assert_eq!(parsed, vec![("a", Some("table a")), ("b", Some("table b"))]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn tables_metadata(&self) -> impl Iterator<Item = (&P::Table, &Meta<P::Table>)> {
        self.tables.iter().map(|(t, m, _)| (t.as_ref(), m))
    }

    /// Iterates mutably over the table and metadata
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// let mut db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     -- original doc a
    ///     CREATE TABLE a (id INT);
    ///     -- original doc b
    ///     CREATE TABLE b (id INT);
    ///     ",
    /// )?;
    /// let metadata = db.tables_metadata_mut().collect::<Vec<_>>();
    /// assert_eq!(metadata.len(), db.number_of_tables());
    /// # Ok(())
    /// # }
    /// ```
    pub fn tables_metadata_mut(
        &mut self,
    ) -> impl Iterator<Item = (&P::Table, &mut Meta<P::Table>)> {
        self.tables.iter_mut().map(|(t, m, _)| ((*t).as_ref(), m))
    }

    /// Returns a reference to the metadata of the specified table grant, if it
    /// exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE ROLE admin;
    ///     CREATE TABLE users (id INT);
    ///     GRANT SELECT ON users TO admin;
    ///     ",
    /// )?;
    /// let grant = db.table_grants().next().unwrap();
    /// assert!(db.table_grant_metadata(grant).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn table_grant_metadata(&self, grant: &P::TableGrant) -> Option<&Meta<P::TableGrant>> {
        self.table_grants.iter().find(|(g, _)| g.as_ref() == grant).map(|(_, m)| m)
    }

    /// Returns a reference to the metadata of the specified column grant, if it
    /// exists in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    ///     CREATE ROLE admin;
    ///     CREATE TABLE users (id INT, name TEXT);
    ///     GRANT SELECT (name) ON users TO admin;
    ///     ",
    /// )?;
    /// let grant = db.column_grants().next().unwrap();
    /// assert!(db.column_grant_metadata(grant).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn column_grant_metadata(&self, grant: &P::ColumnGrant) -> Option<&Meta<P::ColumnGrant>> {
        self.column_grants.iter().find(|(g, _)| g.as_ref() == grant).map(|(_, m)| m)
    }

    /// Returns a reference to the schema by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the schema to retrieve.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(r#"CREATE SCHEMA "Bar";"#)?;
    ///
    /// assert_eq!(db.schema("\"Bar\"").map(SchemaLike::name), Some("Bar"));
    /// assert!(db.schema("bar").is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn schema(&self, name: &str) -> Option<&P::Schema> {
        self.schemas.iter().find_map(|(s, _)| {
            stored_identifier_matches_lookup(s.name(), s.name_is_quoted(), name)
                .then_some(s.as_ref())
        })
    }

    /// Returns a reference to the metadata of the specified schema, if it
    /// exists in the database.
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema to retrieve metadata for.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE SCHEMA my_schema;")?;
    /// let schema = db.schema("my_schema").expect("schema should exist");
    /// assert!(db.schema_metadata(schema).is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn schema_metadata(&self, schema: &P::Schema) -> Option<&Meta<P::Schema>> {
        self.schemas
            .binary_search_by(|(s, _)| s.name().cmp(schema.name()))
            .ok()
            .map(|index| &self.schemas[index].1)
    }

    /// Iterates over the schemas and their metadata.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE SCHEMA a; CREATE SCHEMA b;")?;
    /// let names: Vec<&str> = db.schemas().map(|(s, _)| s.name()).collect();
    /// assert_eq!(names.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn schemas(&self) -> impl Iterator<Item = (&P::Schema, &Meta<P::Schema>)> {
        self.schemas.iter().map(|(s, m)| (s.as_ref(), m))
    }
}
