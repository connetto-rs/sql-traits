//! The SQL a PL/pgSQL body runs, each piece with the names it reaches.

use alloc::{sync::Arc, vec::Vec};

use crate::{
    errors::LookupError,
    impls::{SiteSource, dql::scoped_roots::ScopedRoot},
    structs::{
        EmbeddedSql, ExpressionContext, ExpressionScope, IdentifierCase, PlPgSqlScope, SiteFlow,
        plpgsql_flow::{VariableRead, read_variables},
    },
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
    flow: Arc<SiteFlow<'b, 'db, DB>>,
    database: &'db DB,
}

impl<'b, 'db, DB: DatabaseLike> PlPgSqlSite<'b, 'db, DB> {
    pub(crate) const fn new(
        sql: EmbeddedSql<'b>,
        scope: PlPgSqlScope<'b, 'db, DB>,
        flow: Arc<SiteFlow<'b, 'db, DB>>,
        database: &'db DB,
    ) -> Self {
        Self { sql, scope, flow, database }
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

    /// Returns the variables the [`sql`](Self::sql) reads, each once in the
    /// order first read, with the definitions that may reach the site.
    /// Identifiers compare under `case`.
    ///
    /// A variable is read whole or through a field, by any name reaching
    /// it, an alias or a label-qualified name included, and not where a
    /// column of the same name binds the reference. Its definitions are what
    /// each path from the start of the body to the site may have left in
    /// it, through branches, loops, `EXIT`, `CONTINUE`, `RETURN` and the
    /// errors a handler catches. The value flow of the body is computed once
    /// for every site of one [`PlPgSqlSites::sites`] call.
    ///
    /// # Errors
    ///
    /// Returns the relation-name lookup errors building the
    /// [`context`](Self::context) runs into,
    /// [`LookupError::FunctionBodyUnresolvable`] for a write whose target
    /// names no variable, and the errors [`PlPgSqlSites::sites`] returns.
    ///
    /// # Example
    ///
    /// ```rust
    /// # fn main() -> Result<(), sql_traits::errors::Error> {
    /// use sql_traits::prelude::*;
    /// use sqlparser::dialect::PostgreSqlDialect;
    ///
    /// let db = ParserDB::parse::<PostgreSqlDialect>(
    ///     "CREATE TABLE team_members (team_id INT, user_id INT);
    ///      CREATE FUNCTION can_edit(doc_team INT) RETURNS BOOLEAN LANGUAGE plpgsql STABLE AS $$
    ///      DECLARE me INT := current_setting('app.user_id')::int; teams INT[];
    ///      BEGIN
    ///          IF doc_team < 0 THEN
    ///              me := doc_team;
    ///          END IF;
    ///          teams := ARRAY(SELECT team_id FROM team_members WHERE user_id = me);
    ///          RETURN doc_team = ANY (teams);
    ///      END $$;",
    /// )?;
    /// let can_edit = db
    ///     .function_by_target(TargetName::new("can_edit", false), IdentifierCase::AsWritten)?
    ///     .expect("Function should exist");
    /// let body = can_edit.plpgsql_sites(&db)?;
    /// let sites = body.sites()?;
    /// let reads = sites[3].reads(IdentifierCase::Folded)?;
    /// let [me] = reads.as_slice() else { panic!("me is read") };
    /// assert_eq!(me.variable().name(), "me");
    /// let Reaching::Definitions(definitions) = me.reaching() else { panic!("me is reached") };
    /// let values: Vec<String> = definitions
    ///     .iter()
    ///     .map(|definition| {
    ///         let DefinitionSource::Value(value) = definition.source() else { panic!("assigned") };
    ///         value.to_string()
    ///     })
    ///     .collect();
    /// assert_eq!(values, ["current_setting('app.user_id')::INT", "doc_team"]);
    /// let inputs = definitions[1].reads(IdentifierCase::Folded)?;
    /// let [argument] = inputs.as_slice() else { panic!("doc_team is read") };
    /// assert!(matches!(argument.variable().kind(), VariableKind::Parameter { position: 1, .. }));
    ///
    /// let returned = sites[4].reads(IdentifierCase::Folded)?;
    /// let names: Vec<&str> = returned.iter().map(|read| read.variable().name()).collect();
    /// assert_eq!(names, ["doc_team", "teams"]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn reads(
        &self,
        case: IdentifierCase,
    ) -> Result<Vec<VariableRead<'_, 'db, DB>>, LookupError> {
        let mut variables = Vec::new();
        read_variables(&self.context(case)?, &self.scope, &self.sql, &mut variables)?;
        self.flow.reads(&self.scope, variables)
    }
}
