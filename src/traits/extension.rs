//! Submodule providing a trait for describing PostgreSQL extensions.

use alloc::borrow::Cow;
use core::fmt::Debug;

use crate::{
    traits::{DatabaseLike, Metadata},
    utils::identifier_resolution::normalize_identifier,
};

/// A trait for types that can be treated as PostgreSQL extensions, the
/// objects `CREATE EXTENSION` installs.
///
/// An extension is named once per database and places the objects it brings
/// in one schema, which is where clients and the search path reach them.
///
/// See the [PostgreSQL documentation](https://www.postgresql.org/docs/current/sql-createextension.html).
pub trait ExtensionLike: Clone + Debug + Metadata + Send + Sync {
    /// The database type the extension belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the extension.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>("CREATE EXTENSION pgcrypto;")?;
    /// let extension = db.extensions().next().unwrap();
    /// assert_eq!(extension.name(), "pgcrypto");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns whether the extension name was quoted in SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this extension: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    #[inline]
    fn stored_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns the schema the extension was created in, or `None` for one
    /// created in the default schema without naming it.
    ///
    /// Ingestion records where the extension landed: the schema `WITH SCHEMA`
    /// names, or else the schema the search path selected. An extension whose
    /// control file pins a schema lands there instead, which SQL text does not
    /// show, so the model records the schema the statement asked for.
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
    /// CREATE EXTENSION hstore;
    /// ",
    /// )?;
    /// let schema_of = |name: &str| db.extensions().find(|e| e.name() == name).unwrap().schema();
    /// assert_eq!(schema_of("pgcrypto"), Some("exts"));
    /// assert_eq!(schema_of("hstore"), None);
    /// // Every PostgreSQL database holds PL/pgSQL.
    /// assert_eq!(schema_of("plpgsql"), Some("pg_catalog"));
    /// # Ok(())
    /// # }
    /// ```
    fn schema(&self) -> Option<&str>;

    /// Returns whether the schema identifier was quoted in SQL.
    ///
    /// This only matters when [`Self::schema`] returns `Some`.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn schema_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this extension's schema.
    #[inline]
    fn stored_schema(&self) -> Option<Cow<'_, str>> {
        self.schema().map(|schema| normalize_identifier(schema, self.schema_is_quoted()))
    }

    /// Returns the version `VERSION` asked for, if the statement named one.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use sql_traits::prelude::*;
    ///
    /// let db = ParserDB::parse::<GenericDialect>(
    ///     "CREATE EXTENSION hstore WITH SCHEMA public VERSION '1.4';",
    /// )?;
    /// assert_eq!(db.extensions().next().unwrap().version(), Some("1.4"));
    /// # Ok(())
    /// # }
    /// ```
    fn version(&self) -> Option<&str>;
}

impl<T: ExtensionLike> ExtensionLike for &T {
    type DB = T::DB;

    fn name(&self) -> &str {
        (*self).name()
    }

    fn name_is_quoted(&self) -> bool {
        (*self).name_is_quoted()
    }

    fn schema(&self) -> Option<&str> {
        (*self).schema()
    }

    fn schema_is_quoted(&self) -> bool {
        (*self).schema_is_quoted()
    }

    fn version(&self) -> Option<&str> {
        (*self).version()
    }
}
