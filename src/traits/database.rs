//! Submodule providing a trait for describing SQL Database-like entities.

use alloc::{string::ToString, vec::Vec};
use core::{borrow::Borrow, fmt::Debug};

use geometric_traits::{
    impls::CSR2D,
    prelude::{GenericEdgesBuilder, Kahn, SquareCSR2D},
    traits::EdgesBuilder,
};

use crate::{
    errors::{Error, LookupError},
    structs::{IdentifierCase, TargetName},
    traits::{
        CheckConstraintLike, ColumnGrantLike, ColumnLike, DialectLike, ForeignKeyLike,
        FunctionLike, IndexLike, PolicyLike, RoleLike, SchemaLike, TableGrantLike, TableLike,
        TriggerLike, UniqueIndexLike, ViewLike,
    },
    utils::{
        identifier_resolution::stored_identifier_matches_lookup,
        object_name::{
            function_has_stored_identity, relation_name_is_claimed, resolve_function_in_iter,
            resolve_function_on_search_path_in_iter, resolve_one_function, resolve_target_in_iter,
            resolve_target_on_search_path_in_iter, resolve_view_in_iter,
            resolve_view_on_search_path_in_iter,
        },
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
    /// Type of the columns in the schema.
    type Column: ColumnLike<DB = Self>;
    /// Type of the indices in the schema.
    type Index: IndexLike<DB = Self>;
    /// Type of the foreign keys in the schema.
    type ForeignKey: ForeignKeyLike<DB = Self>;
    /// Type of the functions in the schema.
    type Function: FunctionLike<DB = Self>;
    /// Type of the triggers in the schema.
    type Trigger: TriggerLike<DB = Self>;
    /// Type of the unique indexes in the schema.
    type UniqueIndex: UniqueIndexLike<DB = Self>;
    /// Type of the check constraints in the schema.
    type CheckConstraint: CheckConstraintLike<DB = Self>;
    /// Type of the policies in the schema.
    type Policy: PolicyLike<DB = Self>;
    /// Type of the roles in the schema.
    type Role: RoleLike<DB = Self>;
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
    /// first schema on [`Self::search_path`] holding it, and quoting decides
    /// case sensitivity on both parts.
    ///
    /// Resolution is by name only. PostgreSQL also weighs the argument list,
    /// so a name carrying several declarations is reported as ambiguous here
    /// rather than resolved. A registered builtin lives in `pg_catalog`, which
    /// the recorded search path does not carry, so an unqualified reference to
    /// one stays unresolved and the schema has to be written.
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
        resolve_function_on_search_path_in_iter(self.functions(), &target, self.search_path(), case)
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
