//! Submodule providing a trait for describing SQL Database-like entities.

use alloc::{borrow::Cow, string::ToString, vec::Vec};
use core::{borrow::Borrow, fmt::Debug, hash::Hash};

use geometric_traits::{
    impls::CSR2D,
    prelude::{GenericEdgesBuilder, Kahn, SquareCSR2D},
    traits::EdgesBuilder,
};
use sqlparser::ast::Function;

use crate::{
    errors::{Error, LookupError},
    structs::{IdentifierCase, TargetName},
    traits::{
        CheckConstraintLike, ColumnGrantLike, ColumnLike, DataTypeLike, DialectLike, ExtensionLike,
        ForeignKeyLike, FunctionLike, IndexLike, PolicyLike, RoleLike, RoleMembershipLike,
        RuleLike, SchemaLike, SequenceLike, TableGrantLike, TableLike, TriggerLike,
        UniqueIndexLike, ViewLike,
    },
    utils::{
        call_typing::{Selection, reached_functions, resolve_call},
        identifier_resolution::stored_identifier_matches_lookup,
        object_name::{
            function_has_stored_identity, function_search_path, relation_name_is_claimed,
            resolve_function_in_iter, resolve_function_on_search_path_in_iter,
            resolve_one_function, resolve_sequence_in_iter,
            resolve_sequence_on_search_path_in_iter, resolve_target_in_iter,
            resolve_target_on_search_path_in_iter, resolve_view_in_iter,
            resolve_view_on_search_path_in_iter,
        },
        postgres_types::PgType,
    },
};

/// A trait for types that can be treated as SQL databases.
pub trait DatabaseLike: Clone + Debug + Send + Sync {
    /// Type of the tables in the schema.
    type Table: TableLike<DB = Self>;
    /// Type of the plain views in the schema.
    type View: ViewLike<DB = Self>;
    /// Type of the materialized views in the schema.
    type MaterializedView: ViewLike<DB = Self>;
    /// Type of the sequences in the schema.
    type Sequence: SequenceLike<DB = Self>;
    /// Type of the columns in the schema.
    type Column: ColumnLike<DB = Self>;
    /// Type of the declared data types of the columns in the schema.
    type DataType: DataTypeLike<DB = Self>;
    /// Type of the indices in the schema.
    type Index: IndexLike<DB = Self>;
    /// Type of the foreign keys in the schema.
    type ForeignKey: ForeignKeyLike<DB = Self>;
    /// Type of the functions in the schema.
    type Function: FunctionLike<DB = Self>;
    /// Type of the triggers in the schema.
    type Trigger: TriggerLike<DB = Self>;
    /// Type of the rewrite rules in the schema.
    type Rule: RuleLike<DB = Self>;
    /// Type of the extensions in the database.
    type Extension: ExtensionLike<DB = Self>;
    /// Type of the unique indexes in the schema.
    type UniqueIndex: UniqueIndexLike<DB = Self>;
    /// Type of the check constraints in the schema.
    type CheckConstraint: CheckConstraintLike<DB = Self>;
    /// Type of the policies in the schema.
    type Policy: PolicyLike<DB = Self>;
    /// Type of the roles in the schema.
    type Role: RoleLike<DB = Self>;
    /// Type of the role memberships in the database.
    type RoleMembership: RoleMembershipLike<DB = Self>;
    /// Type of the table grants in the schema.
    type TableGrant: TableGrantLike<DB = Self>;
    /// Type of the column grants in the schema.
    type ColumnGrant: ColumnGrantLike<DB = Self>;
    /// Type of the schemas in the database.
    type Schema: SchemaLike<DB = Self>;
    /// SQL dialect this database is expressed in.
    ///
    /// The dialect owns per-column type predicates (`is_bool`, `is_uuid`,
    /// …) so dialect-specific quirks like MySQL's `TINYINT(1)` boolean and
    /// SQL Server's `UNIQUEIDENTIFIER` never leak into the generic column
    /// trait. See [`DialectLike`].
    type Dialect: DialectLike<DB = Self>;
    /// Identity of a table, foreign table, view or materialized view, which
    /// the relation keeps for as long as it exists, through its renames and
    /// rebuilds.
    type RelationId: Copy + Eq + Ord + Hash + Debug + Send + Sync;
    /// Identity of a table column, on the terms of [`Self::RelationId`].
    type ColumnId: Copy + Eq + Ord + Hash + Debug + Send + Sync;

    /// Returns the SQL dialect this database is expressed in.
    fn dialect(&self) -> &Self::Dialect;

    /// Returns the name of the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// assert_eq!(db.catalog_name(), "unknown_catalog");
    /// # Ok(())
    /// # }
    /// ```
    fn catalog_name(&self) -> &str;

    /// Returns the number of tables in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t1 (id INT);
    /// CREATE TABLE t2 (id INT);
    /// ",
    /// )?;
    /// assert_eq!(db.number_of_tables(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn number_of_tables(&self) -> usize;

    /// Returns the timezone of the database, if any.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("SET TIME ZONE 'UTC';")?;
    /// assert_eq!(db.timezone(), Some("UTC"));
    ///
    /// let db_no_tz = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// assert_eq!(db_no_tz.timezone(), None);
    /// # Ok(())
    /// # }
    /// ```
    fn timezone(&self) -> Option<&str>;

    /// Iterates over the tables defined in the schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT);
    /// CREATE TABLE table2 (name TEXT);
    /// CREATE TABLE table3 (score DECIMAL);
    /// ",
    /// )?;
    /// let table_names: Vec<&str> = db.tables().map(|t| t.table_name()).collect();
    /// assert_eq!(table_names, vec!["table1", "table2", "table3"]);
    /// # Ok(())
    /// # }
    /// ```
    fn tables(&self) -> impl Iterator<Item = &Self::Table>;

    /// Iterates over the triggers defined in the schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE FUNCTION f() RETURNS TRIGGER AS 'BEGIN END;' LANGUAGE plpgsql;
    /// CREATE TRIGGER my_trigger AFTER INSERT ON t FOR EACH ROW EXECUTE PROCEDURE f();
    /// ",
    /// )?;
    /// let triggers: Vec<&str> = db.triggers().map(|t| t.name()).collect();
    /// assert_eq!(triggers, vec!["my_trigger"]);
    /// # Ok(())
    /// # }
    /// ```
    fn triggers(&self) -> impl Iterator<Item = &Self::Trigger>;

    /// Iterates over the rewrite rules defined in the schema, on tables and
    /// views alike.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE VIEW v AS SELECT id FROM t;
    /// CREATE RULE keep_rows AS ON DELETE TO t DO INSTEAD NOTHING;
    /// CREATE RULE v_insert AS ON INSERT TO v DO INSTEAD INSERT INTO t VALUES (new.id);
    /// ",
    /// )?;
    /// let mut rules: Vec<&str> = db.rules().map(|rule| rule.name()).collect();
    /// rules.sort_unstable();
    /// assert_eq!(rules, vec!["keep_rows", "v_insert"]);
    /// # Ok(())
    /// # }
    /// ```
    fn rules(&self) -> impl Iterator<Item = &Self::Rule>;

    /// Iterates over the extensions installed in the database.
    ///
    /// A PostgreSQL database always holds `plpgsql`, so one parsed under the
    /// PostgreSQL dialect starts with it in `pg_catalog`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE SCHEMA exts;
    /// CREATE EXTENSION pgcrypto WITH SCHEMA exts;
    /// ",
    /// )?;
    /// let mut extensions: Vec<(&str, Option<&str>)> =
    ///     db.extensions().map(|extension| (extension.name(), extension.schema())).collect();
    /// extensions.sort_unstable();
    /// assert_eq!(extensions, vec![("pgcrypto", Some("exts")), ("plpgsql", Some("pg_catalog"))]);
    /// # Ok(())
    /// # }
    /// ```
    fn extensions(&self) -> impl Iterator<Item = &Self::Extension>;

    /// Iterates over the indexes defined in the schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE users (id INT, name TEXT);
    /// CREATE INDEX idx_name ON users (name);
    /// ",
    /// )?;
    /// let index_names: Vec<String> =
    ///     db.indexes().filter_map(|i| i.name().map(ToString::to_string)).collect();
    /// assert_eq!(index_names, vec!["idx_name"]);
    /// # Ok(())
    /// # }
    /// ```
    fn indexes(&self) -> impl Iterator<Item = &Self::Index>;

    /// Returns whether the database has at least one table.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db_with_tables = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT);
    /// ",
    /// )?;
    /// assert!(db_with_tables.has_tables());
    ///
    /// let db_without_tables = ParserDB::parse::<GenericDialect>(
    ///     "
    /// -- No tables defined
    /// ",
    /// )?;
    /// assert!(!db_without_tables.has_tables());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_tables(&self) -> bool {
        self.tables().next().is_some()
    }

    /// Returns an iterator over the root tables in the database,
    /// i.e., tables which are extended by some other table and
    /// do not extend any other table. Tables which are not involved
    /// in any extension relationship are not considered root tables.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::TableNotFound`] or
    /// [`LookupError::ColumnNotFound`] when a foreign key in this database
    /// names a table or a column it does not hold, since the extension
    /// relationships are read from the foreign keys.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE base_table (id INT PRIMARY KEY);
    /// CREATE TABLE extended_table1 (id INT PRIMARY KEY REFERENCES base_table(id));
    /// CREATE TABLE extended_table2 (id INT PRIMARY KEY REFERENCES base_table(id));
    /// CREATE TABLE independent_table (id INT PRIMARY KEY);
    /// ",
    /// )?;
    ///
    /// let root_table_names: Vec<&str> = db.root_tables()?.map(|t| t.table_name()).collect();
    /// assert_eq!(root_table_names, vec!["base_table"]);
    /// # Ok(())
    /// # }
    /// ```
    fn root_tables(&self) -> Result<impl Iterator<Item = &Self::Table>, LookupError> {
        let mut tables = Vec::new();
        for table in self.tables() {
            if !table.is_extension(self)? && table.is_extended(self)? {
                tables.push(table);
            }
        }

        Ok(tables.into_iter())
    }

    /// Returns the maximum number of columns found in any table in the
    /// database.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError`] when the metadata of one of the tables this
    /// database reports cannot be resolved.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT, name TEXT);
    /// CREATE TABLE table2 (score DECIMAL, level INT, active BOOLEAN);
    /// ",
    /// )?;
    /// assert_eq!(db.maximum_number_of_columns()?, 3);
    /// # Ok(())
    /// # }
    /// ```
    fn maximum_number_of_columns(&self) -> Result<usize, LookupError> {
        let mut maximum = 0;
        for table in self.tables() {
            maximum = maximum.max(table.columns(self)?.count());
        }

        Ok(maximum)
    }

    /// Returns tables as a Kahn's ordering based on foreign key dependencies,
    /// ignoring potential self-references which would create cycles.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when a foreign key references a table this database
    /// does not hold, when the dependency graph cannot be assembled, or
    /// when the foreign keys form a cycle.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE users (
    ///    id SERIAL PRIMARY KEY,
    ///   name TEXT NOT NULL
    /// );
    /// CREATE TABLE comments (
    ///   id SERIAL PRIMARY KEY,
    /// name TEXT NOT NULL,
    /// user_id INT REFERENCES users(id)
    /// );
    /// CREATE TABLE extended_comments (
    ///  id INT PRIMARY KEY REFERENCES comments(id),
    /// extra_info TEXT
    /// );
    /// ",
    /// )?;
    /// let user_table =
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let comment_table =
    ///     db.table_by_target(TargetName::new("comments", false), IdentifierCase::AsWritten)?.unwrap();
    /// let extended_comment_table = db
    ///     .table_by_target(TargetName::new("extended_comments", false), IdentifierCase::AsWritten)?
    ///     .unwrap();
    /// let ordered_tables = db.table_dag()?;
    /// assert_eq!(ordered_tables, vec![user_table, comment_table, extended_comment_table]);
    /// # Ok(())
    /// # }
    /// ```
    fn table_dag(&self) -> Result<Vec<&Self::Table>, Error> {
        let tables = self.tables().collect::<Vec<&Self::Table>>();

        let mut edges = Vec::new();
        for (table_number, table) in tables.iter().enumerate() {
            for foreign_key in table.foreign_keys(self)? {
                let foreign_key: &Self::ForeignKey = foreign_key.borrow();
                let referenced_table: &Self::Table = foreign_key.referenced_table(self)?.borrow();
                // We ignore self-references to avoid cycles in the DAG.
                if referenced_table == *table {
                    continue;
                }
                let referenced_table_number =
                    tables.binary_search(&referenced_table).map_err(|_| {
                        Error::ReferencedTableNotFoundForForeignKey {
                            referenced_table: referenced_table.table_name().to_string(),
                            host_table: table.table_name().to_string(),
                        }
                    })?;
                edges.push((referenced_table_number, table_number));
            }
        }

        // There is no guarantee that the foreign keys in a table are ordered,
        // so it is necessary to sort and deduplicate the edges.
        edges.sort_unstable();
        // Furthermore, there is no guarantee that there are no foreign keys
        // referencing the same table, so we deduplicate the edges as well.
        edges.dedup();

        let dag: SquareCSR2D<CSR2D<usize, usize, usize>> = GenericEdgesBuilder::default()
            .expected_shape(tables.len())
            .edges(edges)
            .build()
            .map_err(|error| {
                Error::TableDependencyGraph {
                    catalog_name: self.catalog_name().to_string(),
                    reason: error.to_string(),
                }
            })?;
        let dag_ordering = dag.kahn().map_err(|_| {
            Error::CyclicTableDependencies { catalog_name: self.catalog_name().to_string() }
        })?;

        let mut ordered_tables = tables.clone();
        for (table_index, table) in dag_ordering.into_iter().zip(tables.iter()) {
            ordered_tables[table_index] = table;
        }

        Ok(ordered_tables)
    }

    /// Iterates over the functions created in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE FUNCTION add_one(x INT) RETURNS INT AS 'SELECT x + 1;';
    /// CREATE FUNCTION greet(name TEXT) RETURNS TEXT AS 'SELECT \"Hello, \" || name;';
    /// ",
    /// )?;
    /// let function_names: Vec<&str> = db.functions().map(|f| f.name()).collect();
    ///
    /// // There will be more than two functions because the parser may add
    /// // additional built-in functions automatically. We check that certainly
    /// // our two functions are present.
    /// assert!(function_names.contains(&"add_one"));
    /// assert!(function_names.contains(&"greet"));
    ///
    /// # Ok(())
    /// # }
    /// ```
    fn functions(&self) -> impl Iterator<Item = &Self::Function>;

    /// Returns the table stored under exactly this identity.
    ///
    /// Both parts are read as the catalog holds them, the form
    /// [`TableLike::stored_table_schema`] and [`TableLike::stored_table_name`]
    /// report: nothing folds, no quoting is interpreted, so a quote character
    /// stands for itself and matches only a stored name carrying one, and a
    /// table stored without a schema is not reached by a `public` qualifier.
    /// A caller that normalized a name once asks for that name back this way.
    ///
    /// [`Self::table_by_target`] answers the other question, what a written
    /// reference denotes, so it folds an unquoted name and reads no schema and
    /// `public` as one place.
    ///
    /// The body here scans [`Self::tables`], which an implementation holding
    /// an index of its own overrides, as [`GenericDB`] does.
    ///
    /// [`GenericDB`]: crate::structs::GenericDB
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE \"Docs\" (id INT); CREATE TABLE plain (id INT);",
    /// )?;
    ///
    /// assert!(db.table_by_stored_identity(None, "Docs").is_some());
    /// assert!(db.table_by_stored_identity(None, "docs").is_none());
    /// assert!(db.table_by_stored_identity(Some("public"), "plain").is_none());
    /// assert!(db.table_by_stored_identity(None, "plain").is_some());
    ///
    /// // A written reference folds an unquoted name, and either spelling of
    /// // the default schema reaches a table stored in it.
    /// assert!(
    ///     db.table_by_target(TargetName::new("PLAIN", false), IdentifierCase::AsWritten)?.is_some()
    /// );
    /// assert!(
    ///     db.table_by_target(
    ///         TargetName::new("plain", false).with_schema("public", false),
    ///         IdentifierCase::AsWritten
    ///     )?
    ///     .is_some()
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn table_by_stored_identity(&self, schema: Option<&str>, name: &str) -> Option<&Self::Table> {
        self.tables().find(|table| {
            table.stored_table_schema().as_deref() == schema && table.stored_table_name() == name
        })
    }

    /// Iterates over the plain views defined in the schema.
    ///
    /// Views are listed apart from tables rather than mixed in, because a view
    /// has no columns of its own and, unlike a table, cannot be written to in
    /// general.
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT);
    ///      CREATE VIEW v AS SELECT id FROM t;
    ///      CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
    /// )?;
    /// assert_eq!(db.views().count(), 1);
    /// assert_eq!(db.tables().count(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn views(&self) -> impl Iterator<Item = &Self::View>;

    /// Iterates over the materialized views defined in the schema.
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
    /// )?;
    /// assert_eq!(db.materialized_views().count(), 1);
    /// assert!(db.views().next().is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn materialized_views(&self) -> impl Iterator<Item = &Self::MaterializedView>;

    /// Iterates over the sequences defined in the schema.
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SEQUENCE ticket_seq;
    ///      GRANT USAGE ON SEQUENCE ticket_seq TO PUBLIC;",
    /// )?;
    /// assert_eq!(db.sequences().count(), 1);
    /// assert!(db.tables().next().is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn sequences(&self) -> impl Iterator<Item = &Self::Sequence>;

    /// Iterates over the foreign tables, which [`Self::tables`] leaves out
    /// since their rows live behind a foreign server.
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE local (id INT);
    ///      CREATE FOREIGN TABLE remote (id INT, amount INT) SERVER warehouse;",
    /// )?;
    /// assert_eq!(db.tables().count(), 1);
    /// let remote = db.foreign_tables().next().expect("the foreign table is recorded");
    /// assert_eq!(remote.table_name(), "remote");
    /// assert_eq!(remote.columns(&db)?.count(), 2);
    /// assert!(remote.is_foreign(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    fn foreign_tables(&self) -> impl Iterator<Item = &Self::Table>;

    /// Iterates over the tables and then the foreign tables.
    #[inline]
    fn tables_and_foreign_tables(&self) -> impl Iterator<Item = &Self::Table> {
        self.tables().chain(self.foreign_tables())
    }

    /// Resolves a name a statement wrote into the table it denotes, comparing
    /// identifiers as `case` says the engine does.
    ///
    /// An unqualified name resolves through the first schema holding it, with
    /// the temporary schema `pg_temp` searched before [`Self::search_path`]
    /// unless the path names it, and a table stored without a schema resides
    /// in the default schema `public`. What `case` decides is the
    /// comparison of each part, both the name and any qualifier, and of the
    /// schemas on the search path:
    /// [`IdentifierCase::AsWritten`] lets quoting decide, which is
    /// PostgreSQL's rule, [`IdentifierCase::Folded`] folds both sides, which
    /// is SQLite's, and [`IdentifierCase::Exact`] folds neither, which is
    /// MySQL with `lower_case_table_names = 0`.
    ///
    /// This is how PostgreSQL reads a name a query writes now. Unlike
    /// [`Self::table_by_target`], this walks the search path, and either
    /// reports an ambiguous name rather than picking a winner: two tables
    /// differing only in case are one name under folding.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
    /// than one table. A name matching none is `Ok(None)`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// SET search_path TO app;
    /// CREATE TABLE app.docs (id INT);
    /// CREATE TEMP TABLE notes (id INT);
    /// ",
    /// )?;
    /// let resolve =
    ///     |name| db.resolve_target_table(TargetName::new(name, false), IdentifierCase::AsWritten);
    /// // The search path carries the bare name into `app`.
    /// assert_eq!(resolve("docs")?.unwrap().table_schema(), Some("app"));
    /// // The temporary schema is searched first.
    /// assert!(resolve("notes")?.unwrap().is_temporary());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn resolve_target_table(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Table>, LookupError> {
        resolve_target_on_search_path_in_iter(
            self.tables(),
            &target,
            || self.search_path(),
            case,
            |key| relation_name_is_claimed(self, key, case),
        )
    }

    /// Resolves a name a statement wrote into the plain view it denotes,
    /// applying the same rules as [`Self::resolve_target_table`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
    /// than one view, and [`LookupError::InvalidObjectName`] when the name is
    /// malformed.
    #[inline]
    fn resolve_target_view(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::View>, LookupError> {
        resolve_view_on_search_path_in_iter(
            self.views(),
            &target,
            || self.search_path(),
            case,
            |key| relation_name_is_claimed(self, key, case),
        )
    }

    /// Resolves a name a statement wrote into the materialized view it denotes,
    /// applying the same rules as [`Self::resolve_target_table`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
    /// than one materialized view, and [`LookupError::InvalidObjectName`] when
    /// the name is malformed.
    #[inline]
    fn resolve_target_materialized_view(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::MaterializedView>, LookupError> {
        resolve_view_on_search_path_in_iter(
            self.materialized_views(),
            &target,
            || self.search_path(),
            case,
            |key| relation_name_is_claimed(self, key, case),
        )
    }

    /// Resolves a name a statement wrote into the relation it denotes,
    /// whether table, foreign table, plain view, materialized view or
    /// sequence, applying the same rules as [`Self::resolve_target_table`].
    ///
    /// The five kinds share one pool of names, so at most one kind answers a
    /// name and the kind that holds it is the answer.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one relation, and [`LookupError::InvalidObjectName`] when
    /// the name is malformed.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE SCHEMA app;
    /// SET search_path TO app;
    /// CREATE TABLE app.docs (id INT);
    /// CREATE VIEW app.indexed AS SELECT id FROM docs;
    /// ",
    /// )?;
    /// let resolve = |name| db.resolve_target_relation(TargetName::new(name, false), IdentifierCase::AsWritten);
    /// assert!(matches!(
    ///     resolve("docs")?,
    ///     Some(Relation::Table(table)) if table.table_name() == "docs"
    /// ));
    /// assert!(matches!(
    ///     resolve("indexed")?,
    ///     Some(Relation::View(view)) if view.view_name() == "indexed"
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn resolve_target_relation(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<Relation<'_, Self>>, LookupError> {
        if let Some(table) = self.resolve_target_table(target.clone(), case)? {
            return Ok(Some(Relation::Table(table)));
        }
        if let Some(table) = self.resolve_target_foreign_table(target.clone(), case)? {
            return Ok(Some(Relation::ForeignTable(table)));
        }
        if let Some(view) = self.resolve_target_view(target.clone(), case)? {
            return Ok(Some(Relation::View(view)));
        }
        if let Some(view) = self.resolve_target_materialized_view(target.clone(), case)? {
            return Ok(Some(Relation::MaterializedView(view)));
        }
        Ok(self.resolve_target_sequence(target, case)?.map(Relation::Sequence))
    }

    /// Resolves a reference a stored object carries into the relation it
    /// names, of whatever kind, reading the name the way ingestion bound it.
    ///
    /// Ingestion rewrites every stored reference to the qualifier of the
    /// relation it reached when the object was created, leaving it bare only
    /// for a relation stored bare. So the name is read as written, with no
    /// search path, and neither a later path nor a later temporary relation
    /// of the same name reaches it, exactly as PostgreSQL keeps the object it
    /// bound.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one relation, and [`LookupError::InvalidObjectName`] when
    /// the name is malformed.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE docs (id INT);
    /// CREATE VIEW indexed AS SELECT id FROM docs;
    /// ",
    /// )?;
    /// // The bound name reads without a search path, bare for the default
    /// // schema.
    /// assert!(matches!(
    ///     db.resolve_bound_relation(TargetName::new("docs", false))?,
    ///     Some(Relation::Table(_))
    /// ));
    /// assert!(matches!(
    ///     db.resolve_bound_relation(TargetName::new("indexed", false))?,
    ///     Some(Relation::View(_))
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn resolve_bound_relation(
        &self,
        target: TargetName<'_>,
    ) -> Result<Option<Relation<'_, Self>>, LookupError> {
        if let Some(table) = self.table_by_target(target.clone(), IdentifierCase::AsWritten)? {
            return Ok(Some(Relation::Table(table)));
        }
        if let Some(table) =
            self.foreign_table_by_target(target.clone(), IdentifierCase::AsWritten)?
        {
            return Ok(Some(Relation::ForeignTable(table)));
        }
        if let Some(view) = self.view_by_target(target.clone(), IdentifierCase::AsWritten)? {
            return Ok(Some(Relation::View(view)));
        }
        if let Some(view) =
            self.materialized_view_by_target(target.clone(), IdentifierCase::AsWritten)?
        {
            return Ok(Some(Relation::MaterializedView(view)));
        }
        self.sequence_by_target(target, IdentifierCase::AsWritten)
            .map(|sequence| sequence.map(Relation::Sequence))
    }

    /// Resolves a name a statement wrote into the sequence it denotes,
    /// applying the same rules as [`Self::resolve_target_table`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
    /// than one sequence, and [`LookupError::InvalidObjectName`] when the name
    /// is malformed.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE SEQUENCE app.ticket_seq;
    ///      SET search_path = app, public;",
    /// )?;
    /// let sequence = db
    ///     .resolve_target_sequence(TargetName::new("ticket_seq", false), IdentifierCase::AsWritten)?
    ///     .expect("the path reaches `app`");
    /// assert_eq!(sequence.sequence_schema(), Some("app"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn resolve_target_sequence(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Sequence>, LookupError> {
        resolve_sequence_on_search_path_in_iter(
            self.sequences(),
            &target,
            || self.search_path(),
            case,
            |key| relation_name_is_claimed(self, key, case),
        )
    }

    /// Resolves a name a statement wrote into the foreign table it denotes,
    /// applying the same rules as [`Self::resolve_target_table`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches more
    /// than one foreign table.
    #[inline]
    fn resolve_target_foreign_table(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Table>, LookupError> {
        resolve_target_on_search_path_in_iter(
            self.foreign_tables(),
            &target,
            || self.search_path(),
            case,
            |key| relation_name_is_claimed(self, key, case),
        )
    }

    /// Returns the table an identifier and its optional qualifier name,
    /// comparing them as `case` says the engine does and consulting no search
    /// path, so an unqualified target names the default schema `public` and a
    /// `public` qualifier reaches a table stored without one.
    ///
    /// Takes the name in parts, which is what a caller holding identifier
    /// values rather than written SQL has, so a table really called
    /// `my.table` or `we"ird` is reachable. Read written text into parts with
    /// [`TargetName::parse`], which keeps a dot or a doubled quote inside the
    /// identifier it belongs to.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one table, which folding can make of two stored spellings.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE TABLE app.docs (id INT);
    ///      CREATE TABLE plain (id INT);
    ///      CREATE TABLE \"Mixed\" (id INT);
    ///      CREATE TABLE \"my.table\" (id INT);",
    /// )?;
    /// let case = IdentifierCase::AsWritten;
    /// let docs = TargetName::new("docs", false).with_schema("app", false);
    /// assert_eq!(db.table_by_target(docs, case)?.and_then(TableLike::table_schema), Some("app"));
    ///
    /// // Either spelling of the default schema names one place, and an
    /// // unquoted name folds while a quoted one stands as written.
    /// let plain = TargetName::new("PLAIN", false).with_schema("public", false);
    /// assert!(db.table_by_target(plain, case)?.is_some());
    /// assert!(db.table_by_target(TargetName::new("Mixed", true), case)?.is_some());
    /// assert!(db.table_by_target(TargetName::new("mixed", false), case)?.is_none());
    ///
    /// // A dot inside an identifier is part of it, which no text lookup can say.
    /// assert!(db.table_by_target(TargetName::new("my.table", true), case)?.is_some());
    /// let parsed = TargetName::parse("\"my.table\"").expect("one identifier");
    /// assert!(db.table_by_target(parsed, case)?.is_some());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn table_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Table>, LookupError> {
        resolve_target_in_iter(self.tables(), &target, case)
    }

    /// Returns the foreign table a name in parts denotes, applying the same
    /// rules as [`Self::table_by_target`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one foreign table.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FOREIGN TABLE remote (id INT) SERVER warehouse;",
    /// )?;
    /// let target = || TargetName::new("remote", false);
    /// assert!(db.foreign_table_by_target(target(), IdentifierCase::AsWritten)?.is_some());
    /// // A foreign table is no table, so the table lookup does not reach it.
    /// assert!(db.table_by_target(target(), IdentifierCase::AsWritten)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn foreign_table_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Table>, LookupError> {
        resolve_target_in_iter(self.foreign_tables(), &target, case)
    }

    /// Returns the plain view a name in parts denotes, applying the same rules
    /// as [`Self::table_by_target`].
    ///
    /// A materialized view of that name answers [`None`] here: ask
    /// [`Self::materialized_view_by_target`] for those.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one view.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT);
    ///      CREATE SCHEMA app;
    ///      CREATE VIEW app.v AS SELECT id FROM t;",
    /// )?;
    /// let case = IdentifierCase::AsWritten;
    /// let qualified = || TargetName::new("v", false).with_schema("app", false);
    /// assert!(db.view_by_target(qualified(), case)?.is_some());
    /// assert!(db.view_by_target(TargetName::new("v", false), case)?.is_none());
    /// assert!(db.materialized_view_by_target(qualified(), case)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn view_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::View>, LookupError> {
        resolve_view_in_iter(self.views(), &target, case)
    }

    /// Returns the materialized view a name in parts denotes, applying the
    /// same rules as [`Self::table_by_target`].
    ///
    /// A plain view of that name answers [`None`] here: ask
    /// [`Self::view_by_target`] for those.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one materialized view.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
    /// )?;
    /// let case = IdentifierCase::AsWritten;
    /// assert!(db.materialized_view_by_target(TargetName::new("m", false), case)?.is_some());
    /// assert!(db.view_by_target(TargetName::new("m", false), case)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn materialized_view_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::MaterializedView>, LookupError> {
        resolve_view_in_iter(self.materialized_views(), &target, case)
    }

    /// Returns the sequence a name in parts denotes, applying the same rules
    /// as [`Self::table_by_target`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousTableLookup`] when the name matches
    /// more than one sequence.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db =
    ///     ParserDB::parse::<PostgreSqlDialect>("CREATE SCHEMA app; CREATE SEQUENCE app.ticket_seq;")?;
    /// let case = IdentifierCase::AsWritten;
    /// let qualified = TargetName::new("ticket_seq", false).with_schema("app", false);
    /// assert!(db.sequence_by_target(qualified, case)?.is_some());
    /// assert!(db.sequence_by_target(TargetName::new("ticket_seq", false), case)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn sequence_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Sequence>, LookupError> {
        resolve_sequence_in_iter(self.sequences(), &target, case)
    }

    /// Returns the function a name in parts denotes, applying the same rules
    /// as [`Self::table_by_target`].
    ///
    /// A registered builtin lives in `pg_catalog`, so it is asked for by that
    /// schema. Use [`Self::resolve_target_function`] to have the search path
    /// applied.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousFunctionLookup`] when the name carries
    /// more than one declaration, since resolution by name alone cannot
    /// choose between argument lists.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE FUNCTION add_one(x INT) RETURNS INT AS 'SELECT x + 1;';
    ///      CREATE FUNCTION app.touch() RETURNS INT AS 'SELECT 1;';",
    /// )?;
    /// let case = IdentifierCase::AsWritten;
    /// assert_eq!(
    ///     db.function_by_target(TargetName::new("add_one", false), case)?.map(FunctionLike::name),
    ///     Some("add_one")
    /// );
    /// let touch = TargetName::new("touch", false).with_schema("app", false);
    /// assert!(db.function_by_target(touch, case)?.is_some());
    /// assert!(db.function_by_target(TargetName::new("touch", false), case)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn function_by_target(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Function>, LookupError> {
        resolve_function_in_iter(self.functions(), &target, case)
    }

    /// Returns the table ID for the given table object according to its
    /// position in the database's table iterator.
    ///
    /// # Arguments
    ///
    /// * `table` - Table object to get the ID for.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT);
    /// CREATE TABLE table2 (name TEXT);
    /// CREATE TABLE table3 (score DECIMAL);
    /// ",
    /// )?;
    /// let table2 = db
    ///     .table_by_target(TargetName::new("table2", false), IdentifierCase::AsWritten)?
    ///     .expect("Table 'table2' should exist");
    /// let table2_id = db.table_id(table2).expect("Table ID for 'table2' should exist");
    /// assert_eq!(table2_id, 1);
    /// # Ok(())
    /// # }
    /// ```
    fn table_id(&self, table: &Self::Table) -> Option<usize>;

    /// Returns the table at the given table ID according to the database's
    /// table iterator ordering.
    ///
    /// # Arguments
    ///
    /// * `table_id` - Table ID to look up.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE table1 (id INT);
    /// CREATE TABLE table2 (name TEXT);
    /// ",
    /// )?;
    /// let table = db.table_by_id(1).expect("Table at ID 1 should exist");
    /// assert_eq!(table.table_name(), "table2");
    /// assert!(db.table_by_id(2).is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn table_by_id(&self, table_id: usize) -> Option<&Self::Table> {
        self.tables().nth(table_id)
    }

    /// Returns the identity of a table or foreign table this database holds,
    /// or `None` for one it does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id INT); DROP TABLE t; CREATE TABLE t (id INT);",
    /// )?;
    /// let table = db
    ///     .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?
    ///     .expect("the table exists");
    /// let identity = db.table_identity(table).expect("the table is held here");
    /// assert_eq!(db.table_by_identity(identity), Some(table));
    /// # Ok(())
    /// # }
    /// ```
    fn table_identity(&self, table: &Self::Table) -> Option<Self::RelationId>;

    /// Returns the identity of a plain view this database holds, or `None`
    /// for a view it does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id INT); CREATE VIEW v AS SELECT id FROM t;
    ///      ALTER TABLE v RENAME TO w;",
    /// )?;
    /// let view = db.views().next().expect("one view");
    /// let identity = db.view_identity(view).expect("the view is held here");
    /// assert_eq!(db.view_by_identity(identity).map(ViewLike::view_name), Some("w"));
    /// # Ok(())
    /// # }
    /// ```
    fn view_identity(&self, view: &Self::View) -> Option<Self::RelationId>;

    /// Returns the identity of a materialized view this database holds, or
    /// `None` for one it does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (id INT); CREATE MATERIALIZED VIEW m AS SELECT id FROM t;",
    /// )?;
    /// let view = db.materialized_views().next().expect("one materialized view");
    /// let identity = db.materialized_view_identity(view).expect("the view is held here");
    /// assert_eq!(db.materialized_view_by_identity(identity), Some(view));
    /// assert_eq!(db.view_by_identity(identity), None);
    /// # Ok(())
    /// # }
    /// ```
    fn materialized_view_identity(&self, view: &Self::MaterializedView)
    -> Option<Self::RelationId>;

    /// Returns the identity of a table column this database holds, or `None`
    /// for a column it does not hold.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE t (a INT); ALTER TABLE t RENAME COLUMN a TO b;",
    /// )?;
    /// let table = db.tables().next().expect("one table");
    /// let column = table.columns(&db)?.next().expect("one column");
    /// let identity = db.column_identity(column).expect("the column is held here");
    /// assert_eq!(db.column_by_identity(identity).map(ColumnLike::column_name), Some("b"));
    /// # Ok(())
    /// # }
    /// ```
    fn column_identity(&self, column: &Self::Column) -> Option<Self::ColumnId>;

    /// Returns the table holding `identity`, or `None` when no table here
    /// holds it, because it was dropped, never created, or names a foreign
    /// table or a view.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    ///
    /// let ingestor = ParserDBIngestor::new::<PostgreSqlDialect>("catalog".to_owned());
    /// let mut ingestor = Parser::parse_sql(&PostgreSqlDialect {}, "CREATE TABLE t (id INT);")?
    ///     .into_iter()
    ///     .try_fold(ingestor, ParserDBIngestor::apply_statement)?;
    /// let before = ingestor.snapshot();
    /// let identity = before
    ///     .table_identity(before.tables().next().expect("one table"))
    ///     .expect("the table is held here");
    /// for statement in Parser::parse_sql(&PostgreSqlDialect {}, "ALTER TABLE t RENAME TO u;")? {
    ///     ingestor = ingestor.apply_statement(statement)?;
    /// }
    /// let after = ingestor.finish();
    /// assert_eq!(after.table_by_identity(identity).map(TableLike::table_name), Some("u"));
    /// # Ok(())
    /// # }
    /// ```
    fn table_by_identity(&self, identity: Self::RelationId) -> Option<&Self::Table> {
        self.tables().find(|table| self.table_identity(table) == Some(identity))
    }

    /// Returns the foreign table holding `identity`, or `None` when no foreign
    /// table here holds it.
    fn foreign_table_by_identity(&self, identity: Self::RelationId) -> Option<&Self::Table> {
        self.foreign_tables().find(|table| self.table_identity(table) == Some(identity))
    }

    /// Returns the plain view holding `identity`, or `None` when no plain
    /// view here holds it.
    fn view_by_identity(&self, identity: Self::RelationId) -> Option<&Self::View> {
        self.views().find(|view| self.view_identity(view) == Some(identity))
    }

    /// Returns the materialized view holding `identity`, or `None` when no
    /// materialized view here holds it.
    fn materialized_view_by_identity(
        &self,
        identity: Self::RelationId,
    ) -> Option<&Self::MaterializedView> {
        self.materialized_views()
            .find(|view| self.materialized_view_identity(view) == Some(identity))
    }

    /// Returns the table column holding `identity` with whatever name
    /// renames gave it, or `None` when no column here holds it, because it
    /// or its table was dropped.
    fn column_by_identity(&self, identity: Self::ColumnId) -> Option<&Self::Column> {
        self.tables_and_foreign_tables()
            .filter_map(|table| table.columns(self).ok())
            .flatten()
            .find(|column| self.column_identity(column) == Some(identity))
    }

    /// Returns the function stored under exactly this identity.
    ///
    /// Both parts are read as the catalog holds them, so nothing folds and a
    /// function stored without a schema is not reached by a `public`
    /// qualifier. This is the counterpart of
    /// [`Self::table_by_stored_identity`] for functions.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousFunctionLookup`] when the identity
    /// carries several declarations differing in their arguments, which this
    /// surface cannot choose between.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE SCHEMA app; CREATE FUNCTION app.Touch() RETURNS INT AS 'SELECT 1;';",
    /// )?;
    ///
    /// assert!(db.function_by_stored_identity(Some("app"), "touch")?.is_some());
    /// assert!(db.function_by_stored_identity(Some("app"), "Touch")?.is_none());
    /// assert!(db.function_by_stored_identity(None, "touch")?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn function_by_stored_identity(
        &self,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<&Self::Function>, LookupError> {
        let candidates: Vec<&Self::Function> = self
            .functions()
            .filter(|function| function_has_stored_identity(*function, schema, name))
            .collect();
        resolve_one_function(name, &candidates)
    }

    /// Resolves a name a statement wrote into the function it denotes,
    /// applying the same rules as [`Self::resolve_target_table`]: a qualified
    /// name matches in its own schema, an unqualified one resolves through the
    /// first schema holding it, and quoting decides case sensitivity on both
    /// parts. As in PostgreSQL, an unqualified name searches `pg_catalog`
    /// before [`Self::search_path`] unless the path names it, so a builtin is
    /// reached by its bare name.
    ///
    /// Resolution is by name only, so a name carrying several declarations
    /// is reported as ambiguous here rather than resolved. A written call is
    /// resolved by its arguments, the way PostgreSQL resolves it, by
    /// [`Self::functions_reached_by`] and [`Self::resolve_call`].
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::AmbiguousFunctionLookup`] when the name carries
    /// several declarations in the schema that wins.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{prelude::*, structs::TargetName};
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE FUNCTION app.touch() RETURNS INT AS 'SELECT 1;';
    ///      SET search_path TO app;",
    /// )?;
    ///
    /// let touch = db
    ///     .resolve_target_function(TargetName::new("touch", false), IdentifierCase::AsWritten)?
    ///     .expect("the path carries `app`");
    /// assert_eq!(touch.name(), "touch");
    /// let qualified = TargetName::new("touch", false).with_schema("app", false);
    /// assert!(db.resolve_target_function(qualified, IdentifierCase::AsWritten)?.is_some());
    /// let builtin = TargetName::new("gen_random_uuid", false);
    /// let builtin = db.resolve_target_function(builtin, IdentifierCase::AsWritten)?;
    /// assert_eq!(
    ///     builtin.and_then(|f| f.target_name().schema().map(str::to_owned)).as_deref(),
    ///     Some("pg_catalog")
    /// );
    /// let absent = TargetName::new("absent", false);
    /// assert!(db.resolve_target_function(absent, IdentifierCase::AsWritten)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn resolve_target_function(
        &self,
        target: TargetName<'_>,
        case: IdentifierCase,
    ) -> Result<Option<&Self::Function>, LookupError> {
        resolve_function_on_search_path_in_iter(
            self.functions(),
            &target,
            function_search_path(|| self.search_path()),
            case,
        )
    }

    /// Returns the functions a call written in any statement may run, as
    /// PostgreSQL gathers them before it weighs argument types.
    ///
    /// A qualified name reaches its own schema. An unqualified one reaches
    /// `pg_catalog`, unless [`Self::search_path`] names it, and every schema
    /// of the path, a function hidden by one taking the same arguments in a
    /// schema searched earlier left out. Only the functions taking the call's
    /// arguments count, as many as it passes with the trailing ones a default
    /// fills left out, a `VARIADIC` argument collecting the rest, and the
    /// names it passes under named notation. A function stored without a
    /// schema is in `public`.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::InvalidObjectName`] when the called name has
    /// more than two parts or a part built at run time.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("")?;
    /// let Statement::Query(query) =
    ///     Parser::parse_sql(&PostgreSqlDialect {}, "SELECT array_agg(1)")?.remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = *query.body else { unreachable!() };
    /// let SelectItem::UnnamedExpr(Expr::Function(call)) = &select.projection[0] else {
    ///     unreachable!()
    /// };
    /// // `array_agg` takes a scalar or an array, and both take one argument.
    /// let mut reached: Vec<_> = db
    ///     .functions_reached_by(call, IdentifierCase::AsWritten)?
    ///     .map(|function| function.argument_type_names(&db).collect::<Vec<_>>().join(", "))
    ///     .collect();
    /// reached.sort_unstable();
    /// assert_eq!(reached, ["anyarray", "anynonarray"]);
    /// # Ok(())
    /// # }
    /// ```
    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        case: IdentifierCase,
    ) -> Result<impl Iterator<Item = &'db Self::Function>, LookupError> {
        Ok(reached_functions(self, call, case)?.into_iter().map(|(function, _)| function))
    }

    /// Returns the function a call runs, given the type of each argument it
    /// writes, as PostgreSQL's `func_get_detail` picks it among
    /// [`Self::functions_reached_by`]. Of the functions whose parameters take
    /// the argument types by an implicit cast or a consistent polymorphic
    /// binding, an exact match wins, then the one with the most exact
    /// matches, then the one taking preferred types where a conversion is
    /// needed, then the one taking the categories untyped literals lean
    /// towards.
    ///
    /// Each entry of `argument_types` names a type, `unknown` for an untyped
    /// literal such as `NULL` or `'abc'`, or is [`None`] for an argument whose
    /// type the caller does not know. Answers [`None`] when PostgreSQL would
    /// refuse the call as matching no function or several alike, and when
    /// the unknown argument types leave more than one function possible.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::InvalidObjectName`] when the called name has
    /// more than two parts or a part built at run time, and
    /// [`LookupError::ArgumentTypeCount`] when `argument_types` does not hold
    /// one entry per argument `call` writes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE FUNCTION f(x INT) RETURNS INT LANGUAGE sql AS 'SELECT x';
    ///      CREATE FUNCTION f(x BYTEA) RETURNS BYTEA LANGUAGE sql AS 'SELECT x';",
    /// )?;
    /// let Statement::Query(query) =
    ///     Parser::parse_sql(&PostgreSqlDialect {}, "SELECT f(b)")?.remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = *query.body else { unreachable!() };
    /// let SelectItem::UnnamedExpr(Expr::Function(call)) = &select.projection[0] else {
    ///     unreachable!()
    /// };
    /// let chosen = db.resolve_call(call, &[Some("BYTEA")], IdentifierCase::AsWritten)?;
    /// assert_eq!(chosen.and_then(|f| f.argument_type_names(&db).next()).as_deref(), Some("BYTEA"));
    /// // Either declaration may run when the argument's type is not known.
    /// assert!(db.resolve_call(call, &[None], IdentifierCase::AsWritten)?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn resolve_call<'db>(
        &'db self,
        call: &Function,
        argument_types: &[Option<&'db str>],
        case: IdentifierCase,
    ) -> Result<Option<&'db Self::Function>, LookupError> {
        Ok(match resolve_call(self, call, argument_types, case)? {
            Selection::Chosen((function, _)) => Some(function),
            Selection::Refused | Selection::Undetermined(_) => None,
        })
    }

    /// Returns the type a call written in any statement returns, given the
    /// type of each argument it writes. It is the result of the function
    /// [`Self::resolve_call`] picks, as
    /// [`FunctionLike::call_result_type_name`] answers it for the call.
    ///
    /// When the arguments whose types the caller does not know leave several
    /// functions possible, the answer is the type they all return, if they
    /// agree. Answers [`None`] when PostgreSQL would refuse the call, and
    /// when the result depends on a type nobody knows.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::InvalidObjectName`] when the called name has
    /// more than two parts or a part built at run time, and
    /// [`LookupError::ArgumentTypeCount`] when `argument_types` does not hold
    /// one entry per argument `call` writes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{
    ///     ast::{Expr, SelectItem, SetExpr, Statement},
    ///     dialect::PostgreSqlDialect,
    ///     parser::Parser,
    /// };
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (b BYTEA);")?;
    /// let Statement::Query(query) =
    ///     Parser::parse_sql(&PostgreSqlDialect {}, "SELECT array_agg(b), decode(x, 'hex') FROM t")?
    ///         .remove(0)
    /// else {
    ///     unreachable!()
    /// };
    /// let SetExpr::Select(select) = *query.body else { unreachable!() };
    /// let calls: Vec<_> = select
    ///     .projection
    ///     .iter()
    ///     .map(|item| {
    ///         match item {
    ///             SelectItem::UnnamedExpr(Expr::Function(call)) => call,
    ///             _ => unreachable!(),
    ///         }
    ///     })
    ///     .collect();
    /// let case = IdentifierCase::AsWritten;
    /// assert_eq!(
    ///     db.call_result_type_name(calls[0], &[Some("BYTEA")], case)?.as_deref(),
    ///     Some("BYTEA[]")
    /// );
    /// // `decode` returns `bytea` whatever text it decodes.
    /// assert_eq!(
    ///     db.call_result_type_name(calls[1], &[None, Some("unknown")], case)?.as_deref(),
    ///     Some("BYTEA")
    /// );
    /// # Ok(())
    /// # }
    /// ```
    fn call_result_type_name<'db>(
        &'db self,
        call: &Function,
        argument_types: &[Option<&'db str>],
        case: IdentifierCase,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        Ok(match resolve_call(self, call, argument_types, case)? {
            Selection::Chosen((_, result)) => result,
            Selection::Refused => None,
            Selection::Undetermined(possible) => {
                let mut results = possible.into_iter().map(|(_, result)| result);
                let first = results.next().flatten();
                let agreed = first.as_ref().and_then(|first| PgType::parse(first));
                agreed
                    .filter(|agreed| {
                        results.all(|result| {
                            result.and_then(|result| PgType::parse(&result)).as_ref()
                                == Some(agreed)
                        })
                    })
                    .and(first)
            }
        })
    }

    /// Iterates over the policies defined in the schema.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE POLICY my_policy ON t USING (id > 0);
    /// ",
    /// )?;
    /// let policies: Vec<&str> = db.policies().map(|p| p.name()).collect();
    /// assert_eq!(policies, vec!["my_policy"]);
    /// # Ok(())
    /// # }
    /// ```
    fn policies(&self) -> impl Iterator<Item = &Self::Policy>;

    /// Returns whether the datavase has policies defined.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db_with_policies = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE POLICY my_policy ON t USING (id > 0);
    /// ",
    /// )?;
    /// assert!(db_with_policies.has_policies());
    ///
    /// let db_without_policies = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// ",
    /// )?;
    /// assert!(!db_without_policies.has_policies());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_policies(&self) -> bool {
        self.policies().next().is_some()
    }

    /// Iterates over the roles defined in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE ROLE admin;
    /// CREATE ROLE user1;
    /// ",
    /// )?;
    ///
    /// let roles: Vec<_> = db.roles().collect();
    /// assert_eq!(roles.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn roles(&self) -> impl Iterator<Item = &Self::Role>;

    /// Returns a role by canonical stored name, if it exists.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE ROLE admin SUPERUSER;")?;
    ///
    /// let admin = db.role("admin");
    /// assert!(admin.is_some());
    /// assert!(admin.unwrap().is_superuser());
    ///
    /// let nonexistent = db.role("nonexistent");
    /// assert!(nonexistent.is_none());
    /// # Ok(())
    /// # }
    /// ```
    fn role(&self, name: &str) -> Option<&Self::Role> {
        self.roles().find(|role| role.stored_name().as_ref() == name)
    }

    /// Iterates over the role memberships recorded in the database, from
    /// `GRANT role TO role` and from the membership clauses of `CREATE ROLE`,
    /// one for each granted role and member that `REVOKE role FROM role` has
    /// not taken away.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE ROLE parent;
    /// CREATE ROLE child IN ROLE parent;
    /// CREATE ROLE other;
    /// GRANT parent TO other WITH SET FALSE;
    /// ",
    /// )?;
    ///
    /// let members: Vec<_> = db.role_memberships().map(RoleMembershipLike::member_name).collect();
    /// assert_eq!(members, ["child", "other"]);
    /// # Ok(())
    /// # }
    /// ```
    fn role_memberships(&self) -> impl Iterator<Item = &Self::RoleMembership>;

    /// Returns whether the database has any roles defined.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db_with_roles = ParserDB::parse::<GenericDialect>("CREATE ROLE admin;")?;
    /// assert!(db_with_roles.has_roles());
    ///
    /// let db_without_roles = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// assert!(!db_without_roles.has_roles());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_roles(&self) -> bool {
        self.roles().next().is_some()
    }

    /// Iterates over tables that have Row Level Security (RLS) enabled.
    ///
    /// This includes tables with either regular RLS or forced RLS.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError`] when the metadata of one of the tables this
    /// database reports cannot be resolved.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE rls_table (id INT);
    /// ALTER TABLE rls_table ENABLE ROW LEVEL SECURITY;
    /// CREATE TABLE forced_rls_table (id INT);
    /// ALTER TABLE forced_rls_table ENABLE ROW LEVEL SECURITY;
    /// ALTER TABLE forced_rls_table FORCE ROW LEVEL SECURITY;
    /// CREATE TABLE no_rls_table (id INT);
    /// ",
    /// )?;
    ///
    /// let rls_table_names: Vec<&str> = db.rls_tables()?.map(|t| t.table_name()).collect();
    /// assert_eq!(rls_table_names.len(), 2);
    /// assert!(rls_table_names.contains(&"rls_table"));
    /// assert!(rls_table_names.contains(&"forced_rls_table"));
    /// assert!(!rls_table_names.contains(&"no_rls_table"));
    /// # Ok(())
    /// # }
    /// ```
    fn rls_tables(&self) -> Result<impl Iterator<Item = &Self::Table>, LookupError> {
        let mut tables = Vec::new();
        for table in self.tables() {
            if table.has_row_level_security(self)? {
                tables.push(table);
            }
        }

        Ok(tables.into_iter())
    }

    /// Iterates over tables that have forced Row Level Security (RLS) enabled.
    ///
    /// Forced RLS means that even the table owner is subject to RLS policies.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError`] when the metadata of one of the tables this
    /// database reports cannot be resolved.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE rls_table (id INT);
    /// ALTER TABLE rls_table ENABLE ROW LEVEL SECURITY;
    /// CREATE TABLE forced_rls_table (id INT);
    /// ALTER TABLE forced_rls_table ENABLE ROW LEVEL SECURITY;
    /// ALTER TABLE forced_rls_table FORCE ROW LEVEL SECURITY;
    /// CREATE TABLE no_rls_table (id INT);
    /// ",
    /// )?;
    ///
    /// let forced_rls_table_names: Vec<&str> =
    ///     db.forced_rls_tables()?.map(|t| t.table_name()).collect();
    /// assert_eq!(forced_rls_table_names.len(), 1);
    /// assert_eq!(forced_rls_table_names[0], "forced_rls_table");
    /// # Ok(())
    /// # }
    /// ```
    fn forced_rls_tables(&self) -> Result<impl Iterator<Item = &Self::Table>, LookupError> {
        let mut tables = Vec::new();
        for table in self.tables() {
            if table.has_forced_row_level_security(self)? {
                tables.push(table);
            }
        }

        Ok(tables.into_iter())
    }

    /// Returns whether the database has any tables with Row Level Security
    /// enabled.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError`] when the metadata of one of the tables this
    /// database reports cannot be resolved.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db_with_rls = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// ALTER TABLE t ENABLE ROW LEVEL SECURITY;
    /// ",
    /// )?;
    /// assert!(db_with_rls.has_rls_tables()?);
    ///
    /// let db_without_rls = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// assert!(!db_without_rls.has_rls_tables()?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_rls_tables(&self) -> Result<bool, LookupError> {
        Ok(self.rls_tables()?.next().is_some())
    }

    /// Returns the number of tables with Row Level Security enabled.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError`] when the metadata of one of the tables this
    /// database reports cannot be resolved.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t1 (id INT);
    /// ALTER TABLE t1 ENABLE ROW LEVEL SECURITY;
    /// CREATE TABLE t2 (id INT);
    /// ALTER TABLE t2 ENABLE ROW LEVEL SECURITY;
    /// CREATE TABLE t3 (id INT);
    /// ",
    /// )?;
    /// assert_eq!(db.number_of_rls_tables()?, 2);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn number_of_rls_tables(&self) -> Result<usize, LookupError> {
        Ok(self.rls_tables()?.count())
    }

    /// Iterates over the table grants defined in the database.
    ///
    /// Table grants apply privileges to entire tables. This includes direct
    /// table grants (`GRANT ... ON table_name`) and schema-wide table grants
    /// (`GRANT ... ON ALL TABLES IN SCHEMA`).
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE users (id INT);
    /// CREATE ROLE app_user;
    /// CREATE ROLE admin;
    /// GRANT SELECT ON users TO app_user;
    /// GRANT INSERT, UPDATE ON users TO admin;
    /// ",
    /// )?;
    /// let grants: Vec<_> = db.table_grants().collect();
    /// assert_eq!(grants.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn table_grants(&self) -> impl Iterator<Item = &Self::TableGrant>;

    /// Returns whether the database has any table grants defined.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db_with_grants = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE TABLE t (id INT);
    /// CREATE ROLE app_user;
    /// GRANT SELECT ON t TO app_user;
    /// ",
    /// )?;
    /// assert!(db_with_grants.has_table_grants());
    ///
    /// let db_without_grants = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// assert!(!db_without_grants.has_table_grants());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_table_grants(&self) -> bool {
        self.table_grants().next().is_some()
    }

    /// Iterates over the column grants defined in the database.
    ///
    /// Column grants apply privileges to specific columns within tables.
    /// This allows fine-grained access control at the column level.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE users (id INT, name TEXT, secret TEXT);
    /// CREATE ROLE app_user;
    /// GRANT SELECT (id, name) ON users TO app_user;
    /// ",
    /// )?;
    /// let column_grants: Vec<_> = db.column_grants().collect();
    /// assert_eq!(column_grants.len(), 1);
    /// # Ok(())
    /// # }
    /// ```
    fn column_grants(&self) -> impl Iterator<Item = &Self::ColumnGrant>;

    /// Returns whether the database has any column grants defined.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db_with_column_grants = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE TABLE t (id INT, name TEXT);
    /// CREATE ROLE app_user;
    /// GRANT SELECT (id) ON t TO app_user;
    /// ",
    /// )?;
    /// assert!(db_with_column_grants.has_column_grants());
    ///
    /// let db_without_column_grants = ParserDB::parse::<GenericDialect>("CREATE TABLE t (id INT);")?;
    /// assert!(!db_without_column_grants.has_column_grants());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn has_column_grants(&self) -> bool {
        self.column_grants().next().is_some()
    }

    /// Iterates over the schemas defined in the database.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "
    /// CREATE ROLE admin;
    /// CREATE SCHEMA my_schema;
    /// CREATE SCHEMA other_schema AUTHORIZATION admin;
    /// ",
    /// )?;
    /// let schemas: Vec<_> = db.schemas().collect();
    /// assert_eq!(schemas.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    fn schemas(&self) -> impl Iterator<Item = &Self::Schema>;

    /// Returns the schemas an unqualified name resolves against, in order,
    /// each with whether it was quoted.
    ///
    /// This is PostgreSQL's `search_path`. A backend reading a live database
    /// answers with the session's, and one reading SQL text answers with
    /// whatever the text last set. The default is `public` alone, which is what
    /// PostgreSQL falls back to once the entry naming the connected user is
    /// discounted.
    ///
    /// The path also decides where a statement creating a permanent table
    /// without a schema puts it: the first schema on the path that exists, or
    /// no schema at all when that is the default one, which this model already
    /// spells without the prefix.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db =
    ///     ParserDB::parse::<GenericDialect>("CREATE SCHEMA app; SET search_path TO app, public;")?;
    /// let path: Vec<_> = db.search_path().collect();
    /// assert_eq!(path, [("app", false), ("public", false)]);
    ///
    /// let created = ParserDB::parse::<GenericDialect>(
    ///     "CREATE SCHEMA app; SET search_path TO app; CREATE TABLE docs (id INT);",
    /// )?;
    /// assert_eq!(
    ///     created
    ///         .table_by_target(
    ///             TargetName::new("docs", false).with_schema("app", false),
    ///             IdentifierCase::AsWritten
    ///         )?
    ///         .map(TableLike::table_name),
    ///     Some("docs")
    /// );
    /// assert!(
    ///     created
    ///         .table_by_target(TargetName::new("docs", false), IdentifierCase::AsWritten)?
    ///         .is_none()
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn search_path(&self) -> impl Iterator<Item = (&str, bool)> {
        core::iter::once(("public", false))
    }

    /// Returns the schema with the given name, if it exists.
    ///
    /// # Arguments
    ///
    /// * `name` - Name of the schema to retrieve.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     r#"
    ///     CREATE SCHEMA Foo;
    ///     CREATE SCHEMA "Bar";
    ///     "#,
    /// )?;
    ///
    /// assert!(db.schema("foo").is_some());
    /// assert!(db.schema("\"foo\"").is_some());
    /// assert!(db.schema("\"Foo\"").is_none());
    ///
    /// assert!(db.schema("\"Bar\"").is_some());
    /// assert!(db.schema("bar").is_none());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// For identifier-aware lookup from parsed SQL AST, use
    /// [`ParserDB::resolve_schema_ident`](crate::structs::ParserDB::resolve_schema_ident).
    fn schema(&self, name: &str) -> Option<&Self::Schema> {
        self.schemas()
            .find(|s| stored_identifier_matches_lookup(s.name(), s.name_is_quoted(), name))
    }

    /// Returns whether the database has any schemas defined.
    #[inline]
    fn has_schemas(&self) -> bool {
        self.schemas().next().is_some()
    }
}

/// One relation a database holds, tagged with the kind it is.
///
/// Tables, foreign tables, plain views, materialized views and sequences share
/// one pool of names, so a name resolves to at most one of them. A reader that
/// needs to say which kind a lookup reached, whether from a grant, a rule or a
/// statement's reads, handles this enum one variant at a time.
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
/// assert!(matches!(
///     db.resolve_target_relation(TargetName::new("t", false), IdentifierCase::AsWritten)?,
///     Some(Relation::Table(table)) if table.table_name() == "t"
/// ));
/// assert!(matches!(
///     db.resolve_target_relation(TargetName::new("v", false), IdentifierCase::AsWritten)?,
///     Some(Relation::View(view)) if view.view_name() == "v"
/// ));
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub enum Relation<'a, DB: DatabaseLike> {
    /// A table.
    Table(&'a DB::Table),
    /// A foreign table, whose rows live behind a foreign server, so a read or
    /// a grant reaches the remote relation with no row-level security between.
    ForeignTable(&'a DB::Table),
    /// A plain view.
    View(&'a DB::View),
    /// A materialized view.
    MaterializedView(&'a DB::MaterializedView),
    /// A sequence.
    Sequence(&'a DB::Sequence),
}

impl<DB: DatabaseLike> Copy for Relation<'_, DB> {}

impl<DB: DatabaseLike> Debug for Relation<'_, DB> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Table(table) => f.debug_tuple("Table").field(table).finish(),
            Self::ForeignTable(table) => f.debug_tuple("ForeignTable").field(table).finish(),
            Self::View(view) => f.debug_tuple("View").field(view).finish(),
            Self::MaterializedView(view) => f.debug_tuple("MaterializedView").field(view).finish(),
            Self::Sequence(sequence) => f.debug_tuple("Sequence").field(sequence).finish(),
        }
    }
}
