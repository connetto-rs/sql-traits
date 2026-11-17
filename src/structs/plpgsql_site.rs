//! The SQL a PL/pgSQL body runs, each piece with the names it reaches.

use alloc::vec::Vec;

use crate::{
    errors::LookupError,
    impls::{SiteSource, dql::scoped_roots::ScopedRoot},
    structs::{EmbeddedSql, ExpressionContext, ExpressionScope, IdentifierCase, PlPgSqlScope},
    traits::DatabaseLike,
};

/// The statements and expressions of a PL/pgSQL body, owning the body they
/// borrow from.
///
/// [`FunctionLike::plpgsql_sites`](crate::traits::FunctionLike::plpgsql_sites)
/// parses the body into it, and [`Self::sites`] reads them.
#[derive(Debug)]
pub struct PlPgSqlSites<'db, DB: DatabaseLike> {
    source: SiteSource<'db, DB>,
}

impl<'db, DB: DatabaseLike> PlPgSqlSites<'db, DB> {
    pub(crate) const fn new(source: SiteSource<'db, DB>) -> Self {
        Self { source }
    }

    /// Returns every statement and expression the body runs, in body order,
    /// as [`FunctionLike::plpgsql_sites`](crate::traits::FunctionLike::plpgsql_sites)
    /// describes.
    ///
    /// # Errors
    ///
    /// Returns [`LookupError::FunctionBodyUnresolvable`] for an `ALIAS FOR`
    /// naming no variable and for an `OPEN` naming no cursor declared with a
    /// query, and the errors reading the path a statement of the body sets.
    pub fn sites(&self) -> Result<Vec<PlPgSqlSite<'_, 'db, DB>>, LookupError> {
        self.source.plpgsql_sites()
    }
}

/// A statement or expression a PL/pgSQL body runs, with the variables in
/// reach where PostgreSQL 18 compiles it and the path it runs under.
#[derive(Debug, Clone)]
pub struct PlPgSqlSite<'b, 'db, DB: DatabaseLike> {
    sql: EmbeddedSql<'b>,
    scope: PlPgSqlScope<'b, 'db, DB>,
    database: &'db DB,
}

impl<'b, 'db, DB: DatabaseLike> PlPgSqlSite<'b, 'db, DB> {
    pub(crate) const fn new(
        sql: EmbeddedSql<'b>,
        scope: PlPgSqlScope<'b, 'db, DB>,
        database: &'db DB,
    ) -> Self {
        Self { sql, scope, database }
    }

    /// Returns the SQL, a statement of the body or the `SELECT` evaluating
    /// one of its expressions.
    #[must_use]
    pub const fn sql(&self) -> EmbeddedSql<'b> {
        self.sql
    }

    /// Returns the context typing the [`sql`](Self::sql), its column
    /// references bound as [`ExpressionScope::PlPgSql`] describes.
    /// Identifiers compare under `case`.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors resolving the SQL runs into.
    pub fn context(
        &self,
        case: IdentifierCase,
    ) -> Result<ExpressionContext<'_, 'db, DB>, LookupError> {
        let root = match self.sql {
            EmbeddedSql::Statement(statement) => ScopedRoot::Statement(statement),
            EmbeddedSql::Expression(query) => ScopedRoot::Query(query),
        };
        ExpressionContext::for_root(root, &self.scope(), self.database, case)
    }

    /// Returns the variables in reach of the SQL, as an expression scope.
    #[must_use]
    pub const fn scope(&self) -> ExpressionScope<'_, 'db, DB> {
        ExpressionScope::PlPgSql(&self.scope)
    }

    /// Returns the variables in reach of the SQL.
    #[must_use]
    pub const fn plpgsql_scope(&self) -> &PlPgSqlScope<'b, 'db, DB> {
        &self.scope
    }
}
