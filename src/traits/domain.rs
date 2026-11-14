//! Submodule providing a trait for describing PostgreSQL domains.

use alloc::borrow::Cow;
use core::fmt::Debug;

use sqlparser::ast::{Expr, Function};

use crate::{
    errors::LookupError,
    traits::{DatabaseLike, Metadata},
    utils::identifier_resolution::stored_identifier,
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
        stored_identifier(self.name(), self.name_is_quoted())
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
        self.schema().map(|schema| stored_identifier(schema, self.schema_is_quoted()))
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

    /// Returns the expression the domain defaults to, if any, which a column
    /// of the domain declaring no default of its own takes.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db =
    ///     ParserDB::parse::<PostgreSqlDialect>("CREATE DOMAIN span AS interval DEFAULT '1 day';")?;
    /// let span = db.domains().next().unwrap();
    /// assert_eq!(span.default_expression().map(ToString::to_string).as_deref(), Some("'1 day'"));
    /// # Ok(())
    /// # }
    /// ```
    fn default_expression(&self) -> Option<&Expr>;

    /// Returns the expression of each `CHECK` constraint of the domain, in
    /// the order written, where `VALUE` reads the value being checked.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE DOMAIN short_span AS interval CHECK (VALUE > '0') CHECK (VALUE < '1 day');",
    /// )?;
    /// let short_span = db.domains().next().unwrap();
    /// let checks: Vec<_> = short_span.check_expressions().map(ToString::to_string).collect();
    /// assert_eq!(checks, ["VALUE > '0'", "VALUE < '1 day'"]);
    /// # Ok(())
    /// # }
    /// ```
    fn check_expressions(&self) -> impl Iterator<Item = &Expr>;

    /// Returns the functions one call in the domain's default or checks may
    /// reach, resolved under the search path of the statement that created
    /// the domain.
    ///
    /// `call` is a node of [`Self::default_expression`] or of one of
    /// [`Self::check_expressions`] at any depth, subqueries included.
    /// Resolution follows the rules of
    /// [`PolicyLike::functions_reached_by`](crate::traits::PolicyLike::functions_reached_by).
    /// An expression PostgreSQL's grammar writes like a call, such as
    /// `COALESCE` or `CURRENT_USER`, reaches no function.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::ObjectNotInDatabase`] when `database` does not
    /// hold this domain, and [`LookupError::CallNotInExpression`] when `call`
    /// is not a node of its expressions, which is matched by address.
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
    ///      CREATE FUNCTION app.ok(x INT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT x > 0';
    ///      SET search_path TO app, public;
    ///      CREATE DOMAIN public.checked AS INT CHECK (ok(VALUE));
    ///      SET search_path TO public;",
    /// )?;
    /// let checked = db.domains().next().unwrap();
    /// let Some(Expr::Function(call)) = checked.check_expressions().next() else { panic!() };
    /// let reached: Vec<_> = checked.functions_reached_by(call, &db)?.collect();
    /// assert_eq!(reached.len(), 1);
    /// assert_eq!(reached[0].target_name().to_string(), "app.ok");
    /// # Ok(())
    /// # }
    /// ```
    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>;
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

    #[inline]
    fn default_expression(&self) -> Option<&Expr> {
        (*self).default_expression()
    }

    #[inline]
    fn check_expressions(&self) -> impl Iterator<Item = &Expr> {
        (*self).check_expressions()
    }

    #[inline]
    fn functions_reached_by<'db>(
        &'db self,
        call: &Function,
        database: &'db Self::DB,
    ) -> Result<impl Iterator<Item = &'db <Self::DB as DatabaseLike>::Function> + 'db, LookupError>
    {
        (*self).functions_reached_by(call, database)
    }
}

#[cfg(test)]
mod tests {
    use sqlparser::dialect::PostgreSqlDialect;

    use super::*;
    use crate::prelude::*;

    #[test]
    fn reference_impl_answers_as_the_domain() {
        let db = ParserDB::parse::<PostgreSqlDialect>(
            "CREATE FUNCTION ok(x INT) RETURNS BOOLEAN LANGUAGE sql AS 'SELECT true';
             CREATE DOMAIN checked AS INT DEFAULT 1 CHECK (ok(VALUE));",
        )
        .expect("schema parses");
        let domain = db.domains().next().expect("domain exists");
        let default = <&_ as DomainLike>::default_expression(&domain).map(ToString::to_string);
        assert_eq!(default.as_deref(), Some("1"));
        let checks: Vec<_> = <&_ as DomainLike>::check_expressions(&domain).collect();
        let [Expr::Function(call)] = checks.as_slice() else { panic!("one check calls ok") };
        let reached: Vec<_> = <&_ as DomainLike>::functions_reached_by(&domain, call, &db)
            .expect("the call is in the check")
            .map(|function| function.name().to_owned())
            .collect();
        assert_eq!(reached, ["ok"]);
    }
}
