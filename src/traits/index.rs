//! Submodule defining the `IndexLike` trait for SQL indices, and the
//! `TableIndexLike` trait for the indices built on a table.

use alloc::{borrow::Cow, vec::Vec};
use core::fmt::Debug;

use sqlparser::ast::{Expr, IndexType};

use crate::{
    errors::LookupError,
    structs::{IndexKey, SqliteIndexColumn, TargetName, WrittenCall},
    traits::{DatabaseLike, Metadata, Relation, TableLike},
    utils::columns_in_expression::columns_in_expression,
};

/// An index is a rule that specifies that the values in a column
/// (or a group of columns) must used to speed up queries on a relation.
/// This trait represents such an index in a database-agnostic way.
///
/// Every reader here means the same for an index on a table and for one on a
/// materialized view, which PostgreSQL indexes alike. The readers that hand
/// out the columns of a table are on [`TableIndexLike`], since a materialized
/// view has no column of its own to hand out.
pub trait IndexLike: Metadata + Ord + Eq + Debug + Clone + Send + Sync {
    /// The database type the index belongs to.
    type DB: DatabaseLike;

    /// Returns the declared index name, or `None` for an anonymous index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE users (id int, name text); CREATE INDEX idx_name ON users (name);",
    /// )?;
    /// let index = db.indexes().next().unwrap();
    /// assert_eq!(index.name(), Some("idx_name"));
    /// assert!(!index.name_is_quoted());
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> Option<&str>;

    /// Returns whether the index identifier was quoted in SQL.
    ///
    /// This only matters when [`Self::name`] returns `Some`, and answers
    /// `false` for an anonymous index, which has no identifier to quote.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the schema the index lives in, which is always the schema of
    /// the relation it is built on, or `None` when that relation has none, as
    /// [`TableLike::table_schema`] and
    /// [`ViewLike::view_schema`](crate::traits::ViewLike::view_schema) answer.
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
    ///      CREATE TABLE app.users (id int, name text);
    ///      CREATE INDEX \"IdxName\" ON app.users (name);",
    /// )?;
    /// let index = db.indexes().next().unwrap();
    /// assert_eq!(index.name(), Some("IdxName"));
    /// assert!(index.name_is_quoted());
    /// assert_eq!(index.schema(), Some("app"));
    /// assert!(!index.schema_is_quoted());
    /// # Ok(())
    /// # }
    /// ```
    fn schema(&self) -> Option<&str>;

    /// Returns whether the identifier of that schema was quoted in SQL.
    ///
    /// This only matters when [`Self::schema`] returns `Some`.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn schema_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name of the constraint the index backs, a primary key or
    /// unique constraint: the one written, or on PostgreSQL the one it was
    /// given when it was created, which renaming the table or a column later
    /// leaves alone. A written name is returned as PostgreSQL stores it.
    ///
    /// [`None`] for an index backing no constraint.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// #  fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::{errors::LookupError, prelude::*};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let names = table
    ///     .unique_indices(&db)?
    ///     .map(|key| Ok(key.constraint_name(&db)?.map(|name| name.into_owned())))
    ///     .collect::<Result<Vec<_>, LookupError>>()?;
    /// assert_eq!(names, [Some("users_pkey".to_owned()), Some("users_email_key".to_owned())]);
    /// # Ok(())
    /// # }
    /// ```
    fn constraint_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError>;

    /// Returns the name of the index itself, which the catalog lists among
    /// the indexes of its relation.
    ///
    /// A primary key or unique constraint on SQLite is backed by an index
    /// SQLite names `sqlite_autoindex_<table>_<n>` whatever the constraint is
    /// called, and the constraints whose keys it treats as one share it. The
    /// rowid alias is the primary key SQLite keys without an index, and
    /// answers [`None`]. On PostgreSQL the index takes the constraint's name,
    /// so the answer is [`Self::constraint_name`]. Under SQLite and
    /// PostgreSQL, [`None`] therefore means no index backs the constraint.
    ///
    /// Every other index, and every constraint under any other dialect,
    /// answers [`Self::name`], so there [`None`] means the name is not
    /// modelled, a backing index being possible all the same.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{errors::LookupError, prelude::*};
    /// use sqlparser::dialect::SQLiteDialect;
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>(
    ///     "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT CONSTRAINT uq UNIQUE, UNIQUE (email DESC));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let names = table
    ///     .unique_indices(&db)?
    ///     .map(|key| key.backing_index_name(&db))
    ///     .collect::<Result<Vec<_>, LookupError>>()?;
    /// let shared = "sqlite_autoindex_users_1";
    /// assert_eq!(names.iter().map(Option::as_deref).collect::<Vec<_>>(), [None, Some(shared), Some(shared)]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn backing_index_name<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Option<Cow<'db, str>>, LookupError> {
        self.relation(database)?;
        Ok(self.name().map(Cow::Borrowed))
    }

    /// Returns the expression of the index as an SQL AST node.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::ast::Expr;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE TABLE users (id int, name text); CREATE INDEX idx_name ON users (name);")?;
    /// let table = db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.indices(&db)?.next().unwrap();
    /// let expr = index.expression(&db)?;
    /// let inner = match expr {
    ///     Expr::Nested(inner) => inner,
    ///     _ => expr,
    /// };
    /// assert!(matches!(inner, Expr::Identifier(ident) if ident.value == "name"));
    /// # Ok(())
    /// # }
    /// ```
    fn expression<'db>(&'db self, database: &'db Self::DB) -> Result<&'db Expr, LookupError>
    where
        Self: 'db;

    /// Returns the keys of the index in the order written, each with the sort
    /// direction, the null ordering and the operator class written for it.
    ///
    /// A key's expression is the entry [`Self::expression`] holds at its
    /// position. Two indexes over the same expressions can still differ in
    /// what they serve, so comparing two indexes compares their keys.
    ///
    /// Defaults to one key per entry of [`Self::expression`] with no option
    /// written, which is what every index writing no key options means.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::{errors::LookupError, prelude::*};
    /// use sqlparser::{ast::OrderBySort, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id int, title text);
    ///      CREATE INDEX plain ON docs (title);
    ///      CREATE INDEX descending ON docs (title DESC);",
    /// )?;
    /// let orders = db
    ///     .indexes()
    ///     .map(|index| Ok(index.keys(&db)?.map(|key| key.order().cloned()).collect()))
    ///     .collect::<Result<Vec<Vec<_>>, LookupError>>()?;
    /// assert_eq!(orders, [vec![Some(OrderBySort::Desc)], vec![None]]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn keys<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = IndexKey<'db>>, LookupError>
    where
        Self: 'db,
    {
        Ok(key_expressions(self.expression(database)?).iter().map(IndexKey::new))
    }

    /// Returns the relation the index is built on, a table or a materialized
    /// view.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE users (id int, name text);
    ///      CREATE INDEX users_name ON users (name);
    ///      CREATE MATERIALIZED VIEW names AS SELECT name FROM users;
    ///      CREATE INDEX names_name ON names (name);",
    /// )?;
    /// let on_table = db.indexes().next().unwrap();
    /// assert!(matches!(on_table.relation(&db)?, Relation::Table(table) if table.table_name() == "users"));
    /// let on_view = db.materialized_view_indexes().next().unwrap();
    /// assert!(matches!(
    ///     on_view.relation(&db)?,
    ///     Relation::MaterializedView(view) if view.view_name() == "names"
    /// ));
    /// # Ok(())
    /// # }
    /// ```
    fn relation<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<Relation<'db, Self::DB>, LookupError>
    where
        Self: 'db;

    /// Returns whether the index enforces uniqueness over its key.
    ///
    /// A `CREATE UNIQUE INDEX` answers `true` here while
    /// [`TableLike::unique_indices`] lists only the indexes backing key
    /// constraints, so this is the reader that covers both spellings.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE users (id int, email text);
    ///      CREATE UNIQUE INDEX users_email ON users (email);
    ///      CREATE INDEX users_id ON users (id);",
    /// )?;
    /// let unique: Vec<_> = db.indexes().map(|index| index.is_unique()).collect();
    /// assert_eq!(unique, [true, false]);
    /// # Ok(())
    /// # }
    /// ```
    fn is_unique(&self) -> bool;

    /// Returns whether the index is valid, as `pg_index.indisvalid` reports,
    /// which an index created `ON ONLY` a partitioned table holding a
    /// partition is not.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::{errors::LookupError, prelude::*};
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE r (id int) PARTITION BY RANGE (id);
    ///      CREATE INDEX before_partitions ON ONLY r (id);
    ///      CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (0) TO (10);
    ///      CREATE INDEX after_partitions ON ONLY r (id);",
    /// )?;
    /// let mut valid = db
    ///     .indexes()
    ///     .map(|index| Ok((index.name(), index.is_valid(&db)?)))
    ///     .collect::<Result<Vec<_>, LookupError>>()?;
    /// valid.sort();
    /// assert_eq!(
    ///     valid,
    ///     [
    ///         (Some("after_partitions"), false),
    ///         (Some("before_partitions"), true),
    ///         (Some("r1_id_idx"), true),
    ///     ]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_valid(&self, _database: &Self::DB) -> Result<bool, LookupError> {
        Ok(true)
    }

    /// Returns whether every entry of the index is a plain column, written
    /// with or without parentheses, and none is an expression.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE users (id int, name text); CREATE INDEX idx_name ON users (name);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.indices(&db)?.next().unwrap();
    /// assert!(index.is_simple(&db)?);
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE users (id int, name text); CREATE INDEX ON users ((lower(name)), id);",
    /// )?;
    /// assert!(!db.indexes().next().unwrap().is_simple(&db)?);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn is_simple(&self, database: &Self::DB) -> Result<bool, LookupError> {
        Ok(key_expressions(self.expression(database)?).iter().all(|entry| {
            let mut column = entry;
            while let Expr::Nested(inner) = column {
                column = inner;
            }
            matches!(column, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
        }))
    }

    /// Returns the names of the columns the index carries in its `INCLUDE`
    /// list, in the order written, each with whether it was quoted.
    ///
    /// They are stored alongside the key for index-only scans and take no part
    /// in its ordering or its uniqueness. A name is one the relation exposes,
    /// a column of a table or an output column of a materialized view, so two
    /// lists on one relation compare under the database's identifier rules.
    ///
    /// Defaults to none, which is what every index not spelling `INCLUDE`
    /// means.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE users (id int, name text, email text);
    ///      CREATE MATERIALIZED VIEW contacts AS SELECT id, name, email AS \"Email\" FROM users;
    ///      CREATE INDEX contacts_name ON contacts (name) INCLUDE (\"Email\", id);",
    /// )?;
    /// let index = db.materialized_view_indexes().next().unwrap();
    /// let included: Vec<_> = index.include_column_names().collect();
    /// assert_eq!(included, [("Email", true), ("id", false)]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn include_column_names(&self) -> impl Iterator<Item = (&str, bool)> {
        core::iter::empty()
    }

    /// Returns the access method the index was written with, or `None` when
    /// the statement named none and the engine default applies.
    ///
    /// A default is kept apart from a written method even where the two agree,
    /// as `btree` does on PostgreSQL, because only the statement can say which
    /// one it was.
    ///
    /// Defaults to `None`, which is what every index not spelling `USING`
    /// means.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::IndexType, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE docs (id int, body tsvector);
    ///      CREATE INDEX docs_body ON docs USING gin (body);
    ///      CREATE INDEX docs_id ON docs (id);",
    /// )?;
    /// let methods: Vec<_> = db.indexes().map(|index| index.access_method()).collect();
    /// assert_eq!(methods, [Some(&IndexType::GIN), None]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn access_method(&self) -> Option<&IndexType> {
        None
    }

    /// Returns the predicate of a partial index, the `WHERE` clause choosing
    /// the rows the index holds, or `None` for an index over every row.
    ///
    /// Defaults to `None`, which is what every index not spelling `WHERE`
    /// means.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE orders (id int, shipped boolean);
    ///      CREATE INDEX pending ON orders (id) WHERE NOT shipped;",
    /// )?;
    /// let index = db.indexes().next().unwrap();
    /// assert_eq!(index.predicate().map(ToString::to_string).as_deref(), Some("NOT shipped"));
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn predicate(&self) -> Option<&Expr> {
        None
    }

    /// Returns the functions one call in the index's key or predicate may
    /// reach, resolved under the search path of the statement that created
    /// the index.
    ///
    /// `call` is a [`WrittenCall`] of [`Self::expression`] or of
    /// [`Self::predicate`] at any depth, subqueries included. Resolution
    /// follows the rules of
    /// [`PolicyLike::functions_reached_by`](crate::traits::PolicyLike::functions_reached_by).
    /// An expression PostgreSQL's grammar writes like a call, such as
    /// `COALESCE` or `CURRENT_USER`, reaches no function.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index, and [`LookupError::CallNotInExpression`] when `call`
    /// is not a node of its key or predicate, which is matched by address.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::{ast::Expr, dialect::PostgreSqlDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE SCHEMA app;
    ///      CREATE FUNCTION app.ok(x INT) RETURNS BOOLEAN LANGUAGE sql IMMUTABLE AS 'SELECT x > 0';
    ///      CREATE TABLE public.t (x INT);
    ///      SET search_path TO app, public;
    ///      CREATE INDEX positive ON public.t (x) WHERE ok(x);
    ///      SET search_path TO public;",
    /// )?;
    /// let index = db.indexes().next().unwrap();
    /// let Some(Expr::Function(call)) = index.predicate() else { panic!() };
    /// let reached: Vec<_> = index.functions_reached_by(call, &db)?.collect();
    /// assert_eq!(reached.len(), 1);
    /// assert_eq!(reached[0].target_name().to_string(), "app.ok");
    /// # Ok(())
    /// # }
    /// ```
    fn functions_reached_by<'db, 'call>(
        &'db self,
        call: impl Into<WrittenCall<'call>>,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>;

    /// Returns whether the index treats null values as distinct from one
    /// another, so that a unique index admits any number of rows holding
    /// `NULL` in its key.
    ///
    /// `NULLS NOT DISTINCT`, which PostgreSQL accepts from version 15, answers
    /// `false`, and any other index answers `true`, as
    /// `pg_index.indnullsnotdistinct` records it. A primary key cannot write
    /// the clause and answers `true`, which never matters because its columns
    /// are `NOT NULL`.
    ///
    /// Defaults to `true`, which is what every index not spelling
    /// `NULLS NOT DISTINCT` means.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE users (email text, phone text);
    ///      CREATE UNIQUE INDEX users_email ON users (email) NULLS NOT DISTINCT;
    ///      CREATE UNIQUE INDEX users_phone ON users (phone);",
    /// )?;
    /// let distinct: Vec<_> = db.indexes().map(|index| index.nulls_distinct()).collect();
    /// assert_eq!(distinct, [false, true]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn nulls_distinct(&self) -> bool {
        true
    }
}

/// An index built on a table, whose key and `INCLUDE` list name columns the
/// table declares.
pub trait TableIndexLike: IndexLike {
    /// Returns a reference to the table this index belongs to.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE users (id int, name text); CREATE INDEX idx_name ON users (name);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.indices(&db)?.next().unwrap();
    /// assert_eq!(TableIndexLike::table(index, &db).table_name(), "users");
    /// # Ok(())
    /// # }
    /// ```
    fn table<'db>(&'db self, database: &'db Self::DB) -> &'db <Self::DB as DatabaseLike>::Table
    where
        Self: 'db;

    /// Returns the columns which appear in the index.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE TABLE users (id int, name text); CREATE INDEX idx_name ON users (name);",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("users", false), IdentifierCase::AsWritten)?.unwrap();
    /// let index = table.indices(&db)?.next().unwrap();
    /// let columns: Vec<_> = index.columns(&db)?.collect();
    /// assert_eq!(columns.len(), 1);
    /// assert_eq!(columns[0].column_name(), "name");
    /// # Ok(())
    /// # }
    /// ```
    fn columns<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        let table = <Self as TableIndexLike>::table(self, database);
        let expr = self.expression(database)?;

        let all_columns: Vec<&<Self::DB as DatabaseLike>::Column> =
            table.columns(database)?.collect();

        let mut target = TargetName::new(table.table_name(), table.table_name_is_quoted());
        if let Some(schema) = table.table_schema() {
            target = target.with_schema(schema, table.table_schema_is_quoted());
        }

        // A reference this table does not own, or a column it does not
        // declare, leaves the index reporting no columns rather than a wrong
        // one. A creation refuses both before they can be recorded.
        let found_cols: Vec<&<Self::DB as DatabaseLike>::Column> = columns_in_expression(
            expr,
            database.catalog_name(),
            &target,
            &all_columns,
            database.identifier_case(),
        )
        .unwrap_or_default();

        Ok(found_cols.into_iter())
    }

    /// Returns the columns the index carries in its `INCLUDE` list, in the
    /// order written, the table's columns
    /// [`IndexLike::include_column_names`] names.
    ///
    /// Defaults to none, which is what every index not spelling `INCLUDE`
    /// means.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE users (id int, name text, email text);
    ///      CREATE INDEX users_name ON users (name) INCLUDE (email, id);",
    /// )?;
    /// let index = db.indexes().next().unwrap();
    /// let included: Vec<_> = index.include_columns(&db)?.map(|column| column.column_name()).collect();
    /// assert_eq!(included, ["email", "id"]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn include_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Column>, LookupError>
    where
        Self: 'db,
    {
        Ok(core::iter::empty())
    }

    /// Returns the entries of the record SQLite stores for each row of the
    /// index, in the order and with the answers `pragma_index_xinfo` gives,
    /// or `None` when SQLite builds no index for it.
    ///
    /// The written keys come first. A key naming a column, inside any
    /// parentheses and `COLLATE`, holds that column, and any other key holds
    /// its expression. A key compares under the outermost `COLLATE` written
    /// on it, else under the column's declared collation when it holds a
    /// column, else under `BINARY`. Auxiliary entries follow, the rowid of a
    /// rowid table, or the primary key entries of a `WITHOUT ROWID` table
    /// that no key already holds under the same collation. `CREATE INDEX`
    /// appends those in the primary key's direction, and the index of a key
    /// constraint appends them ascending. The primary key of a
    /// `WITHOUT ROWID` table keeps one entry for a column it lists twice
    /// under one collation, and carries every other column the table stores,
    /// under `BINARY`.
    ///
    /// SQLite builds one index for the key constraints of a table that list
    /// the same columns under the same collations, whatever their
    /// directions, and every constraint of such a group answers the entries
    /// of that one index. Its keys take the directions of the constraint
    /// SQLite built it for, the first one declared, except that a
    /// `WITHOUT ROWID` table builds the index of an `INTEGER PRIMARY KEY`
    /// after every other.
    ///
    /// `None` for an index of any other dialect and for the primary key that
    /// aliases the rowid. Defaults to `None`, so an implementation over a
    /// SQLite source must override it.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this index.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::SQLiteDialect;
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>(
    ///     "CREATE TABLE w (a TEXT COLLATE NOCASE, b INT, PRIMARY KEY (a DESC, b)) WITHOUT ROWID;
    ///      CREATE INDEX lowered ON w (lower(a) COLLATE RTRIM);",
    /// )?;
    /// let index = db.indexes().next().unwrap();
    /// let entries: Vec<_> = index
    ///     .sqlite_index_columns(&db)?
    ///     .unwrap()
    ///     .map(|entry| {
    ///         let held = match entry.indexed() {
    ///             SqliteIndexed::Column(column) => column.column_name().to_owned(),
    ///             SqliteIndexed::Expression(expression) => expression.to_string(),
    ///             SqliteIndexed::Rowid => "rowid".to_owned(),
    ///         };
    ///         (held, entry.is_descending(), entry.collation(), entry.is_key())
    ///     })
    ///     .collect();
    /// assert_eq!(
    ///     entries,
    ///     [
    ///         ("lower(a) COLLATE RTRIM".to_owned(), false, "RTRIM", true),
    ///         ("a".to_owned(), true, "NOCASE", false),
    ///         ("b".to_owned(), false, "BINARY", false),
    ///     ]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Constraints listing the same columns under collations equal without
    /// regard to case share one index, which keeps the first one's
    /// direction. The rowid alias has none.
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::SQLiteDialect;
    ///
    /// let db = ParserDB::parse::<SQLiteDialect>(
    ///     "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT COLLATE nocase,
    ///          UNIQUE (a DESC), UNIQUE (a COLLATE NOCASE), UNIQUE (a COLLATE RTRIM));",
    /// )?;
    /// let table =
    ///     db.table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)?.unwrap();
    /// let keys: Vec<_> = table
    ///     .unique_indices(&db)?
    ///     .map(|key| {
    ///         let first = key.sqlite_index_columns(&db)?.and_then(|mut entries| entries.next());
    ///         Ok(first.map(|entry| (entry.is_descending(), entry.collation())))
    ///     })
    ///     .collect::<Result<_, sql_traits::errors::LookupError>>()?;
    /// assert_eq!(
    ///     keys,
    ///     [None, Some((true, "nocase")), Some((true, "nocase")), Some((false, "RTRIM"))]
    /// );
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    fn sqlite_index_columns<'db>(
        &'db self,
        _database: &'db Self::DB,
    ) -> Result<
        Option<impl Iterator<Item = SqliteIndexColumn<'db, <Self::DB as DatabaseLike>::Column>>>,
        LookupError,
    >
    where
        Self: 'db,
    {
        Ok(None::<core::iter::Empty<_>>)
    }
}

/// The entries of an index expression, one per key.
///
/// The expression mirrors the parenthesized list of a parsed index, which
/// nests a single key and makes a tuple of several.
fn key_expressions(expression: &Expr) -> &[Expr] {
    match expression {
        Expr::Tuple(entries) => entries,
        Expr::Nested(entry) => core::slice::from_ref(entry),
        entry => core::slice::from_ref(entry),
    }
}
