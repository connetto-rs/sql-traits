//! Trait describing a SQL dialect.
//!
//! Type predicates like `is_bool` and `is_uuid` live here rather than on
//! [`crate::traits::ColumnLike`]. The reason is dialect
//! ambiguity: SQLite has no boolean type and stores booleans as integer
//! affinity, so a column declared `INTEGER` might or might not be a
//! boolean depending on application convention. A `bool`-returning
//! `column.is_bool(&db)` would silently pick one interpretation and
//! surprise users of the other dialect. Forcing every call through
//! `db.dialect().is_bool(&db, column)` makes the returned
//! [`TypeMatchLike`] three-valued shape
//! visible at every call site.
//!
//! [`crate::traits::DatabaseLike::Dialect`] binds to
//! `DialectLike<DB = Self>`, matching how
//! [`crate::traits::ColumnLike`] and its siblings tie back to their `DB`.

use core::{fmt::Debug, hash::Hash};

use sqlparser::dialect::Dialect;

use crate::traits::{DatabaseLike, TypeMatchLike};

/// Dialect-scoped type classification for a database backend.
///
/// Implementers own every dialect-conditional interpretation of column
/// types, so per-dialect quirks (MySQL's `TINYINT(1)` boolean, SQL Server's
/// `UNIQUEIDENTIFIER`, SQLite's affinity-only booleans) never leak into
/// the shared normalizer or the generic column predicates.
pub trait DialectLike: Debug + Clone + Default + Send + Sync + Hash + Eq + Ord + 'static {
    /// The database backend this dialect belongs to, closing the same
    /// mutual-recursion loop as `ColumnLike`, `TableLike`, and the rest of
    /// the `*Like` family: `DatabaseLike::Dialect: DialectLike<DB = Self>`.
    type DB: DatabaseLike<Dialect = Self>;

    /// Three-valued match result produced by this dialect's predicates.
    /// Every stock backend uses [`crate::traits::TypeMatch`],
    /// custom backends can carry richer state (reasons, provenance) by
    /// picking a bespoke type.
    type Match: TypeMatchLike;

    /// Classifies a column as boolean under this dialect.
    ///
    /// # Semantics
    ///
    /// * [`TypeMatchLike::is_yes`]: the DDL declares an unambiguous boolean
    ///   type family (`BOOLEAN`, `BOOL`, MySQL `TINYINT(1)`).
    /// * [`TypeMatchLike::is_maybe`]: the DDL is compatible with a boolean
    ///   under this dialect's storage conventions but is not specific enough to
    ///   guarantee it (SQLite `INTEGER` / `NUMERIC` / `TINYINT`: SQLite stores
    ///   booleans as integer affinity by convention).
    /// * [`TypeMatchLike::is_no`]: the DDL is incompatible with a boolean under
    ///   this dialect.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect, SQLiteDialect};
    ///
    /// fn check<D: sqlparser::dialect::Dialect + Default + 'static>(sql: &str) -> TypeMatch {
    ///     let db = ParserDB::parse::<D>(sql).expect("parse");
    ///     let table = db
    ///         .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .unwrap();
    ///     let col =
    ///         table.column("flag", &db, IdentifierCase::AsWritten).expect("column lookup").unwrap();
    ///     db.dialect().is_bool(&db, col)
    /// }
    ///
    /// // MySQL: TINYINT(1) is the idiomatic boolean spelling.
    /// assert_eq!(check::<MySqlDialect>("CREATE TABLE t (flag TINYINT(1));"), TypeMatch::Yes);
    /// // Postgres: BOOLEAN is the only spelling that reads as boolean.
    /// assert_eq!(check::<PostgreSqlDialect>("CREATE TABLE t (flag BOOLEAN);"), TypeMatch::Yes);
    /// // SQLite: INTEGER holds 0/1 booleans by convention but the DDL is ambiguous.
    /// assert_eq!(check::<SQLiteDialect>("CREATE TABLE t (flag INTEGER);"), TypeMatch::Maybe);
    /// # Ok(()) }
    /// ```
    fn is_bool(
        &self,
        database: &Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Self::Match;

    /// Classifies a column as UUID under this dialect.
    ///
    /// # Semantics
    ///
    /// * [`TypeMatchLike::is_yes`]: the DDL declares a native UUID type family
    ///   (Postgres `UUID`, SQL Server `UNIQUEIDENTIFIER`).
    /// * [`TypeMatchLike::is_maybe`]: the DDL declares a type commonly used to
    ///   hold a UUID by convention but not exclusively so (MySQL `CHAR(36)` /
    ///   `BINARY(16)`, SQLite `TEXT` / `BLOB`).
    /// * [`TypeMatchLike::is_no`]: the DDL is incompatible with a UUID under
    ///   this dialect.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::{MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
    ///
    /// fn check<D: sqlparser::dialect::Dialect + Default + 'static>(sql: &str) -> TypeMatch {
    ///     let db = ParserDB::parse::<D>(sql).expect("parse");
    ///     let table = db
    ///         .table_by_target(TargetName::new("t", false), IdentifierCase::AsWritten)
    ///         .expect("unambiguous lookup")
    ///         .unwrap();
    ///     let col =
    ///         table.column("id", &db, IdentifierCase::AsWritten).expect("column lookup").unwrap();
    ///     db.dialect().is_uuid(&db, col)
    /// }
    ///
    /// // Postgres native UUID.
    /// assert_eq!(check::<PostgreSqlDialect>("CREATE TABLE t (id UUID);"), TypeMatch::Yes);
    /// // SQL Server: UNIQUEIDENTIFIER is the native keyword.
    /// assert_eq!(check::<MsSqlDialect>("CREATE TABLE t (id UNIQUEIDENTIFIER);"), TypeMatch::Yes);
    /// // MySQL: CHAR(36) is a common carrier but ambiguous.
    /// assert_eq!(check::<MySqlDialect>("CREATE TABLE t (id CHAR(36));"), TypeMatch::Maybe);
    /// // SQLite: TEXT is a common carrier but ambiguous.
    /// assert_eq!(check::<SQLiteDialect>("CREATE TABLE t (id TEXT);"), TypeMatch::Maybe);
    /// # Ok(()) }
    /// ```
    fn is_uuid(
        &self,
        database: &Self::DB,
        column: &<Self::DB as DatabaseLike>::Column,
    ) -> Self::Match;

    /// Returns the sqlparser dialect a body written as text is parsed under.
    ///
    /// [`FunctionLike::referenced_tables`](crate::traits::FunctionLike::referenced_tables)
    /// and the analyses beside it parse a body written as a string with it,
    /// so a backend answers the dialect its function bodies are written in.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::{PostgreSqlDialect, SQLiteDialect};
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE TABLE t (id INT);")?;
    /// assert!(db.dialect().sqlparser_dialect().is::<PostgreSqlDialect>());
    /// assert!(!db.dialect().sqlparser_dialect().is::<SQLiteDialect>());
    /// # Ok(())
    /// # }
    /// ```
    fn sqlparser_dialect(&self) -> &dyn Dialect;
}
