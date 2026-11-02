//! Submodule providing a trait for describing PostgreSQL domains.

use alloc::borrow::Cow;
use core::fmt::Debug;

use crate::{
    traits::{DatabaseLike, Metadata},
    utils::identifier_resolution::normalize_identifier,
};

/// A trait for types that can be treated as PostgreSQL domains, the types
/// `CREATE DOMAIN` defines over another type.
///
/// A domain is a type of its own that compares, sorts and indexes as the
/// type it is defined over, its base type, while its constraints narrow the
/// values it holds.
/// [`DataTypeLike::base_type`](crate::traits::DataTypeLike::base_type)
/// follows a column's type through its domains to that base type.
///
/// See the [PostgreSQL documentation](https://www.postgresql.org/docs/current/sql-createdomain.html).
pub trait DomainLike: Clone + Debug + Metadata + Send + Sync {
    /// The database type the domain belongs to.
    type DB: DatabaseLike;

    /// Returns the name of the domain.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>("CREATE DOMAIN span AS interval;")?;
    /// assert_eq!(db.domains().next().unwrap().name(), "span");
    /// # Ok(())
    /// # }
    /// ```
    fn name(&self) -> &str;

    /// Returns whether the domain name was quoted in SQL.
    ///
    /// The default `false` folds every identifier to lowercase, so an
    /// implementation over a source that preserves quoting must override it.
    #[inline]
    fn name_is_quoted(&self) -> bool {
        false
    }

    /// Returns the name PostgreSQL stores for this domain: an unquoted
    /// identifier folds to lowercase, a quoted one keeps its case.
    #[inline]
    fn stored_name(&self) -> Cow<'_, str> {
        normalize_identifier(self.name(), self.name_is_quoted())
    }

    /// Returns the schema the domain was created in, or `None` for one
    /// created in the default schema.
    ///
    /// Ingestion records where the domain landed: the schema its name
    /// qualifies it with, or else the schema the search path selected.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE SCHEMA units;
    /// CREATE DOMAIN units.span AS interval;
    /// CREATE DOMAIN amount AS NUMERIC(10, 2);
    /// ",
    /// )?;
    /// let schema_of = |name: &str| db.domains().find(|d| d.name() == name).unwrap().schema();
    /// assert_eq!(schema_of("span"), Some("units"));
    /// assert_eq!(schema_of("amount"), None);
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

    /// Returns the name PostgreSQL stores for this domain's schema.
    #[inline]
    fn stored_schema(&self) -> Option<Cow<'_, str>> {
        self.schema().map(|schema| normalize_identifier(schema, self.schema_is_quoted()))
    }

    /// Returns the type the domain is declared over, as written.
    ///
    /// The type may itself name a domain, so this is one step towards the
    /// base type that
    /// [`DataTypeLike::base_type`](crate::traits::DataTypeLike::base_type)
    /// reaches in one call.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "
    /// CREATE DOMAIN span AS interval;
    /// CREATE DOMAIN short_span AS span CHECK (VALUE < '1 day');
    /// ",
    /// )?;
    /// let short_span = db.domains().find(|domain| domain.name() == "short_span").unwrap();
    /// let declared = short_span.data_type(&db);
    /// assert_eq!(declared.name(&db), "span");
    /// assert_eq!(declared.base_type(&db).unwrap().name(&db), "INTERVAL");
    /// # Ok(())
    /// # }
    /// ```
    fn data_type<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> &'db <Self::DB as DatabaseLike>::DataType;
}

impl<T: DomainLike> DomainLike for &T {
    type DB = T::DB;

    #[inline]
    fn name(&self) -> &str {
        (*self).name()
    }

    #[inline]
    fn name_is_quoted(&self) -> bool {
        (*self).name_is_quoted()
    }

    #[inline]
    fn schema(&self) -> Option<&str> {
        (*self).schema()
    }

    #[inline]
    fn schema_is_quoted(&self) -> bool {
        (*self).schema_is_quoted()
    }

    #[inline]
    fn data_type<'db>(
        &'db self,
        database: &'db Self::DB,
    ) -> &'db <Self::DB as DatabaseLike>::DataType {
        (*self).data_type(database)
    }
}
